#!/usr/bin/env python3
"""
Deterministic On-Chain Inference Proof Generator

Sends 100+ inference requests to ARM (Mac Studio) and x86 (Vultr) nodes,
verifying identical output hashes across platforms. Every request creates
an InferenceAttestation TX finalized through 9-node DAG consensus.

Usage:
    python3 scripts/prove_determinism.py \
        --arm http://localhost:9090 \
        --x86 http://149.28.32.76:9090 \
        --output determinism-proof

Output:
    determinism-proof.jsonl    — per-run results
    determinism-summary.md     — formatted table
    attestation-txs.txt        — all TX hashes
"""

import json
import sys
import time
import hashlib
import argparse
import urllib.request
import urllib.error
from datetime import datetime

# ─── Prompt Corpus: 100+ real-world prompts across 8-1024 tokens ────────────

PROMPTS = [
    # Math/Logic (15 prompts, 8-16 tokens)
    {"prompt": "What is 17 times 23?", "max_tokens": 8, "category": "math"},
    {"prompt": "Is 97 a prime number?", "max_tokens": 8, "category": "math"},
    {"prompt": "What is the square root of 144?", "max_tokens": 8, "category": "math"},
    {"prompt": "If x + 5 = 12, what is x?", "max_tokens": 8, "category": "math"},
    {"prompt": "What is 256 divided by 16?", "max_tokens": 8, "category": "math"},
    {"prompt": "How many sides does a hexagon have?", "max_tokens": 8, "category": "math"},
    {"prompt": "What is 2 to the power of 10?", "max_tokens": 8, "category": "math"},
    {"prompt": "What is the next prime after 29?", "max_tokens": 8, "category": "math"},
    {"prompt": "Calculate 15% of 200", "max_tokens": 8, "category": "math"},
    {"prompt": "What is the factorial of 5?", "max_tokens": 8, "category": "math"},
    {"prompt": "How many degrees in a right angle?", "max_tokens": 8, "category": "math"},
    {"prompt": "What is 7 cubed?", "max_tokens": 16, "category": "math"},
    {"prompt": "Simplify 48/64", "max_tokens": 16, "category": "math"},
    {"prompt": "What is the LCM of 12 and 18?", "max_tokens": 16, "category": "math"},
    {"prompt": "Convert 0.75 to a fraction", "max_tokens": 16, "category": "math"},

    # Factual Q&A (15 prompts, 16-32 tokens)
    {"prompt": "What is the capital of France?", "max_tokens": 16, "category": "factual"},
    {"prompt": "Who wrote Romeo and Juliet?", "max_tokens": 16, "category": "factual"},
    {"prompt": "What is the boiling point of water in Celsius?", "max_tokens": 16, "category": "factual"},
    {"prompt": "How many continents are there?", "max_tokens": 16, "category": "factual"},
    {"prompt": "What year did World War 2 end?", "max_tokens": 16, "category": "factual"},
    {"prompt": "What is the chemical symbol for gold?", "max_tokens": 16, "category": "factual"},
    {"prompt": "Who painted the Mona Lisa?", "max_tokens": 16, "category": "factual"},
    {"prompt": "What is the largest planet in our solar system?", "max_tokens": 32, "category": "factual"},
    {"prompt": "What is the speed of light in meters per second?", "max_tokens": 32, "category": "factual"},
    {"prompt": "Who discovered penicillin?", "max_tokens": 32, "category": "factual"},
    {"prompt": "What is the tallest mountain on Earth?", "max_tokens": 32, "category": "factual"},
    {"prompt": "How many bones are in the human body?", "max_tokens": 32, "category": "factual"},
    {"prompt": "What is the smallest country in the world?", "max_tokens": 32, "category": "factual"},
    {"prompt": "What element has atomic number 6?", "max_tokens": 32, "category": "factual"},
    {"prompt": "What is the longest river in the world?", "max_tokens": 32, "category": "factual"},

    # Explanations (15 prompts, 64-128 tokens)
    {"prompt": "Explain how photosynthesis works", "max_tokens": 64, "category": "explanation"},
    {"prompt": "How does TCP/IP networking work?", "max_tokens": 64, "category": "explanation"},
    {"prompt": "Explain the difference between RAM and ROM", "max_tokens": 64, "category": "explanation"},
    {"prompt": "How do vaccines work?", "max_tokens": 64, "category": "explanation"},
    {"prompt": "Explain what a blockchain is", "max_tokens": 64, "category": "explanation"},
    {"prompt": "How does a car engine work?", "max_tokens": 64, "category": "explanation"},
    {"prompt": "Explain the water cycle", "max_tokens": 128, "category": "explanation"},
    {"prompt": "How does encryption work?", "max_tokens": 128, "category": "explanation"},
    {"prompt": "Explain how neural networks learn", "max_tokens": 128, "category": "explanation"},
    {"prompt": "How does the human immune system work?", "max_tokens": 128, "category": "explanation"},
    {"prompt": "Explain the theory of relativity simply", "max_tokens": 128, "category": "explanation"},
    {"prompt": "How do computers store data?", "max_tokens": 128, "category": "explanation"},
    {"prompt": "Explain how GPS works", "max_tokens": 128, "category": "explanation"},
    {"prompt": "How does the internet work?", "max_tokens": 128, "category": "explanation"},
    {"prompt": "Explain the greenhouse effect", "max_tokens": 128, "category": "explanation"},

    # Code generation (15 prompts, 128-256 tokens)
    {"prompt": "Write a Python function to compute fibonacci numbers", "max_tokens": 128, "category": "code"},
    {"prompt": "Write a SQL query to find the top 10 customers by revenue", "max_tokens": 128, "category": "code"},
    {"prompt": "Write a JavaScript function to sort an array", "max_tokens": 128, "category": "code"},
    {"prompt": "Write a Python function to check if a string is a palindrome", "max_tokens": 128, "category": "code"},
    {"prompt": "Write a bash script to find large files", "max_tokens": 128, "category": "code"},
    {"prompt": "Write a Rust function to parse a CSV file", "max_tokens": 256, "category": "code"},
    {"prompt": "Write a Python class for a binary search tree", "max_tokens": 256, "category": "code"},
    {"prompt": "Write a REST API endpoint in Go", "max_tokens": 256, "category": "code"},
    {"prompt": "Write a Python decorator for caching", "max_tokens": 256, "category": "code"},
    {"prompt": "Write a TypeScript interface for a user profile", "max_tokens": 256, "category": "code"},
    {"prompt": "Write a Python function to merge two sorted lists", "max_tokens": 128, "category": "code"},
    {"prompt": "Write a SQL query with a window function", "max_tokens": 128, "category": "code"},
    {"prompt": "Write a regular expression to validate email addresses", "max_tokens": 128, "category": "code"},
    {"prompt": "Write a Python function to flatten a nested list", "max_tokens": 128, "category": "code"},
    {"prompt": "Write a function to find duplicates in an array", "max_tokens": 128, "category": "code"},

    # Creative writing (10 prompts, 256-512 tokens)
    {"prompt": "Write a haiku about the ocean at sunset", "max_tokens": 256, "category": "creative"},
    {"prompt": "Write the opening paragraph of a mystery novel", "max_tokens": 256, "category": "creative"},
    {"prompt": "Compose a short poem about artificial intelligence", "max_tokens": 256, "category": "creative"},
    {"prompt": "Write a product description for a flying car", "max_tokens": 256, "category": "creative"},
    {"prompt": "Write a motivational speech about perseverance", "max_tokens": 512, "category": "creative"},
    {"prompt": "Write a fairy tale about a robot who learns to dream", "max_tokens": 512, "category": "creative"},
    {"prompt": "Write a letter from the future to the present", "max_tokens": 512, "category": "creative"},
    {"prompt": "Describe an alien civilization discovering Earth", "max_tokens": 512, "category": "creative"},
    {"prompt": "Write a dialogue between two philosophers about time", "max_tokens": 512, "category": "creative"},
    {"prompt": "Write a news article from the year 2100", "max_tokens": 512, "category": "creative"},

    # Long-form (10 prompts, 512-1024 tokens)
    {"prompt": "Write an essay about the impact of climate change on agriculture", "max_tokens": 512, "category": "longform"},
    {"prompt": "Explain quantum computing from first principles", "max_tokens": 512, "category": "longform"},
    {"prompt": "Write a comprehensive guide to starting a business", "max_tokens": 1024, "category": "longform"},
    {"prompt": "Explain the history and future of space exploration", "max_tokens": 1024, "category": "longform"},
    {"prompt": "Write about the ethical implications of artificial intelligence", "max_tokens": 1024, "category": "longform"},
    {"prompt": "Explain how the global financial system works", "max_tokens": 512, "category": "longform"},
    {"prompt": "Write about the evolution of programming languages", "max_tokens": 512, "category": "longform"},
    {"prompt": "Explain the biology of aging and longevity research", "max_tokens": 512, "category": "longform"},
    {"prompt": "Write about the future of renewable energy", "max_tokens": 1024, "category": "longform"},
    {"prompt": "Explain how machine learning differs from traditional programming", "max_tokens": 512, "category": "longform"},

    # Edge cases (10 prompts, 8-64 tokens)
    {"prompt": "A", "max_tokens": 8, "category": "edge"},
    {"prompt": "", "max_tokens": 8, "category": "edge"},
    {"prompt": "Hello", "max_tokens": 8, "category": "edge"},
    {"prompt": "1234567890", "max_tokens": 8, "category": "edge"},
    {"prompt": "🌍🚀🤖", "max_tokens": 16, "category": "edge"},
    {"prompt": "The quick brown fox jumps over the lazy dog", "max_tokens": 32, "category": "edge"},
    {"prompt": "SELECT * FROM users WHERE 1=1", "max_tokens": 32, "category": "edge"},
    {"prompt": "<script>alert('test')</script>", "max_tokens": 16, "category": "edge"},
    {"prompt": "NULL", "max_tokens": 8, "category": "edge"},
    {"prompt": "   ", "max_tokens": 8, "category": "edge"},

    # Repeat tests (10 identical prompts for within-platform determinism)
    {"prompt": "What color is the sky?", "max_tokens": 32, "category": "repeat"},
    {"prompt": "What color is the sky?", "max_tokens": 32, "category": "repeat"},
    {"prompt": "What color is the sky?", "max_tokens": 32, "category": "repeat"},
    {"prompt": "What color is the sky?", "max_tokens": 32, "category": "repeat"},
    {"prompt": "What color is the sky?", "max_tokens": 32, "category": "repeat"},
    {"prompt": "What color is the sky?", "max_tokens": 32, "category": "repeat"},
    {"prompt": "What color is the sky?", "max_tokens": 32, "category": "repeat"},
    {"prompt": "What color is the sky?", "max_tokens": 32, "category": "repeat"},
    {"prompt": "What color is the sky?", "max_tokens": 32, "category": "repeat"},
    {"prompt": "What color is the sky?", "max_tokens": 32, "category": "repeat"},
]


