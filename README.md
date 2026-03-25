# ARC Chain — Testnet

A high-performance Layer 1 blockchain built from scratch in Rust. Purpose-built for AI agent coordination with deterministic inference, zero-fee settlements, DAG consensus, GPU-accelerated execution, and post-quantum cryptography.

**Not a fork. Not a copy. Every line is original.**

---

## Measured Performance

| Metric | Value | Conditions |
|--------|-------|------------|
| **Single-node peak TPS** | **183,000** | CPU verify + Sequential exec, M2 Ultra |
| **Multi-node sustained TPS** | **33,230** | 2 validators, real QUIC, real DAG consensus |
| **Peak TPS** | **350,000** | 1-second burst window |
| **Commit rate** | **100%** | 500K/500K transactions committed |
| **State lookups** | **22.3M/sec** | DashMap baseline, M2 Ultra |
| **GPU Ed25519 verify** | **379,000/sec** | Metal compute shader, 13.68x over CPU |
| **Ed25519 signing** | **82,800/sec** | Single-core, ed25519-dalek |

All numbers measured on Apple M2 Ultra Mac Studio (24 cores, 64 GB).

---

## Architecture

```
Users / AI Agents
       |
       v
+- arc-net ------------------------------------------------+
|  QUIC transport (quinn), TLS 1.3, shred propagation,     |
|  XOR FEC erasure coding, TX gossip, peer exchange (PEX)   |
+---------------------+------------------------------------+
                      v
+- arc-consensus ---------------------------------------+
|  DAG block proposals (Mysticeti-inspired),             |
|  stake-weighted 2-round finality, VRF proposer select  |
+---------------------+--------------------------------+
                      v
+- arc-node --------------------------------------------+
|  Block production pipeline, signature verification,    |
|  RPC API (20+ endpoints + ETH JSON-RPC), consensus mgr |
+-----------+---------------------+--------------------+
            v                     v
+- arc-state -----------+  +- arc-vm ------------------+
|  DashMap + JMT         |  |  Wasmer 6.0 WASM runtime  |
|  GPU-resident cache    |  |  revm 19 EVM interpreter   |
|  BlockSTM parallel     |  |  Gas metering, precompiles |
|  WAL persistence       |  +---------------------------+
+------------------------+
            |
+- arc-gpu -----------------+
|  Metal/WGSL Ed25519 batch  |
|  GPU account buffer (wgpu) |
|  Unified memory state      |
+----------------------------+
```

---

## Codebase

**99,600+ LOC Rust** | **1,231 tests** | **14 crates**

| Crate | LOC | Tests | What It Does |
|-------|-----|-------|-------------|
| `arc-types` | 14,490 | 264 | 24 transaction types, blocks, accounts, governance, staking, bridge, account abstraction, social recovery, inference attestation/challenge, state rent |
| `arc-state` | 13,203 | 147 | DashMap state DB, Jellyfish Merkle Tree, segmented WAL with auto-rotate, adaptive BlockSTM parallel execution, GPU-resident state cache, JMT auto-pruning, receipt pruning, state rent, state sync |
| `arc-crypto` | 11,680 | 220 | Ed25519, Secp256k1, BLS12-381, BLAKE3, Falcon-512 (post-quantum), ML-DSA, VRF, threshold crypto, Pedersen commitments, Stwo STARK prover |
| `arc-vm` | 8,439 | 145 | Wasmer WASM runtime, revm EVM interpreter, gas metering, host imports, 11 precompiles, AI inference oracle |
| `arc-node` | 8,424 | 61 | Pipelined block production, adaptive execution (auto-selects Sequential vs BlockSTM), RPC server (30 HTTP + ETH JSON-RPC), consensus manager, STARK proof gen, DA erasure coding, encrypted mempool |
| `arc-consensus` | 7,971 | 137 | DAG consensus, 2-round finality, beacon chain shard coordinator, validator roles (Proposer/Verifier/Observer), slashing, cross-shard coordination, epoch transitions |
| `arc-bench` | 5,336 | — | 10 benchmark binaries (multinode, parallel, signed, soak, production, mixed, node, propose-verify, gpu-state) |
| `arc-gpu` | 5,250 | 45 | Metal MSL + WGSL Ed25519 batch verification (379K sigs/sec on M2 Ultra), GPU account buffer, hardware auto-detection (CUDA/Metal/AVX-512/NEON), AVX-512 + NEON + CUDA verification kernels |
| `arc-net` | 2,355 | 26 | QUIC transport (quinn), shred propagation, XOR FEC, TX gossip, peer exchange, challenge-response auth |
| `arc-mempool` | 876 | 17 | Lock-free SegQueue FIFO, DashSet deduplication, encrypted mempool (BLS threshold) |
| `arc-cli` | 660 | — | Command-line client: keygen, RPC queries, transaction submission |
| `arc-inference` | 620 | 17 | On-chain INT4 inference runtime, 4 hardware tiers, VRF committee selection (7-of-N, 5/7 quorum), EIP-1559 inference gas lane |
| `arc-channel` | 480 | 10 | Off-chain bilateral payment channels: ChannelStateMachine, BLAKE3 state commitments, Ed25519 co-signing |

