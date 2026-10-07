"""Paired comparison of ARC against a reference, judged by the tolerance policy.

Input: scored files (arc.quality-scored.v1) for ARC and the reference, the
items, the policy, and optionally a second reference run (noise floor) and
perplexity reports from the existing SmolLM3 quality method
(arc.modern-quality.v1, `hf_checks.py bf16-ppl`).

Output: arc.quality-report.v1 (JSON) and a Markdown summary.
"""

from __future__ import annotations

import json
from pathlib import Path

from . import stats

REPORT_SCHEMA = "arc.quality-report.v1"


def merge_scored(paths: list[str]) -> tuple[dict, list[dict]]:
    """Results by id, plus each file's engine/run metadata."""
    results: dict[str, dict] = {}
    metas = []
    for path in paths:
        doc = json.loads(Path(path).read_text(encoding="utf-8"))
        if doc.get("schema") != "arc.quality-scored.v1":
            raise ValueError(f"{path}: not an arc.quality-scored.v1 file")
        metas.append({"file": Path(path).name, "engine": doc.get("engine"), "run": doc.get("run")})
        for result in doc["results"]:
            if result["id"] in results:
                raise ValueError(f"{result['id']} is scored twice")
            results[result["id"]] = result
    return results, metas


def _points(fraction):
    return None if fraction is None else 100.0 * fraction


def _token_identity(left: dict, right: dict) -> tuple[bool | None, bool | None, int | None]:
    """(same prompt ids, identical generation, first divergent position)."""
    if left.get("tokens") is None or right.get("tokens") is None:
        return None, None, None
    same_prompt = None
    if left.get("prompt_tokens") is not None and right.get("prompt_tokens") is not None:
        same_prompt = left["prompt_tokens"] == right["prompt_tokens"]
    a, b = left["tokens"], right["tokens"]
    if a == b:
        return same_prompt, True, None
    first = next((i for i, (x, y) in enumerate(zip(a, b)) if x != y), min(len(a), len(b)))
    return same_prompt, False, first


def compare_block(ids: list[str], arc: dict, ref: dict, confidence: float) -> dict:
    pairs = [(bool(ref[i]["correct"]), bool(arc[i]["correct"])) for i in ids]
    block = stats.summarize(pairs, confidence)
    agree = sum(1 for i in ids if arc[i].get("answer_key") == ref[i].get("answer_key"))
    block["answer_agreement"] = agree / len(ids) if ids else None
    identity = [_token_identity(arc[i], ref[i]) for i in ids]
    with_tokens = [x for x in identity if x[1] is not None]
    if with_tokens:
        block["token_level"] = {
            "items": len(with_tokens),
            "same_prompt_ids": sum(1 for x in with_tokens if x[0]),
            "identical_generations": sum(1 for x in with_tokens if x[1]),
            "first_divergence_median": _median([x[2] for x in with_tokens if x[2] is not None]),
        }
    return block


def _median(values: list[int]):
    if not values:
        return None
    values = sorted(values)
    middle = len(values) // 2
    return values[middle] if len(values) % 2 else (values[middle - 1] + values[middle]) / 2


def judge(block: dict, margin_points: float, certification_items: int) -> dict:
    lower, upper = (_points(x) for x in block["delta_ci"])
    if block["n"] == 0:
        verdict = "NO DATA"
    elif lower >= -margin_points:
        verdict = "PASS"
    elif upper < -margin_points:
        verdict = "FAIL"
    else:
        verdict = "INCONCLUSIVE"
    return {
        "verdict": verdict,
        "margin_points": margin_points,
        "certification_items": certification_items,
        "certifying": block["n"] >= certification_items,
        "items_needed_at_observed_discordance": (
            stats.items_needed(max(block["discordant_rate"], 1.0 / max(block["n"], 1)), margin_points / 100.0)
            if block["n"] else None
        ),
    }


