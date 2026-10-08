#!/usr/bin/env python3
"""Island benchmark report: measured tables plus a labelled Kimi K2 projection.

Usage: report.py BENCH.json [BENCH.json ...] [--out REPORT.md] [--json PROJECTION.json]

Every bench file is `arc-island bench` output, labelled with where it ran
("CI runner", "Studio lab"). The report has three parts:

1. Measured: hop latency (ring pings, no compute), throughput of 1/2/4 stage
   processes, the emulated-WAN sweep, and whether every run was byte-identical
   to the single process.
2. Model check: the pipeline round-time model of research-6 §2.6, fed with the
   measured compute and hop costs, against the measured emulated-WAN runs:
   per answer, steady-state aggregate and wall-clock aggregate, each with its
   own error.
3. Projection (NOT a measurement): Kimi K2 per-island and swarm speed from the
   measured hop cost plus research-6's bandwidth math (§2.1, §2.5, §2.6).
   Kimi K2.6 has the same text shapes as K2 (docs/protocol/kimi-k26-checkpoint.md
   §1: 61 layers, hidden 7,168, 384 experts top-8, expert width 2,048), so the
   numbers apply to K2.6 too. It assumes kernels that run at the effective
   memory bandwidths research-6 calibrated from other engines, and batched
   weight reads across a micro-batch's sequences; ARC's integer engine does
   neither today (the measured engine bandwidth is in part 1).
"""

import json
import math
import sys
from statistics import median

# --- Kimi K2 (research-6 §2.1, from the Hugging Face config) ---------------
KIMI_LAYERS = 61
KIMI_MOE_LAYERS = 60
KIMI_EXPERTS = 384
KIMI_TOPK = 8
KIMI_HIDDEN = 7168
KIMI_BYTES_PER_TOKEN = 22.3e9          # INT4 experts + INT8 elsewhere, batch 1
KIMI_EXPERT_BYTES = 3 * 7168 * 2048 * 0.5625   # one routed expert, INT4 g32
KIMI_ROUTED_BYTES = KIMI_TOPK * KIMI_MOE_LAYERS * KIMI_EXPERT_BYTES
KIMI_SHARED_BYTES = KIMI_BYTES_PER_TOKEN - KIMI_ROUTED_BYTES  # read once per step
KIMI_FLOPS_PER_TOKEN = 2 * 32.7e9
KIMI_KV_PER_TOKEN = 34.3 * 1024        # INT8 MLA cache, all layers
KIMI_WEIGHTS = 582e9                   # INT4 experts + INT8 elsewhere (§2.1)
KV_CONTEXT = 4096                      # ASSUMED tokens of context per sequence
KV_CONTEXTS = [4096, 8192, 32768]      # sensitivity
# Lossless boundary activation: the synthetic MLA models' residual streams fit
# i32 (measured, part 1); a Kimi boundary at i32 is 7168 * 4 bytes.
KIMI_ACTIVATION_BYTES = KIMI_HIDDEN * 4
# Speculative decoding (research-6 §2.7): one draft token per pass, 1.85
# tokens accepted per pass = DeepSeek-V3's MTP acceptance. ASSUMED: Kimi K2 has
# no MTP head, its drafter's acceptance is unknown and its cost is not modelled.
DRAFT_TOKENS_PER_PASS = 1.85
RESEARCH_HOP_S = 0.3e-3                # research-6 §2.5's streaming-transport hop
BILLION_PER_DAY = 1e9 / 86_400

# Devices: effective bandwidth calibrated in research-6 §2.5 (M3 Ultra,
# RTX 5090) or assumed (labelled). FLOPs are an assumption for the
# compute-bound side of batching.
DEVICES = {
    "M3 Ultra 512 GB": {"bw": 456e9, "flops": 26e12, "usable_gb": 410, "source": "research-6 §2.5 (calibrated)"},
    "RTX 5090 32 GB": {"bw": 1108e9, "flops": 200e12, "usable_gb": 27, "source": "research-6 §2.5; 200 TOPS effective assumed"},
    "Mac 64 GB (M4 Pro)": {"bw": 153e9, "flops": 8e12, "usable_gb": 51, "source": "assumed: 56% of 273 GB/s, as research-6 calibrates the M3 Ultra"},
}


