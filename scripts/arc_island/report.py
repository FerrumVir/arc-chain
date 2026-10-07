#!/usr/bin/env python3
"""Island benchmark report: measured tables plus a labelled Kimi K2 projection.

Usage: report.py BENCH.json [BENCH.json ...] [--out REPORT.md] [--json PROJECTION.json]

Every bench file is `arc-island bench` output, labelled with where it ran
("CI runner", "Studio lab"). The report has three parts:

1. Measured: hop latency (ring pings, no compute), throughput of 1/2/4 stage
   processes, the emulated-WAN sweep, and whether every run was byte-identical
   to the single process.
2. Model check: the pipeline round-time model of research-6 §2.6, fed with the
   measured compute and hop costs, against the measured emulated-WAN runs.
3. Projection (NOT a measurement): Kimi K2 per-island and swarm speed from the
   measured hop cost plus research-6's bandwidth math (§2.1, §2.5, §2.6). It
   assumes kernels that run at the effective memory bandwidths research-6
   calibrated from other engines; ARC's integer engine does not run at those
   speeds today (the measured engine bandwidth is in part 1).
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
KV_CONTEXT = 4096                      # tokens of context held per sequence
# Lossless boundary activation: the synthetic MLA models' residual streams fit
# i32 (measured, part 1); a Kimi boundary at i32 is 7168 * 4 bytes.
KIMI_WIRE_BYTES = KIMI_HIDDEN * 4
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
    """Distinct routed experts per layer touched by b sequences (§2.6)."""
    return KIMI_EXPERTS * (1 - (1 - KIMI_TOPK / KIMI_EXPERTS) ** b)


def kimi_step_seconds(b, stages, device):
    """One stage's time for a micro-batch of b positions (batched kernels)."""
    d = DEVICES[device]
    weight_bytes = KIMI_SHARED_BYTES + KIMI_MOE_LAYERS * distinct_experts(b) * KIMI_EXPERT_BYTES
    memory = weight_bytes / stages / d["bw"]
    compute = b * KIMI_FLOPS_PER_TOKEN / stages / d["flops"]
    return max(memory, compute)


def kv_fit(device, stages, concurrent):
    """Whether `concurrent` sequences' MLA cache at KV_CONTEXT tokens fits
    beside each device's share of the weights."""
    kv = concurrent * KV_CONTEXT * KIMI_KV_PER_TOKEN / stages / 1e9
    free = DEVICES[device]["usable_gb"] - KIMI_WEIGHTS / stages / 1e9
    return kv <= free, kv, free


def round_seconds(stages, groups, step, serial, one_way, overhead):
    """research-6 §2.6 with the uplink as a second per-stage resource: a frame
    crosses every stage once per round; each stage handles every micro-batch
    once per round on its compute and on its uplink."""
    ring = stages * (step + serial + one_way + overhead)
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
    errors = []
    comp = b["compute"]
    t_pos = comp["single_process_ms_per_position"] / 1e3
    o = (hop_overhead_ms(b) or 0.05) / 1e3
    for r in b["wan"]:
        s, d, g, depth = r["stages"], r["one_way_ms"] / 1e3, r["micro_batches"], r["depth"]
        serial = depth * r["wire_bytes_per_position"] * 8 / (r["uplink_mbit"] * 1e6)
        step = depth * t_pos / s
        t = round_seconds(s, g, step, serial, d, o)
        pred_answer, pred_agg = 1 / t, g * depth / t
        err = (r["per_answer_decode_tok_s_mean"] - pred_answer) / pred_answer
        errors.append(abs(err))
        rows.append([s, r["one_way_ms"], r["uplink_mbit"], depth, r["concurrency"],
                     r["per_answer_decode_tok_s_mean"], pred_answer, r["aggregate_tok_s"], pred_agg,
                     "yes" if r["bit_exact_vs_single_process"] else "**NO**"])
    if not rows:
        return ""
    head = (f"**Emulated WAN, {b['label']}** (every stage's uplink shaped: one-way delay ± 10% jitter, bounded uplink; "
            f"{b['wan'][0]['wire_bytes_per_position']} B per position on the wire = a Kimi K2 boundary at i32; "
            f"G = stages micro-batches of `depth` sequences). Predicted = research-6 §2.6 round-time model fed with this "
            f"run's measured compute per position and hop overhead. Median |error| per answer: "
            f"{fmt(100 * median(errors), 1)}%.")
    return head + "\n\n" + table(
        ["stages", "one-way ms", "uplink Mbit/s", "depth", "B", "per answer tok/s", "predicted", "aggregate tok/s", "predicted", "bit-exact"],
        rows)