def compare(items: list[dict], arc_paths: list[str], ref_paths: list[str], policy: dict,
            ref_b_paths: list[str] | None = None, ppl_paths: list[str] | None = None,
            label: str = "") -> dict:
    by_id = {item["id"]: item for item in items}
    arc, arc_meta = merge_scored(arc_paths)
    ref, ref_meta = merge_scored(ref_paths)
    ids = sorted(i for i in by_id if i in arc and i in ref)
    missing = sorted(i for i in by_id if i not in arc or i not in ref)
    confidence = float(policy.get("confidence", 0.95))

    benchmarks = {}
    for name in sorted({by_id[i]["benchmark"] for i in ids}):
        subset = [i for i in ids if by_id[i]["benchmark"] == name]
        benchmarks[name] = compare_block(subset, arc, ref, confidence)

    groups = {}
    for group, rule in policy["groups"].items():
        subset = [i for i in ids if by_id[i]["benchmark"] in rule["members"]]
        if not subset:
            continue
        block = compare_block(subset, arc, ref, confidence)
        block["policy"] = judge(block, float(rule["margin_points"]), int(rule["certification_items"]))
        groups[group] = block

    pooled = compare_block(ids, arc, ref, confidence)
    shortfall = float(policy["pooled"]["max_shortfall_points"])
    pooled["policy"] = {
        "max_shortfall_points": shortfall,
        "verdict": "PASS" if ids and _points(pooled["delta"]) >= -shortfall else ("NO DATA" if not ids else "FAIL"),
    }

    noise = None
    if ref_b_paths:
        ref_b, _ = merge_scored(ref_b_paths)
        shared = [i for i in ids if i in ref_b]
        if shared:
            arc_vs_ref = sum(1 for i in shared if arc[i].get("answer_key") != ref[i].get("answer_key")) / len(shared)
            ref_vs_ref = sum(1 for i in shared if ref_b[i].get("answer_key") != ref[i].get("answer_key")) / len(shared)
            rule = policy["noise_floor"]
            limit = rule["max_disagreement_ratio"] * ref_vs_ref + rule["slack_points"] / 100.0
            noise = {
                "items": len(shared),
                "arc_vs_reference_disagreement": arc_vs_ref,
                "reference_vs_reference_disagreement": ref_vs_ref,
                "limit": limit,
                "verdict": "PASS" if arc_vs_ref <= limit else "FAIL",
            }

    logit = []
    for path in ppl_paths or []:
        doc = json.loads(Path(path).read_text(encoding="utf-8"))
        limit = float(policy["logit_track"]["max_ppl_delta_percent"])
        logit.append({
            "text": doc.get("text"),
            "scored_tokens": doc.get("scored_tokens"),
            "reference_ppl": doc["bf16_reference"]["ppl"],
            "arc_ppl": doc["integer_engine"]["ppl"],
            "ppl_delta_percent": doc["ppl_delta_percent"],
            "top1_agreement": doc.get("top1_agreement"),
            "arc_logits_digest": doc["integer_engine"].get("logits_digest"),
            "max_ppl_delta_percent": limit,
            "verdict": "PASS" if doc["ppl_delta_percent"] <= limit else "FAIL",
        })

    verdicts = [g["policy"]["verdict"] for g in groups.values()]
    verdicts.append(pooled["policy"]["verdict"])
    verdicts += [x["verdict"] for x in logit]
    if noise:
        verdicts.append(noise["verdict"])
    if not ids or "NO DATA" in verdicts:
        overall = "NO DATA"
    elif "FAIL" in verdicts:
        overall = "FAIL"
    elif "INCONCLUSIVE" in verdicts:
        overall = "INCONCLUSIVE"
    else:
        overall = "PASS"
    certifying = bool(groups) and all(g["policy"]["certifying"] for g in groups.values()) \
        and len(groups) == len(policy["groups"])
    return {
        "schema": REPORT_SCHEMA,
        "label": label,
        "policy": {"status": policy.get("status"), "confidence": confidence},
        "overall": {
            "verdict": overall,
            "certifying": certifying,
            "scope": "certification" if certifying else "smoke (below the certification sizes)",
        },
        "items": {"compared": len(ids), "missing": missing},
        "engines": {"arc": arc_meta, "reference": ref_meta},
        "groups": groups,
        "benchmarks": benchmarks,
        "pooled": pooled,
        "noise_floor": noise,
        "logit_track": logit,
    }


