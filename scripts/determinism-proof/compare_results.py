#!/usr/bin/env python3
"""Compare every determinism-proof leg and decide the verdict (stdlib only).

Green only if every expected runner/kernel pair completed and the SHA-256 of
its full transcript (shards joined in order) is identical across all pairs,
every SIMD pair shows the vectorised kernel accepted every projection it was
offered (and every scalar pair offered none), every pair's engine API
cross-check passed, and every pair loaded the pinned model bytes. Writes a
markdown hash matrix (also appended to the GitHub job summary) and an evidence
JSON. On a mismatch it names the first transcript line where the pairs
diverge and what that line measures.
"""

from __future__ import annotations

import argparse
import collections
import datetime
import hashlib
import json
import os
from pathlib import Path

RESULT_SCHEMA = "arc-determinism-proof-result-v1"
PLATFORMS = {
    "macos-15": "macOS 15, Apple Silicon (arm64)",
    "macos-15-intel": "macOS 15, Intel (x86_64)",
    "ubuntu-24.04": "Linux, Ubuntu 24.04 (x86_64)",
    "ubuntu-latest": "Linux, Ubuntu (x86_64)",
    "windows-latest": "Windows Server (x86_64)",
}
REFUSAL_KEYS = (
    "refused_unavailable",
    "refused_shape",
    "refused_inner_dim_above_i32_bound",
    "refused_activation_out_of_domain",
    "refused_scale_multiply_would_overflow",
)
HEADER_MEANING = {
    "model_file_blake3": "the model file bytes differ",
    "weight_blake3": "the INT8 weights differ after GGUF dequantization and per-row requantization",
    "resident_q16_blake3": "the Q16 embedding table or norm vectors differ",
    "rope_tables_blake3": (
        "the RoPE tables differ; compute_rope_tables derives them with the platform "
        "libm (f64 powf/sin/cos), so the fix is to pin the table values"
    ),
    "tokenizer_vocab_blake3": "the tokenizer vocabulary differs",
    "prompt_set_blake3": "the prompt set differs",
}


def load_results(results_dir: Path) -> dict[tuple[str, str], list[dict]]:
    groups: dict[tuple[str, str], list[dict]] = collections.defaultdict(list)
    if not results_dir.is_dir():
        return groups
    for path in sorted(results_dir.rglob("result.json")):
        try:
            result = json.loads(path.read_text(encoding="utf-8"))
        except (OSError, ValueError):
            continue
        if result.get("schema") != RESULT_SCHEMA:
            continue
        result["_dir"] = path.parent
        groups[(result.get("runner_label", "?"), result.get("kernel", "?"))].append(result)
    return groups


def split_transcript(data: bytes) -> tuple[list[str], list[str], str]:
    """Return (header lines, prompt block lines, terminator) of one transcript."""
    lines = data.decode("utf-8").split("\n")
    if lines and lines[-1] == "":
        lines.pop()
    terminator = lines.pop() if lines else ""
    first_block = next(
        (index for index, line in enumerate(lines) if line.startswith("prompt ")), len(lines)
    )
    return lines[:first_block], lines[first_block:], terminator


def assemble(group: list[dict]) -> tuple[bytes | None, list[str]]:
    """Join one pair's shards into the single-run transcript bytes."""
    problems: list[str] = []
    shards: dict[int, tuple[int, dict]] = {}
    for result in group:
        try:
            index_text, _, count_text = str(result.get("shard", "0/1")).partition("/")
            index, count = int(index_text), int(count_text)
        except ValueError:
            problems.append(f"unparseable shard {result.get('shard')!r}")
            continue
        if index in shards:
            problems.append(f"shard {index} reported twice")
        shards[index] = (count, result)
    counts = {count for count, _ in shards.values()}
    if len(counts) != 1:
        problems.append("no shards, or shards disagree on the shard count")
        return None, problems
    count = counts.pop()
    missing = [index for index in range(count) if index not in shards]
    if missing:
        problems.append(f"missing shard(s) {missing} of {count}")
        return None, problems
    header: list[str] | None = None
    body: list[str] = []
    for index in range(count):
        _, result = shards[index]
        label = f"shard {index}/{count}"
        if result.get("status") != "complete":
            for problem in result.get("problems") or ["did not complete"]:
                problems.append(f"{label}: {problem}")
        transcript = result.get("transcript")
        if not transcript:
            problems.append(f"{label}: no transcript")
            continue
        try:
            data = (result["_dir"] / transcript["file"]).read_bytes()
        except OSError as error:
            problems.append(f"{label}: transcript unreadable: {error}")
            continue
        if hashlib.sha256(data).hexdigest() != transcript.get("sha256"):
            problems.append(f"{label}: transcript bytes changed after the leg hashed them")
        shard_header, blocks, terminator = split_transcript(data)
        expected = "end" if count == 1 else f"end-of-shard {index}/{count}"
        if terminator != expected:
            problems.append(f"{label}: transcript ends with {terminator!r}, expected {expected!r}")
        if header is None:
            header = shard_header
        elif shard_header != header:
            problems.append(f"{label}: transcript header differs from shard 0")
        body.extend(blocks)
    if problems or header is None:
        return None, problems
    return ("\n".join(header + body + ["end"]) + "\n").encode("utf-8"), problems


