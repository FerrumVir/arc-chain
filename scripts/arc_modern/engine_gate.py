#!/usr/bin/env python3
"""CPU engine gate and speed report for the dyadic profile (SmolLM3-3B).

  check   compare `arc-modern golden` runs with the pinned golden digests of the
          independent Python executor: package, every case's tokens, output
          hash, logits digest and every per-position logits hash. Any
          difference fails, with the first diverging position. SIMD runs must
          also carry a well-formed census that shows SIMD work was done
          (attempted > 0, accepted > 0, attempted = accepted + refusals), fast
          SIMD runs an attention census with SIMD heads, and with
          --fail-on-refusal zero refusals and zero reference-attention heads
          (no silent scalar fallback). --expect-kernel names the kernel this
          runner must resolve SIMD specs to.
  report  render `arc-modern bench` results as Markdown: decode tok/s per
          spec, the per-change ladder, thread scaling, measured read
          bandwidth and utilisation, the phase profile of a decoded token,
          kernel throughput, and device projections (labelled as such).

Standard library only. Every number printed comes from the JSON files given;
projections state their inputs.
"""

from __future__ import annotations

import argparse
import json
import sys
from pathlib import Path

# CPU-achievable read bandwidth (GB/s) by device class, from
# outputs/arc-planet-scale-20261005/sources/research-8-engine-speed.md §2.3
# ("Model inputs"). These are assumptions for projections, not measurements.
DEVICE_BANDWIDTH = [
    ("Apple M1 (CPU)", 60.0),
    ("Apple M2 / M3 base (CPU)", 90.0),
    ("Apple M4 base (CPU)", 103.0),
    ("Apple M Pro (CPU)", 150.0),
    ("Apple M Max (CPU cluster)", 200.0),
    ("PC, dual-channel DDR4", 42.0),
    ("PC, dual-channel DDR5", 72.0),
]

# The order in which a spec's changes were introduced (per-change ladder).
LADDER = [
    ("ref:scalar", "reference forward, scalar kernel (PR #137 `scalar`)"),
    ("ref:legacy", "reference forward, legacy limb kernel (PR #137 `simd`)"),
    ("ref:{best}", "reference forward, new exact kernel (kernel change only)"),
    ("fast:{best}", "fast engine + new kernel (fused projections, SIMD attention, no allocation)"),
]


def load(path: str) -> dict:
    return json.loads(Path(path).read_text())


def compare_run(golden: dict, run: dict) -> list[str]:
    problems = []
    if run.get("schema") != "arc.modern-run.v1":
        return [f"not an arc.modern-run.v1 file (schema {run.get('schema')})"]
    gpkg = golden.get("package", {}).get("sha256")
    rpkg = run.get("package", {}).get("sha256")
    if gpkg and gpkg != rpkg:
        problems.append(f"package sha256 {rpkg} != golden {gpkg}")
    gcases = golden.get("cases", [])
    rcases = run.get("cases", [])
    if [c.get("id") for c in gcases] != [c.get("id") for c in rcases]:
        problems.append("case ids differ")
        return problems
    for g, r in zip(gcases, rcases):
        cid = g["id"]
        for field in ("tokens", "output_hash", "logits_digest"):
            if g.get(field) != r.get(field):
                problems.append(f"case {cid}: {field} differs")
        gh = g.get("logits_hashes", [])
        rh = r.get("logits_hashes", [])
        if gh != rh:
            first = next(
                (i for i, (a, b) in enumerate(zip(gh, rh)) if a != b),
                min(len(gh), len(rh)),
            )
            problems.append(
                f"case {cid}: logits hash differs first at forward call {first} "
                f"(prompt has {len(g.get('prompt_tokens', []))} tokens; {len(gh)} vs {len(rh)} calls)"
            )
    if golden.get("matrix_digest") != run.get("matrix_digest"):
        problems.append(
            f"matrix digest {run.get('matrix_digest')} != golden {golden.get('matrix_digest')}"
        )
    return problems


