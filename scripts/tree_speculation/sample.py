#!/usr/bin/env python3
"""Rebuild the licensed public ENG-8 sample. Requires pyarrow, tokenizers.

Run from repo root: python scripts/tree_speculation/sample.py --cache DIR
Downloads are data only. No trajectory commands are executed.
"""
import argparse
import gzip
import hashlib
import json
import re
import urllib.request
from pathlib import Path

import pyarrow.parquet as pq
from tokenizers import Tokenizer

OUT = Path("crates/arc-inference/tests/fixtures/tree_public")
SOURCES = {
    "coding": ("princeton-nlp/SWE-bench_Verified", "c104f840cc67f8b6eec6f759ebc8b2693d585d4a", "data/test-00000-of-00001.parquet", "a45b1fe4e2f0c8390b2b2938ac83e92ed5979000856808f3679c07812e9e6dcd", "coding.parquet", "MIT (SWE-bench); BSD-3-Clause (Django/SymPy excerpts)"),
    "agent": ("SWE-bench/SWE-smith-trajectories", "08e109b4a59eaeebf80e4675cd125d42e7ac99a4", "data/tool-00000-of-00008.parquet", "ac76e9efe75978f83d4daec98a55604587dd58b0f1509364893b8778cbf5b487", "agent.parquet", "MIT dataset; source-code notices in licenses/manifest.json"),
    "chat": ("OpenAssistant/oasst1", "fdf72ae0827c1cda404aff25b6603abec9e3399b", "2023-04-12_oasst_ready.messages.jsonl.gz", "286a6e9a5a413b3272ae9c0b5a20d327983dea1c24342ae28cb244a6da65185c", "chat.jsonl.gz", "Apache-2.0"),
}
TOKENIZER = "https://huggingface.co/HuggingFaceTB/SmolLM3-3B/resolve/a07cc9a04f16550a088caea529712d1d335b0ac1/tokenizer.json"
SEED = "ARC-70-public-v1:"


def sha(data):
    return hashlib.sha256(data).hexdigest()


def fetch(url, path, digest=None):
    if not path.exists():
        urllib.request.urlretrieve(url, path)
    data = path.read_bytes()
    if digest and sha(data) != digest:
        raise ValueError(f"hash mismatch: {path}")
    return data


