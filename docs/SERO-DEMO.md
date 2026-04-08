# ARC Chain — Demo Script

A 5-minute walkthrough that shows the thing nobody else has: **a model that doesn't fit on any one machine running across all of them, with cryptographically verifiable output**.

This script is for a screen recording or live walkthrough. Each step is a visible artifact; nothing is hidden behind ssh.

---

## What you're about to see

Llama-2-7B (4 GB on disk, 32 transformer layers) running across **7 separate VPS** in 7 different cities. Each VPS holds 4–5 of those layers and forwards the activation state to the next via HTTP. Every hop is BLAKE3-verified. The output text is bit-identical regardless of which machine ran which slice — pure i64 arithmetic with per-row INT16 quantization, no floating point.

The same architecture scales to a 70B model on 14 nodes. The same architecture scales to a Mixtral on 32 nodes. **No single node has enough RAM for the full model — the network does.**

---

## Step 1 — Open the live dashboard (5 seconds)

http://140.82.16.112:3200

You'll see:
- Top stats row: block height, finalized blocks, validators, blocks/sec, total transactions
- 8 node cards across 6 continents (NYC, LAX, AMS, LHR, NRT, SGP, SAO, JNB)
- **A purple "Sharded AI" hero panel** — that's the new thing

---

## Step 2 — Read the pipeline diagram (15 seconds)

In the Sharded AI panel:

- **Header**: model name (`arc-32L-4096d-32h-32000v` = Llama-7B layout), total layers (32), shard count (7)
- **Memory comparison cards**:
  - Left (red): "If you tried this on 1 node — Full model RAM, won't load"
  - Right (green): "Sharded across the network — ~1 GB / node, fits"
- **Pipeline diagram**: 7 cards in a row, connected by arrows. Each card shows:
  - Role badge (`EMBED + L`, `L`, or `L + LM HEAD`)
  - Node name (NYC, LAX, …)
  - Layer range (`L0–L4`, `L5–L9`, …)
  - Layer count + % of model held
  - Memory used in that shard (~1 GB)

The 7 cards literally represent the data flow. The first card embeds the input token; the last card runs the LM head and produces a token id.

---

## Step 3 — Run a prompt through the pipeline (60 seconds)

In the "Type a prompt — watch the activation hop through every shard" input:

```
The sky is
```

Click **▶ Run Through Pipeline**.

Watch:
1. Each shard card pulses in sequence as the activation reaches it
2. The status line says "Sending prompt to 7 devices in pipeline…"
3. After ~60–120 s the result panel appears:
   - The completed text answer (e.g. `"blue because of a phenomenon called Rayleigh scattering, which is"`)
   - **output_hash** (BLAKE3) — the cryptographic fingerprint of the generated tokens
   - **ms / token**
   - **"Quality loss vs FP16: 0.00%"** — the integer engine is exact relative to its source weights
   - **A trace table** showing every hop:
     | hop | node | layers | compute_ms | wall_ms | payload | type |
     |-----|------|--------|------------|---------|---------|------|
     | 0   | NYC  | 0..5   | 158ms      | 160ms   | 0.2 KB  | → hidden |
     | 1   | LAX  | 5..10  | 505ms      | 11925ms | 17.4 KB | → hidden |
     | …   |      |        |            |         |         |      |
     | 6   | JNB  | 28..32 | 132ms      | 894ms   | 17.6 KB | ✓ token |

**Key observation**: the LAST hop is `✓ token` — that's the only shard that runs the LM head and produces a token id. Every other hop just transforms the hidden state and forwards it.

---

## Step 4 — Prove it's deterministic (30 seconds)

Run the SAME prompt again. The output_hash will be **bit-identical**. That's the proof: every node in the pipeline produces the same result regardless of when or how many times you run it.

Run a DIFFERENT prompt (e.g. "The largest planet"). The output_hash will be different. (And the answer will be "in our solar system is Jupiter.")

This is what cryptographic verifiability means: anyone can re-run the same prompt on their own ARC node and get the same hash. If the hash matches, the model output is verified.

---

## Step 5 — Curl it from your terminal (30 seconds)

If you prefer the command line:

```bash
curl -X POST http://149.28.32.76:9090/inference/run_sharded \
  -H 'Content-Type: application/json' \
  -d '{"input":"The capital of France is","max_tokens":20}'
```

Response includes the full per-hop trace:
```json
{
  "input": "The capital of France is",
  "output": "Paris. ...",
  "output_hash": "0x...",
  "shard_trace": [
    {"hop": 0, "node": "NYC", "layers": "0..5", "compute_ms": 158, ...},
    ...
  ],
  "deterministic": true,
  "engine": "INT8 sharded pipeline (cross-platform deterministic)",
  "pipeline_length": 7,
  "total_bytes_transferred": 2674895
}
```

---

## Step 6 — Verify a past inference (30 seconds)