def census_refusals(run: dict) -> int:
    census = run.get("census")
    if not isinstance(census, dict):
        return 0
    return sum(v for k, v in census.items() if k.startswith("refused") and isinstance(v, int))


def _count(table: object, field: str) -> int | None:
    value = table.get(field) if isinstance(table, dict) else None
    # bool is an int subclass; a census count is never a bool.
    if isinstance(value, bool) or not isinstance(value, int) or value < 0:
        return None
    return value


def dispatch_problems(run: dict, fail_on_refusal: bool, expect_kernel: str | None) -> list[str]:
    """What a run's dispatch evidence fails to show (empty: sufficient).

    Scalar runs need none. A SIMD run (legacy or exact kernel) must carry a
    census in which SIMD work happened and every attempt is either accepted
    or a counted refusal; a fast-engine SIMD run must also carry the attention
    census. A missing, zero or malformed census proves nothing about which
    kernel produced the digests, so it fails (ARC-54 finding 4).
    """
    path = run.get("kernel_path")
    if path in (None, "scalar"):
        return []
    problems = []
    if expect_kernel and path != "legacy" and path != expect_kernel:
        problems.append(f"kernel path {path}, expected {expect_kernel} on this runner")
    census = run.get("census")
    attempted = _count(census, "attempted")
    accepted = _count(census, "accepted")
    refused_fields = (
        [k for k in census if k.startswith("refused")] if isinstance(census, dict) else []
    )
    refused = [_count(census, k) for k in refused_fields]
    if attempted is None or accepted is None or not refused_fields or None in refused:
        problems.append(f"census missing or malformed ({census!r})")
    else:
        if attempted == 0 or accepted == 0:
            problems.append(f"census shows no SIMD work (attempted {attempted}, accepted {accepted})")
        if attempted != accepted + sum(refused):
            problems.append(
                f"census inconsistent: attempted {attempted} != accepted {accepted} "
                f"+ refused {sum(refused)}"
            )
        if fail_on_refusal and sum(refused):
            problems.append(f"{sum(refused)} SIMD refusals (scalar fallback ran)")
    if run.get("engine") == "fast" and path != "legacy":
        attention = run.get("attention_census")
        simd_heads = _count(attention, "simd_heads")
        reference_heads = _count(attention, "reference_heads")
        if simd_heads is None or reference_heads is None:
            problems.append(f"attention census missing or malformed ({attention!r})")
        else:
            if simd_heads == 0:
                problems.append("attention census shows no SIMD heads")
            if fail_on_refusal and reference_heads:
                problems.append(f"{reference_heads} attention heads fell back to the reference")
    return problems