def distinct_experts(b):
    """Distinct routed experts per layer touched by b positions (§2.6)."""
    return KIMI_EXPERTS * (1 - (1 - KIMI_TOPK / KIMI_EXPERTS) ** b)


def kimi_step_seconds(b, stages, device, batched=True):
    """One stage's time for a micro-batch of b positions. Batched kernels read
    the shared weights once and each distinct expert once per step; without
    them (this runtime today) every position reads its own weights."""
    d = DEVICES[device]
    if not batched:
        return b * kimi_step_seconds(1, stages, device)
    weight_bytes = KIMI_SHARED_BYTES + KIMI_MOE_LAYERS * distinct_experts(b) * KIMI_EXPERT_BYTES
    memory = weight_bytes / stages / d["bw"]
    compute = b * KIMI_FLOPS_PER_TOKEN / stages / d["flops"]
    return max(memory, compute)


def kimi_wire_bytes(stages):
    """Bytes per position on the busiest uplink: the i32 activation plus the
    full accumulated stage records, item fields and length-prefix/Step framing.
    This envelope conservatively includes all stages even on earlier hops."""
    # Conservative full-record envelope per decode item: all stages' hashes
    # (including duplicated shared boundaries), record headers, head logits /
    # selected token, item fields and a whole length-prefix/Step header per position.
    # Actual early hops carry fewer records; the last carries no activation.
    return KIMI_ACTIVATION_BYTES + 32 * (KIMI_LAYERS + stages) + 17 * stages + 36 + 39 + 17


def kv_fit(device, stages, concurrent, context=KV_CONTEXT):
    """Whether `concurrent` sequences' MLA cache at `context` tokens fits
    beside each device's share of the weights."""
    kv = concurrent * context * KIMI_KV_PER_TOKEN / stages / 1e9
    free = DEVICES[device]["usable_gb"] - KIMI_WEIGHTS / stages / 1e9
    return kv <= free, kv, free


def round_seconds(stages, groups, step, serial, one_way, overhead):
    """research-6 §2.6 with the uplink as a second per-stage resource. A frame
    crosses every stage once per round: S compute steps and S delayed hops,
    of which S - 1 carry the activation (the last stage returns commitments
    only). Each stage handles every micro-batch once per round on its compute
    and on its uplink, which overlap."""
    ring = stages * (step + one_way + overhead) + (stages - 1) * serial
    busy = groups * max(step, serial)
    return max(ring, busy)


def load(paths):
    out = []
    for p in paths:
        with open(p) as f:
            out.append(json.load(f))
    return out


def fmt(x, digits=2):
    if x is None or (isinstance(x, float) and math.isnan(x)):
        return "–"
    if isinstance(x, float):
        return f"{x:,.{digits}f}"
    return f"{x:,}"


def table(headers, rows):
    lines = ["| " + " | ".join(headers) + " |", "|" + "---|" * len(headers)]
    for r in rows:
        lines.append("| " + " | ".join(fmt(c) if not isinstance(c, str) else c for c in r) + " |")
    return "\n".join(lines)


def hop_overhead_ms(bench, payload=28672):
    """Median per-hop software cost (loopback TCP, 2+ stages) at `payload`."""
    xs = [h["per_hop_median_ms"] for h in bench["hops"] if h["payload_bytes"] == payload and h["stages"] >= 2]
    return median(xs) if xs else None


