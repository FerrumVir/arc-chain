#!/usr/bin/env python3
"""Draft-agreement experiment (scratch branch, not for merge).

teacher-input: turn ARC's recorded generations into the teacher driver's input.
report:        score llama.cpp's teacher-forced argmax against ARC's tokens.

Agreement at position i is 1 when llama.cpp's choice, given ARC's exact prefix,
equals ARC's token i. Three comparisons are scored:
  pen    llama.cpp argmax after ARC's own repetition penalty == ARC's token
         (what a drafter mirroring the worker's deterministic selection proposes)
  raw    llama.cpp raw argmax == ARC's token (plain greedy llama.cpp)
  model  llama.cpp raw argmax == ARC's raw argmax before the penalty
E[J] replays each measured agreement sequence as greedy speculative decoding
with draft length k: a pass accepts the leading agreeing drafts (at most k) and
commits them plus the verifier's own token, so J = accepted + 1 (capped at the
end of the output). E[J] is committed tokens per verification pass, pooled.
"""

import argparse
import glob
import json
import os
import re
import statistics
import sys

KS = [4, 8, 16, 32, 64]
CATEGORIES = ["chat", "code", "math", "longform", "copy"]
PROFILES = ["interleaved", "legacy"]
QUANTS = ["Q4_K_M", "Q3_K_M", "Q2_K", "ARCf32"]
MARGIN_BUCKETS = [(0, 0.25), (0.25, 0.5), (0.5, 1.0), (1.0, 2.0), (2.0, float("inf"))]
VARIANTS = ["pen", "raw", "model"]
BUCKETS = [(1, 1), (2, 3), (4, 7), (8, 15), (16, 31), (32, 63), (64, 10**9)]


def load_arc(arc_dir):
    runs = []
    for path in sorted(glob.glob(os.path.join(arc_dir, "**", "arc-*.json"), recursive=True)):
        with open(path) as f:
            runs.append(json.load(f))
    return runs


def teacher_input(args):
    lines = 0
    with open(args.out, "w") as out:
        for run in load_arc(args.arc):
            if run["profile"] != args.profile:
                continue
            for rec in run["prompts"]:
                fed, gen, first = rec["fed"], rec["generated"], rec["first"]
                assert len(fed) == first + len(gen), rec["id"]
                ident = f"{run['profile']}/{rec['id']}"
                fields = [ident, first, len(gen), len(fed)] + fed + gen
                out.write(" ".join(str(x) for x in fields) + "\n")
                lines += 1
    print(f"{lines} sequences for profile {args.profile}")
    if lines == 0:
        sys.exit("no ARC sequences found")


def maximal_runs(seq):
    out, r = [], 0
    for a in seq:
        if a:
            r += 1
        elif r:
            out.append(r)
            r = 0
    if r:
        out.append(r)
    return out


def replay(seq, k):
    n, s, passes, tokens = len(seq), 0, 0, 0
    while s < n:
        r = 0
        while s + r < n and r < k and seq[s + r]:
            r += 1
        commit = min(r + 1, n - s)
        passes += 1
        tokens += commit
        s += commit
    return passes, tokens


def first_divergence(seq):
    for i, a in enumerate(seq):
        if not a:
            return i
    return len(seq)


def pct(values, q):
    if not values:
        return None
    ordered = sorted(values)
    return ordered[min(len(ordered) - 1, int(q * len(ordered)))]


def summarize(seqs):
    n_tok = sum(len(s) for s in seqs)
    agree = sum(sum(s) for s in seqs)
    p = agree / n_tok if n_tok else None
    runs = [r for s in seqs for r in maximal_runs(s)]
    firsts = [first_divergence(s) for s in seqs]
    ej = {}
    for k in KS:
        passes = tokens = 0
        for s in seqs:
            a, b = replay(s, k)
            passes += a
            tokens += b
        ej[str(k)] = tokens / passes if passes else None
    iid = {
        str(k): (None if p is None else (k + 1 if p == 1 else (1 - p ** (k + 1)) / (1 - p)))
        for k in KS
    }
    return {
        "sequences": len(seqs),
        "tokens": n_tok,
        "agree": agree,
        "p": p,
        "runs": {
            "count": len(runs),
            "mean": statistics.mean(runs) if runs else 0,
            "median": statistics.median(runs) if runs else 0,
            "p90": pct(runs, 0.9) or 0,
            "max": max(runs) if runs else 0,
            "histogram": {
                (f"{lo}+" if hi > 10**8 else (f"{lo}" if lo == hi else f"{lo}-{hi}")): sum(
                    1 for r in runs if lo <= r <= hi
                )
                for lo, hi in BUCKETS
            },
        },
        "ej": ej,
        "ej_iid_from_p": iid,
        "first_divergence": {
            "median": statistics.median(firsts) if firsts else None,
            "mean": statistics.mean(firsts) if firsts else None,
            "min": min(firsts) if firsts else None,
            "max": max(firsts) if firsts else None,
            "never": sum(1 for f, s in zip(firsts, seqs) if f == len(s)),
            "values": firsts,
        },
    }