def cmd_check(args: argparse.Namespace) -> int:
    golden = load(args.golden)
    lines = [
        "| run | spec | threads | decode tok/s | prefill tok/s | matrix digest | golden "
        "| SIMD accepted/refused | attention SIMD/reference heads | dispatch evidence |",
        "|---|---|---|---|---|---|---|---|---|---|",
    ]
    failures = 0
    expected_seen = False
    for path in args.runs:
        run = load(path)
        digest_problems = compare_run(golden, run)
        evidence_problems = dispatch_problems(run, args.fail_on_refusal, args.expect_kernel)
        expected_seen |= run.get("kernel_path") == args.expect_kernel
        spec = run.get("spec") or run.get("kernel")
        refused = census_refusals(run)
        census = run.get("census") if isinstance(run.get("census"), dict) else {}
        attention = run.get("attention_census")
        heads = (
            f"{attention.get('simd_heads')}/{attention.get('reference_heads')}"
            if isinstance(attention, dict)
            else "-"
        )
        timing = run.get("timing", {})
        failures += 1 if digest_problems or evidence_problems else 0
        scalar = run.get("kernel_path") in (None, "scalar")
        lines.append(
            f"| {Path(path).name} | `{spec}` | {run.get('threads')} | "
            f"{timing.get('decode_tok_s', 0):.2f} | {timing.get('prefill_tok_s', 0):.2f} | "
            f"`{str(run.get('matrix_digest'))[:16]}…` | "
            f"{'**DIFFERS**' if digest_problems else '**identical**'} | "
            f"{census.get('accepted', 0)}/{refused} | {heads} | "
            f"{'n/a' if scalar else ('**FAIL**' if evidence_problems else 'ok')} |"
        )
        for problem in digest_problems + evidence_problems:
            print(f"FAIL {path}: {problem}", file=sys.stderr)
    missing = bool(args.expect_kernel) and not expected_seen
    if missing:
        print(f"FAIL: no run used the {args.expect_kernel} kernel", file=sys.stderr)
    text = "\n".join(
        [
            f"### Golden digests per forced kernel ({args.label})",
            "",
            f"Golden: `{golden.get('matrix_digest')}` from {golden.get('verifier', 'the Python executor')}.",
            "",
            *lines,
            "",
            f"{len(args.runs) - failures}/{len(args.runs)} runs identical to the golden digests "
            "with sufficient dispatch evidence."
            + (f" **No run used the expected {args.expect_kernel} kernel.**" if missing else ""),
            "",
        ]
    )
    print(text)
    if args.summary_md:
        with open(args.summary_md, "a", encoding="utf-8") as f:
            f.write(text + "\n")
    return 1 if failures or missing else 0


def fmt(x: float, digits: int = 2) -> str:
    return f"{x:.{digits}f}"


def best_kernel(bench: dict) -> str:
    kernels = bench.get("platform", {}).get("kernels_available", [])
    for name in ("avx2", "neon"):
        if name in kernels:
            return name
    return "scalar"