def measured_section(b):
    label = b["label"]
    c = b["model"]["config"]
    plat = b["platform"]
    parts = [f"### {label}: {plat['os']} {plat['arch']}, {plat['logical_cpus']} logical CPUs"]
    parts.append(
        f"Model: synthetic MLA + MoE, {c['n_layers']} layers, d_model {c['d_model']}, "
        f"{c['n_routed_experts']} routed experts top-{c['n_experts_per_tok']}, "
        f"profile `{b['model'].get('profile', '?')}`; package blake3 `{b['model']['package']['blake3'][:16]}…`. "
        f"WAN delay line: {plat.get('wan_timer_mode', '–')}."
    )
    comp = b["compute"]
    parts.append(
        f"Single process, no network: {fmt(comp['single_process_decode_tok_s'])} tok/s decode "
        f"({fmt(comp['single_process_ms_per_position'])} ms per position); engine effective weight "
        f"bandwidth {fmt(comp['effective_weight_gb_s'], 3)} GB/s "
        f"({fmt(comp['active_weight_bytes_per_token'] / 1e6)} MB of active weights per token)."
    )
    w = b["boundary_widths"]
    parts.append(
        f"Boundary activations on the wire (lossless width): {w['width_histogram']} over {w['vectors']} vectors; "
        f"mean {fmt(w['mean_bytes_per_vector'], 0)} B per vector vs {w['i64_bytes_per_vector']} B as i64."
    )
    rows = [
        [h["stages"], h["payload_bytes"], h["ring_hops"], h["ring_median_ms"], h["ring_p90_ms"], h["per_hop_median_ms"] * 1000]
        for h in b["hops"]
    ]
    if rows:
        parts.append("**Hop latency** (ring round trip of a ping frame through every stage process, no compute; loopback TCP):")
        parts.append(table(["stages", "payload B", "hops", "ring median ms", "ring p90 ms", "per hop µs"], rows))
    rows = [
        [t["stages"], t["micro_batches"], t["concurrency"], t["generated_tokens"], t["per_answer_decode_tok_s_mean"],
         t["aggregate_tok_s"], "yes" if t["bit_exact_vs_single_process"] else "**NO**"]
        for t in b["throughput"] if "micro_batches" in t
    ]
    if rows:
        parts.append("**Throughput, stage processes on one host** (decode tok/s per answer, mean; aggregate = generated / wall time):")
        parts.append(table(["stages", "G", "B", "tokens", "per answer tok/s", "aggregate tok/s", "bit-exact"], rows))
    return "\n\n".join(parts)


def wan_section(b):
    rows = []
    err_answer, err_steady, err_wall = [], [], []
    comp = b["compute"]
    t_pos = comp["single_process_ms_per_position"] / 1e3
    o = (hop_overhead_ms(b) or 0.05) / 1e3
    for r in b["wan"]:
        s, d, g, depth = r["stages"], r["one_way_ms"] / 1e3, r["micro_batches"], r["depth"]
        serial = depth * r["wire_bytes_per_position"] * 8 / (r["uplink_mbit"] * 1e6)
        step = depth * t_pos / s
        t = round_seconds(s, g, step, serial, d, o)
        pred_answer, pred_agg = 1 / t, g * depth / t
        err_answer.append((r["per_answer_decode_tok_s_mean"] - pred_answer) / pred_answer)
        err_wall.append((r["aggregate_tok_s"] - pred_agg) / pred_agg)
        steady = r.get("aggregate_steady_tok_s")
        if steady:
            err_steady.append((steady - pred_agg) / pred_agg)
        rows.append([s, r["one_way_ms"], r["uplink_mbit"], depth, r["concurrency"],
                     r["per_answer_decode_tok_s_mean"], pred_answer, steady, r["aggregate_tok_s"], pred_agg,
                     "yes" if r["bit_exact_vs_single_process"] else "**NO**"])
    if not rows:
        return "", {}
    def stat(xs):
        return (f"median {fmt(100 * median(xs), 1)}%, range {fmt(100 * min(xs), 1)}% to {fmt(100 * max(xs), 1)}%"
                if xs else "not recorded")
    errors = {
        "per_answer": err_answer,
        "aggregate_steady": err_steady,
        "aggregate_wall": err_wall,
    }
    tokens = b["wan"][0].get("generated_tokens", 0) // max(b["wan"][0]["concurrency"], 1)
    head = (f"**Emulated WAN, {b['label']}** (every stage's uplink shaped: one-way delay ± 10% jitter, bounded uplink; "
            f"{b['wan'][0]['wire_bytes_per_position']} B of activation per position on every hop but the last "
            f"(a Kimi K2 boundary at i32; commitments ride on top); G = stages micro-batches of `depth` sequences; "
            f"{tokens} tokens per answer). Predicted = research-6 §2.6 round-time model fed with this run's measured "
            f"compute per position and hop overhead; it predicts steady state.\n\n"
            f"Model error (measured − predicted) ÷ predicted, over {len(rows)} runs:\n"
            f"- per answer: {stat(err_answer)};\n"
            f"- aggregate, steady state (tokens while every sequence decodes): {stat(err_steady)};\n"
            f"- aggregate, wall clock (prefill and pipeline fill/drain included): {stat(err_wall)}.")
    return head + "\n\n" + table(
        ["stages", "one-way ms", "uplink Mbit/s", "depth", "B", "per answer tok/s", "predicted",
         "aggregate steady tok/s", "aggregate wall tok/s", "predicted aggregate", "bit-exact"],
        rows), errors