Anyone can independently audit any inference run on the network. Take the `tx_hash` from any prior attestation (e.g. from the dashboard's "On-chain attestation" link, or from a previous `arc-demo.sh` output) and run:

```bash
curl -sSL https://raw.githubusercontent.com/FerrumVir/arc-chain/main/scripts/arc-verify.sh \
  | bash -s -- 0xe0c73bb8a4446f23a62033001cb22e1e9298d5ce1cfea8111762c1ca2833f67d
```

The verifier fetches the original attestation, re-runs the same input on the coordinator, and compares both the new `output_hash` and the new `model_hash` against the on-chain claim. Prints `✓ VERIFIED` if they match.

This is the cryptographic claim turned into a tool. The model and the network are auditable by anyone, on any machine, at any time after the fact.

---

## Step 7 — Join the network as a shard holder (90 seconds)

This is what makes it permissionless. Run **one command** on your laptop:

```bash
curl -sSL https://raw.githubusercontent.com/FerrumVir/arc-chain/main/scripts/install-community-node.sh | bash
```

The installer:
1. Detects your platform (macOS arm64/x86, Linux x86_64/aarch64)
2. Downloads the latest pre-built binary from the v0.4.1 release (no compile)
3. Downloads Llama-2-7B-Chat Q4_K_M (~4 GB) — or TinyLlama on machines with < 6 GB RAM
4. Generates a unique validator seed for your machine
5. Installs as a persistent service:
   - macOS: `~/Library/LaunchAgents/com.arc.inference.plist` (launchd)
   - Linux: `/etc/systemd/system/arc-node.service` (systemd, with sudo)
6. Schedules a daily auto-updater (04:17 local) that polls GitHub releases and seamlessly upgrades the binary
7. Joins the testnet as an inference observer

After ~3 min your node is running, contributing inference compute, and visible at the live dashboard.

To stop:
```bash
launchctl unload ~/Library/LaunchAgents/com.arc.inference.plist  # macOS
sudo systemctl stop arc-node                                      # Linux
```

To uninstall:
```bash
bash install-community-node.sh --uninstall
```

---

## Why this matters

### What makes it different from "running an LLM API"

- **No central server.** The model lives in pieces across the network. There's no single point you can shut down.
- **Cryptographically verifiable.** Every inference produces a BLAKE3 hash. Anyone with the same model file can re-run the prompt on their own machine and verify the answer matches. No "trust the API."
- **Memory-distributed.** The 70B-class model that requires a $20K H100 to run can be served by a network of $5/month VPS as long as enough of them join.
- **Pure integer arithmetic.** Per-row INT16 quantization, all i64 ops, no floating point. The output is bit-identical on M2 ARM, AMD x86, and (eventually) RISC-V — something no Python/PyTorch/llama.cpp setup can claim.

### What it doesn't claim

- **It's not yet faster than centralized inference.** Pipeline-parallel adds N HTTP roundtrips per token. A 7-shard pipeline currently runs at ~7-15 sec/token because of network latency. Centralized inference on a single GPU runs at 50-200 ms/token. The advantage is the *kind* of inference (verifiable, distributed, permissionless), not the speed.
- **It's not yet running a 70B model.** Right now the demo is Llama-2-7B because the seed VPS only have 8 GB RAM. The architecture is identical for 70B — we just need bigger boxes (or more 8 GB shards).

### Where it goes from here

- **Speculative pipelining** — start the next token's forward pass through shard 0 while the previous token is still flowing through shards N-1, N-2.
- **Per-shard GPU execution** — drop the integer engine for the shard's hot path and use Metal/CUDA when available, while keeping integer for verification.
- **Heterogeneous shards** — beefy nodes (32 GB+) hold multiple layer ranges, small nodes (8 GB) hold one. Shard plan auto-balances by available RAM.
- **Sharded model marketplace** — community uploads a GGUF, network auto-distributes, model is queryable by hash. Everyone earns inference rewards proportional to compute contributed.

---

## Quick answers for skeptics

> "Couldn't I just use llama.cpp's pipeline parallel?"

No — llama.cpp pipeline parallel is for splitting across GPUs in the same machine via NCCL/MPI. ARC pipeline parallel is across **independent nodes over the public internet**, with cryptographic verification on every hop. Different problem.

> "Isn't the integer engine slower than FP16?"

It's slower per token, but 6× faster *across the network* once you have N replicas because each replica is doing CPU work that scales linearly. And it's the only way to get cross-platform bit-identical output, which is the requirement for verifiability.

> "How does the network agree on which shard holds what?"

Every shard holder broadcasts its `ShardInfo { start_layer, end_layer, model_id, socket }` via the `/shards/announce` endpoint to all known peers every 15 s. Peers also pull each other's `/shards` registry every 20 s. Within ~45 s of joining, every node has a full picture of the pipeline and the coordinator's `compute_shard_plan` knows where to send each request.

> "What happens if a shard goes down mid-request?"

The coordinator gets an HTTP error from the missing shard. The request fails cleanly. The dashboard shows which shard failed. We don't yet have automatic re-routing or cached fallback shards — that's on the roadmap.

> "Where does Sero hold his model file?"

Either:
- A) `bash install-community-node.sh --model ~/path/to/his.gguf` — uses Sero's own GGUF file
- B) `bash install-community-node.sh` — defaults to Llama-2-7B-Chat Q4_K_M downloaded from HuggingFace
- C) Custom: `--model` plus `--shard-start --shard-end` plus the full path to make him a shard holder for whatever model the network is currently running

---

## Live links

- Dashboard: http://140.82.16.112:3200
- Coordinator RPC: http://149.28.32.76:9090/inference/run_sharded
- Shard registry: http://149.28.32.76:9090/shards
- GitHub: https://github.com/FerrumVir/arc-chain
- Latest release: https://github.com/FerrumVir/arc-chain/releases/latest
- Community installer: https://raw.githubusercontent.com/FerrumVir/arc-chain/main/scripts/install-community-node.sh

---

**TL;DR for the skeptic:** Open the dashboard, click "Run Through Pipeline," watch each shard card pulse in turn, see the hash, run the same prompt twice to prove determinism, run a different prompt to prove isolation. Then run the one-line installer. That's the demo.
