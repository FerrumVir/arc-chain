![Rust](https://img.shields.io/badge/Rust-121K%2B_LOC-orange)
![Tests](https://img.shields.io/badge/tests-1%2C363_defined-brightgreen)
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

## Why this doesn't exist anywhere else

Five things, in one runtime. Every row is checkable, and the commands are in
[`docs/RECEIPTS.md`](docs/RECEIPTS.md).

| | Everywhere else | Here |
|---|---|---|
| **Verifying an AI answer** | Trust the API, or pay orders of magnitude more to prove it in zero-knowledge | Re-run it and compare one 32-byte hash — the cost of a single forward pass |
| **Same answer on different chips** | Floating-point drifts; no one claims bit-identity | Bit-identical on ARM, x86 and GPU — with the float backend run alongside as a control that *does* diverge |
| **Inference inside consensus** | Oracles, trusted hardware, optimistic challenge windows | Validators re-execute the inference and vote on it, exactly like a transaction. Disagree and you get slashed |
| **Post-quantum signatures** | Ethereum: nothing live, roadmap ~2029. Algorand: PQ accounts Q3 2026 | ML-DSA-65 and Falcon-512 as first-class transaction types, running today |
| **Post-quantum verify inside a contract** | Ethereum's EIP-8052 is still a proposal | `falcon512_verify` precompile at address `0x08`, live |

**I don't know of another chain that has all five.** If you know one, open an
issue and I'll put it in this table myself.

And a result that surprised me: **the post-quantum signature verifies faster
than the classical one it replaces.** Falcon-512 at 20.9 µs against Ed25519's
30.1 µs, through the same code path the mempool uses. Everyone assumes
quantum-safe means slow and heavy. On Apple silicon it's the opposite.

```bash
cargo run --release -p arc-crypto --example pq_bench
```

### On the zero-knowledge side, to be straight with you

The Circle STARK prover here is **StarkWare's Stwo** — the best prover in the
world, and I use it on purpose. What's mine is the circuit built on top of it:
an AIR that proves a Llama-2-7B dense layer and actually *binds* the result.
A full 4096 × 4096 attention projection — 16.7 million multiply-accumulates,
a 2²⁴-row trace — proves as a single STARK in 30 seconds on a desktop.

The interesting part isn't that it proves. It's that it refuses:

```bash
cargo run --release --example soundness_check --features stwo-prover
```

Four forged outputs, all rejected. My first version of that circuit had four
constraints, two of which did nothing, and it would have signed off on a fake
answer. I found it, fixed it, and left the test in so nobody has to take my
word for it.

**Lagrange's DeepProve is ahead of me on zkML and it isn't close** — they prove
full LLM inference in production. I went the other direction: make the
computation reproducible so you don't need the expensive proof. Different
trade, not a better prover.

---

## What's live right now

| | |
|---|---|
| **6 seed validators** | NYC · LAX · AMS · LHR · NRT · SGP |
| **Cluster round** | 9.5 M+ and advancing (~0.2 rounds/s) - `curl $SEED/health` for the live figure |
| **DAG finality** | ~24 ms (2-round commit) measured in-lab; see the note below for what the public seeds are doing today |
| **Self-healing** | each seed runs `arc-self-heal` - drift or RPC silence auto-restarts the node with shard flags preserved |
| **Deterministic inference** | INT16 engine, ARM Mac = x86 Linux = identical output hash, verified |
| **Sharded inference** | 32-layer Llama-2-7B in 6 layer ranges, each replicated on 3 of the 6 seeds (18 shards), BLAKE3 over every hop |
| **Attestations** | Every sharded inference produces an `InferenceAttestation` (0x16) with model hash + input hash + output hash |
| **Live dashboard** | http://140.82.16.112:3200 |
| **Web wallet** | http://140.82.16.112:3100 |

**Honest status of the public testnet, checked 2026-08-17.** This is a testnet
and it is currently limping in two specific ways. Neither is hidden behind a
green `status: ok`, so read them before you draw conclusions from a `/health`
response:

- **Block production is stalled on four of the six seeds.** AMS, LHR, NRT and
  SGP last sealed a block ~6.3 days ago (LHR ~6.7). Only NYC and LAX are
  committing. DAG rounds still advance on all six, which is why `/health`
  keeps saying `ok` — round progress and block commit are separate.
  Consequence: an attestation submitted today lands in the mempool and is
  **not** mined, so it reads `block_height: null`.
- **The seeds are not yet a single replicated chain.** They share a DAG round
  but not state: `/block/43000` returns a different hash on each seed, and
  heights range from ~51 K to ~135 K. A faucet credit or transaction on one
  seed will not be found on another. Pin one seed for anything involving
  balances. The repair plan is `docs/superpowers/plans/2026-06-04-replicated-chain-model-1.md`
  and it has not been started.

Version skew is real too: NYC runs v0.7.2, the other five run v0.7.9, and
**nothing on the network runs v0.7.11** — that version exists only as a desktop
bundle. See [`ALERTS.md`](ALERTS.md) for the current alert list.

---

## Test it yourself

### ⬇ Download ARC Node - pick your computer

| Your computer | One-click download |
|---|---|
| 🍎 **Mac (Apple Silicon - M1/M2/M3/M4)** | **[Download for Apple Silicon Mac](https://github.com/FerrumVir/arc-chain/releases/download/v0.7.7/ARC.Node_0.7.7_aarch64.dmg)** |
| 🍎 **Mac (Intel)** | **[Download for Intel Mac](https://github.com/FerrumVir/arc-chain/releases/download/v0.7.7/ARC.Node_0.7.7_x64.dmg)** |
| 🪟 **Windows 10 / 11** | **[Download for Windows](https://github.com/FerrumVir/arc-chain/releases/download/v0.7.7/ARC.Node_0.7.7_x64-setup.exe)** |
| 🐧 **Linux (Ubuntu / Debian)** | **[Download .deb](https://github.com/FerrumVir/arc-chain/releases/download/v0.7.7/ARC.Node_0.7.7_amd64.deb)** |
| 🐧 **Linux (Fedora / RHEL)** | **[Download .rpm](https://github.com/FerrumVir/arc-chain/releases/download/v0.7.7/ARC.Node-0.7.7-1.x86_64.rpm)** |
| 🐧 **Linux (any distro)** | **[Download .AppImage](https://github.com/FerrumVir/arc-chain/releases/download/v0.7.7/ARC.Node_0.7.7_amd64.AppImage)** |

> **Not sure which Mac?** Apple menu → *About This Mac*. If chip says "Apple M1/M2/M3/M4" → Apple Silicon. If "Intel" → Intel.

**Install in 60 seconds:**

- **Mac**: open the `.dmg` → drag ARC Node to Applications → first launch right-click → Open
- **Windows**: run the `.exe` → "More info" → "Run anyway" → Next → Install → Finish
- **Linux**: `sudo apt install ./ARC.Node_0.7.7_amd64.deb` (or `rpm -i`, or `chmod +x` the AppImage)

The app onboards in 3 clicks (welcome → identity → join), runs in your tray, auto-starts on login, auto-updates when v0.7.7+ ships.

**📖 Full walkthrough:** [Getting Started with ARC Node](docs/GETTING_STARTED.md) - install, identity, first inference, faucet, earnings, and FAQ.

---

### Or run from the command line

**Run a real inference on the live network, see the BLAKE3 hash, watch it verify:**

```bash
curl -sSL https://raw.githubusercontent.com/FerrumVir/arc-chain/main/scripts/arc-demo.sh | bash
```

Discovers the live shard pipeline, dispatches a Llama-2-7B prompt, prints the per-hop trace, then re-runs the prompt asking for a genuine recomputation.

A word on that re-run, because it is the point of the demo. The coordinator caches by (model, prompt, max_tokens), so simply POSTing the same prompt twice is answered out of cache in microseconds and proves nothing. The script sends `force_recompute`. Against a coordinator that honours it you get `✓ DETERMINISTIC` — two independent pipeline walks, same 32 bytes. Against the live v0.7.9 seeds, which do not have the flag yet, you get `● SERVED FROM CACHE (hash match)` instead. That is the honest label, and it is what you should expect today.

**Re-verify any past attestation from scratch on your own machine:**

```bash
curl -sSL https://raw.githubusercontent.com/FerrumVir/arc-chain/main/scripts/arc-verify.sh | bash -s -- --latest
```

Sweeps every seed for a real attestation record, re-executes the same prompt against the same model, compares `output_hash` and `model_hash`. Prints `VERIFIED (recomputed)` or `VERIFIED (from cache)` depending on which it actually got. Today only LHR holds a meaningful attestation history, and the script finds it for you.

Note what `model_hash` commits to: it is a BLAKE3 of the model's shape label (`arc-32L-4096d-32h-32000v`), not of the weight bytes. It proves the same declared model, not the same tensors. Binding attestations to a hash over the actual weights is on the roadmap below.

**Join the network as a node** (one command, auto-detects platform):

```bash
curl -sSL https://raw.githubusercontent.com/FerrumVir/arc-chain/main/scripts/install-community-node.sh | bash
```

Pre-built binary, installed as a launchd / systemd service with a daily updater. Joins with `--stake 0 --community-mode`, which is the safe posture: a stake-0 node is tracked and can serve inference, but takes no consensus role, so it cannot affect block production. No model download is required to join — pass `--model /path/to.gguf` if you also want to serve inference.

The installer picks the newest release that actually ships an `arc-node` CLI binary for your platform, which is **v0.7.7** right now. The two newest releases (v0.7.10, v0.7.11) are desktop-only bundles with no CLI asset. Pin explicitly with `ARC_NODE_VERSION=0.7.7` if you prefer. `arc-node-linux-aarch64` has never been published — ARM Linux builds from source.

---

## The improvements that made this real

The core thesis - "inference that passes consensus" - only works if the arithmetic is perfectly reproducible. Getting there took a sequence of concrete breakthroughs, each one verified on the live network:

1. **Pure-integer transformer inference.** Every matmul, softmax, layer norm, and activation in i64 fixed-point. No floating point anywhere on the hot path. Eliminates the only source of hardware drift.

2. **Cross-architecture bit identity, proven.** Same prompt → same output hash on Apple M2 Ultra and x86_64 Vultr VPS. No approximations, no tolerance thresholds. Byte-for-byte equal.

3. **BLAKE3 verification in O(1).** Consensus participants re-run an inference and compare one 32-byte hash. That costs the same as one forward pass — where proving the same inference in zero-knowledge costs orders of magnitude more. This is the whole trade: reproducibility instead of proof.

4. **Sharded inference with hop-level integrity.** 32-layer model split into 6 layer ranges across 6 VPS, each range replicated 3×. Each shard verifies the previous shard's BLAKE3 hash before computing its own layers. A single corrupted hop invalidates the whole chain - the network notices immediately.

5. **Pipelined prefill across shards.** Prompt prefill runs one task per shard joined by channels, so the node holding layers 6–12 works on position *p* while the node holding 0–6 is already on *p+1*. The per-token decode loop that follows is necessarily sequential — each token depends on the previous token's logits — so a long prompt pipelines well and a long generation does not.

6. **Latency-aware replica selection.** Each layer range has 3 replicas; the coordinator keeps a rolling EWMA of per-hop latency and dispatches to the fastest. Because the engine is deterministic, the output is identical whichever replica answers, so this is a free speed knob. (Racing the top-K in parallel rather than picking one is designed but not shipped - see the roadmap.)

7. **Deterministic result cache, content-addressed.** Integer-only means identical inputs produce identical outputs, so results are addressable by (model, prompt, length) and a repeat serves in microseconds. Worth being precise about what that does and does not show: a cache hit is not evidence that the pipeline recomputed. `force_recompute` exists to get a real second walk.

8. **VRF committee selection.** For inference gas lane transactions, a committee is pseudo-randomly selected per request, seeded by the output hash, and recorded with the result for auditability. The selection is live and deterministic; the vote-collection and slashing half is **not yet wired** — members are chosen but never polled. Treat the `committee` field in an inference response as provenance, not as a verification that happened.

9. **Post-quantum signatures in production.** Falcon-512 and ML-DSA live alongside Ed25519, BLS12-381, secp256k1. Most chains still list this on a roadmap.

10. **DashMap lock-inversion fix in `index_account_tx`** (this week). Consensus thread was holding one shard's write lock while acquiring another → classic deadlock. Found via `gdb -p` on a stuck node with a debuginfo build, fixed in a few lines. Stability debt paid; cluster holds tight round spread now.

---

## Measured performance

**Read this row first, because it is the one people get wrong.** There are two
very different latency stories here and the millisecond numbers below are the
*local single-node* one.

| Where | Latency | What it is |
|---|---|---|
| **One node, whole model in memory, M2 Ultra** | **76–139 ms/token** | the numbers in the table below |
| **Sharded across the 6 public seeds** | **~2–10 s/token** | what `arc-demo.sh` actually produces today |

A 16-token response on the public testnet takes roughly **1–3 minutes**, not
milliseconds. The seeds are CPU-only VPS — `/info` reports `gpu.available:true`
but names `llvmpipe`, which is a software rasterizer, not a GPU. A cold shard
that has not served recently pays a large first-hit penalty on top: a measured
cold run spent 14.5 s and 16.5 s on two layer ranges that a warm node serves in
~200 ms. Warm your prompts before demoing.

All numbers below on Apple M2 Ultra (24 cores, 64 GB) unless noted, single node.

| Metric | Value | Conditions |
|---|---|---|
| **Inference (GPU)** | **76 ms/token** | Deterministic INT16, single node, model fully local |
| **Inference (CPU)** | **139 ms/token** | Deterministic INT16, single node, model fully local |
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

Coordinators batch the whole prompt into one round-trip per shard (`"prefill":"batch"`) and pick the lowest-latency replica for each range. Racing several replicas at once and taking the first to finish is designed but not shipped.

Each node holds 15–17 of the 32 layers, about 2.9–3.3 GB. Verify the live map yourself: `curl http://104.238.171.11:9090/shards`.

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
┌─ arc-inference ──────────────┐ ┌─ arc-olm ────────────────┐
│  Pure-integer INT8/INT16     │ │  On-chain LM runtime,     │
│  transformer engine,         │ │  INT16 deterministic      │
│  committee selection,        │ │  inference                │
│  distributed dispatch        │ └───────────────────────────┘
└──────────────────────────────┘
       │
┌─ arc-gpu ──────────────────┐
│  Metal/WGSL Ed25519 batch   │
│  GPU state cache (wgpu)     │
│  Unified memory             │
└─────────────────────────────┘
```

---

## Codebase

~121,900 lines of Rust across 16 workspace members (14 under `crates/`, plus
`agents/` and `relayer/`). 1,363 `#[test]` / `#[tokio::test]` functions defined
— run `cargo test --workspace` for the pass count on your machine.

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
| `arc-mempool` | 876 | Lock-free queue, deduplication, BLS threshold encrypted mempool |
| `arc-cli`, `arc-channel`, `arc-bench`, `arc-relayer`, `arc-agents` | misc | CLI, payment channels, benchmarks, bridge, example agents |

Plus: Python SDK (2,688 LOC), TypeScript SDK (2,011 LOC), Solidity contracts (1,944 LOC), Next.js block explorer.

---

## What's shipping on-chain

Every line below is live on the testnet right now:

| | |
|---|---|
| DAG consensus, 2-round commit | ✅ 6 nodes, 3 continents |
| Self-heal daemon per seed (drift or RPC-silence auto-restart) | ✅ `arc-self-heal.service` |
| Deterministic INT16 inference, bit-identical cross-arch | ✅ ARM = x86 proof |
| Sharded inference across 6 seeds, 3× replication, hop-level BLAKE3 | ✅ `/inference/run_sharded` |
| k-of-n consensus inference with divergence detection | ✅ `/inference/run_consensus` |
| Latency-aware replica selection per layer range | ✅ rolling EWMA |
| Auto-shard node onboarding | ✅ `--auto-shard` flag |
| Inference attestations as a transaction type | ✅ tx type `0x16` |
| EVM (Solidity) + WASM (Rust / C / Go) both | ✅ revm 19, Wasmer 6.0 |
| 5 signature algorithms incl. 2 post-quantum | ✅ Ed25519 · Falcon-512 · BLS · ML-DSA · secp256k1 |
| BLS threshold encrypted mempool (MEV protection) | ✅ commit-reveal |
| Zero-fee agent settlements | ✅ `Settle` (0x06) · `RegisterAgent` (0x07) |
| Web wallet, live dashboard | ✅ |

### Built but not yet doing its job

Listed separately because the code exists and the endpoint answers, but the
thing you would assume from the name is not happening yet:

| | |
|---|---|
| Validator slashing (equivocation, liveness) | implemented in `arc-consensus`; no slash has been triggered on the live net |
| VRF committee re-execution | committee is selected and recorded; votes are never collected |
| Attestations reaching a block | submitted to the mempool, but 4 of 6 seeds are not sealing, so they read `block_height: null` |
| Model identity in attestations | `model_hash` commits to the shape label, not the weight bytes |

### Roadmap — designed, not shipped

Previous versions of this README listed these as live. They are not; the
endpoints return 404 on the current binary.

| | |
|---|---|
| Content-addressed model chunks (no GGUF download) | `/chunks/get/{hash}` — planned |
| Heterogeneous hardware scheduler, race-top-K | `/inference/plan` — planned |
| Peer-to-peer weight distribution | planned |
| Replicated chain across the seeds (one shared state) | plan written, not started |
| Block explorer | offline |

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
| `/block/latest`, `/block/{n}` | blocks (per-seed — see the state note above) |
| `/inference/run`, `/inference/run_sharded` | single-node + sharded inference |
| `/inference/attestations` | attestation records |
| `/inference/results` | node-local inference results (in-memory, lost on restart) |
| `/tx/submit`, `/tx/{hash}` | transactions |
| `/validators`, `/shards` | network state |
| `/account/{addr}` | balances |
| `/faucet/claim`, `/faucet/status` | free testnet tokens (10,000 ARC, 60 s cooldown) |
| `/worker/earnings/{addr}` | attestation count × 2.5 ARC (display arithmetic — see below) |
| `/workers/scoreboard` | registered community workers |
| `/eth` | Ethereum JSON-RPC (MetaMask compatible) |

Two endpoints deserve a warning label:

- **`/models`** reports `fully_covered: false` with `covered_layers: 96` against
  `total_layers: 32`. That is a counting bug — it sums the layer spans of all 18
  shards instead of taking their union, so any replication factor above 1 makes
  it report a false negative. `/shards` on the same node computes coverage
  correctly and says `true`. Trust `/shards`.
- **`/worker/earnings/{addr}`** multiplies an attestation count by a 2.5 ARC
  constant. It is display arithmetic, not a balance: nothing reads or writes an
  on-chain balance in that handler, so it will not reconcile against
  `/account/{addr}`. It also counts from an in-memory transaction map that is
  pruned, so the figure can go *down* between two polls and resets to zero when
  a node restarts.

See `docs/HOW-SHARDING-WORKS.md` for the wire protocol.

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