def fmt(x, digits=2):
    if x is None:
        return "n/a"
    if isinstance(x, float):
        return f"{x:.{digits}f}"
    return str(x)


def report(args):
    arc_runs = load_arc(args.arc)
    arc = {}
    for run in arc_runs:
        for rec in run["prompts"]:
            arc[(run["profile"], rec["id"])] = (run, rec)
    teacher = {}
    for path in sorted(glob.glob(os.path.join(args.teacher, "**", "teacher-*.jsonl"), recursive=True)):
        named = re.fullmatch(
            r"teacher-(Q4_K_M|Q3_K_M|Q2_K|ARCf32)-(interleaved|legacy)(?:-[a-z]+)?\.jsonl",
            os.path.basename(path),
        )
        if not named:
            continue
        quant, profile = named.group(1), named.group(2)
        with open(path) as f:
            for line in f:
                if line.strip():
                    row = json.loads(line)
                    teacher[(quant, profile, row["id"].split("/", 1)[1])] = row

    results, per_prompt, missing = {}, [], []
    ranks, fixed = {}, {}
    for quant in QUANTS:
        for profile in PROFILES:
            for variant in VARIANTS:
                by_cat = {c: [] for c in CATEGORIES}
                for (prof, pid), (run, rec) in sorted(arc.items()):
                    if prof != profile:
                        continue
                    row = teacher.get((quant, profile, pid))
                    if row is None:
                        if variant == "pen":
                            missing.append(f"{quant}/{profile}/{pid}")
                        continue
                    target = rec["arc_raw_argmax"] if variant == "model" else rec["generated"]
                    mine = row["raw"] if variant in ("raw", "model") else row["pen"]
                    assert len(mine) == len(target), (quant, profile, pid)
                    seq = [int(a == b) for a, b in zip(mine, target)]
                    by_cat[rec["category"]].append(seq)
                    if variant == "pen" and "pen_rank" in row:
                        bucket = ranks.setdefault((quant, profile), [])
                        for i, a in enumerate(seq):
                            bucket.append((a, row["pen_rank"][i], row["pen_gap"][i], row["pen_margin"][i]))
                        corrected = [int(a or r == 1) for a, r in zip(seq, row["pen_rank"])]
                        fixed.setdefault((quant, profile), []).append(corrected)
                    if variant == "pen":
                        gaps = [g for g, a in zip(row["pen_gap"], seq) if not a]
                        per_prompt.append(
                            {
                                "quant": quant,
                                "profile": profile,
                                "id": pid,
                                "tokens": len(seq),
                                "p_pen": sum(seq) / len(seq),
                                "p_raw": sum(int(a == b) for a, b in zip(row["raw"], rec["generated"]))
                                / len(seq),
                                "first_divergence": first_divergence(seq),
                                "near_tie_disagreements": sum(1 for g in gaps if g < 1.0),
                                "disagreements": len(gaps),
                                "teacher_s": row.get("seconds"),
                            }
                        )
                if not any(by_cat.values()):
                    continue
                entry = {c: summarize(v) for c, v in by_cat.items() if v}
                entry["all"] = summarize([s for v in by_cat.values() for s in v])
                results.setdefault(quant, {}).setdefault(profile, {})[variant] = entry

    arc_facts = []
    for run in sorted(arc_runs, key=lambda r: (r["profile"], str(r["category"]))):
        recs = run["prompts"]
        arc_facts.append(
            {
                "profile": run["profile"],
                "category": run["category"],
                "prompts": len(recs),
                "tokens": sum(len(r["generated"]) for r in recs),
                "stopped_at_eos": sum(1 for r in recs if r["stopped_at_eos"]),
                "gate_identical": (run.get("gate") or {}).get("identical"),
                "arc_s": run.get("total_s"),
            }
        )

    second = {}
    for key, cells in sorted(ranks.items()):
        misses = [c for c in cells if not c[0]]
        rank1 = [c for c in misses if c[1] == 1]
        top3 = [c for c in misses if c[1] <= 2]
        def med(values):
            return statistics.median(values) if values else None
        buckets = []
        for lo, hi in MARGIN_BUCKETS:
            inside = [c for c in cells if lo <= c[3] < hi]
            miss = [c for c in inside if not c[0]]
            buckets.append({
                "margin": f"{lo}-{hi}",
                "share": len(inside) / len(cells) if cells else None,
                "miss_rate": len(miss) / len(inside) if inside else None,
                "second_choice_of_misses": (sum(1 for c in miss if c[1] == 1) / len(miss)) if miss else None,
            })
        oracle = summarize(fixed.get(key, []))
        second[f"{key[0]}/{key[1]}"] = {
            "positions": len(cells),
            "mismatches": len(misses),
            "second_choice_rate": len(rank1) / len(misses) if misses else None,
            "top3_rate": len(top3) / len(misses) if misses else None,
            "gap_at_second_choice_median": med([c[2] for c in rank1]),
            "gap_at_second_choice_p90": pct([c[2] for c in rank1], 0.9),
            "gap_at_other_misses_median": med([c[2] for c in misses if c[1] != 1]),
            "margin_median_at_matches": med([c[3] for c in cells if c[0]]),
            "margin_median_at_misses": med([c[3] for c in misses]),
            "margin_buckets": buckets,
            "corrector_oracle_p": oracle["p"],
            "corrector_oracle_ej": oracle["ej"],
        }

    out = {
        "label": "MEASURED on CI runners",
        "second_choice": second,
        "run_id": args.run_id,
        "definitions": __doc__,
        "arc": arc_facts,
        "results": results,
        "per_prompt": per_prompt,
        "missing": missing,
    }
    with open(args.out_json, "w") as f:
        json.dump(out, f, indent=1)

    md = []
    md.append(f"# Same-model draft agreement, llama.cpp vs ARC exact engine (MEASURED, CI run {args.run_id})")
    md.append("")
    md.append(
        "Teacher-forced on ARC's exact context. p = per-token agreement; runs = maximal runs of "
        "agreeing tokens; E[J] = committed tokens per verification pass (accepted drafts + the "
        "verifier's token) replaying the measured sequences with draft length k; first div = index "
        "of the first disagreeing generated token (median, mean). pen = llama.cpp with ARC's "
        "repetition penalty mirrored; raw = llama.cpp plain greedy."
    )
    for quant in QUANTS:
        if quant not in results:
            continue
        for variant in ("pen", "raw"):
            md.append("")
            md.append(f"## {quant}, {variant}")
            md.append("")
            md.append(
                "| profile | category | prompts | tokens | p | run mean | run median | run p90 | run max "
                "| E[J] k=4 | k=8 | k=16 | k=32 | k=64 | first div median (mean) | never diverged |"
            )
            md.append("|---|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|")
            for profile in PROFILES:
                entry = results[quant].get(profile, {}).get(variant)
                if not entry:
                    continue
                for cat in CATEGORIES + ["all"]:
                    m = entry.get(cat)
                    if not m:
                        continue
                    fd = m["first_divergence"]
                    md.append(
                        f"| {profile} | {cat} | {m['sequences']} | {m['tokens']} | {fmt(m['p'], 3)} "
                        f"| {fmt(m['runs']['mean'])} | {fmt(m['runs']['median'])} | {m['runs']['p90']} "
                        f"| {m['runs']['max']} | "
                        + " | ".join(fmt(m["ej"][str(k)]) for k in KS)
                        + f" | {fmt(fd['median'], 1)} ({fmt(fd['mean'], 1)}) | {fd['never']} |"
                    )
        md.append("")
        md.append(f"Model-only agreement ({quant}, llama.cpp raw argmax vs ARC raw argmax): " + "; ".join(
            f"{profile} p = {fmt(results[quant][profile]['model']['all']['p'], 3)}"
            for profile in PROFILES
            if "model" in results[quant].get(profile, {})
        ))
    if second:
        md.append("")
        md.append("## Second choice at mismatches (pen, all categories)")
        md.append("")
        md.append(
            "| drafter / profile | positions | mismatches | ARC = 2nd choice | ARC in top 3 | gap at 2nd-choice misses, median (p90) "
            "| gap at other misses, median | margin median at matches / misses | p if every 2nd-choice miss were fixed | E[J] fixed k=4 / 8 / 16 / 32 / 64 |"
        )
        md.append("|---|---:|---:|---:|---:|---|---:|---|---:|---|")
        for key, v in second.items():
            md.append(
                f"| {key} | {v['positions']} | {v['mismatches']} | {fmt(v['second_choice_rate'], 3)} | {fmt(v['top3_rate'], 3)} "
                f"| {fmt(v['gap_at_second_choice_median'])} ({fmt(v['gap_at_second_choice_p90'])}) "
                f"| {fmt(v['gap_at_other_misses_median'])} "
                f"| {fmt(v['margin_median_at_matches'])} / {fmt(v['margin_median_at_misses'])} "
                f"| {fmt(v['corrector_oracle_p'], 3)} | " + " / ".join(fmt(v["corrector_oracle_ej"][str(k)]) for k in KS) + " |"
            )
        md.append("")
        md.append("Miss rate by the drafter's own first-minus-second margin (logits):")
        md.append("")
        for key, v in second.items():
            md.append(f"- {key}: " + "; ".join(
                f"{b['margin']}: {fmt(100 * b['share'], 1) if b['share'] is not None else 'n/a'}% of positions, "
                f"miss {fmt(100 * b['miss_rate'], 1) if b['miss_rate'] is not None else 'n/a'}%"
                for b in v["margin_buckets"]
            ))
    md.append("")
    md.append("## Run-length histograms (pen, all categories)")
    md.append("")
    for quant in QUANTS:
        for profile in PROFILES:
            entry = results.get(quant, {}).get(profile, {}).get("pen")
            if entry:
                md.append(f"- {quant} / {profile}: {entry['all']['runs']['histogram']}")
    md.append("")
    md.append("## ARC side")
    md.append("")
    md.append("| profile | category | prompts | tokens | stopped at EOS | try_generate gate | ARC seconds |")
    md.append("|---|---|---:|---:|---:|---|---:|")
    for a in arc_facts:
        md.append(
            f"| {a['profile']} | {a['category']} | {a['prompts']} | {a['tokens']} | {a['stopped_at_eos']} "
            f"| {a['gate_identical']} | {fmt(a['arc_s'], 0)} |"
        )
    if missing:
        md.append("")
        md.append(f"Missing teacher rows: {len(missing)} ({', '.join(missing[:10])}{' ...' if len(missing) > 10 else ''})")
    md.append("")
    md.append("## Per prompt (pen)")
    md.append("")
    md.append("| quant | profile | prompt | tokens | p pen | p raw | first div | disagreements | within 1 logit |")
    md.append("|---|---|---|---:|---:|---:|---:|---:|---:|")
    for r in per_prompt:
        md.append(
            f"| {r['quant']} | {r['profile']} | {r['id']} | {r['tokens']} | {fmt(r['p_pen'], 3)} "
            f"| {fmt(r['p_raw'], 3)} | {r['first_divergence']} | {r['disagreements']} | {r['near_tie_disagreements']} |"
        )
    with open(args.out_md, "w") as f:
        f.write("\n".join(md) + "\n")
    print("\n".join(md[: 12 + 2 * 20]))


def main():
    parser = argparse.ArgumentParser()
    sub = parser.add_subparsers(dest="cmd", required=True)
    t = sub.add_parser("teacher-input")
    t.add_argument("--arc", required=True)
    t.add_argument("--profile", required=True)
    t.add_argument("--out", required=True)
    r = sub.add_parser("report")
    r.add_argument("--arc", required=True)
    r.add_argument("--teacher", required=True)
    r.add_argument("--out-md", required=True)
    r.add_argument("--out-json", required=True)
    r.add_argument("--run-id", default="local")
    args = parser.parse_args()
    if args.cmd == "teacher-input":
        teacher_input(args)
    else:
        report(args)


if __name__ == "__main__":
    main()
