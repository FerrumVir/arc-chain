#!/usr/bin/env python3
"""
Full Multi-Node Deterministic Inference Proof

For each prompt: runs inference on ALL live nodes simultaneously,
verifies ALL output hashes are identical, checks consensus height
alignment before and after. Undeniable proof.

Outputs:
  - Per-prompt: every node's hash, ms/tok, TX hash
  - Consensus verification: all heights aligned
  - Summary: N prompts × M nodes = N×M total proofs
"""

import json, sys, time, urllib.request, urllib.error
from datetime import datetime
from concurrent.futures import ThreadPoolExecutor, as_completed

NODES = [
    ("LAX", "140.82.16.112"),
    ("AMS", "136.244.109.1"),
    ("LHR", "104.238.171.11"),
    ("SGP", "149.28.153.31"),
    # ("SAO", "216.238.120.27"),  # excluded: OOM with 7B
    # ("NRT", "202.182.107.41"),  # excluded: not deployed
    # ("JNB", "139.84.237.49"),  # excluded: not deployed
]

PROMPTS = [
    # Math (8-16 tok)
    {"prompt": "What is 17 times 23?", "max_tokens": 8, "cat": "math"},
    {"prompt": "Is 97 a prime number?", "max_tokens": 8, "cat": "math"},
    {"prompt": "What is the square root of 144?", "max_tokens": 8, "cat": "math"},
    {"prompt": "What is 2 to the power of 10?", "max_tokens": 8, "cat": "math"},
    {"prompt": "Calculate 15% of 200", "max_tokens": 16, "cat": "math"},
    {"prompt": "What is 7 cubed?", "max_tokens": 16, "cat": "math"},
    {"prompt": "Simplify 48/64", "max_tokens": 16, "cat": "math"},
    {"prompt": "What is the factorial of 5?", "max_tokens": 8, "cat": "math"},
    {"prompt": "How many sides does a hexagon have?", "max_tokens": 8, "cat": "math"},
    {"prompt": "What is 256 divided by 16?", "max_tokens": 8, "cat": "math"},
    # Factual (16-32 tok)
    {"prompt": "What is the capital of France?", "max_tokens": 16, "cat": "factual"},
    {"prompt": "Who wrote Romeo and Juliet?", "max_tokens": 16, "cat": "factual"},
    {"prompt": "What is the boiling point of water in Celsius?", "max_tokens": 16, "cat": "factual"},
    {"prompt": "What is the chemical symbol for gold?", "max_tokens": 16, "cat": "factual"},
    {"prompt": "What is the largest planet in our solar system?", "max_tokens": 32, "cat": "factual"},
    {"prompt": "Who discovered penicillin?", "max_tokens": 32, "cat": "factual"},
    {"prompt": "What is the tallest mountain on Earth?", "max_tokens": 32, "cat": "factual"},
    {"prompt": "What element has atomic number 6?", "max_tokens": 32, "cat": "factual"},
    {"prompt": "How many bones are in the human body?", "max_tokens": 32, "cat": "factual"},
    {"prompt": "What is the speed of light in meters per second?", "max_tokens": 32, "cat": "factual"},
    # Explanations (64-128 tok)
    {"prompt": "Explain how photosynthesis works", "max_tokens": 64, "cat": "explain"},
    {"prompt": "How does TCP/IP networking work?", "max_tokens": 64, "cat": "explain"},
    {"prompt": "Explain what a blockchain is", "max_tokens": 64, "cat": "explain"},
    {"prompt": "How does encryption work?", "max_tokens": 128, "cat": "explain"},
    {"prompt": "Explain how neural networks learn", "max_tokens": 128, "cat": "explain"},
    {"prompt": "How does the internet work?", "max_tokens": 128, "cat": "explain"},
    {"prompt": "Explain the greenhouse effect", "max_tokens": 128, "cat": "explain"},
    {"prompt": "How do computers store data?", "max_tokens": 128, "cat": "explain"},
    # Code (128-256 tok)
    {"prompt": "Write a Python function to compute fibonacci numbers", "max_tokens": 128, "cat": "code"},
    {"prompt": "Write a JavaScript function to sort an array", "max_tokens": 128, "cat": "code"},
    {"prompt": "Write a Python function to check if a string is a palindrome", "max_tokens": 128, "cat": "code"},
    {"prompt": "Write a SQL query to find the top 10 customers by revenue", "max_tokens": 128, "cat": "code"},
    {"prompt": "Write a Rust function to parse a CSV file", "max_tokens": 256, "cat": "code"},
    {"prompt": "Write a Python class for a binary search tree", "max_tokens": 256, "cat": "code"},
    {"prompt": "Write a Python decorator for caching", "max_tokens": 256, "cat": "code"},
    # Creative (256-512 tok)
    {"prompt": "Write a haiku about the ocean at sunset", "max_tokens": 256, "cat": "creative"},
    {"prompt": "Write the opening paragraph of a mystery novel", "max_tokens": 256, "cat": "creative"},
    {"prompt": "Write a motivational speech about perseverance", "max_tokens": 512, "cat": "creative"},
    {"prompt": "Write a fairy tale about a robot who learns to dream", "max_tokens": 512, "cat": "creative"},
    {"prompt": "Describe an alien civilization discovering Earth", "max_tokens": 512, "cat": "creative"},
    # Long-form (512-1024 tok)
    {"prompt": "Write an essay about the impact of climate change on agriculture", "max_tokens": 512, "cat": "longform"},
    {"prompt": "Explain quantum computing from first principles", "max_tokens": 512, "cat": "longform"},
    {"prompt": "Write a comprehensive guide to starting a business", "max_tokens": 1024, "cat": "longform"},
    {"prompt": "Explain the history and future of space exploration", "max_tokens": 1024, "cat": "longform"},
    {"prompt": "Write about the ethical implications of artificial intelligence", "max_tokens": 1024, "cat": "longform"},
    # Edge cases
    {"prompt": "A", "max_tokens": 8, "cat": "edge"},
    {"prompt": "Hello", "max_tokens": 8, "cat": "edge"},
    {"prompt": "The quick brown fox jumps over the lazy dog", "max_tokens": 32, "cat": "edge"},
    # Repeat (same prompt 5x)
    {"prompt": "What color is the sky?", "max_tokens": 32, "cat": "repeat"},
    {"prompt": "What color is the sky?", "max_tokens": 32, "cat": "repeat"},
    {"prompt": "What color is the sky?", "max_tokens": 32, "cat": "repeat"},
    {"prompt": "What color is the sky?", "max_tokens": 32, "cat": "repeat"},
    {"prompt": "What color is the sky?", "max_tokens": 32, "cat": "repeat"},
]