**Additional code:**
- Python SDK: 2,688 LOC
- TypeScript SDK: 2,011 LOC
- Solidity contracts: 1,944 LOC (ARC20, ARC721, ARC1155, staking, bridge, state root, UUPS proxy)
- Block explorer: 4,421 LOC (Next.js + TypeScript)
- Developer docs: 9 guides (3,131 LOC)

---

## Key Features

### Consensus
- **DAG consensus** (Mysticeti-inspired) — all validators propose blocks in parallel
- **2-round finality** — ~450ms at 150ms/round
- **Stake tiers** — Spark (500K), Arc (5M), Core (50M) ARC
- **VRF proposer selection** — verifiable random leader rotation
- **Slashing** — equivocation + liveness detection, progressive penalties
- **Encrypted mempool** — BLS threshold commit-reveal for MEV protection

### Cryptography
- **Ed25519** — primary transaction signing (118K sigs/sec)
- **Falcon-512** — post-quantum signatures (NIST selected)
- **BLS12-381** — aggregate validator signatures (N sigs -> 1 verify)
- **Stwo Circle STARK** — ZK proofs, post-quantum, no trusted setup
- **GPU batch verification** — Metal + WGSL compute shaders via wgpu

### Execution
- **BlockSTM** — optimistic parallel transaction execution with conflict detection
- **GPU-resident state** — account data in GPU unified memory for compute shader access
- **WASM + EVM** — dual smart contract runtime (Wasmer 6.0 + revm 19)
- **24 transaction types** — transfers, settlements, staking, governance, bridge, channels, shard proofs, inference attestation/challenge
- **No-burn tokenomics** — 100% of fees distributed: 40% proposers, 25% verifiers, 15% observers, 20% treasury. Fixed 1.03B supply.
- **Zero-fee settlements** — AI agents settle for free
- **STARK proof generation** — per-block proof with compression (mock on stable, real Stwo via feature flag)
- **DA erasure coding** — 4+2 Reed-Solomon with Merkle commitment per block
- **11 precompiles** — BLAKE3, Ed25519, VRF, Oracle, Merkle, BlockInfo, Identity, Falcon-512, ZK-verify, AI-inference, BLS-verify

### State
- **DashMap** — lock-free concurrent reads/writes
- **Jellyfish Merkle Tree** — O(log n) inclusion + non-membership proofs, auto-pruning every 100 blocks (keeps 1000 versions)
- **WAL persistence** — Segmented WAL with auto-rotate at 256MB, CRC32 integrity, LZ4 compression, pruning after snapshots
- **GPU state cache** — CPU-side DashMap mirror + wgpu buffer for batch compute

### Networking
- **QUIC** (quinn) — multiplexed streams, TLS 1.3, 0-RTT reconnect
- **Shred propagation** — 1,280-byte chunks with XOR erasure coding
- **Peer exchange (PEX)** — automatic peer discovery

---

## Quick Start

### Prerequisites

- Rust nightly (`rustup default nightly`)
- ~2 GB disk for build, ~4 GB with model
- Node.js 22+ (for explorer, optional)

### See it live right now (zero install)

The testnet is running. Try it:

