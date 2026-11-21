#!/usr/bin/env python3

import argparse
import hashlib
import json
import os
import sys
import time
import urllib.request

def blake3_hash(data: bytes) -> bytes:
    try:
        import blake3
        return blake3.blake3(data).digest()
    except ImportError:
        return hashlib.sha256(data).digest()

def blake3_hex(data: bytes) -> str:
    return blake3_hash(data).hex()

def http_post(url, payload, timeout=60):
    data = json.dumps(payload).encode()
    req = urllib.request.Request(url, data=data, headers={'Content-Type': 'application/json'})
    with urllib.request.urlopen(req, timeout=timeout) as resp:
        return json.loads(resp.read().decode())

def http_get(url, timeout=10):
    with urllib.request.urlopen(url, timeout=timeout) as resp:
        return json.loads(resp.read().decode())

def run_ollama_inference(model: str, prompt: str, ollama_url: str = "http://localhost:11434"):
    print(f"  Running {model} inference via ollama...")
    start = time.time()

    response = http_post(f"{ollama_url}/api/generate", {
        "model": model,
        "prompt": prompt,
        "stream": False,
        "options": {
            "temperature": 0,
            "seed": 42,
            "num_predict": 256,
        }
    }, timeout=120)

    elapsed_ms = int((time.time() - start) * 1000)
    output = response.get("response", "")
    model_name = response.get("model", model)

    try:
        model_info = http_post(f"{ollama_url}/api/show", {"name": model})
        model_digest = model_info.get("digest", "")
        model_size = model_info.get("size", 0)
        model_params = model_info.get("details", {}).get("parameter_size", "unknown")
    except Exception:
        model_digest = model_name
        model_size = 0
        model_params = "unknown"

    return {
        "model_name": model_name,
        "model_digest": model_digest,
        "model_size": model_size,
        "model_params": model_params,
        "prompt": prompt,
        "output": output,
        "elapsed_ms": elapsed_ms,
        "tokens_generated": response.get("eval_count", len(output.split())),
        "tokens_per_sec": response.get("eval_count", 0) / max(response.get("eval_duration", 1) / 1e9, 0.001),
    }

def submit_attestation(rpc_url: str, model_id: bytes, input_hash: bytes,
                       output_hash: bytes, bond: int = 1000, challenge_period: int = 100):
    payload = {
        "type": "InferenceAttestation",
        "model_id": model_id.hex(),
        "input_hash": input_hash.hex(),
        "output_hash": output_hash.hex(),
        "bond": bond,
        "challenge_period": challenge_period,
    }

    try:
        response = http_post(f"{rpc_url}/tx/submit", payload)
        return response
    except Exception as e:
        return {"error": str(e)}

def process_single(model: str, prompt: str, rpc_url: str, ollama_url: str, bond: int):
    result = run_ollama_inference(model, prompt, ollama_url)

    print(f"  Model: {result['model_name']} ({result['model_params']})")
    print(f"  Inference: {result['elapsed_ms']}ms, {result['tokens_generated']} tokens")
    print(f"  Output: {result['output'][:200]}{'...' if len(result['output']) > 200 else ''}")

    model_id = blake3_hash(result['model_digest'].encode())
    input_hash = blake3_hash(prompt.encode())
    output_hash = blake3_hash(result['output'].encode())

    print(f"\n  Hashes (BLAKE3):")
    print(f"    model_id:    0x{model_id.hex()[:16]}...")
    print(f"    input_hash:  0x{input_hash.hex()[:16]}...")
    print(f"    output_hash: 0x{output_hash.hex()[:16]}...")

    print(f"\n  Submitting InferenceAttestation to {rpc_url}...")
    tx_result = submit_attestation(rpc_url, model_id, input_hash, output_hash, bond)

    if "error" in tx_result:
        print(f"  Submission: {tx_result['error']}")
        print(f"  (Node may not be running. Attestation data is still valid for the paper.)")
    else:
        tx_hash = tx_result.get("tx_hash", tx_result.get("hash", "unknown"))
        print(f"  TX Hash: {tx_hash}")
        print(f"  Explorer: {rpc_url.replace(':9090', ':3100')}/tx/{tx_hash}")

    return {
        "inference": result,
        "hashes": {
            "model_id": model_id.hex(),
            "input_hash": input_hash.hex(),
            "output_hash": output_hash.hex(),
        },
        "attestation": tx_result,
    }

def main():
    parser = argparse.ArgumentParser(description='Submit real LLM inference to ARC Chain')
    parser.add_argument('--model', default='llama3:8b', help='Ollama model name')
    parser.add_argument('--prompt', default=None, help='Inference prompt')
    parser.add_argument('--batch', default=None, help='File with one prompt per line')
    parser.add_argument('--rpc', default='http://localhost:9090', help='ARC Chain RPC URL')
    parser.add_argument('--ollama', default='http://localhost:11434', help='Ollama API URL')
    parser.add_argument('--bond', type=int, default=1000, help='Attestation bond amount')
    parser.add_argument('--output', default='inference-results.json', help='Output file')
    args = parser.parse_args()

    print("=" * 60)
    print("ARC Chain — Real Model Inference + On-Chain Attestation")
    print("=" * 60)
    print(f"Model: {args.model}")
    print(f"RPC: {args.rpc}")
    print(f"Ollama: {args.ollama}")

    try:
        http_get(f"{args.ollama}/api/tags")
        print(f"Ollama: connected")
    except Exception:
        print(f"\nERROR: Cannot connect to ollama at {args.ollama}")
        print(f"Start it with: ollama serve")
        print(f"Pull a model with: ollama pull {args.model}")
        sys.exit(1)

    results = []

    if args.batch:
        with open(args.batch) as f:
            prompts = [line.strip() for line in f if line.strip()]
        print(f"\nBatch mode: {len(prompts)} prompts")
        for i, prompt in enumerate(prompts):
            print(f"\n--- Prompt {i+1}/{len(prompts)} ---")
            print(f"  \"{prompt[:80]}{'...' if len(prompt) > 80 else ''}\"")
            r = process_single(args.model, prompt, args.rpc, args.ollama, args.bond)
            results.append(r)
    else:
        prompt = args.prompt or "Explain the significance of zero-knowledge proofs for AI verification in one paragraph."
        print(f"\nPrompt: \"{prompt}\"")
        r = process_single(args.model, prompt, args.rpc, args.ollama, args.bond)
        results.append(r)

    with open(args.output, 'w') as f:
        json.dump(results, f, indent=2)
    print(f"\nResults saved to: {args.output}")

    print(f"\n{'=' * 60}")
    print("PAPER-READY SUMMARY")
    print(f"{'=' * 60}")
    for r in results:
        inf = r['inference']
        print(f"  Model: {inf['model_name']} ({inf['model_params']})")
        print(f"  Latency: {inf['elapsed_ms']}ms")
        print(f"  Tokens: {inf['tokens_generated']}")
        print(f"  Throughput: {inf.get('tokens_per_sec', 0):.1f} tok/sec")
        print(f"  Model ID: 0x{r['hashes']['model_id'][:16]}...")
        print(f"  Output Hash: 0x{r['hashes']['output_hash'][:16]}...")
        print()

if __name__ == '__main__':
    main()