MEASURED_TARGETS = [25, 50, 100, 150, 200]
PROJECTED_TARGETS = [50, 100, 200, 300, 400]


def wan_targets_section(b):
    """The measured curve read the other way: for each emulated link, the
    smallest swept configuration (fewest concurrent sequences, then fewest
    stages) whose steady-state aggregate reached each target, and its
    per-answer speed there."""
    groups = {}
    for r in b["wan"]:
        groups.setdefault((r["one_way_ms"], r["uplink_mbit"]), []).append(r)
    rows = []

    def agg(r):
        return r.get("aggregate_steady_tok_s") or r["aggregate_tok_s"]

    for (ms, mbit), runs in sorted(groups.items()):
        best_seen = max(agg(r) for r in runs)
        for target in MEASURED_TARGETS:
            hits = [r for r in runs if agg(r) >= target]
            if hits:
                r = min(hits, key=lambda r: (r["concurrency"], r["stages"]))
                rows.append([ms, mbit, target, r["stages"], r["depth"], r["concurrency"],
                             r["per_answer_decode_tok_s_mean"], agg(r)])
            else:
                rows.append([ms, mbit, target, "–", "–", "–", "–", f"not reached (best {fmt(best_seen, 0)})"])
    if not rows:
        return ""
    return (f"**Curve, measured on the emulated WAN ({b['label']})**: the smallest swept configuration (2/4/8 stages × "
            "depth 1/4/16, G = stages) whose steady-state aggregate reached each target, with Kimi-sized activations "
            "on the wire. Synthetic model, so compute per stage is small; the network and the uplink set these "
            "numbers.\n\n" + table(
                ["one-way ms", "uplink Mbit/s", "target tok/s", "stages", "depth", "concurrent", "per answer tok/s",
                 "aggregate tok/s"], rows))


def island_answer(device, stages, hop, wire_bps, positions=1):
    """Single-stream verify round, including transfer of every position.

    Charge the full-record envelope on every hop, conservatively including
    the return hop where the runtime actually sends commitments only.
    """
    transfer = positions * kimi_wire_bytes(stages) * 8 / wire_bps
    return round_seconds(stages, 1, kimi_step_seconds(positions, stages, device),
                         0.0, hop + transfer, 0.0)


