# ARC Chain

A high-performance Layer 1 blockchain built from scratch in Rust. Purpose-built for AI agent coordination with zero-fee settlements, DAG consensus, GPU-accelerated execution, and post-quantum cryptography.

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

All numbers measured on Apple M2 Ultra Mac Studio (24 cores, 64 GB). See [BENCHMARK_RESULTS.md](BENCHMARK_RESULTS.md) for full methodology.

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

**77,244 LOC Rust** | **1,054 tests** | **11 crates**

| Crate | LOC | Tests | What It Does |
|-------|-----|-------|-------------|
| `arc-types` | 14,490 | 264 | 23 transaction types, blocks, accounts, governance, staking, bridge, account abstraction, social recovery, inference attestation/challenge, state rent |
| `arc-state` | 13,203 | 147 | DashMap state DB, Jellyfish Merkle Tree, segmented WAL with auto-rotate, adaptive BlockSTM parallel execution, GPU-resident state cache, JMT auto-pruning, receipt pruning, state rent, state sync |
| `arc-crypto` | 11,680 | 220 | Ed25519, Secp256k1, BLS12-381, BLAKE3, Falcon-512 (post-quantum), ML-DSA, VRF, threshold crypto, Pedersen commitments, Stwo STARK prover |
| `arc-vm` | 8,439 | 145 | Wasmer WASM runtime, revm EVM interpreter, gas metering, host imports, 11 precompiles, AI inference oracle |
| `arc-node` | 8,424 | 61 | Pipelined block production, adaptive execution (auto-selects Sequential vs BlockSTM), RPC server (30 HTTP + ETH JSON-RPC), consensus manager, STARK proof gen, DA erasure coding, encrypted mempool |
| `arc-consensus` | 7,971 | 137 | DAG consensus, 2-round finality, beacon chain shard coordinator, validator roles (Proposer/Verifier/Observer), slashing, cross-shard coordination, epoch transitions |
| `arc-bench` | 5,336 | — | 10 benchmark binaries (multinode, parallel, signed, soak, production, mixed, node, propose-verify, gpu-state) |
| `arc-gpu` | 3,810 | 37 | Metal MSL + WGSL Ed25519 batch verification (379K sigs/sec on M2 Ultra), GPU account buffer, unified/managed memory, buffer pooling |
| `arc-net` | 2,355 | 26 | QUIC transport (quinn), shred propagation, XOR FEC, TX gossip, peer exchange, challenge-response auth |
| `arc-mempool` | 876 | 17 | Lock-free SegQueue FIFO, DashSet deduplication, encrypted mempool (BLS threshold) |
| `arc-cli` | 660 | — | Command-line client: keygen, RPC queries, transaction submission |

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
- **23 transaction types** — transfers, settlements, staking, governance, bridge, channels, shard proofs, inference attestation/challenge
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

- Rust 1.85+ (edition 2024)
- Node.js 22+ (for explorer)

### Build and test

```bash
git clone https://github.com/FerrumVir/arc-chain.git
cd arc-chain
cargo build --release
cargo test --workspace --lib    # 1,031 tests
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
cd explorer && npm install && npm run dev
# Explorer at http://localhost:3100
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

ETH JSON-RPC: `eth_blockNumber`, `eth_getBalance`, `eth_call`, `eth_estimateGas`, `eth_getLogs`

---

## Scaling Projections

Compound scaling from measured 27K TPS baseline:

| Optimization | Multiplier | Status |
|-------------|------------|--------|
| More CPU cores (96 vs 10) | 6-10x | Ready (Rayon) |
| GPU batch sig verification | 2-3x | Implemented |
| BlockSTM parallel execution | 3-5x | Implemented |
| GPU-resident state | 2-4x | Implemented |
| Pipelined block production | 1.5-2x | Implemented |
| STARK proof generation | per-block | Implemented (mock + Stwo) |
| DA erasure coding | per-block | Implemented |
| Inference Tier 2 (optimistic) | Off-chain AI with fraud proofs | Implemented |
| Inference Tier 3 (STARK-verified) | ZK-proven off-chain AI | Implemented |

**Measured single-node:** 69.3K TPS (M4), **Projected:** 300K-1.3M TPS (A100/H100)
**Projected multi-node:** 1B+ TPS (100 H100 nodes with sharding)

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

## Home Node Support

ARC Chain is designed so regular people can participate in network validation from home hardware:

| Role | Hardware | Stake | Fee Share | Est. Cost |
|------|----------|-------|-----------|-----------|
| **Observer** | Raspberry Pi / laptop | 50,000 ARC | 15% of fees | ~$1/mo electricity |
| **Verifier** | Mac Mini / desktop | 500,000 ARC | 25% of fees | ~$3/mo electricity |
| **Proposer** | GPU server | 5,000,000 ARC | 40% of fees | Server-class hardware |

- **Observers** monitor the network, attest to block validity, and earn 15% of total fees. Minimal hardware requirements — a Raspberry Pi is sufficient.
- **Verifiers** validate transactions, check state transitions, and earn 25% of total fees. A Mac Mini or equivalent desktop handles the workload.
- **Proposers** produce blocks, run full execution, and earn 40% of total fees. Requires GPU-capable server hardware.
- **Treasury** receives the remaining 20% for protocol development and ecosystem grants.
- **Bootstrap fund**: 40M ARC allocated over 2 years for early validator subsidies to ensure profitability before fee volume ramps up.

No tokens are ever burned. The fixed supply of 1.03B ARC is fully preserved.

---

## Deployment

### Docker Compose

```bash
docker compose up -d --build
# Node: http://localhost:9090
# Explorer: http://localhost:3100
```

### Bare Metal (Ubuntu/Debian)

```bash
git clone https://github.com/FerrumVir/arc-chain.git /opt/arc-chain
cd /opt/arc-chain && bash deploy.sh
```

Creates systemd services for `arc-node` and `arc-explorer` with automatic restart.

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
+-- explorer/             # Next.js block explorer with client-side verification
+-- faucet/               # Rust testnet faucet
+-- docs/                 # 9 developer guides
+-- SPEC.md               # L1 technical specification
+-- BENCHMARK_RESULTS.md  # Performance report with methodology
+-- STATUS.md             # Feature completeness matrix
```

---

## Documentation

| Document | Description |
|----------|-------------|
| [SPEC.md](SPEC.md) | Full L1 technical specification |
| [BENCHMARK_RESULTS.md](BENCHMARK_RESULTS.md) | Measured performance, scaling projections, methodology |
| [STATUS.md](STATUS.md) | Feature completeness matrix — what's done, what's next |
| [docs/quickstart.md](docs/quickstart.md) | Getting started guide |
| [docs/architecture.md](docs/architecture.md) | System architecture deep-dive |
| [docs/rpc-api.md](docs/rpc-api.md) | RPC endpoint reference |
| [docs/smart-contracts.md](docs/smart-contracts.md) | Contract development guide |
| [docs/sdk-python.md](docs/sdk-python.md) | Python SDK reference |
| [docs/sdk-typescript.md](docs/sdk-typescript.md) | TypeScript SDK reference |

---

## License

BUSL-1.1 — Business Source License 1.1
