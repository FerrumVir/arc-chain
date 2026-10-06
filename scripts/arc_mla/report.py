#!/usr/bin/env python3
"""Markdown summary of the MLA + MoE workflow's artifacts (any that exist).

    python3 scripts/arc_mla/report.py ARTIFACTS_DIR >> "$GITHUB_STEP_SUMMARY"

ARTIFACTS_DIR holds the downloaded `kimi-arch-*` artifacts. Prints the
cross-platform hash matrix (tiny models and the Moonlight verifier slice),
the Moonlight package, golden, layout and Python-reference results, the
tokenizer check and the perplexity table. Exits 1 if any recorded check
failed.
"""

from __future__ import annotations

import json
import re
import sys
from pathlib import Path


def load(path: Path):
    try:
        return json.loads(path.read_text())
    except (OSError, ValueError):
        return None


def peak_rss_gib(path: Path):
    try:
        m = re.search(r"Maximum resident set size \(kbytes\): (\d+)", path.read_text())
    except OSError:
        return None
    return round(int(m.group(1)) / 1024 / 1024, 2) if m else None


def main(argv: list) -> int:
    root = Path(argv[1])
    failed = False
    out = ["## MLA + MoE profile (Moonlight-16B-A3B, Kimi-K2 architecture)", ""]

    # Cross-platform hash matrix: tiny models.
    rows = []
    for art in sorted(root.glob("kimi-arch-*")):
        for variant in ("moonlight", "kimi"):
            vdir = art / variant if (art / variant).exists() else art / "tiny" / variant
            for kernel in ("scalar", "simd"):
                run = load(vdir / f"run-{kernel}.json")
                if run:
                    plat = run.get("platform", {})
                    conv = load(vdir / "convert-0-4.json") or {}
                    check = load(vdir / "layout-check.json") or {}
                    rows.append((variant, plat.get("os"), plat.get("arch"), kernel,
                                 conv.get("package", {}).get("sha256", "")[:16], run["matrix_digest"][:16],
                                 run["boundary_matrix_digest"][:16], check.get("all_match")))
                    if check.get("all_match") is False:
                        failed = True
    if rows:
        out += ["### Tiny models: hash matrix", "",
                "| model | OS | arch | kernel | package sha256 | matrix digest | boundary digest | layouts + replay |",
                "|---|---|---|---|---|---|---|---|"]
        out += [f"| tiny {v} | {o} | {a} | {k} | `{p}…` | `{m}…` | `{b}…` | {c} |" for v, o, a, k, p, m, b, c in rows]
        for variant in ("moonlight", "kimi"):
            digests = {r[5] for r in rows if r[0] == variant}
            packages = {r[4] for r in rows if r[0] == variant}
            ok = len(digests) == 1 and len(packages) == 1
            failed |= not ok
            out.append(f"\ntiny {variant}: one package hash and one matrix digest on every runner and kernel: **{ok}**")
        out.append("")

    # Moonlight.
    ml = root / "kimi-arch-moonlight"
    if ml.exists():
        conv, prep = load(ml / "convert.json"), load(ml / "prepare.json")
        out += ["### Moonlight-16B-A3B-Instruct (linux x86-64 CI runner)", ""]
        if conv:
            out.append(f"- Package (Rust, converted on the runner): `{conv['package']['sha256']}`, "
                       f"{conv['package']['bytes']:,} bytes, {conv['seconds']:.0f} s, peak RSS "
                       f"{peak_rss_gib(ml / 'time-convert.txt')} GiB; model root `{conv.get('model_root')}`")
        if conv and prep:
            same = conv["package"]["sha256"] == prep["sha256"] and conv.get("model_root") == prep.get("model_root")
            failed |= not same
            out.append(f"- Independent Python preparer, same sha256 and model root: **{same}**")
        runs = {k: load(ml / f"run-{k}.json") for k in ("scalar", "simd")}
        if all(runs.values()):
            same = all(runs["scalar"][k] == runs["simd"][k] for k in ("matrix_digest", "boundary_matrix_digest"))
            failed |= not same
            t = {k: r["timing"] for k, r in runs.items()}
            out.append(f"- Golden generation, scalar = SIMD: **{same}** (matrix `{runs['scalar']['matrix_digest'][:16]}…`); "
                       f"decode {t['scalar']['decode_tok_s']:.2f} tok/s scalar, {t['simd']['decode_tok_s']:.2f} SIMD; "
                       f"prefill {t['simd']['prefill_tok_s']:.2f} tok/s SIMD (CI runner, single stream)")
            for case in runs["scalar"]["cases"]:
                out.append(f"  - `{case['id']}`: {json.dumps(case.get('text'), ensure_ascii=False)}")
        golden = load(ml / "python-golden.json")
        if golden:
            ok = golden["checks"]["all_match"]
            failed |= not ok
            out.append(f"- Python executor re-derived every logits vector, boundary digest and token: **{ok}** "
                       f"(fast-path fallbacks {golden['checks']['fast_path_fallbacks']})")
        check = ml / "layout-check.md"
        if check.exists():
            out += ["", check.read_text()]
            failed |= (load(ml / "layout-check.json") or {}).get("all_match") is False

    # Verifier slice on other platforms.
    for art in sorted(root.glob("kimi-arch-matrix-*")):
        md = art / "slice-check.md"
        if md.exists():
            out += ["", md.read_text()]
            failed |= (load(art / "slice-check.json") or {}).get("all_match") is False

    # Tokenizer.
    tok = load(root / "kimi-arch-tokenizer" / "tokenizer-check.json")
    if tok:
        ok = tok["failures"] == 0
        failed |= not ok
        out += ["", "### Tokenizer and chat prompt", "",
                f"{tok['rows'] - tok['failures']}/{tok['rows']} corpus rows identical to the tiktoken library "
                f"({tok['chat_rows']} rendered chat prompts identical to jinja2)."]

    # Quality.
    q = root / "kimi-arch-quality"
    reports = sorted(q.glob("quality-*.json")) if q.exists() else []
    checks = [load(p) for p in sorted(q.glob("float-reference-check-*.json"))] if q.exists() else []
    if checks:
        ok = all(c and c["ok"] for c in checks)
        failed |= not ok
        out += ["", f"Float reference vs transformers DeepseekV3 on the tiny models: **{ok}** "
                f"(max |logit diff| {max(c['max_abs_logit_diff'] for c in checks if c):.2e})"]
    if reports:
        out += ["", "### Perplexity vs the BF16 weights (CI runner)", "",
                "| text | scored tokens | BF16 weights, float32 | ARC integer | delta | top-1 agreement |",
                "|---|---|---|---|---|---|"]
        for path in reports:
            r = load(path)
            out.append(f"| {r['text']} | {r['scored_tokens']} | {r['bf16_reference']['ppl']:.4f} | "
                       f"{r['integer_engine']['ppl']:.4f} | {r['ppl_delta_percent']:+.3f}% | {100 * r['top1_agreement']:.1f}% |")
    print("\n".join(out))
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