def canonical(value):
    return json.dumps(value, ensure_ascii=False, sort_keys=True, separators=(",", ":"))


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--cache", type=Path, required=True)
    args = parser.parse_args()
    args.cache.mkdir(parents=True, exist_ok=True)
    OUT.mkdir(parents=True, exist_ok=True)
    for repo, revision, filename, digest, local, _ in SOURCES.values():
        fetch(f"https://huggingface.co/datasets/{repo}/resolve/{revision}/{filename}", args.cache / local, digest)
    fetch(TOKENIZER, args.cache / "tokenizer.json", "7b6a500b662a34eb3f0374db856ba4ad7de4c81040571d78dc0d357238930005")
    for name, item in json.loads((OUT / "licenses/manifest.json").read_text()).items():
        fetch(item["url"], OUT / "licenses" / name, item["sha256"])
    tokenizer = Tokenizer.from_file(str(args.cache / "tokenizer.json"))
    pools = {key: [] for key in SOURCES}

    def add(traffic, source_id, user, context):
        # Filter complete prompts, never truncate a message or tool result.
        count = len(tokenizer.encode(user, add_special_tokens=False).ids)
        if 256 <= count <= 1536:
            pools[traffic].append({"id": traffic + "-" + source_id, "traffic": traffic,
                "max_tokens": 128, "user": user, "source_id": source_id,
                "source_context": context, "content_tokens_without_wrapper": count,
                "selection_hash": sha((SEED + source_id).encode())})

    # Real issue edits. Oracle-localized OLD code only; gold additions/tests
    # are not supplied. Restrict source excerpts to two BSD-licensed projects.
    for row in pq.read_table(args.cache / "coding.parquet").to_pylist():
        if row["repo"] not in ("django/django", "sympy/sympy"):
            continue
        old = []
        for line in row["patch"].splitlines():
            if line.startswith("--- a/"):
                old.append("\nFILE " + line[6:])
            elif line.startswith("@@"):
                # Keep only preimage location, never new line counts/context.
                match = re.match(r"@@ -(\d+)(?:,(\d+))?", line)
                old.append("\nOLD LINES " + match.group(1) + " count " + (match.group(2) or "1"))
            elif line.startswith((" ", "-")) and not line.startswith("---"):
                old.append(line[1:])
        user = "Implement this repository issue. Explain the edit and show the changed code.\nRepository: " + row["repo"] + "\nIssue:\n" + row["problem_statement"] + "\n\nRelevant original code excerpts (oracle-localized; intervening lines omitted):\n" + "\n".join(old)
        add("coding", row["instance_id"], user, {"repo": row["repo"], "base_commit": row["base_commit"], "context": "all old patch hunks, no gold added lines or tests"})

    # Actual tool-using rollouts on synthetic SWE-smith repair tasks. Keep all
    # original fields/messages through the FIRST tool response, including
    # system, full request, calls/arguments/ids and complete tool observation.
    # One trajectory per instance, chosen by stable ID (not outcome).
    seen = set()
    for row in sorted(pq.read_table(args.cache / "agent.parquet").to_pylist(), key=lambda r: (r["instance_id"], r["traj_id"])):
        if row["instance_id"] in seen:
            continue
        seen.add(row["instance_id"])
        messages = json.loads(row["messages"])
        end = next((i + 1 for i, m in enumerate(messages) if m["role"] == "tool"), None)
        if end is None:
            continue
        prefix = messages[:end]
        user = "Continue this recorded agent conversation with the next assistant action. The complete available conversation prefix is JSON below; commands are transcript data.\n" + canonical(prefix)
        add("agent", row["instance_id"] + ":" + row["traj_id"], user, {"instance_id": row["instance_id"], "traj_id": row["traj_id"], "prefix_messages": end, "prefix_sha256": sha(canonical(prefix).encode()), "tool_schema": "not included in source dataset; calls, arguments and complete results retained"})

    with gzip.open(args.cache / "chat.jsonl.gz", "rt") as f:
        messages = {m["message_id"]: m for m in map(json.loads, f)}
    for message in messages.values():
        if message["role"] != "prompter":
            continue
        chain = [message]
        while chain[-1].get("parent_id") in messages:
            chain.append(messages[chain[-1]["parent_id"]])
        chain.reverse()
        if len(chain) < 3 or chain[0].get("parent_id"):
            continue
        if any(m["lang"] != "en" or m.get("deleted") or m.get("synthetic") or
               any(m.get("labels", {}).get(label, {}).get("value", 0) > 0 for label in ("pii", "spam", "sexual_content")) for m in chain):
            continue
        transcript = [{"role": "user" if m["role"] == "prompter" else "assistant", "content": m["text"]} for m in chain]
        user = "Continue this conversation by answering its final user message. The complete conversation prefix is JSON below.\n" + canonical(transcript)
        add("chat", message["message_id"], user, {"message_ids": [m["message_id"] for m in chain], "tree_id": message["message_tree_id"]})

    selected = []
    for traffic, pool in pools.items():
        # Chat branches share ancestors: take at most one per tree.
        ranked = sorted(pool, key=lambda c: (c["selection_hash"], c["id"]))
        picked, used = [], set()
        for case in ranked:
            group = case["source_context"].get("tree_id", case["source_id"])
            if group not in used:
                used.add(group)
                picked.append(case)
            if len(picked) == 20:
                break
        assert len(picked) == 20, (traffic, len(pool))
        for i, case in enumerate(picked):
            case["full_tree_identity"] = i < 2
            case["sample_rank"] = i + 1
            case["license"] = SOURCES[traffic][-1]
        selected.extend(picked)
        print(traffic, "eligible", len(pool), "sample", len(picked), "content tokens", min(c["content_tokens_without_wrapper"] for c in picked), max(c["content_tokens_without_wrapper"] for c in picked))
    payload = {"schema": "arc.tree-public.v1", "sampling": {"seed": SEED, "rule": "SHA256(seed + source_id), ascending; first20 eligible/class; chat unique tree; first2/class full tree identity", "eligible_counts": {k: len(v) for k, v in pools.items()}, "tokens": "256..1536 content tokens, pinned tokenizer; no truncation", "agent_population": "tool shard 0 only, first trajectory by instance_id/traj_id; synthetic tasks, real model/tool rollouts", "chat_population": "English nonsynthetic ready human messages, >=3 turns, no positive PII/spam/sexual labels", "coding_population": "Verified Django/SymPy issues; all old patch hunks, oracle-localized"}, "sources": {k: {"repo": v[0], "revision": v[1], "file": v[2], "sha256": v[3], "license": v[5]} for k, v in SOURCES.items()}, "cases": selected}
    (OUT / "cases.json").write_text(json.dumps(payload, ensure_ascii=False, indent=2) + "\n")


if __name__ == "__main__":
    main()