def census(runs: list[dict]) -> dict:
    total = {"attempted": 0, "accepted": 0, "refused": 0}
    for run in runs:
        counts = ((run.get("kernel") or {}).get("projection_census")) or {}
        total["attempted"] += int(counts.get("attempted", 0))
        total["accepted"] += int(counts.get("accepted", 0))
        total["refused"] += sum(int(counts.get(key, 0)) for key in REFUSAL_KEYS)
    return total


def first_divergence(reference: bytes, other: bytes) -> dict | None:
    left = reference.decode("utf-8").split("\n")
    right = other.decode("utf-8").split("\n")
    prompt_line = None
    for number, (a, b) in enumerate(zip(left, right), start=1):
        if a.startswith("prompt "):
            prompt_line = a
        if a == b:
            continue
        key = a.split(" ", 1)[0]
        meaning = HEADER_MEANING.get(key)
        if meaning is None and key == "pos":
            fields = dict(part.split("=", 1) for part in a.split()[2:] if "=" in part)
            other_fields = dict(part.split("=", 1) for part in b.split()[2:] if "=" in part)
            if fields.get("kv") != other_fields.get("kv"):
                meaning = (
                    "the K/V rows written at this position differ: the divergence is at or "
                    "before a Q/K/V projection or RoPE in some layer of this forward"
                )
            else:
                meaning = (
                    "the K/V rows agree but the logits differ: the divergence is after the "
                    "K/V write (attention, output projection, MLP, final norm or LM head)"
                )
        elif meaning is None and key == "prompt_ids":
            meaning = "the tokenizer produced different prompt token IDs"
        elif meaning is None and key == "generated":
            meaning = "token selection differs although every recorded logit digest matched"
        return {
            "line": number,
            "prompt": prompt_line,
            "reference": a,
            "other": b,
            "meaning": meaning or "this transcript field differs",
        }
    if len(left) != len(right):
        return {
            "line": min(len(left), len(right)) + 1,
            "prompt": prompt_line,
            "reference": "<end>" if len(left) < len(right) else left[len(right)],
            "other": "<end>" if len(right) < len(left) else right[len(left)],
            "meaning": "one transcript is longer",
        }
    return None


def rate(runs: list[dict], forwards_key: str, seconds_key: str) -> float | None:
    forwards = sum(int((run.get("timing") or {}).get(forwards_key) or 0) for run in runs)
    seconds = sum(float((run.get("timing") or {}).get(seconds_key) or 0.0) for run in runs)
    return forwards / seconds if forwards and seconds > 0 else None