def projection(benches):
    """Kimi K2 projections from measured hop overhead + research-6 math."""
    overheads = [(b["label"], hop_overhead_ms(b)) for b in benches if hop_overhead_ms(b) is not None]
    label, o_ms = max(overheads, key=lambda x: x[1]) if overheads else ("assumed", 0.3)
    o = o_ms / 1e3
    out = {"hop_overhead_ms": o_ms, "hop_overhead_from": label, "islands": [], "swarm": [], "network": []}
    md = [f"Measured software cost per hop used below: {fmt(o_ms * 1000, 1)} µs ({label}, loopback TCP, "
          f"28 KiB frame; the larger of the measured hosts). A real NIC adds wire time: "
          f"{fmt(KIMI_WIRE_BYTES * 8 / 10e9 * 1e6, 1)} µs on 10 GbE, {fmt(KIMI_WIRE_BYTES * 8 / 80e9 * 1e6, 1)} µs on TB5 (80 Gb/s)."]

    # Islands (T1): pipeline over a LAN or Thunderbolt.
    rows = []
    for device, stages, link, wire_bps in [
        ("M3 Ultra 512 GB", 2, "TB5", 80e9), ("M3 Ultra 512 GB", 4, "TB5", 80e9),
        ("M3 Ultra 512 GB", 2, "10 GbE", 10e9), ("RTX 5090 32 GB", 26, "25 GbE", 25e9),
    ]:
        h = o + KIMI_WIRE_BYTES * 8 / wire_bps
        for b in [1, 8, 32]:
            g = stages if b > 1 else 1
            step = kimi_step_seconds(b, stages, device)
            t = round_seconds(stages, g, step, 0.0, h, 0.0)
            answer, agg = 1 / t, g * b / t
            spec = answer * 1.34 if b == 1 else None
            fits, kv, free = kv_fit(device, stages, g * b)
            rows.append([device, stages, link, b, g * b, answer, agg, spec,
                         "yes" if fits else f"no ({fmt(kv, 1)} > {fmt(free, 1)} GB)"])
            out["islands"].append({"device": device, "stages": stages, "link": link, "depth": b, "concurrency": g * b,
                                   "per_answer_tok_s": answer, "aggregate_tok_s": agg})
    md.append("**Islands (T1), pipeline parallel, PROJECTION** — per answer and aggregate tok/s; "
              "concurrency = stages × depth (one micro-batch per stage); the last column applies research-6 §2.7's "
              "1.34× for one exact speculative draft (Kimi K2 has no MTP head; needs a drafter).")
    md.append(table(["devices", "stages", "link", "depth", "concurrent", "per answer tok/s", "aggregate tok/s",
                     "with 1 draft", "KV fits"], rows))

    # Swarm (T2-batch): pipeline across homes, emulated in the measured sweep.
    rows = []
    # 26 GPUs rather than the 22 the weights need: 22 leave ~0.5 GB each for KV.
    for device, stages in [("RTX 5090 32 GB", 26), ("Mac 64 GB (M4 Pro)", 12)]:
        for one_way_ms in [10, 30, 60]:
            for mbit in [20, 100]:
                ceiling = mbit * 1e6 / (KIMI_WIRE_BYTES * 8)
                for b in [1, 4, 16, 64]:
                    serial = b * KIMI_WIRE_BYTES * 8 / (mbit * 1e6)
                    step = kimi_step_seconds(b, stages, device)
                    t = round_seconds(stages, stages, step, serial, one_way_ms / 1e3, o)
                    answer, agg = 1 / t, stages * b / t
                    fits, kv, free = kv_fit(device, stages, stages * b)
                    rows.append([device, stages, one_way_ms, mbit, b, stages * b, answer, agg, ceiling,
                                 "yes" if fits else f"no ({fmt(kv, 1)} > {fmt(free, 1)} GB)"])
                    out["swarm"].append({"device": device, "stages": stages, "one_way_ms": one_way_ms, "uplink_mbit": mbit,
                                         "depth": b, "concurrency": stages * b, "per_answer_tok_s": answer,
                                         "aggregate_tok_s": agg, "uplink_ceiling_tok_s": ceiling, "kv_fits": fits})
    md.append("**Swarm pipeline across homes (T2-batch), PROJECTION** — G = stages micro-batches of `depth`; "
              "uplink ceiling = uplink ÷ (28 KiB × 8): every token's activation crosses every stage's uplink, so no "
              f"schedule can exceed it. KV fits = the concurrent sequences' MLA cache at {KV_CONTEXT} tokens of context "
              "(34.3 KiB per token, spread over the stages) fits beside the device's share of the 582 GB of weights.")
    md.append(table(["devices", "stages", "one-way ms", "uplink Mbit/s", "depth", "concurrent", "per answer tok/s",
                     "aggregate tok/s", "uplink ceiling tok/s", "KV fits"], rows))

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
    md.append(f"**One billion tokens a day ({fmt(BILLION_PER_DAY, 0)} tok/s sustained), swarm tier, PROJECTION** "
              "(the deepest micro-batch depth whose KV fits; decode tokens only, no prefill; 100% utilisation):")
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
    md = ["## Measured"]
    for b in benches:
        md.append(measured_section(b))
        w = wan_section(b)
        if w:
            md.append(w)
    md.append("## Bit-exactness")
    md.append("Every island run above was compared with the single process (tokens, every logits hash, the hash at "
              "every layer boundary of every position): " + ("**all identical**." if not failures else f"**FAILURES**: {failures}"))
    proj, proj_md = projection(benches)
    md.append("## Projection for Kimi K2 (not a measurement)")
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