def report_one(bench: dict, label: str) -> list[str]:
    out = [f"### Speed on {label} (CI runner, measured)", ""]
    plat = bench.get("platform", {})
    model = bench.get("model", {})
    out.append(
        f"{plat.get('os')} {plat.get('arch')}, {plat.get('logical_cpus')} logical CPUs, "
        f"kernels {', '.join(plat.get('kernels_available', []))}; "
        f"{model.get('weight_bytes', 0) / 1e9:.3f} GB INT8 weights per token, "
        f"{model.get('kv_bytes_per_position', 0)} KV bytes per position."
    )
    out.append("")
    bw = {b["threads"]: b["read_gb_s"] for b in bench.get("bandwidth", [])}
    if bw:
        out.append("Read bandwidth over the weights (simple parallel sum): " + ", ".join(
            f"{t} threads {fmt(v, 1)} GB/s" for t, v in sorted(bw.items())
        ) + ".")
        out.append("")
    best = best_kernel(bench)
    default_tiling = bench.get("tiling")
    orders = bench.get("row_orders") or {}
    if orders.get("single") or orders.get("batched"):
        out.append(
            f"Row order of the SIMD kernels (`--tiling {default_tiling}`): "
            f"{orders.get('single')} for single-token calls (decode), "
            f"{orders.get('batched')} for batched calls (prefill)."
        )
        out.append("")
    cal = bench.get("tiling_calibration") or {}
    if cal.get("chosen"):
        out.append(
            f"Calibrated on this machine at model load (`{cal.get('kernel')}`, best of {cal.get('passes')} passes "
            f"over {cal.get('weight_bytes', 0) / 1e6:.0f} MB of weights): rows4 {fmt(1000 * cal.get('rows4_seconds', 0), 1)} ms, "
            f"stream {fmt(1000 * cal.get('stream_seconds', 0), 1)} ms, so single-token calls use {cal.get('chosen')}."
        )
        out.append("")
    ladder_specs = [k.format(best=best) for k, _ in LADDER]
    for ctx in bench.get("contexts", []):
        runs = ctx.get("decode", [])
        threads = bench.get("threads")
        main = [
            r for r in runs
            if r.get("threads") == threads and r.get("tiling", default_tiling) == default_tiling
        ]
        by_spec = {r["spec"]: r for r in main}
        out.append(f"#### Context {ctx['context']} positions, {ctx['decode_tokens']} decoded tokens")
        out.append("")
        pre = ctx.get("prefill", {})
        pre_order = f", {pre['row_order']}" if pre.get("row_order") else ""
        out.append(
            f"Prefill of the context (`{pre.get('spec')}`{pre_order}): {fmt(pre.get('tok_s', 0))} tok/s. "
            f"All specs produced identical logits: **{ctx.get('digests_equal')}**."
        )
        out.append("")
        out.append("| change | spec | threads | decode tok/s | vs previous | vs PR #137 simd | effective GB/s | of measured bandwidth |")
        out.append("|---|---|---|---|---|---|---|---|")
        prev = None
        legacy = by_spec.get("ref:legacy", {}).get("tok_s")
        for key, what in LADDER:
            spec = key.format(best=best)
            r = by_spec.get(spec)
            if not r:
                continue
            tok_s = r["tok_s"]
            util = r["effective_gb_s"] / bw[threads] if bw.get(threads) else None
            out.append(
                f"| {what} | `{spec}` | {r['threads']} | **{fmt(tok_s)}** | "
                f"{fmt(tok_s / prev) + '×' if prev else '—'} | "
                f"{fmt(tok_s / legacy) + '×' if legacy else '—'} | {fmt(r['effective_gb_s'], 1)} | "
                f"{fmt(100 * util, 0) + '%' if util else '—'} |"
            )
            prev = tok_s
        others = [r for r in runs if r not in main or r["spec"] not in ladder_specs]
        if others:
            out.append("")
            out.append("| other runs | tiling | threads | decode tok/s | effective GB/s | of measured bandwidth at those threads |")
            out.append("|---|---|---|---|---|---|")
            for r in others:
                util = r["effective_gb_s"] / bw[r["threads"]] if bw.get(r["threads"]) else None
                tiling = r.get("tiling", "—")
                if r.get("row_order") and r["row_order"] != tiling:
                    tiling = f"{tiling} ({r['row_order']})"
                out.append(
                    f"| `{r['spec']}` | {tiling} | {r['threads']} | {fmt(r['tok_s'])} | "
                    f"{fmt(r['effective_gb_s'], 1)} | {fmt(100 * util, 0) + '%' if util else '—'} |"
                )
        prof = ctx.get("profile")
        if prof:
            phases = prof.get("per_token_seconds", {})
            total = prof.get("total_per_token_seconds") or sum(phases.values())
            out.append("")
            out.append(f"Where a decoded token's time goes (`{prof.get('spec')}`, {prof.get('threads')} threads, {fmt(1000 * total, 1)} ms/token):")
            out.append("")
            out.append("| phase | ms/token | share |")
            out.append("|---|---|---|")
            for name, sec in sorted(phases.items(), key=lambda kv: -kv[1]):
                out.append(f"| {name} | {fmt(1000 * sec, 2)} | {fmt(100 * sec / total, 1) if total else '0'}% |")
        out.append("")
    micro = bench.get("kernel_micro", [])
    if micro:
        out.append("#### Kernel throughput on the model's matrices (all threads)")
        out.append("")
        out.append("Small matrices stay in the last-level cache between calls, so their rate shows the kernel's compute ceiling; the LM head (262 MB) streams from memory.")
        out.append("")
        out.append("| kernel | tiling | matrix | rows × cols | digit planes | ms/call | GB/s of weights |")
        out.append("|---|---|---|---|---|---|---|")
        for m in micro:
            out.append(
                f"| {m['kernel']} | {m.get('tiling', '—')} | {m['matrix']} | {m['rows']} × {m['cols']} | "
                f"{m['limbs']} | {fmt(1000 * m['seconds_per_call'], 3)} | {fmt(m['gb_s'], 1)} |"
            )
        out.append("")
    return out