def projection(benches, errors):
    """Kimi K2 projections from measured hop overhead + research-6 math."""
    overheads = [(b["label"], hop_overhead_ms(b)) for b in benches if hop_overhead_ms(b) is not None]
    label, o_ms = max(overheads, key=lambda x: x[1]) if overheads else ("assumed", 0.3)
    o = o_ms / 1e3
    steady = [e for b in errors for e in b.get("aggregate_steady", [])]
    wall = [e for b in errors for e in b.get("aggregate_wall", [])]
    out = {"hop_overhead_ms": o_ms, "hop_overhead_from": label, "islands": [], "swarm": [], "network": [],
           "kv_sensitivity": [], "curve": [],
           "aggregate_error_steady_median": median(steady) if steady else None,
           "aggregate_error_wall_median": median(wall) if wall else None}
    md = [
        "Everything below is arithmetic, not a measurement, for **Kimi K2**; K2.6 has the same text shapes "
        "(`docs/protocol/kimi-k26-checkpoint.md` §1), so it applies to the requested K2.6 unchanged. No Kimi weights "
        "and no real network were run.",
        f"Inputs: the measured software cost per hop, {fmt(o_ms * 1000, 1)} µs ({label}, loopback TCP, 28 KiB frame; "
        f"the larger of the measured hosts); research-6's memory bandwidths (456 GB/s M3 Ultra, 1,108 GB/s RTX 5090), "
        "which ARC's engine does not reach (see the measured host-specific bandwidths above); batched weight reads across a micro-batch's "
        "sequences, which this runtime does not have (the swarm table also shows per-sequence reads); and "
        f"**{KV_CONTEXT} tokens of context per sequence** (sensitivity table below).",
        "**Aggregates are optimistic.** The same model, fed with measured costs, put the emulated-WAN steady-state "
        f"aggregate error at a median {fmt(100 * median(steady), 1) if steady else '–'}%, and wall-clock "
        f"aggregate error (with prefill and fill/drain) at a median {fmt(100 * median(wall), 1) if wall else '–'}%, "
        "using (measured − predicted) / predicted. Read "
        "every projected aggregate below as an upper bound.",
    ]

    # Islands (T1): pipeline over a LAN or Thunderbolt.
    rows = []
    for device, stages, link, wire_bps in [
        ("M3 Ultra 512 GB", 2, "TB5", 80e9), ("M3 Ultra 512 GB", 4, "TB5", 80e9),
        ("M3 Ultra 512 GB", 2, "10 GbE", 10e9), ("RTX 5090 32 GB", 26, "25 GbE", 25e9),
    ]:
        for b in [1, 8, 32]:
            g = stages if b > 1 else 1
            step = kimi_step_seconds(b, stages, device)
            transfer = b * kimi_wire_bytes(stages) * 8 / wire_bps
            t = round_seconds(stages, g, step, 0.0, o + transfer, 0.0)
            answer, agg = 1 / t, g * b / t
            slow = draft = None
            if b == 1:
                slow = 1 / island_answer(device, stages, RESEARCH_HOP_S, wire_bps)
                draft = DRAFT_TOKENS_PER_PASS / island_answer(device, stages, o, wire_bps, positions=2)
            fits, kv, free = kv_fit(device, stages, g * b)
            rows.append([device, stages, link, b, g * b, answer, slow, draft, agg,
                         "yes" if fits else f"no ({fmt(kv, 1)} > {fmt(free, 1)} GB)"])
            out["islands"].append({"device": device, "stages": stages, "link": link, "depth": b, "concurrency": g * b,
                                   "per_answer_tok_s": answer, "per_answer_at_0_3ms_hop": slow,
                                   "per_answer_one_draft": draft, "aggregate_tok_s": agg})
    md.append("**Islands (T1), pipeline parallel, PROJECTION**. Concurrency = stages × depth (one micro-batch per stage). "
              "\"0.3 ms hop\" replaces the measured loopback hop with research-6's streaming-transport hop (a real NIC, "
              "driver and GPU copies). \"1 draft\" is the model's verify pass over 2 positions (one draft token) at "
              f"{DRAFT_TOKENS_PER_PASS} tokens accepted per pass — DeepSeek-V3's MTP acceptance, ASSUMED: Kimi has no "
              "MTP head; acceptance is unknown and draft generation cost is assumed zero. Transfer scales with "
              "the number of positions, including the two-position verify pass. Full-record wire envelopes "
              "are charged on every hop, conservatively including the return hop.")
    md.append(table(["devices", "stages", "link", "depth", "concurrent", "per answer tok/s", "at 0.3 ms hop",
                     "1 draft", "aggregate tok/s", "KV fits"], rows))

    # Swarm (T2-batch): pipeline across homes, emulated in the measured sweep.
    rows = []
    # 26 GPUs rather than the 22 the weights need: 22 leave ~0.5 GB each for KV.
    for device, stages in [("RTX 5090 32 GB", 26), ("Mac 64 GB (M4 Pro)", 12)]:
        wire = kimi_wire_bytes(stages)
        for one_way_ms in [10, 30, 60]:
            for mbit in [20, 100]:
                ceiling = mbit * 1e6 / (wire * 8)
                for b in [1, 4, 16, 64]:
                    serial = b * wire * 8 / (mbit * 1e6)
                    t = round_seconds(stages, stages, kimi_step_seconds(b, stages, device), serial, one_way_ms / 1e3, o)
                    t_seq = round_seconds(stages, stages, kimi_step_seconds(b, stages, device, batched=False),
                                          serial, one_way_ms / 1e3, o)
                    answer, agg, agg_seq = 1 / t, stages * b / t, stages * b / t_seq
                    fits, kv, free = kv_fit(device, stages, stages * b)
                    rows.append([device, stages, one_way_ms, mbit, b, stages * b, answer, agg, agg_seq, ceiling,
                                 "yes" if fits else f"no ({fmt(kv, 1)} > {fmt(free, 1)} GB)"])
                    out["swarm"].append({"device": device, "stages": stages, "one_way_ms": one_way_ms, "uplink_mbit": mbit,
                                         "depth": b, "concurrency": stages * b, "per_answer_tok_s": answer,
                                         "aggregate_tok_s": agg, "aggregate_per_sequence_reads_tok_s": agg_seq,
                                         "uplink_ceiling_tok_s": ceiling, "kv_fits": fits})
    md.append("**Swarm pipeline across homes (T2-batch), PROJECTION**. G = stages micro-batches of `depth`. Uplink "
              "modeled payload cap = uplink ÷ (bytes per position × 8), using a conservative envelope: activation "
              "28,672 B + hashes 32 × (61 + stages) + stage headers 17 × stages + head logits/token 36 B "
              "+ item fields 39 B + length-prefix/Step header 17 B per position. Early hops carry fewer commitments; "
              "the return hop has no activation. This is a model capacity estimate, not an exact wire trace; "
              "TLS/IP overhead is excluded. \"Per-sequence reads\" = the same with every sequence reading "
              f"its own weights (this runtime today). KV fits = the MLA cache at {KV_CONTEXT} tokens of context fits "
              "beside the device's share of the 582 GB of weights.")
    md.append(table(["devices", "stages", "one-way ms", "uplink Mbit/s", "depth", "concurrent", "per answer tok/s",
                     "aggregate tok/s", "per-sequence reads", "uplink ceiling tok/s", "KV fits"], rows))

    # Context sensitivity: the most aggregate one pipeline reaches before KV runs out.
    rows = []
    for device, stages in [("RTX 5090 32 GB", 26), ("Mac 64 GB (M4 Pro)", 12)]:
        wire = kimi_wire_bytes(stages)
        for one_way_ms in [10, 30, 60]:
            for mbit in [20, 100]:
                row = [device, stages, one_way_ms, mbit]
                for context in KV_CONTEXTS:
                    best = None
                    for b in range(1, 1025):
                        if not kv_fit(device, stages, stages * b, context)[0]:
                            break
                        serial = b * wire * 8 / (mbit * 1e6)
                        t = round_seconds(stages, stages, kimi_step_seconds(b, stages, device), serial,
                                          one_way_ms / 1e3, o)
                        best = (stages * b / t, stages * b, 1 / t)
                    row.append(f"{fmt(best[0], 0)} ({best[1]} seqs, {fmt(best[2])}/answer)" if best else "none fits")
                    out["kv_sensitivity"].append({"device": device, "stages": stages, "one_way_ms": one_way_ms,
                                                  "uplink_mbit": mbit, "context": context,
                                                  "max_aggregate_tok_s": best[0] if best else None,
                                                  "concurrency": best[1] if best else None})
                rows.append(row)
    md.append("**Context sensitivity, PROJECTION**: the most aggregate one swarm pipeline reaches before its KV cache "
              "runs out, by tokens of context held per sequence (agentic traffic is prompt-heavy, so 4k is optimistic).")
    md.append(table(["devices", "stages", "one-way ms", "uplink Mbit/s"] + [f"{c // 1024}k context" for c in KV_CONTEXTS],
                    rows))

    # The curve the other way: what it takes to reach an aggregate.
    rows = []
    for device, stage_options in [("RTX 5090 32 GB", [26, 44]), ("Mac 64 GB (M4 Pro)", [12, 24])]:
        for stages in stage_options:
            wire = kimi_wire_bytes(stages)
            for one_way_ms in [10, 30, 60]:
                for mbit in [20, 100]:
                    ceiling = mbit * 1e6 / (wire * 8)
                    for target in PROJECTED_TARGETS:
                        found = None
                        for b in range(1, 257):
                            if not kv_fit(device, stages, stages * b)[0]:
                                break
                            serial = b * wire * 8 / (mbit * 1e6)
                            t = round_seconds(stages, stages, kimi_step_seconds(b, stages, device), serial,
                                              one_way_ms / 1e3, o)
                            if stages * b / t >= target:
                                found = (b, 1 / t, stages * b / t)
                                break
                        if found:
                            b, answer, agg = found
                            rows.append([device, stages, one_way_ms, mbit, target, b, stages * b, answer, agg])
                        else:
                            why = (f"no: uplink ceiling {fmt(ceiling, 0)}" if target >= ceiling
                                   else "no: KV memory runs out first")
                            rows.append([device, stages, one_way_ms, mbit, target, "–", "–", "–", why])
                        out["curve"].append({"device": device, "stages": stages, "one_way_ms": one_way_ms,
                                             "uplink_mbit": mbit, "target_tok_s": target,
                                             "depth": found[0] if found else None,
                                             "concurrency": stages * found[0] if found else None,
                                             "per_answer_tok_s": found[1] if found else None,
                                             "aggregate_tok_s": found[2] if found else None})
    md.append("**Curve: what one swarm pipeline needs to reach an aggregate, PROJECTION** — the smallest micro-batch "
              "depth (G = stages micro-batches) whose projected aggregate reaches the target with the KV cache fitting "
              f"({KV_CONTEXT} tokens of context, assumed), and the per-answer speed at that point. Upper bounds: the "
              "sequence counts are lower bounds (see the model error above), and batched weight reads are assumed. "
              "Two stage counts per device class: the memory minimum (with KV headroom) and about twice that. More "
              "stages give more KV room, so more sequences can be in flight, but each answer is slower and the uplink "
              "ceiling stays the same: every stage's uplink carries every token.")
    md.append(table(["devices", "stages", "one-way ms", "uplink Mbit/s", "target tok/s", "depth", "concurrent",
                     "per answer tok/s", "aggregate tok/s"], rows))

    # Network scale: pipelines needed for a billion tokens a day.
    rows = []
    best = {}
    for r in out["swarm"]:
        key = (r["device"], r["one_way_ms"], r["uplink_mbit"])
        if r["kv_fits"] and (key not in best or r["depth"] > best[key]["depth"]):
            best[key] = r
    for r in best.values():
        pipelines = math.ceil(BILLION_PER_DAY / r["aggregate_tok_s"])
        rows.append([r["device"], r["stages"], r["one_way_ms"], r["uplink_mbit"], r["depth"], r["concurrency"],
                     r["per_answer_tok_s"], r["aggregate_tok_s"], pipelines, pipelines * r["stages"]])
        out["network"].append({**r, "pipelines_for_1e9_per_day": pipelines, "devices": pipelines * r["stages"]})
    md.append(f"**One billion tokens a day ({fmt(BILLION_PER_DAY, 0)} tok/s sustained), swarm tier, PROJECTION, lower "
              f"bound on devices** (the deepest micro-batch depth whose KV fits at {KV_CONTEXT} tokens; batched weight "
              "reads; decode tokens only, no prefill; 100% utilisation):")
    md.append(table(["devices", "stages", "one-way ms", "uplink Mbit/s", "depth", "concurrent", "per answer tok/s",
                     "aggregate per pipeline", "pipelines", "devices"], rows))
    return out, "\n\n".join(md)