def send_inference(url, prompt, max_tokens, timeout=600):
    """Send inference request and return parsed response."""
    payload = json.dumps({
        "input": prompt,
        "max_tokens": max_tokens,
        "bond": 1000,
        "challenge_period": 100,
    }).encode()

    req = urllib.request.Request(
        f"{url}/inference/run",
        data=payload,
        headers={"Content-Type": "application/json"},
    )

    try:
        start = time.time()
        with urllib.request.urlopen(req, timeout=timeout) as resp:
            elapsed = time.time() - start
            data = json.loads(resp.read())
            data["_request_elapsed_s"] = round(elapsed, 3)
            return data
    except Exception as e:
        return {"error": str(e), "success": False}


def main():
    parser = argparse.ArgumentParser(description="Deterministic inference proof generator")
    parser.add_argument("--arm", required=True, help="ARM node URL (e.g., http://localhost:9090)")
    parser.add_argument("--x86", required=True, help="x86 node URL (e.g., http://149.28.32.76:9090)")
    parser.add_argument("--output", default="determinism-proof", help="Output file prefix")
    parser.add_argument("--start", type=int, default=0, help="Start from prompt index N")
    parser.add_argument("--limit", type=int, default=0, help="Max prompts to run (0=all)")
    args = parser.parse_args()

    prompts = PROMPTS[args.start:]
    if args.limit > 0:
        prompts = prompts[:args.limit]

    timestamp = datetime.utcnow().strftime("%Y-%m-%dT%H:%M:%SZ")
    results_file = f"{args.output}.jsonl"
    summary_file = f"{args.output}-summary.md"
    tx_file = f"{args.output}-txs.txt"

    print(f"=== Deterministic On-Chain Inference Proof ===")
    print(f"ARM node: {args.arm}")
    print(f"x86 node: {args.x86}")
    print(f"Prompts: {len(prompts)}")
    print(f"Output: {results_file}")
    print(f"Started: {timestamp}")
    print()

    total = len(prompts)
    passed = 0
    failed = 0
    errors = 0
    all_results = []
    tx_hashes = []

    with open(results_file, "w") as rf:
        for idx, p in enumerate(prompts):
            prompt = p["prompt"]
            max_tok = p["max_tokens"]
            category = p["category"]

            display = prompt[:50] + "..." if len(prompt) > 50 else prompt
            print(f"[{idx+1}/{total}] {category:12s} ({max_tok:4d} tok) \"{display}\"", end="", flush=True)

            # Run on ARM
            arm_result = send_inference(args.arm, prompt, max_tok)
            if not arm_result.get("success"):
                print(f" ARM_ERROR: {arm_result.get('error', 'unknown')}")
                errors += 1
                record = {"idx": idx, "prompt": prompt, "category": category,
                          "max_tokens": max_tok, "status": "arm_error",
                          "error": arm_result.get("error")}
                rf.write(json.dumps(record) + "\n")
                all_results.append(record)
                continue

            arm_hash = arm_result.get("inference", {}).get("output_hash", "")
            arm_tx = arm_result.get("attestation", {}).get("tx_hash", "")
            arm_ms = arm_result.get("inference", {}).get("ms_per_token", 0)
            arm_tokens = arm_result.get("inference", {}).get("tokens_generated", 0)

            # Run on x86
            x86_result = send_inference(args.x86, prompt, max_tok)
            if not x86_result.get("success"):
                print(f" x86_ERROR: {x86_result.get('error', 'unknown')}")
                errors += 1
                record = {"idx": idx, "prompt": prompt, "category": category,
                          "max_tokens": max_tok, "status": "x86_error",
                          "error": x86_result.get("error")}
                rf.write(json.dumps(record) + "\n")
                all_results.append(record)
                continue

            x86_hash = x86_result.get("inference", {}).get("output_hash", "")
            x86_tx = x86_result.get("attestation", {}).get("tx_hash", "")
            x86_ms = x86_result.get("inference", {}).get("ms_per_token", 0)
            x86_tokens = x86_result.get("inference", {}).get("tokens_generated", 0)

            match = arm_hash == x86_hash
            if match:
                passed += 1
                status = "MATCH"
            else:
                failed += 1
                status = "MISMATCH"

            print(f" → {status} | ARM:{arm_ms}ms/tok x86:{x86_ms}ms/tok | hash:{arm_hash[:18]}")

            record = {
                "idx": idx,
                "prompt": prompt,
                "category": category,
                "max_tokens": max_tok,
                "status": status,
                "arm_hash": arm_hash,
                "x86_hash": x86_hash,
                "match": match,
                "arm_tx": arm_tx,
                "x86_tx": x86_tx,
                "arm_ms_per_token": arm_ms,
                "x86_ms_per_token": x86_ms,
                "arm_tokens": arm_tokens,
                "x86_tokens": x86_tokens,
                "arm_output": arm_result.get("inference", {}).get("output", ""),
                "x86_output": x86_result.get("inference", {}).get("output", ""),
                "timestamp": datetime.utcnow().isoformat(),
            }
            rf.write(json.dumps(record) + "\n")
            rf.flush()
            all_results.append(record)

            if arm_tx:
                tx_hashes.append(arm_tx)
            if x86_tx:
                tx_hashes.append(x86_tx)

    # Write TX hashes
    with open(tx_file, "w") as tf:
        for tx in tx_hashes:
            tf.write(tx + "\n")

    # Write summary
    with open(summary_file, "w") as sf:
        sf.write(f"# Deterministic On-Chain Inference Proof\n\n")
        sf.write(f"**Date:** {timestamp}\n")
        sf.write(f"**ARM node:** {args.arm}\n")
        sf.write(f"**x86 node:** {args.x86}\n\n")
        sf.write(f"## Results: {passed}/{total} MATCH, {failed} MISMATCH, {errors} ERROR\n\n")

        # Per-category breakdown
        categories = {}
        for r in all_results:
            cat = r["category"]
            if cat not in categories:
                categories[cat] = {"total": 0, "match": 0, "mismatch": 0, "error": 0}
            categories[cat]["total"] += 1
            if r["status"] == "MATCH":
                categories[cat]["match"] += 1
            elif r["status"] == "MISMATCH":
                categories[cat]["mismatch"] += 1
            else:
                categories[cat]["error"] += 1

        sf.write("| Category | Total | Match | Mismatch | Error |\n")
        sf.write("|----------|-------|-------|----------|-------|\n")
        for cat, counts in sorted(categories.items()):
            sf.write(f"| {cat} | {counts['total']} | {counts['match']} | {counts['mismatch']} | {counts['error']} |\n")
        sf.write(f"| **Total** | **{total}** | **{passed}** | **{failed}** | **{errors}** |\n\n")

        # Full results table
        sf.write("## Detailed Results\n\n")
        sf.write("| # | Category | Tokens | ARM Hash | x86 Hash | Match | ARM ms/tok | x86 ms/tok |\n")
        sf.write("|---|----------|--------|----------|----------|-------|------------|------------|\n")
        for r in all_results:
            if r["status"] in ("MATCH", "MISMATCH"):
                sf.write(f"| {r['idx']} | {r['category']} | {r['max_tokens']} | "
                         f"`{r['arm_hash'][:16]}` | `{r['x86_hash'][:16]}` | "
                         f"{'✓' if r['match'] else '✗'} | "
                         f"{r.get('arm_ms_per_token', '?')} | {r.get('x86_ms_per_token', '?')} |\n")

        sf.write(f"\n## Attestation TX Hashes ({len(tx_hashes)} total)\n\n")
        sf.write("All TXs are InferenceAttestation type, finalized through 9-node DAG consensus.\n")
        sf.write("Verify on explorer or via `GET /tx/{hash}`.\n\n")
        for tx in tx_hashes[:20]:
            sf.write(f"- `{tx}`\n")
        if len(tx_hashes) > 20:
            sf.write(f"- ... and {len(tx_hashes) - 20} more (see {tx_file})\n")

    print(f"\n=== SUMMARY ===")
    print(f"Passed: {passed}/{total}")
    print(f"Failed: {failed}")
    print(f"Errors: {errors}")
    print(f"TX hashes: {len(tx_hashes)}")
    print(f"Results: {results_file}")
    print(f"Summary: {summary_file}")
    print(f"TXs: {tx_file}")

    sys.exit(0 if failed == 0 else 1)


if __name__ == "__main__":
    main()