def projections(bench: dict) -> list[str]:
    """Device projections from the measured utilisation of the fastest spec."""
    bw = {b["threads"]: b["read_gb_s"] for b in bench.get("bandwidth", [])}
    threads = bench.get("threads")
    best = best_kernel(bench)
    model = bench.get("model", {})
    weight_bytes = model.get("weight_bytes", 0)
    kv = model.get("kv_bytes_per_position", 0)
    rows = []
    for ctx in bench.get("contexts", []):
        candidates = [
            r for r in ctx.get("decode", [])
            if r["spec"] == f"fast:{best}" and r["threads"] == threads
        ]
        # The fastest measured tiling of the fast engine at full threads.
        run = max(candidates, key=lambda r: r["tok_s"], default=None)
        if not run or not bw.get(threads):
            continue
        util = run["effective_gb_s"] / bw[threads]
        rows.append((ctx["context"], util, run["tok_s"]))
    if not rows:
        return []
    out = [
        "### Device projections (PROJECTION, not measured)",
        "",
        "tok/s ≈ utilisation × device CPU-achievable bandwidth ÷ bytes per token, where utilisation "
        "is the fast engine's effective bandwidth divided by this runner's measured read bandwidth, "
        "and device bandwidths are research-8's §2.3 assumptions. Real devices must be measured "
        "(Proof Kit); thermals, background load and the OS scheduler are not modelled.",
        "",
        "| device | CPU bandwidth assumed | " + " | ".join(f"ctx {c} (util {fmt(100 * u, 0)}%)" for c, u, _ in rows) + " |",
        "|---|---|" + "---|" * len(rows),
    ]
    for name, gbs in DEVICE_BANDWIDTH:
        cells = []
        for ctx, util, _ in rows:
            bytes_per_token = weight_bytes + kv * ctx
            cells.append(fmt(util * gbs * 1e9 / bytes_per_token, 1))
        out.append(f"| {name} | {fmt(gbs, 0)} GB/s | " + " | ".join(cells) + " |")
    out.append("")
    return out


def cmd_report(args: argparse.Namespace) -> int:
    lines = []
    for spec in args.bench:
        label, _, path = spec.partition("=")
        if not path:
            label, path = Path(spec).stem, spec
        if not Path(path).is_file():
            lines.append(f"(no bench file for {label})")
            continue
        bench = load(path)
        lines.extend(report_one(bench, label))
        lines.extend(projections(bench))
    text = "\n".join(lines) + "\n"
    print(text)
    if args.summary_md:
        with open(args.summary_md, "a", encoding="utf-8") as f:
            f.write(text)
    return 0


def main(argv: list[str]) -> int:
    # Tables use characters outside cp1252 (Windows consoles); always emit UTF-8.
    for stream in (sys.stdout, sys.stderr):
        if hasattr(stream, "reconfigure"):
            stream.reconfigure(encoding="utf-8")
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    sub = parser.add_subparsers(dest="command", required=True)
    p = sub.add_parser("check", help="compare golden runs with the pinned digests")
    p.add_argument("--golden", required=True)
    p.add_argument("--label", default="this runner")
    p.add_argument("--fail-on-refusal", action="store_true")
    p.add_argument(
        "--expect-kernel",
        help="kernel (avx2, neon) every exact SIMD run must use; at least one run must",
    )
    p.add_argument("--summary-md")
    p.add_argument("runs", nargs="+")
    p.set_defaults(func=cmd_check)
    p = sub.add_parser("report", help="render bench results")
    p.add_argument("--summary-md")
    p.add_argument("bench", nargs="+", help="BENCH.json or LABEL=BENCH.json")
    p.set_defaults(func=cmd_report)
    args = parser.parse_args(argv)
    return args.func(args)


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