def _fmt(value, digits=1, signed=False):
    if value is None:
        return "n/a"
    return f"{value:+.{digits}f}" if signed else f"{value:.{digits}f}"


def markdown(report: dict) -> str:
    lines = [f"### {report['label'] or 'ARC vs reference quality'}", ""]
    overall = report["overall"]
    lines.append(
        f"**Policy ({report['policy']['status']}): {overall['verdict']}**, {overall['scope']}. "
        f"{report['items']['compared']} items compared"
        + (f", {len(report['items']['missing'])} missing." if report["items"]["missing"] else ".")
    )
    lines += ["", "| group | n | reference acc. | ARC acc. | delta (pts) | 95% CI (pts) | discordant | same answer | identical tokens | margin | verdict |",
              "|---|---|---|---|---|---|---|---|---|---|---|"]
    rows = list(report["groups"].items()) + [("pooled", report["pooled"])]
    for name, block in rows:
        token = block.get("token_level")
        identical = f"{token['identical_generations']}/{token['items']}" if token else "n/a"
        policy = block.get("policy", {})
        margin = f"-{policy['margin_points']}" if "margin_points" in policy else f"-{policy.get('max_shortfall_points')} (point)"
        lo, hi = block["delta_ci"]
        lines.append(
            f"| {name} | {block['n']} | {_fmt(_points(block['reference_accuracy']))}% | "
            f"{_fmt(_points(block['arc_accuracy']))}% | {_fmt(_points(block['delta']), signed=True)} | "
            f"[{_fmt(_points(lo), signed=True)}, {_fmt(_points(hi), signed=True)}] | "
            f"{block['only_a'] + block['only_b']} ({block['only_a']} ref-only, {block['only_b']} ARC-only) | "
            f"{_fmt(_points(block['answer_agreement']))}% | {identical} | {margin} | {policy.get('verdict', '')} |"
        )
    if report["benchmarks"]:
        lines += ["", "| benchmark | n | reference acc. | ARC acc. | delta (pts) | McNemar p |", "|---|---|---|---|---|---|"]
        for name, block in report["benchmarks"].items():
            lines.append(
                f"| {name} | {block['n']} | {_fmt(_points(block['reference_accuracy']))}% | "
                f"{_fmt(_points(block['arc_accuracy']))}% | {_fmt(_points(block['delta']), signed=True)} | "
                f"{block['mcnemar_p']:.3f} |"
            )
    if report["logit_track"]:
        lines += ["", "| text (logit track) | scored tokens | reference PPL | ARC PPL | delta | top-1 agreement | verdict |",
                  "|---|---|---|---|---|---|---|"]
        for row in report["logit_track"]:
            lines.append(
                f"| {row['text']} | {row['scored_tokens']} | {row['reference_ppl']:.4f} | {row['arc_ppl']:.4f} | "
                f"{row['ppl_delta_percent']:+.3f}% | {_fmt(_points(row['top1_agreement']))}% | {row['verdict']} |"
            )
    if report["noise_floor"]:
        n = report["noise_floor"]
        lines += ["", f"Noise floor: ARC vs reference disagree on {_fmt(_points(n['arc_vs_reference_disagreement']))}% "
                  f"of answers, reference vs reference on {_fmt(_points(n['reference_vs_reference_disagreement']))}% "
                  f"(limit {_fmt(_points(n['limit']))}%): **{n['verdict']}**."]
    lines += ["", "Engines:"]
    for side in ("arc", "reference"):
        for meta in report["engines"][side]:
            engine = meta.get("engine") or {}
            lines.append(f"- {side}: {engine.get('label') or engine.get('kind')} ({meta['file']})")
    return "\n".join(lines) + "\n"