```bash
# Chain stats from a live node (US West)
curl http://140.82.16.112:9090/stats

# Node health + peers + uptime
curl http://140.82.16.112:9090/health

# Chain info + GPU status
curl http://140.82.16.112:9090/info
```

### Join the testnet (one command)

```bash
git clone https://github.com/FerrumVir/arc-chain.git
cd arc-chain
./scripts/join-testnet.sh
```

This builds the node, connects to seed peers across 3 continents, and starts syncing. Once running:

```bash
# Check chain stats (block height, TPS, peers)
curl http://localhost:9090/stats

# Check node health
curl http://localhost:9090/health

# Run verified inference (downloads TinyLlama 1.1B, ~638 MB)
./scripts/join-testnet.sh --with-inference

# Test inference via RPC
curl -X POST http://localhost:9090/inference/run \
  -H 'Content-Type: application/json' \
  -d '{"input":"[INST] What is 2+2? [/INST]","max_tokens":16}'
```

The inference output hash is deterministic — you'll get the same hash on any hardware (ARM, x86, GPU). Every inference is recorded as an `InferenceAttestation` transaction on-chain.

### What you'll see

```bash
# Live chain stats
curl http://localhost:9090/stats
# → {"block_height":245,"total_accounts":100,"total_transactions":356,"mempool_size":0}

# Node info (GPU detected, peers connected)
curl http://localhost:9090/info
# → {"chain":"ARC Chain","version":"0.1.0","block_height":245,"gpu":{"available":true}}

# All inference attestations recorded on-chain
curl http://localhost:9090/inference/attestations
# → {"attestations":[{"model_id":"0x...","input_hash":"0x...","output_hash":"0x..."}],"count":356}

# Run deterministic inference — same hash on every machine on earth
curl -X POST http://localhost:9090/inference/run \
  -H 'Content-Type: application/json' \
  -d '{"input":"[INST] What is 2+2? [/INST]","max_tokens":16}'
# → {"output":"Sure! The answer is 2+2 = 4.","output_hash":"0x...","ms_per_token":76}
# 76 ms/token on GPU, 139 ms/token on CPU — faster than floating-point
# The output_hash is identical on ARM, x86, and GPU. Verify it yourself.
```

### Testnet faucet

```bash
cd faucet && cargo run --release
# Distributes testnet ARC tokens to new addresses
```

### AI agents

Three agent types ship with the chain:

```bash
cd agents && cargo run --release
```

- **Oracle agent** — submits inference attestations with economic bonds
- **Router agent** — routes inference requests to capable nodes
- **Sentiment agent** — on-chain sentiment analysis via deterministic inference

### Block explorer

With your node running, open the explorer in a browser:

```bash
open explorer/index-live.html
# Or navigate to: file:///path/to/arc-chain/explorer/index-live.html
```

Live dashboard showing blocks, transactions, accounts, inference attestations, and validator status. Polls your local node's RPC automatically.

### Build and test

```bash
cargo build --release
cargo test --workspace --lib    # 1,231 tests
```

### Run benchmarks

```bash
# Multi-node TPS benchmark (2 validators, real consensus)
cargo run --release --bin arc-bench-multinode

# GPU-resident state benchmark
cargo run --release --bin arc-bench-gpu-state

# Single-node parallel execution
cargo run --release --bin arc-bench-parallel

# All available benchmarks
cargo run --release --bin arc-bench
cargo run --release --bin arc-bench-signed
cargo run --release --bin arc-bench-production
cargo run --release --bin arc-bench-mixed
cargo run --release --bin arc-bench-soak
cargo run --release --bin arc-bench-propose-verify
cargo run --release --bin arc-bench-node
```

### Run a node

```bash
cargo run --release -p arc-node
# RPC at http://localhost:9090
# Health check: curl http://localhost:9090/health
```

### Run the explorer

```bash
open explorer/index-live.html
# Opens live dashboard that polls your local node
```

---

## RPC API

20+ HTTP endpoints + ETH JSON-RPC compatibility.