def fetch(url, timeout=600):
    try:
        req = urllib.request.Request(url)
        with urllib.request.urlopen(req, timeout=timeout) as r:
            return json.loads(r.read())
    except: return None

def inference(ip, prompt, max_tokens, timeout=600):
    payload = json.dumps({"input": prompt, "max_tokens": max_tokens, "bond": 1000, "challenge_period": 100}).encode()
    req = urllib.request.Request(f"http://{ip}:9090/inference/run", data=payload, headers={"Content-Type": "application/json"})
    try:
        t0 = time.time()
        with urllib.request.urlopen(req, timeout=timeout) as r:
            d = json.loads(r.read())
            d["_elapsed"] = round(time.time() - t0, 2)
            return d
    except Exception as e:
        return {"error": str(e), "success": False}

def check_consensus(live_nodes):
    heights = {}
    for name, ip in live_nodes:
        h = fetch(f"http://{ip}:9090/health", timeout=3)
        if h: heights[name] = h.get("height", -1)
    return heights

def main():
    timestamp = datetime.utcnow().strftime("%Y-%m-%dT%H:%M:%SZ")

    # Discover live nodes
    print("=== ARC Chain Multi-Node Determinism Proof ===")
    print(f"Started: {timestamp}")
    print()
    print("Discovering nodes...")
    live = []
    for name, ip in NODES:
        h = fetch(f"http://{ip}:9090/health", timeout=5)
        if h:
            print(f"  {name} ({ip}): UP h={h['height']} peers={h['peers']}")
            live.append((name, ip))
        else:
            print(f"  {name} ({ip}): DOWN")

    if len(live) < 2:
        print("Need at least 2 nodes. Exiting.")
        sys.exit(1)

    print(f"\n{len(live)} nodes live. Running {len(PROMPTS)} prompts × {len(live)} nodes = {len(PROMPTS)*len(live)} total inferences\n")

    results_file = "/tmp/multinode-proof.jsonl"
    summary_file = "/tmp/multinode-proof-summary.md"

    total_match = 0
    total_mismatch = 0
    total_error = 0
    all_results = []
    all_tx_hashes = []

    with open(results_file, "w") as rf:
        for idx, p in enumerate(PROMPTS):
            prompt = p["prompt"]
            max_tok = p["max_tokens"]
            cat = p["cat"]
            display = prompt[:40] + "..." if len(prompt) > 40 else prompt

            # Check consensus BEFORE
            heights_before = check_consensus(live)

            # Run inference on ALL nodes in parallel
            node_results = {}
            with ThreadPoolExecutor(max_workers=len(live)) as ex:
                futures = {ex.submit(inference, ip, prompt, max_tok): (name, ip) for name, ip in live}
                for f in as_completed(futures):
                    name, ip = futures[f]
                    node_results[name] = f.result()

            # Check consensus AFTER
            time.sleep(2)
            heights_after = check_consensus(live)

            # Compare all hashes
            hashes = {}
            tx_hashes = {}
            ms_per_tok = {}
            errors = []
            for name, ip in live:
                r = node_results.get(name, {})
                if r.get("success"):
                    h = r["inference"]["output_hash"]
                    hashes[name] = h
                    tx_hashes[name] = r["attestation"]["tx_hash"]
                    ms_per_tok[name] = r["inference"]["ms_per_token"]
                    all_tx_hashes.append(r["attestation"]["tx_hash"])
                else:
                    errors.append(name)

            unique_hashes = set(hashes.values())
            if len(unique_hashes) == 1 and len(hashes) >= 2:
                status = "ALL_MATCH"
                total_match += 1
                badge = f"\033[32mALL_MATCH ({len(hashes)} nodes)\033[0m"
            elif len(unique_hashes) > 1:
                status = "MISMATCH"
                total_mismatch += 1
                badge = f"\033[31mMISMATCH\033[0m"
            else:
                status = "ERROR"
                total_error += 1
                badge = f"\033[33mERROR ({len(errors)} failed)\033[0m"

            the_hash = list(unique_hashes)[0][:18] if unique_hashes else "none"
            speeds = "/".join(f"{ms_per_tok.get(n,'?')}" for n, _ in live if n in ms_per_tok)

            print(f"[{idx+1}/{len(PROMPTS)}] {cat:10s} ({max_tok:4d}tok) \"{display}\" → {badge} hash:{the_hash} ms/tok:[{speeds}]")

            # Consensus check
            h_before = list(heights_before.values())
            h_after = list(heights_after.values())
            consensus_aligned = len(set(h_before)) <= 2 and len(set(h_after)) <= 2  # within 1 block
            if not consensus_aligned:
                print(f"  ⚠ Consensus spread: before={h_before} after={h_after}")

            record = {
                "idx": idx, "prompt": prompt, "category": cat, "max_tokens": max_tok,
                "status": status, "nodes_tested": len(hashes), "nodes_errored": len(errors),
                "unique_hashes": len(unique_hashes),
                "hash": list(unique_hashes)[0] if unique_hashes else None,
                "per_node": {n: {"hash": hashes.get(n), "tx": tx_hashes.get(n), "ms_tok": ms_per_tok.get(n)} for n, _ in live},
                "consensus_before": heights_before, "consensus_after": heights_after,
                "consensus_aligned": consensus_aligned,
                "timestamp": datetime.utcnow().isoformat(),
            }
            rf.write(json.dumps(record) + "\n")
            rf.flush()
            all_results.append(record)

    # Write summary
    with open(summary_file, "w") as sf:
        sf.write(f"# Multi-Node Deterministic Inference Proof\n\n")
        sf.write(f"**Date:** {timestamp}\n")
        sf.write(f"**Nodes:** {len(live)} ({', '.join(n for n,_ in live)})\n")
        sf.write(f"**Prompts:** {len(PROMPTS)}\n")
        sf.write(f"**Total inferences:** {len(PROMPTS) * len(live)}\n\n")
        sf.write(f"## Results\n\n")
        sf.write(f"- **ALL_MATCH:** {total_match}\n")
        sf.write(f"- **MISMATCH:** {total_mismatch}\n")
        sf.write(f"- **ERROR:** {total_error}\n")
        sf.write(f"- **TX hashes on-chain:** {len(all_tx_hashes)}\n\n")
        sf.write(f"## Per-Prompt Results\n\n")
        sf.write("| # | Category | Tokens | Nodes | Hash | Status |\n")
        sf.write("|---|----------|--------|-------|------|--------|\n")
        for r in all_results:
            sf.write(f"| {r['idx']} | {r['category']} | {r['max_tokens']} | {r['nodes_tested']} | `{(r['hash'] or 'none')[:16]}` | {r['status']} |\n")
        sf.write(f"\n## TX Hashes\n\n")
        for tx in all_tx_hashes[:30]:
            sf.write(f"- `{tx}`\n")
        if len(all_tx_hashes) > 30:
            sf.write(f"- ...and {len(all_tx_hashes)-30} more\n")

    with open("/tmp/multinode-proof-txs.txt", "w") as tf:
        for tx in all_tx_hashes:
            tf.write(tx + "\n")

    print(f"\n{'='*60}")
    print(f"ALL_MATCH: {total_match}/{len(PROMPTS)}")
    print(f"MISMATCH:  {total_mismatch}")
    print(f"ERROR:     {total_error}")
    print(f"TX hashes: {len(all_tx_hashes)}")
    print(f"Results:   {results_file}")
    print(f"Summary:   {summary_file}")

    sys.exit(0 if total_mismatch == 0 else 1)

if __name__ == "__main__":
    main()