def main(argv):
    out_path = json_path = None
    paths = []
    it = iter(argv)
    for a in it:
        if a == "--out":
            out_path = next(it)
        elif a == "--json":
            json_path = next(it)
        else:
            paths.append(a)
    benches = load(paths)
    failures = [f for b in benches for f in b.get("failures", [])]
    md = ["Historical topology experiments: the LAN/large-machine projections below are not the deployment target. See REGIONAL-SWARM.md and regional CI budgets for the 7 Oct ordinary-node scope.", "## Measured"]
    errors = []
    for b in benches:
        md.append(measured_section(b))
        w, e = wan_section(b)
        if w:
            md.append(w)
            errors.append(e)
        w = wan_targets_section(b)
        if w:
            md.append(w)
    md.append("## Bit-exactness")
    md.append("Every island run above was compared with the single process (tokens, every logits hash, the hash at "
              "every layer boundary of every position): " + ("**all identical**." if not failures else f"**FAILURES**: {failures}"))
    proj, proj_md = projection(benches, errors)
    md.append("## Projection for Kimi K2 / K2.6 (not a measurement)")
    md.append(proj_md)
    text = "\n\n".join(md) + "\n"
    if out_path:
        with open(out_path, "w") as f:
            f.write(text)
    else:
        sys.stdout.write(text)
    if json_path:
        with open(json_path, "w") as f:
            json.dump(proj, f, indent=2)
    return 1 if failures else 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