| Method | Endpoint | Description |
|--------|----------|-------------|
| GET | `/health` | Node health |
| GET | `/stats` | Live TPS, height, total transactions |
| GET | `/block/latest` | Latest block |
| GET | `/block/{height}` | Block by height |
| GET | `/blocks?from=&to=&limit=` | Paginated block list |
| GET | `/account/{address}` | Account state |
| GET | `/account/{address}/txs` | Transaction history |
| POST | `/tx/submit` | Submit signed transaction |
| POST | `/tx/submit_batch` | Batch submission |
| GET | `/tx/{hash}` | Transaction + receipt |
| GET | `/tx/{hash}/proof` | Merkle inclusion proof |
| GET | `/validators` | Current validator set |
| GET | `/agents` | Registered AI agents |

| POST | `/inference/run` | Run deterministic inference (returns output + hash + ms/token) |
| GET | `/inference/attestations` | All inference attestations on-chain |
| POST | `/faucet/claim` | Claim testnet tokens |
| GET | `/faucet/status` | Faucet status and rate limits |
| GET | `/sync/snapshot` | State sync snapshot for new nodes |
| POST | `/contract/{address}/call` | Call a smart contract |
| GET | `/channel/{id}/state` | Payment channel state |

ETH JSON-RPC: `eth_blockNumber`, `eth_getBalance`, `eth_call`, `eth_estimateGas`, `eth_getLogs`

---

## Transaction Types (24)

| Type | Code | Description |
|------|------|-------------|
| Transfer | `0x01` | Send ARC between accounts |
| Stake | `0x02` | Stake ARC to become a validator |
| Unstake | `0x03` | Begin unstaking (with cooldown) |
| Deploy | `0x04` | Deploy WASM or EVM smart contract |
| Call | `0x05` | Call a deployed contract |
| Settle | `0x06` | Zero-fee AI agent settlement |
| RegisterAgent | `0x07` | Register an AI agent on-chain |
| Governance | `0x08` | Submit or vote on governance proposal |
| Bridge Lock/Unlock | `0x09-0x0B` | Cross-chain bridge operations |
| Channel Open/Close | `0x0C-0x0E` | Payment channel lifecycle |
| ShardProof | `0x15` | Submit STARK proof of computation |
| InferenceAttestation | `0x16` | Attest to off-chain inference result with bond |
| InferenceChallenge | `0x17` | Challenge an attestation (dispute) |
| InferenceRegister | `0x18` | Register validator inference capabilities |
| + 10 more | | Batch, social recovery, state rent, etc. |

## Cryptographic Signatures

| Algorithm | Use | Status |
|-----------|-----|--------|
| **Ed25519** | Primary transaction signing (118K sigs/sec) | Production |
| **Falcon-512** | Post-quantum signatures (NIST selected) | Production |
| **BLS12-381** | Aggregate validator signatures (N sigs → 1 verify) | Production |
| **ML-DSA** | Post-quantum (NIST ML-DSA / Dilithium) | Production |
| **ECDSA secp256k1** | Ethereum compatibility | Production |

## Smart Contracts

Dual runtime — deploy in Solidity (EVM) or any language that compiles to WASM:

| Contract Standard | Description |
|-------------------|-------------|
| **ARC20** | Fungible token (ERC-20 equivalent) |
| **ARC721** | NFT (ERC-721 equivalent) |
| **ARC1155** | Multi-token (ERC-1155 equivalent) |
| **UUPSProxy** | Upgradeable proxy pattern |
| **ARCStaking** | Staking with tier system |
| **ArcBridge** | Cross-chain bridge contract |
| **ArcStateRoot** | State root commitments for rollups |

## Testnet Faucet

Get testnet ARC tokens to start experimenting:

```bash
# Claim tokens (one claim per address per hour)
curl -X POST http://localhost:9090/faucet/claim \
  -H 'Content-Type: application/json' \
  -d '{"address":"0x<your-address>"}'

# Or run the standalone faucet with web UI
cd faucet && cargo run --release
```

## Staking (Coming Soon)

Staking is implemented in the protocol but not yet active on testnet. For now, anyone can:
- **Run a node** and join the testnet immediately
- **Deploy smart contracts** (EVM or WASM)
- **Run inference** and see on-chain attestations
- **Test all 24 transaction types** via the RPC API

When staking activates on mainnet, three tiers will be available:

| Tier | Role | Stake | Fee Share |
|------|------|-------|-----------|
| **Spark** | Observer | 500K ARC | 15% |
| **Arc** | Verifier | 5M ARC | 25% |
| **Core** | Proposer | 50M ARC | 40% |

