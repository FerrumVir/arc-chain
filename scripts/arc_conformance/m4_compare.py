"""Compare ARC's canonical-profile evaluation with the pinned llama.cpp reference (M3/M4).

Criteria are fixed here, before any run:

* tokenizer: ARC's and llama.cpp's token streams for the whole corpus are
  identical (M3). Any difference is reported with its first index.
* quality: ARC's perplexity is reported beside llama.cpp's on the same GGUF,
  text, context and chunk count, with the ratio. The gap is recorded, not
  judged: the profile is known to be lossy, and no threshold was set in
  advance by the owner.
* identity: the ARC result must name the artifact BLAKE3, profile and
  tokenizer it measured, and the artifact must match the pinned manifest.

    python3 -m arc_conformance.m4_compare --arc-json arc.json \
        --llama-ppl-log llama-perplexity.log --arc-tokens arc.txt --llama-tokens llama.txt \
        --manifest docs/protocol/packages/llama-2-7b-q4km.manifest.json
"""

from __future__ import annotations

import argparse
import json
import re
import sys
from pathlib import Path

FINAL_PPL = re.compile(r"Final estimate:\s*PPL\s*=\s*([0-9.]+)\s*\+/-\s*([0-9.]+)")
TOKEN = re.compile(r"-?\d+")


def llama_ppl(log_text: str) -> tuple[float, float]:
    match = FINAL_PPL.search(log_text)
    if not match:
        raise ValueError("no 'Final estimate: PPL = X +/- Y' line in the llama.cpp log")
    return float(match.group(1)), float(match.group(2))


def read_tokens(text: str) -> list[int]:
    """Token ids from `llama-tokenize --ids` ([1, 2, ...]) or one id per line."""
    return [int(t) for t in TOKEN.findall(text)]


def compare_tokens(arc: list[int], reference: list[int]) -> dict:
    first = next((i for i, (a, b) in enumerate(zip(arc, reference)) if a != b), None)
    if first is None and len(arc) != len(reference):
        first = min(len(arc), len(reference))
    return {
        "arc_tokens": len(arc),
        "reference_tokens": len(reference),
        "identical": first is None,
        "first_difference": first,
        "context": None if first is None else {
            "arc": arc[max(0, first - 3): first + 4],
            "reference": reference[max(0, first - 3): first + 4],
        },
    }


def summarize(arc_json: dict, llama_log: str | None, arc_tokens: list[int] | None,
              llama_tokens: list[int] | None, manifest: dict | None) -> dict:
    out = {"schema": "arc.m4.comparison.v1", "arc": {
        k: arc_json.get(k) for k in ("artifact_blake3", "profile", "tokenizer", "n_ctx",
                                     "chunks", "scored_tokens", "ppl")}}
    problems = []
    if manifest is not None:
        pinned = manifest["artifact"]["blake3"]
        measured = str(arc_json.get("artifact_blake3", "")).removeprefix("0x")
        out["artifact_matches_manifest"] = measured == pinned
        if measured != pinned:
            problems.append("the measured artifact is not the pinned package")
        if arc_json.get("profile") != manifest["execution"]["profile"]:
            problems.append("the measured profile is not the package's profile")
    if llama_log is not None:
        ppl, err = llama_ppl(llama_log)
        out["reference"] = {"ppl": ppl, "stderr": err}
        out["ppl_ratio_arc_over_reference"] = arc_json["ppl"] / ppl
    if arc_tokens is not None and llama_tokens is not None:
        out["tokenizer"] = compare_tokens(arc_tokens, llama_tokens)
        if not out["tokenizer"]["identical"]:
            problems.append("token streams differ (M3)")
    out["problems"] = problems
    return out


def main(argv: list[str]) -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--arc-json", type=Path, required=True)
    parser.add_argument("--llama-ppl-log", type=Path)
    parser.add_argument("--arc-tokens", type=Path)
    parser.add_argument("--llama-tokens", type=Path)
    parser.add_argument("--manifest", type=Path)
    args = parser.parse_args(argv[1:])
    summary = summarize(
        json.loads(args.arc_json.read_text()),
        args.llama_ppl_log.read_text() if args.llama_ppl_log else None,
        read_tokens(args.arc_tokens.read_text()) if args.arc_tokens else None,
        read_tokens(args.llama_tokens.read_text()) if args.llama_tokens else None,
        json.loads(args.manifest.read_text()) if args.manifest else None,
    )
    print(json.dumps(summary, indent=1))
    return 1 if summary["problems"] else 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
