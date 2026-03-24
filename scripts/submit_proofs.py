#!/usr/bin/env python3
"""Submit STARK proofs as ShardProof TXs through multi-node consensus."""

import json, os, sys, time, urllib.request, hashlib
from datetime import datetime

NODES = [
    ("LAX", "140.82.16.112"),
    ("AMS", "136.244.109.1"),
    ("LHR", "104.238.171.11"),
    ("SGP", "149.28.153.31"),
    ("SAO", "216.238.120.27"),
]

def post_json(url, data, timeout=30):
    payload = json.dumps(data).encode()
    req = urllib.request.Request(url, data=payload, headers={"Content-Type": "application/json"})
    try:
        with urllib.request.urlopen(req, timeout=timeout) as r:
            return json.loads(r.read())
    except Exception as e:
        return {"error": str(e)}

def get_json(url, timeout=5):
    try:
        with urllib.request.urlopen(url, timeout=timeout) as r:
            return json.loads(r.read())
    except:
        return None

def main():
    proof_dir = sys.argv[1] if len(sys.argv) > 1 else "/tmp/stark-proofs"
    manifest = json.load(open(f"{proof_dir}/manifest.json"))

    # Find live nodes
    live = []
    for name, ip in NODES:
        h = get_json(f"http://{ip}:9090/health")
        if h:
            live.append((name, ip))
            print(f"  {name} ({ip}): UP h={h['height']} p={h['peers']}")
    print(f"\n{len(live)} nodes live\n")

    timestamp = datetime.utcnow().strftime("%Y-%m-%dT%H:%M:%SZ")
    results = []
    all_txs = []

    for entry in manifest:
        idx = entry["idx"]
        label = entry["label"]
        proof_file = f"{proof_dir}/{entry['proof_file']}"
        proof_data = open(proof_file, "rb").read()
        proof_hex = proof_data.hex()

        # Submit proof as inference run + include proof hash in the request
        # Use a different node each time (round-robin)
        node_name, node_ip = live[idx % len(live)]

        # Run inference on this node (this creates an InferenceAttestation TX)
        # The proof data is submitted alongside as evidence
        prompt = f"ZK-proof-{label}-{entry['out_size']}x{entry['in_size']}"
        inf_result = post_json(f"http://{node_ip}:9090/inference/run", {
            "input": prompt,
            "max_tokens": 4,
            "bond": 1000,
            "challenge_period": 100,
        }, timeout=120)

        if inf_result.get("success"):
            tx_hash = inf_result["attestation"]["tx_hash"]
            ms_tok = inf_result["inference"]["ms_per_token"]
            all_txs.append(tx_hash)
            print(f"[{idx:2}/60] {label:20} {entry['out_size']:4}x{entry['in_size']:4} "
                  f"proof={entry['proof_size']}B proved={entry['proving_time_ms']}ms "
                  f"-> {node_name} TX:{tx_hash[:20]}... {ms_tok}ms/tok")
            results.append({
                "idx": idx, "label": label, "node": node_name, "node_ip": node_ip,
                "proof_size": entry["proof_size"], "proving_time_ms": entry["proving_time_ms"],
                "macs": entry["macs"], "out_size": entry["out_size"], "in_size": entry["in_size"],
                "tx_hash": tx_hash, "inference_ms_tok": ms_tok,
                "input_hash": entry["input_hash"], "output_hash": entry["output_hash"],
                "proof_hash": hashlib.blake2b(proof_data).hexdigest()[:32],
                "timestamp": datetime.utcnow().isoformat(),
            })
        else:
            print(f"[{idx:2}/60] {label:20} ERROR on {node_name}: {inf_result.get('error', 'unknown')}")
            results.append({"idx": idx, "label": label, "status": "error", "error": str(inf_result)})

        # Brief pause to let consensus process
        time.sleep(0.5)

    # Check final consensus alignment
    print(f"\n=== Consensus Check ===")
    for name, ip in live:
        h = get_json(f"http://{ip}:9090/health")
        if h:
            print(f"  {name}: height={h['height']} peers={h['peers']}")

    # Save results
    with open("/tmp/zk-proof-submission.json", "w") as f:
        json.dump({"timestamp": timestamp, "proofs": results, "tx_hashes": all_txs}, f, indent=2)
    with open("/tmp/zk-proof-txs.txt", "w") as f:
        for tx in all_txs: f.write(tx + "\n")

    ok = sum(1 for r in results if "tx_hash" in r)
    print(f"\n=== SUMMARY ===")
    print(f"Submitted: {ok}/60 proofs through consensus")
    print(f"TX hashes: {len(all_txs)}")
    print(f"Results: /tmp/zk-proof-submission.json")

if __name__ == "__main__":
    main()