No tokens are ever burned. Fixed supply of 1.03B ARC.

---

## Inference Tiers

ARC Chain supports three tiers of AI inference execution, each with different trust/cost tradeoffs:

| Tier | Execution | Verification | Precompile/TX | Use Case |
|------|-----------|-------------|---------------|----------|
| **Tier 1** | On-chain | Deterministic (precompile) | Precompile `0x0A` | Small models, low-latency, full trust |
| **Tier 2** | Off-chain (optimistic) | Fraud proofs via InferenceAttestation (`0x16`) / InferenceChallenge (`0x17`) | TX types `0x16`, `0x17` | Large models, cost-efficient, challenge window |
| **Tier 3** | Off-chain (STARK-verified) | ZK proof via ShardProof (`0x15`) | TX type `0x15` | Maximum trust, cryptographic verification |

- **Tier 1**: Inference runs inside the EVM/WASM VM via the `ai_inference` precompile at address `0x0A`. Fully deterministic, every validator re-executes.
- **Tier 2**: Inference runs off-chain. The result is posted on-chain via `InferenceAttestation` (`0x16`). Anyone can challenge with `InferenceChallenge` (`0x17`) during the dispute window. Optimistic — accepted unless challenged.
- **Tier 3**: Inference runs off-chain with a STARK proof of correct execution. The proof is submitted via `ShardProof` (`0x15`) and verified on-chain. No dispute window needed.

---

---

## Project Structure

```
arc-chain/
+-- crates/
|   +-- arc-crypto/       # Signatures, hashing, BLS, ZK, VRF, threshold, STARK
|   +-- arc-types/        # TX types, blocks, governance, economics, bridge, AA
|   +-- arc-state/        # StateDB, JMT, WAL, BlockSTM, GPU cache, sync
|   +-- arc-vm/           # WASM (Wasmer), EVM (revm), gas, precompiles
|   +-- arc-mempool/      # TX queue, encrypted mempool, dedup
|   +-- arc-consensus/    # DAG, finality, slashing, MEV ordering, epochs
|   +-- arc-net/          # QUIC, shreds, FEC, gossip, PEX, peer auth
|   +-- arc-node/         # Consensus manager, VRF, RPC, pipeline
|   +-- arc-gpu/          # Metal/WGSL Ed25519, GPU memory, buffer pool
|   +-- arc-bench/        # 10 benchmark binaries
|   +-- arc-cli/          # CLI client (keygen, RPC, TX submit)
+-- contracts/
|   +-- standards/        # ARC20, ARC721, ARC1155, UUPSProxy
|   +-- ARCStaking.sol    # Staking with tiers
|   +-- ArcBridge.sol     # Cross-chain bridge
|   +-- ArcStateRoot.sol  # State root commitments
+-- sdks/
|   +-- python/           # arc_sdk: tx building, signing, RPC, ABI
|   +-- typescript/       # @arc-chain/sdk: tx building, signing, RPC, ABI
+-- explorer/             # Live block explorer (single-page HTML, polls RPC)
+-- faucet/               # Rust testnet faucet
+-- papers/               # Research papers
```

---

## Documentation

| Document | Description |
|----------|-------------|
| [papers/](papers/) | Research papers: foundations of trustworthy AI, three-tier verification |

---

## License

Code is public. Anyone can use it.

**Under $10M revenue:** Full production rights. Build whatever you want — no fees, no restrictions, no approval needed.

**Over $10M revenue:** A reasonable commercial license with a reasonable fee. We're not trying to gouge anyone — reach out and we'll work something out. tj@arc.ai

**Build on ARC:** Always free, any size. Deploy contracts, launch tokens, run agents, build L2s, rollups, subnets. If it settles on ARC, you have zero restrictions regardless of your revenue.

**Run ARC:** Always free. Validators, node operators, inference providers.

**Fork ARC to launch a competing chain:** No. This is 99,000+ lines of original Rust built from scratch by one person. Don't take it and repackage it as your own. If you want to work with us, reach out — we'd rather partner than litigate.

Becomes fully open source (Apache 2.0) on March 25, 2030. See [LICENSE](LICENSE) for full terms.
