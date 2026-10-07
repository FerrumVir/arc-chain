#!/usr/bin/env python3
"""Check pipeline layouts and stage replays against a golden run (spec 6.5).

    python3 scripts/arc_mla/layout_check.py --run RUN.json \
        --layout one=S0.json --layout two=S0.json,S1.json --layout four=A.json,B.json,C.json,D.json \
        [--replay NAME=REPORT.json ...] --out CHECK.json --summary-md CHECK.md

RUN.json is an `arc.mla-run.v1` golden run (whole model, one process). Each
layout lists its stage reports (`arc.mla-stage-run.v1`, from `arc-mla stage`
or `mla_moe_reference stage-replay`) in pipeline order. For every layout:

* every stage's output boundary digests equal the run's boundary digests at
  that layer (the single-process run records all of them);
* every stage after the first consumed exactly the previous stage's output;
* the last stage's logits hashes, re-derived tokens and output hash equal the
  run's.

Each replay report must reproduce the run's boundary digests at its output
layer (and the head outputs when it is the last stage): that is a verifier on
another machine re-running one stage from the committed input boundary.
Exit status 1 on any mismatch.
"""

from __future__ import annotations

import argparse
import json
import sys
from pathlib import Path


def load(path: str) -> dict:
    # Explicit UTF-8: the run file carries the cases' text (Chinese included), and
    # Windows' default codec is cp1252.
    return json.loads(Path(path).read_text(encoding="utf-8"))


def check_stage(run: dict, report: dict, label: str, problems: list) -> dict:
    cases = run["cases"]
    end = report["stage"]["end_layer"]
    first = report["stage"]["first_layer"]
    want = [c["boundary_digests"][end] for c in cases]
    got = report["output"]["digests"]
    ok_boundary = got == want
    if not ok_boundary:
        problems.append(f"{label}: boundary {end} digests differ from the golden run")
    if report["input"]["kind"] == "boundary":
        want_in = [c["boundary_digests"][first] for c in cases]
        if report["input"]["digests"] != want_in:
            problems.append(f"{label}: input boundary {first} digests differ from the golden run")
    head_ok = None
    if report.get("head"):
        head_ok = True
        for case, head in zip(cases, report["head"]):
            tokens = case["tokens"][:case["max_tokens"]]
            if head["logits_hashes"] != case["logits_hashes"]:
                problems.append(f"{label}: case {case['id']} logits hashes differ")
                head_ok = False
            if head["derived_tokens"] != tokens or head["output_hash"] != case["output_hash"]:
                problems.append(f"{label}: case {case['id']} re-derived tokens differ")
                head_ok = False
    platform = report.get("platform") or {}
    return {"label": label, "stage": [first, end], "boundary_match": ok_boundary, "head_match": head_ok,
            "kernel": report.get("kernel"), "os": platform.get("os"), "arch": platform.get("arch"),
            "positions_per_s": (report.get("timing") or {}).get("positions_per_s")}


def main(argv: list) -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--run", required=True)
    parser.add_argument("--layout", action="append", default=[], help="NAME=REPORT.json,REPORT.json,...")
    parser.add_argument("--replay", action="append", default=[], help="NAME=REPORT.json")
    parser.add_argument("--out", required=True)
    parser.add_argument("--summary-md", required=True)
    parser.add_argument("--label", default="")
    args = parser.parse_args(argv)
    run = load(args.run)
    problems: list = []
    layouts = []
    for spec in args.layout:
        name, files = spec.split("=", 1)
        reports = [load(f) for f in files.split(",")]
        rows = [check_stage(run, r, f"{name} stage {i}", problems) for i, r in enumerate(reports)]
        n_layers = len(run["cases"][0]["boundary_digests"]) - 1
        cuts = [r["stage"]["first_layer"] for r in reports] + [reports[-1]["stage"]["end_layer"]]
        if cuts[0] != 0 or cuts[-1] != n_layers or any(
                a["stage"]["end_layer"] != b["stage"]["first_layer"] for a, b in zip(reports, reports[1:])):
            problems.append(f"{name}: stages do not tile [0, {n_layers})")
        for a, b in zip(reports, reports[1:]):
            if a["output"]["digests"] != b["input"].get("digests"):
                problems.append(f"{name}: a stage did not consume the previous stage's output")
        layouts.append({"name": name, "cuts": cuts, "stages": rows})
    replays = []
    for spec in args.replay:
        name, file = spec.split("=", 1)
        replays.append(check_stage(run, load(file), f"replay {name}", problems))
    result = {"schema": "arc.mla-layout-check.v1", "label": args.label, "run_matrix_digest": run["matrix_digest"],
              "run_boundary_matrix_digest": run.get("boundary_matrix_digest"), "layouts": layouts,
              "replays": replays, "problems": problems, "all_match": not problems}
    Path(args.out).write_text(json.dumps(result, indent=1) + "\n", encoding="utf-8")
    lines = [f"### Pipeline layouts and stage replay{(' - ' + args.label) if args.label else ''}", "",
             "| layout | stage | kernel | OS / arch | output boundary = golden | logits + tokens = golden |",
             "|---|---|---|---|---|---|"]
    for layout in layouts:
        for row in layout["stages"]:
            head = "n/a" if row["head_match"] is None else str(row["head_match"])
            lines.append(f"| {layout['name']} {layout['cuts']} | [{row['stage'][0]}, {row['stage'][1]}) | "
                         f"{row['kernel']} | {row['os']}/{row['arch']} | {row['boundary_match']} | {head} |")
    for row in replays:
        head = "n/a" if row["head_match"] is None else str(row["head_match"])
        lines.append(f"| {row['label']} | [{row['stage'][0]}, {row['stage'][1]}) | {row['kernel']} | "
                     f"{row['os']}/{row['arch']} | {row['boundary_match']} | {head} |")
    lines += ["", f"All match: **{not problems}**."]
    lines += [f"- {p}" for p in problems]
    Path(args.summary_md).write_text("\n".join(lines) + "\n", encoding="utf-8")
    print("\n".join(lines))
    return 0 if not problems else 1


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
