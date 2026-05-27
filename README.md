![Rust](https://img.shields.io/badge/Rust-99%2C600%2B_LOC-orange)
![Tests](https://img.shields.io/badge/tests-1%2C204_passing-brightgreen)
![License](https://img.shields.io/badge/license-BUSL--1.1-blue)
![Inference](https://img.shields.io/badge/inference-consensus--verified-purple)
![Testnet](https://img.shields.io/badge/testnet-live-green)

# ARC Chain - Trustworthy AI

**A high-performance Layer 1 blockchain built from scratch in Rust. Purpose-built so AI inference can pass network consensus - the same way transactions do.**

**Not a fork. Not a copy. Every line is original.**

📄 Paper: *On the Foundations of Trustworthy Artificial Intelligence*

---

## The claim

Every AI response from a cloud provider is a claim you can't check. You don't know which model ran. You don't know whether the output was truncated, cached, routed, or silently modified. You trust the logo.

ARC makes inference **verifiable by a blockchain the same way transactions are verifiable**. The engine runs in pure integer arithmetic, no floating point. The output hash is bit-identical on ARM, x86, GPU - every chip on earth. Validators can re-run any inference and vote on it the same way they vote on blocks. Invalid outputs are slashed. Honest ones are attested on-chain with cryptographic proof of which model produced them.

**This is AI that passes consensus.** Inference becomes a first-class on-chain primitive. An oracle you don't need to trust. A model output you can replay, verify, and settle against - at any scale, on any hardware.

---

## What's live right now

| | |
|---|---|
| **6 seed validators** | NYC · LAX · AMS · LHR · NRT · SGP |
| **Cluster round** | 1.3 M+ and advancing |
| **DAG finality** | ~24 ms (2-round commit) |
| **Self-healing** | each seed runs `arc-self-heal` - drift or RPC silence auto-restarts the node with shard flags preserved |
| **Deterministic inference** | INT16 engine, ARM Mac = x86 Linux = identical output hash, verified |
| **Sharded inference** | 32-layer Llama-2-7B distributed across 6 seeds with 3× replication per layer range, BLAKE3 over every hop |
| **On-chain attestations** | Every inference produces `InferenceAttestation` (0x16) with model hash + input hash + output hash |
| **Live dashboard** | http://140.82.16.112:3200 |
| **Web wallet** | http://140.82.16.112:3100 |

---

## Test it yourself

### ⬇ Download ARC Node - pick your computer

| Your computer | One-click download |
|---|---|
| 🍎 **Mac (Apple Silicon - M1/M2/M3/M4)** | **[Download for Apple Silicon Mac](https://github.com/FerrumVir/arc-chain/releases/download/v0.7.3/ARC.Node_0.7.3_aarch64.dmg)** |
| 🍎 **Mac (Intel)** | **[Download for Intel Mac](https://github.com/FerrumVir/arc-chain/releases/download/v0.7.3/ARC.Node_0.7.3_x64.dmg)** |
| 🪟 **Windows 10 / 11** | **[Download for Windows](https://github.com/FerrumVir/arc-chain/releases/download/v0.7.3/ARC.Node_0.7.3_x64-setup.exe)** |
| 🐧 **Linux (Ubuntu / Debian)** | **[Download .deb](https://github.com/FerrumVir/arc-chain/releases/download/v0.7.3/ARC.Node_0.7.3_amd64.deb)** |
| 🐧 **Linux (Fedora / RHEL)** | **[Download .rpm](https://github.com/FerrumVir/arc-chain/releases/download/v0.7.3/ARC.Node-0.7.3-1.x86_64.rpm)** |
| 🐧 **Linux (any distro)** | **[Download .AppImage](https://github.com/FerrumVir/arc-chain/releases/download/v0.7.3/ARC.Node_0.7.3_amd64.AppImage)** |

> **Not sure which Mac?** Apple menu → *About This Mac*. If chip says "Apple M1/M2/M3/M4" → Apple Silicon. If "Intel" → Intel.

**Install in 60 seconds:**

- **Mac**: open the `.dmg` → drag ARC Node to Applications → first launch right-click → Open
- **Windows**: run the `.exe` → "More info" → "Run anyway" → Next → Install → Finish
- **Linux**: `sudo apt install ./ARC.Node_0.7.3_amd64.deb` (or `rpm -i`, or `chmod +x` the AppImage)

The app onboards in 3 clicks (welcome → identity → join), runs in your tray, auto-starts on login, auto-updates when v0.7.3+ ships.

**📖 Full walkthrough:** [Getting Started with ARC Node](docs/GETTING_STARTED.md) - install, identity, first inference, faucet, earnings, and FAQ.

---

### Or run from the command line

**Run a real inference on the live network, see the BLAKE3 hash, watch it verify:**

```bash
curl -sSL https://raw.githubusercontent.com/FerrumVir/arc-chain/main/scripts/arc-demo.sh | bash
```

Discovers the live shard pipeline, dispatches a Llama-2-7B prompt, prints the per-hop trace (compute_ms, wall_ms, payload bytes per node), re-runs the prompt, proves `output_hash` is bit-identical across both runs.

**Re-verify any past on-chain attestation from scratch on your own machine:**

```bash
curl -sSL https://raw.githubusercontent.com/FerrumVir/arc-chain/main/scripts/arc-verify.sh | bash -s -- --latest
```

Fetches the newest attestation, re-executes the same prompt against the same model, compares hashes. Prints `✓ VERIFIED` when they match.

**Join the network as a node** (one command, auto-detects platform, auto-picks a layer range):

```bash
curl -sSL https://raw.githubusercontent.com/FerrumVir/arc-chain/main/scripts/install-community-node.sh | bash
```

Pre-built 16 MB binary. Registers as a launchd / systemd service. Auto-updates daily. Connects to the 6 seeds and starts serving inference with on-chain attestations.

---

## The improvements that made this real

The core thesis - "inference that passes consensus" - only works if the arithmetic is perfectly reproducible. Getting there took a sequence of concrete breakthroughs, each one verified on the live network:

1. **Pure-integer transformer inference.** Every matmul, softmax, layer norm, and activation in i64 fixed-point. No floating point anywhere on the hot path. Eliminates the only source of hardware drift.

2. **Cross-architecture bit identity, proven.** Same prompt → same output hash on Apple M2 Ultra and x86_64 Vultr VPS. No approximations, no tolerance thresholds. Byte-for-byte equal.

3. **BLAKE3 verification in O(1).** Consensus participants re-run an inference, compare one 32-byte hash. zkML proof-of-inference costs 10⁵–10⁶× the original compute; hash-match costs the same as one forward pass. At 7 B parameters we're already 700× the largest model ever verified by zkML.

4. **Sharded inference with hop-level integrity.** 32-layer model split across 8 VPS. Each shard verifies the previous shard's BLAKE3 hash before computing its own layers. A single corrupted hop invalidates the whole chain - the network notices immediately.

5. **Deterministic KV cache.** Integer-only means identical inputs produce identical outputs, so intermediate hidden states are content-addressable. Repeated prompts serve in microseconds. Across the whole network.

6. **Heterogeneous hardware scheduler.** Every node advertises measured compute p50 + current queue depth. The coordinator races the top-K fastest workers per layer range and takes the first-finish - **determinism guarantees the output is identical whichever worker wins**. A 4090 joining today slots in automatically and outruns the CPU seeds; no manual config.

7. **Content-addressed model chunks over HTTP.** `GET /chunks/get/{hash}` serves weight bytes straight from a peer. Any new node auto-picks an uncovered layer range (`--auto-shard`) and pulls the chunk from the network - no 4 GB GGUF download. A laptop with 2 GB of RAM can contribute.

8. **VRF committee re-execution.** For inference gas lane transactions (tier 2 and 3), 7 validators are pseudo-randomly selected per request and must agree on the output hash. Disagreement triggers slashing. Same game-theoretic trust model as normal block finality, applied to AI.

9. **Post-quantum signatures in production.** Falcon-512 and ML-DSA live alongside Ed25519, BLS12-381, secp256k1. Most chains still list this on a roadmap.

10. **DashMap lock-inversion fix in `index_account_tx`** (this week). Consensus thread was holding one shard's write lock while acquiring another → classic deadlock. Found via `gdb -p` on a stuck node with a debuginfo build, fixed in a few lines. Stability debt paid; cluster holds tight round spread now.

---

## Measured performance

All numbers on Apple M2 Ultra (24 cores, 64 GB) unless noted.

| Metric | Value | Conditions |
|---|---|---|
| **Inference (GPU)** | **76 ms/token** | Deterministic INT16 |
| **Inference (CPU)** | **139 ms/token** | Deterministic INT16 |
| Standard float (Candle Q4) | 175 ms/token | Not deterministic |
| Single-node peak TPS | 183,000 | CPU verify + sequential exec |
| Multi-node sustained TPS | 33,230 | 2 validators, real QUIC, real DAG |
| Peak TPS | 350,000 | 1-second burst window |
| Commit rate | 100% | 500 K / 500 K transactions |
| State lookups | 22.3 M/sec | DashMap baseline |
| GPU Ed25519 verify | 379,000 / sec | Metal compute shader (13.68× CPU) |
| Ed25519 signing | 82,800 / sec | Single-core |
| DAG finality | ~24 ms | 2-round commit rule |

The deterministic integer engine is **2.3× faster than floating-point** on GPU. Integer ops associate; floating-point ops don't; that removes a class of barriers that force the GPU to serialize.

---

## How the sharding works

```
                  Llama-2-7B - 32 transformer layers, 6 seed nodes,
                    3× replication per layer range

  token id  →  [0,6)  →  [6,12)  →  [12,17)  →  [17,22)  →  [22,27)  →  [27,32)  →  token id
                EMBED                                                      LM HEAD

  range           replicas (any one answers, failover to the next)
  ─────           ────────────────────────────────────────────────
  [0,6)           AMS · LAX · NYC
  [6,12)          AMS · LAX · LHR
  [12,17)         AMS · LHR · NRT
  [17,22)         LHR · NRT · SGP
  [22,27)         NRT · NYC · SGP
  [27,32)         LAX · NYC · SGP

  NYC 149.28.32.76   LAX 140.82.16.112   AMS 136.244.109.1
  LHR 104.238.171.11 NRT 202.182.107.41  SGP 149.28.153.31   (port 9090)
```

Each `→` is a `POST /inference/forward_shard` to the next shard. Each shard verifies the previous shard's BLAKE3 hash before computing. The last shard runs `final_norm + LM head + argmax` and returns the next token id. The coordinator collects tokens until `max_tokens` or EOS.

Coordinators can batch the whole prompt into one round-trip per shard (`"prefill":"batch"`) or race redundant workers on the same layer range when multiple nodes cover it.

---

## Architecture

```
Users / AI Agents
       │
       ▼
┌─ arc-net ────────────────────────────────────────────────┐
│  QUIC transport (quinn 0.11), TLS 1.3, shred propagation, │
│  XOR FEC, TX gossip, peer exchange (PEX)                  │
└──────────────────────┬───────────────────────────────────┘
                       ▼
┌─ arc-consensus ──────────────────────────────────────┐
│  DAG block proposals (Mysticeti-inspired),            │
│  stake-weighted 2-round finality, VRF proposer select │
└──────────────────────┬───────────────────────────────┘
                       ▼
┌─ arc-node ───────────────────────────────────────────┐
│  Block production, 34-endpoint RPC + ETH JSON-RPC,    │
│  sharded inference coordinator, consensus manager     │
└──────┬────────────────────────┬──────────────────────┘
       ▼                        ▼
┌─ arc-state ──────────┐ ┌─ arc-vm ──────────────────┐
│  DashMap + JMT        │ │  Wasmer 6.0 WASM runtime   │
│  GPU-resident cache   │ │  revm 19 EVM interpreter    │
│  BlockSTM parallel    │ │  Gas metering, precompiles  │
│  WAL persistence      │ └─────────────────────────────┘
└───────────────────────┘
       │
┌─ arc-swarm ─────────────┐ ┌─ arc-inference ──────────────┐
│  Content-addressed      │ │  Pure-integer INT8/INT16     │
│  model chunks + scheduler│ │  transformer engine           │
│  Peer-to-peer weights   │ │  VRF committee re-execution   │
└─────────────────────────┘ └──────────────────────────────┘
       │
┌─ arc-gpu ──────────────────┐
│  Metal/WGSL Ed25519 batch   │
│  GPU state cache (wgpu)     │
│  Unified memory             │
└─────────────────────────────┘
```

---

## Codebase

99,600+ LOC Rust across 16 crates. 1,204 tests passing.

| Crate | LOC | What it does |
|---|---|---|
| `arc-types` | 14,490 | 24 transaction types, blocks, accounts, governance, staking, bridge, inference attestation/challenge |
| `arc-state` | 13,203 | DashMap state DB, Jellyfish Merkle Tree, WAL, BlockSTM parallel execution, GPU-resident cache |
| `arc-crypto` | 11,680 | Ed25519, secp256k1, BLS12-381, BLAKE3, Falcon-512, ML-DSA, VRF, Stwo STARK prover |
| `arc-olm` | 9,760 | On-chain language model runtime, INT16 deterministic inference |
| `arc-vm` | 8,439 | Wasmer WASM + revm EVM, gas metering, 11 precompiles, AI inference oracle |
| `arc-node` | 8,424 | Block production, 34-endpoint RPC, sharded inference coordinator |
| `arc-inference` | 8,343 | Pure-integer engine, committee selection, distributed dispatch |
| `arc-consensus` | 7,971 | DAG consensus, 2-round finality, slashing, VRF, epoch transitions |
| `arc-gpu` | 5,250 | Metal MSL + WGSL Ed25519 batch verify (379 K / sec), GPU memory |
| `arc-net` | 2,355 | QUIC transport, shred propagation, FEC, gossip, peer exchange |
| `arc-swarm` | 1,900 | Content-addressed model chunks, scheduler, peer-to-peer weights |
| `arc-mempool` | 876 | Lock-free queue, deduplication, BLS threshold encrypted mempool |
| `arc-cli`, `arc-channel`, `arc-bench`, `arc-relayer`, `arc-agents` | misc | CLI, payment channels, benchmarks, bridge, example agents |

Plus: Python SDK (2,688 LOC), TypeScript SDK (2,011 LOC), Solidity contracts (1,944 LOC), Next.js block explorer.

---

## What's shipping on-chain

Every line below is live on the testnet right now:

| | |
|---|---|
| DAG consensus, 2-round commit, ~24 ms finality | ✅ 6 nodes, 3 continents |
| Self-heal daemon per seed (drift or RPC-silence auto-restart) | ✅ `arc-self-heal.service` |
| Deterministic INT16 inference, bit-identical cross-arch | ✅ ARM = x86 proof |
| Sharded inference across 6 seeds, 3× replication, hop-level BLAKE3 | ✅ `/inference/run_sharded` (fast path) |
| k-of-n consensus inference with divergence detection | ✅ `/inference/run_consensus` (dashboard default) |
| Content-addressed model chunks (no GGUF download) | ✅ `/chunks/get/{hash}` |
| Heterogeneous hardware scheduler, race-top-K | ✅ `/inference/plan` |
| Auto-shard node onboarding | ✅ `--auto-shard` flag |
| On-chain inference attestations | ✅ tx type `0x16` |
| EVM (Solidity) + WASM (Rust / C / Go) both | ✅ revm 19, Wasmer 6.0 |
| 5 signature algorithms incl. 2 post-quantum | ✅ Ed25519 · Falcon-512 · BLS · ML-DSA · secp256k1 |
| Validator slashing (equivocation, liveness) | ✅ in protocol |
| BLS threshold encrypted mempool (MEV protection) | ✅ commit-reveal |
| Zero-fee agent settlements | ✅ `Settle` (0x06) · `RegisterAgent` (0x07) |
| Web wallet, live dashboard, block explorer | ✅ |

---

## Network endpoints

All 6 seeds serve the same API on port 9090. Auto-pick a healthy one:

```bash
COORDINATOR=$(bash scripts/arc-pick-coordinator.sh)
curl "$COORDINATOR/health"
```

| Node | Location | RPC |
|---|---|---|
| NYC | New York | http://149.28.32.76:9090 |
| LAX | Los Angeles | http://140.82.16.112:9090 |
| AMS | Amsterdam | http://136.244.109.1:9090 |
| LHR | London | http://104.238.171.11:9090 |
| NRT | Tokyo | http://202.182.107.41:9090 |
| SGP | Singapore | http://149.28.153.31:9090 |

Key endpoints:

| Path | Purpose |
|---|---|
| `/health`, `/stats`, `/info` | node + chain health |
| `/inference/run`, `/inference/run_sharded` | single-node + sharded inference |
| `/inference/plan` | swarm scheduler preview |
| `/inference/attestations` | all on-chain attestations |
| `/chunks/info`, `/chunks/network`, `/chunks/get/{hash}` | content-addressed model weights |
| `/tx/submit`, `/tx/{hash}` | transactions |
| `/validators`, `/shards`, `/models` | network state |
| `/faucet/claim` | free testnet tokens |
| `/eth` | Ethereum JSON-RPC (MetaMask compatible) |

Full API: 34 endpoints. See `docs/HOW-SHARDING-WORKS.md` for the wire protocol.

---

## ARC Token

ARC exists today as ERC-20 on Ethereum: `0x672fdba7055bddfa8fd6bd45b1455ce5eb97f499`.

Fixed supply: 1.03 B. No inflation. No burns.

When mainnet launches, ERC-20 holders migrate to native ARC via a bridge contract. On testnet, use the faucet.

---

## Disclaimer

ARC Chain is in active development. This is a testnet. Do not use real funds. Software is provided as-is, no warranty.

---

## License

BUSL-1.1. Source-available today. Becomes Apache 2.0 on 2030-03-25.

**Free forever:**
- Any project under $10 M revenue - full production rights, no approval
- Anything built on ARC Chain at any scale (contracts, tokens, agents, L2s, rollups)
- Validators, inference providers, observers
- Research, education, personal projects, forks, experiments

**Commercial license ($50 K/yr) for $10 M+ revenue orgs that want to:**
- Fork this codebase to launch a competing L1
- Extract consensus / inference / crypto for a competing network
- Repackage the code as their own chain

Built solo from scratch, every line. I want it used. I don't want it taken. Commercial license: tj@arc.ai.
