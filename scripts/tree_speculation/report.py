#!/usr/bin/env python3
"""Validate evidence completeness and emit raw JSON plus the review summary."""
import argparse
import collections
import json
import math
from pathlib import Path


def quantile(xs, q):
    """Linear interpolation between sorted case values (R type 7)."""
    xs = sorted(xs)
    x = (len(xs) - 1) * q
    lo, hi = math.floor(x), math.ceil(x)
    return xs[lo] + (xs[hi] - xs[lo]) * (x - lo)


def summarize(records):
    meta = records[0]
    baseline = [r for r in records if r["kind"] == "baseline"]
    full = [r for r in records if r["kind"] == "full_tree"]
    assert records[-1]["kind"] == "complete", "incomplete measurement"
    assert len(baseline) == 60 and len({r["id"] for r in baseline}) == 60
    assert collections.Counter(r["traffic"] for r in baseline) == {"coding": 20, "agent": 20, "chat": 20}
    subset = {r["id"] for r in baseline if r["full_tree_identity"]}
    assert len(subset) == 6 and len(full) == 18
    assert {(r["id"], r["drafter"]) for r in full} == {(i, d) for i in subset for d in ("lookup", "recycle", "hybrid")}
    for r in baseline:
        assert r["max_tokens"] >= 128
        assert 0 < len(r["output_tokens"]) <= r["max_tokens"]
        assert not any(t in r["eos"] for t in r["output_tokens"][:-1])
        assert len(r["output_tokens"]) == r["max_tokens"] or r["output_tokens"][-1] in r["eos"]
        assert r["verification_passes"] == r["decode_tokens"] == len(r["output_tokens"]) - 1
        assert len(r["logits_hashes"]) == r["prompt_tokens"] + r["decode_tokens"]
    assert all(r["tokens_logits_kv_identical"] for r in full)
    by_id = {r["id"]: r for r in baseline}
    for r in full:
        b = by_id[r["id"]]
        assert r["decode_tokens"] == b["decode_tokens"]
        assert r["output_blake3"] == b["output_blake3"] and r["kv_digest"] == b["kv_digest"]
        if r["drafter"] == "lookup":
            assert (r["verification_passes"], r["physical_rows"], r["path_lowered_rows"]) == tuple(b["lookup_replay"][k] for k in ("passes", "nodes", "expanded_rows"))
    lines = [f"ENG-8 public sample evidence — PR head `{meta['pr_head']}` (tested merge `{meta['head']}`)", "",
        "60 sampled public cases, 20/class; 128-token allowance, EOS honored. Coding: real SWE-bench Verified Django/SymPy issues with oracle-localized old code (no gold additions). Agent: actual SWE-smith tool rollouts on synthetic repair tasks, full prefix through the first tool response; source does not provide tool definitions. Chat: real human OpenAssistant conversations, complete multi-turn prefixes. Sampling populations, pinned revisions, hashes and licenses are in `tree_public/cases.json` and `licenses/manifest.json`. These bounded public samples are not ARC production traffic or Kimi acceptance measurements.", "",
        "All 60 outputs use the target with batched prefill and depth-zero greedy decode. Lookup replay uses only prompt + already-emitted tokens when proposing; future greedy tokens are used only to score the fixed proposal. Recycling/hybrid are NOT replayed from greedy traces: rejected-node logits would be missing. Full shared-node lookup/recycle/hybrid verification runs on the predeclared first two sampled cases/class (6 cases, 18 comparisons); all tokens, every logits hash and committed KV match batched greedy. Those six greedy outputs also match independent serial `ModernModel::generate`. Lookup replay passes/node counts match all six full runs. The other 54 cases have replay acceptance evidence, not full-tree identity checks.", "",
        "Hidden-feature hook: final-layer residual before final RMSNorm, Q16 i64, with source token/position. The next proposal receives only the last committed row (last prompt row initially, last accepted node thereafter), plus the pending root token. A deterministic untrained stub tests integration and exactness for shared-node and fallback paths. No trained EAGLE/Medusa implementation, training run, or claimed head speedup.", "",
        "Timing boundaries: prefill includes cache/row setup, batched prompt forward, feature extraction, prompt hashing/observer updates, and first selection. Decode includes proposals, target verification, hashing, observer updates, accepted-path commit and bookkeeping. Draft and verify subtimes are inside decode; lookup replay time is scoring/drafting CPU only and excludes target inference. Baseline and full-tree runs now use the same batched prefill path. One ordered trial per case, no warmup/repetitions/confidence interval: timing differences are descriptive and cannot establish a speedup. Package loading, tokenization, decoding text, serial identity reference and file writes are outside these timers.", "",
        "| Traffic / mode | n | Decode tokens | Passes | Pooled tokens/pass | Per-case p10 | Median | p90 | Shared rows/pass | Path rows/pass |",
        "|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|"]
    groups = collections.defaultdict(list)
    for r in baseline:
        replay = r["lookup_replay"]
        groups[(r["traffic"], "lookup-replay")].append({"tokens": r["decode_tokens"], "passes": replay["passes"], "rows": replay["nodes"], "expanded": replay["expanded_rows"]})
    for r in full:
        groups[(r["traffic"], r["drafter"] + "-full")].append({"tokens": r["decode_tokens"], "passes": r["verification_passes"], "rows": r["physical_rows"], "expanded": r["path_lowered_rows"]})
    acceptance, projections = [], []
    for (traffic, mode), group in sorted(groups.items()):
        sums = {k: sum(r[k] for r in group) for k in ("tokens", "passes", "rows", "expanded")}
        values = [r["tokens"] / r["passes"] for r in group if r["passes"]]
        # Immediate EOS has no decode pass; exclude from per-case quantiles,
        # report count explicitly rather than assigning an invented rate.
        stats = {"p10": quantile(values, .1), "median": quantile(values, .5), "p90": quantile(values, .9)} if values else {"p10": 0, "median": 0, "p90": 0}
        denom = max(1, sums["passes"])
        rate, rows, expanded = (sums[k] / denom for k in ("tokens", "rows", "expanded"))
        acceptance.append({"traffic": traffic, "mode": mode, "cases": len(group), "quantile_cases": len(values), **sums, "tokens_per_pass": rate, **stats})
        lines.append(f"| {traffic}/{mode} | {len(group)} | {sums['tokens']} | {sums['passes']} | {rate:.3f} | {stats['p10']:.3f} | {stats['median']:.3f} | {stats['p90']:.3f} | {rows:.2f} | {expanded:.2f} |")
        for hops in (8, 16, 32, 60):
            for latency in (10, 30, 60):
                for compute in (0, 50):
                    for gbps in (1, 10):
                        shared_ms = hops * rows * 7168 * 8 * 8 / (gbps * 1e6)
                        path_ms = hops * expanded * 7168 * 8 * 8 / (gbps * 1e6)
                        projections.append({"traffic": traffic, "mode": mode, "hops": hops, "hop_ms": latency, "compute_ms": compute, "link_gbps": gbps,
                            "tokens_per_pass": rate, "rows_per_pass": rows, "path_rows_per_pass": expanded,
                            "latency_only_tok_s": 1000 * rate / (hops * latency + compute),
                            "shared_tok_s": 1000 * rate / (hops * latency + compute + shared_ms),
                            "path_lowered_tok_s": 1000 * rate / (hops * latency + compute + path_ms)})
    lines += ["", "Quantiles interpolate sorted per-case tokens/pass (R type 7); pooled rate is sum(tokens)/sum(passes), not the median. Immediate-EOS cases have no decode rate and are excluded from quantiles (counts are in raw JSON). Full-tree n=2/class is identity coverage and exploratory acceptance only; it does not support population comparisons between drafters.", "",
        "Per-case evidence (case aliases below map in order to the full IDs/raw records). `G` = batched greedy; `L` = lookup replay; all durations seconds. Full-tree tokens/logits/KV identity passed for rows marked yes.", "",
        "| Case | Source ID | Prompt tokens | Output tokens / allowance | G prefill | G decode | L passes | L tokens/pass | L replay time | Full identity |",
        "|---|---|---:|---:|---:|---:|---:|---:|---:|---|"]
    counts, aliases = collections.Counter(), {}
    for r in baseline:
        counts[r["traffic"]] += 1
        alias = f"{r['traffic']}/{counts[r['traffic']]:02}"
        aliases[r["id"]] = alias
        p = r["lookup_replay"]["passes"]
        lines.append(f"| {alias} | `{r['source_id']}` | {r['prompt_tokens']} | {len(r['output_tokens'])}/{r['max_tokens']} | {r['timing']['prefill_seconds']:.3f} | {r['timing']['decode_seconds']:.3f} | {p} | {r['decode_tokens']/p if p else 0:.3f} | {r['lookup_replay']['seconds']:.6f} | {'yes' if r['full_tree_identity'] else 'replay only'} |")
    lines += ["", "| Case / full drafter | Passes | Tokens/pass | Prefill s | Decode s | Draft s | Verify s | G decode s |", "|---|---:|---:|---:|---:|---:|---:|---:|"]
    for r in full:
        t = r["timing"]
        p = r["verification_passes"]
        lines.append(f"| {aliases[r['id']]}/{r['drafter']} | {p} | {r['decode_tokens']/p if p else 0:.3f} | {t['prefill_seconds']:.3f} | {t['decode_seconds']:.3f} | {t['draft_seconds']:.3f} | {t['verify_seconds']:.3f} | {by_id[r['id']]['timing']['decode_seconds']:.3f} |")
    lines += ["", "Network projection, NOT measured throughput: one sequential traversal per verification pass; 8/16/32/60 hops, 10/30/60 ms per hop, 0 or 50 ms assumed compute, 1 or 10 Gbit/s links. Only forward i64 hidden payload is counted (7,168 × 8 bytes/row). Return logits/tokens, serialization and drafting must fit inside the assumed compute term; 50 ms is illustrative, does not scale with rows, and is not a compute estimate. No live network, lossless-codec ratio, or Kimi acceptance was measured. Shorter/faster pipelines and different drafters are possibilities, not proven necessary/sufficient routes to 59 tokens/s.", "",
        "| Traffic / mode | Shared/path row ratio | Projected shared tok/s | Projected path tok/s | Assumed shared/path throughput ratio |", "|---|---:|---:|---:|---:|"]
    for p in projections:
        if (p["hops"], p["hop_ms"], p["compute_ms"], p["link_gbps"]) == (8, 10, 50, 1):
            ratio = p["path_rows_per_pass"] / max(p["rows_per_pass"], 1)
            speed = p["shared_tok_s"] / p["path_lowered_tok_s"] if p["path_lowered_tok_s"] else 0
            lines.append(f"| {p['traffic']}/{p['mode']} | {ratio:.3f}× fewer rows | {p['shared_tok_s']:.3f} | {p['path_lowered_tok_s']:.3f} | {speed:.3f}× |")
    lines += ["", "The last table assumes 8×10 ms, 50 ms compute, 1 Gbit/s; full grid in raw JSON. Shared-node savings depend on the drafter/tree. Historical 2.5–2.8× row and 1.7–1.9× projected throughput factors at `422e1c88` applied to recycle/hybrid, not lookup (roughly 1.05×/1.01×). Old greedy/tree totals used serial versus batched prefill and are not comparable. Studio lab comparisons have been removed because no raw Studio evidence was supplied.", ""]
    return {"metadata": meta, "records": records[1:], "acceptance": acceptance, "projections": projections}, "\n".join(lines)


if __name__ == "__main__":
    parser = argparse.ArgumentParser()
    parser.add_argument("input", type=Path)
    parser.add_argument("--json", type=Path, required=True)
    parser.add_argument("--summary", type=Path, required=True)
    args = parser.parse_args()
    result, summary = summarize([json.loads(line) for line in args.input.read_text().splitlines()])
    args.json.write_text(json.dumps(result, indent=2) + "\n")
    args.summary.write_text(summary)