def gib(value: int | None) -> str:
    return f"{value / 2**30:.1f} GiB" if value else "?"


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--results-dir", type=Path, required=True)
    parser.add_argument("--manifest", type=Path, required=True)
    parser.add_argument(
        "--expect", action="append", required=True, help="runner:kernel, repeatable"
    )
    parser.add_argument("--out-dir", type=Path, required=True)
    parser.add_argument("--summary", type=Path, help="markdown file to append to")
    args = parser.parse_args()

    manifest = json.loads(args.manifest.read_text(encoding="utf-8"))
    pinned_blake3 = manifest["artifact"]["blake3"]
    pinned_sha256 = manifest["artifact"]["sha256"]
    pinned_bytes = manifest["artifact"]["bytes"]
    expected = [tuple(item.split(":", 1)) for item in args.expect]
    groups = load_results(args.results_dir)

    pairs = []
    for runner, kernel in expected:
        group = groups.get((runner, kernel), [])
        runs = [result["run"] for result in group if isinstance(result.get("run"), dict)]
        problems: list[str] = []
        data = None
        if not group:
            problems.append("no result uploaded")
        else:
            data, assemble_problems = assemble(group)
            problems.extend(assemble_problems)
        counts = census(runs)
        if kernel == "simd":
            census_ok = counts["attempted"] > 0 and counts["accepted"] == counts["attempted"]
            census_ok = census_ok and counts["refused"] == 0
            if runs and not census_ok:
                problems.append(f"SIMD kernel did not accept every projection: {counts}")
        else:
            census_ok = counts["attempted"] == 0
            if runs and not census_ok:
                problems.append(f"scalar leg offered projections to the SIMD kernel: {counts}")
        for run in runs:
            model = run.get("model") or {}
            if model.get("file_blake3") != pinned_blake3 or model.get("file_bytes") != pinned_bytes:
                problems.append("loaded model bytes are not the pinned artifact")
            if run.get("engine_crosscheck_ok") is not True:
                problems.append("engine API cross-check failed")
        crosschecks = [run["engine_crosscheck"] for run in runs if run.get("engine_crosscheck")]
        host = (group[0].get("host") if group else None) or {}
        effective = sorted({(run.get("kernel") or {}).get("effective", "?") for run in runs})
        pairs.append(
            {
                "runner": runner,
                "platform": PLATFORMS.get(runner, runner),
                "kernel": kernel,
                "kernel_effective": ", ".join(effective) if effective else None,
                "cpu_model": host.get("cpu_model"),
                "memory_bytes": host.get("memory_bytes"),
                "shards": len(group),
                "combined_sha256": hashlib.sha256(data).hexdigest() if data else None,
                "transcript_bytes": len(data) if data else None,
                "decode_tokens_per_second_ci_runner": rate(
                    runs, "decode_forwards", "decode_seconds"
                ),
                "prefill_tokens_per_second_ci_runner": rate(
                    runs, "prefill_forwards", "prefill_seconds"
                ),
                "max_rss_bytes": max((run.get("max_rss_bytes") or 0 for run in runs), default=0)
                or None,
                "projection_census": counts,
                "engine_crosscheck": crosschecks[0] if crosschecks else None,
                "problems": problems,
                "_data": data,
            }
        )

    hashes = [pair["combined_sha256"] for pair in pairs if pair["combined_sha256"]]
    reference = collections.Counter(hashes).most_common(1)[0][0] if hashes else None
    reference_data = next((p["_data"] for p in pairs if p["combined_sha256"] == reference), None)
    for pair in pairs:
        pair["identical"] = bool(reference) and pair["combined_sha256"] == reference
        if pair["_data"] is not None and reference_data is not None and not pair["identical"]:
            pair["first_divergence"] = first_divergence(reference_data, pair["_data"])
    verdict = all(pair["identical"] and not pair["problems"] for pair in pairs)

    header_fields: dict[str, str] = {}
    if reference_data is not None:
        header, _, _ = split_transcript(reference_data)
        for line in header[1:]:
            key, _, value = line.partition(" ")
            header_fields[key] = value

    run_url = os.environ.get("RUN_URL")
    now = datetime.datetime.now(datetime.timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")
    identical_count = sum(1 for pair in pairs if pair["identical"] and not pair["problems"])
    lines = [
        f"## ARC cross-platform determinism proof: {'PASS' if verdict else 'FAIL'}",
        "",
        f"**{identical_count} of {len(pairs)}** platform/kernel runs completed and produced a "
        "byte-identical transcript.",
        "",
        f"Combined SHA-256 (reference): `{reference or 'none'}`",
        "",
        "| Platform (GitHub runner) | CPU reported by the runner | RAM | Kernel | "
        "Combined SHA-256 | Identical | Decode tok/s (CI runner) | SIMD projections accepted |",
        "|---|---|---|---|---|---|---|---|",
    ]
    for pair in pairs:
        counts = pair["projection_census"]
        if pair["kernel"] == "simd":
            accepted = f"{counts['accepted']:,} of {counts['attempted']:,}"
        else:
            accepted = f"not used ({counts['attempted']:,} offered)"
        speed = pair["decode_tokens_per_second_ci_runner"]
        lines.append(
            "| {platform} (`{runner}`) | {cpu} | {ram} | {kernel} (`{effective}`) | `{sha}` | "
            "{same} | {speed} | {accepted} |".format(
                platform=pair["platform"],
                runner=pair["runner"],
                cpu=pair["cpu_model"] or "?",
                ram=gib(pair["memory_bytes"]),
                kernel=pair["kernel"],
                effective=pair["kernel_effective"] or "?",
                sha=pair["combined_sha256"] or "missing",
                same="yes" if pair["identical"] and not pair["problems"] else "**NO**",
                speed=f"{speed:.2f}" if speed else "?",
                accepted=accepted,
            )
        )
    lines += [
        "",
        f"- Model: Llama-2-7B-Chat Q4_K_M GGUF, SHA-256 `{pinned_sha256}`, BLAKE3 "
        f"`{pinned_blake3}` ({pinned_bytes:,} bytes), as pinned in "
        "`docs/protocol/packages/llama-2-7b-q4km.manifest.json`.",
        f"- Execution profile: `{header_fields.get('execution_profile', '?')}`; generation: "
        f"`{header_fields.get('generation_semantics', '?')}` (greedy argmax); prompts: "
        f"{header_fields.get('prompt_count', '?')}; new tokens per prompt: "
        f"{header_fields.get('max_new_tokens', '?')}.",
        f"- Engine digests: weights `{header_fields.get('weight_blake3', '?')}`, RoPE tables "
        f"`{header_fields.get('rope_tables_blake3', '?')}`.",
        "- The transcript records the BLAKE3 of the exact i64 logits and of the K/V rows at "
        "every forward position, plus every generated token ID. Its SHA-256 is computed by "
        "Python's hashlib, not by the code under test.",
        "- Scope: CPUs only. GPU backends are not covered. The weights start as 4-bit GGUF "
        "and are requantized to per-row INT8 at load. Tokens per second are GitHub-hosted CI "
        "runner measurements, not product benchmarks.",
    ]
    if run_url:
        lines.append(f"- Workflow run: {run_url}")
    lines.append(f"- Compared at {now}.")
    reference_pair = (
        next((p for p in pairs if p["combined_sha256"] == reference), None) if reference else None
    )
    reference_results = (
        groups.get((reference_pair["runner"], reference_pair["kernel"]), [])
        if reference_pair
        else []
    )

    def shard_index(item: dict) -> int:
        try:
            return int(str(item.get("shard", "0/1")).partition("/")[0])
        except ValueError:
            return 0

    answers = []
    for result in sorted(reference_results, key=shard_index):
        for prompt in ((result.get("run") or {}).get("prompts")) or []:
            text = " ".join(str(prompt.get("text", "")).split()).replace("```", "'''")
            answers.append(f"{prompt.get('id')}: {text}")
    if answers:
        lines += [
            "",
            "Generated answers (decoded from the reference run's token IDs, which every "
            "identical run shares; each ends at end-of-sequence or at the token budget):",
            "",
            "```text",
            *answers,
            "```",
        ]
    for pair in pairs:
        if pair["problems"] or not pair["identical"]:
            lines.append("")
            lines.append(f"**{pair['runner']} / {pair['kernel']}**")
            for problem in pair["problems"]:
                lines.append(f"- problem: {problem}")
            divergence = pair.get("first_divergence")
            if divergence:
                lines.append(
                    f"- first divergent transcript line {divergence['line']} "
                    f"(in {divergence['prompt'] or 'the header'}): {divergence['meaning']}"
                )
                lines.append(f"  - reference: `{divergence['reference']}`")
                lines.append(f"  - this run:  `{divergence['other']}`")
    markdown = "\n".join(lines) + "\n"

    args.out_dir.mkdir(parents=True, exist_ok=True)
    (args.out_dir / "evidence.md").write_text(markdown, encoding="utf-8")
    if reference_data is not None:
        (args.out_dir / "reference-transcript.txt").write_bytes(reference_data)
    evidence = {
        "schema": "arc-determinism-proof-evidence-v1",
        "verdict": "PASS" if verdict else "FAIL",
        "reference_combined_sha256": reference,
        "compared_at": now,
        "workflow_run": run_url,
        "model": {"sha256": pinned_sha256, "blake3": pinned_blake3, "bytes": pinned_bytes},
        "transcript_header": header_fields,
        "pairs": [{key: value for key, value in pair.items() if key != "_data"} for pair in pairs],
    }
    (args.out_dir / "evidence.json").write_text(
        json.dumps(evidence, indent=2) + "\n", encoding="utf-8"
    )
    print(markdown)
    if args.summary:
        with args.summary.open("a", encoding="utf-8") as handle:
            handle.write(markdown)
    return 0 if verdict else 1


if __name__ == "__main__":
    raise SystemExit(main())
