# ARC Chain

A high-performance Layer 1 blockchain built from scratch in Rust. Purpose-built for AI agent coordination with zero-fee settlements, DAG consensus, GPU-accelerated execution, and post-quantum cryptography.

**Not a fork. Not a copy. Every line is original.**

---

## Measured Performance

| Metric | Value | Conditions |
|--------|-------|------------|
| **Sustained TPS** | **27,000** | 2 validators, real QUIC, real consensus, real Ed25519 signatures |
| **Peak TPS** | **350,000** | 1-second burst window |
| **Commit rate** | **100%** | 500K/500K transactions committed |
| **State lookups** | **15.2M/sec** | GPU-resident state cache, Metal unified memory |
| **Ed25519 signing** | **118,000/sec** | Single-core, ed25519-dalek |
| **GPU sig verify** | **121,000/sec** | Metal compute shader, branchless Shamir |

All numbers measured on Apple M4 MacBook Pro (10 cores, 16 GB). See [BENCHMARK_RESULTS.md](BENCHMARK_RESULTS.md) for full methodology.

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

**75,001 LOC Rust** | **1,050 tests** | **11 crates**

| Crate | LOC | Tests | What It Does |
|-------|-----|-------|-------------|
| `arc-types` | 14,071 | 258 | 21 transaction types, blocks, accounts, governance, staking, bridge, account abstraction, social recovery |
| `arc-crypto` | 11,680 | 240 | Ed25519, Secp256k1, BLS12-381, BLAKE3, Falcon-512 (post-quantum), ML-DSA, VRF, threshold crypto, Pedersen commitments, Stwo STARK prover |
| `arc-state` | 12,127 | 138 | DashMap state DB, Jellyfish Merkle Tree, WAL persistence, BlockSTM parallel execution, GPU-resident state cache, state sync |
| `arc-vm` | 8,265 | 145 | Wasmer WASM runtime, revm EVM interpreter, gas metering, host imports, precompiles, AI inference oracle |
| `arc-node` | 8,298 | 65 | Pipelined block production, signature verification, RPC server (20+ HTTP + ETH JSON-RPC), consensus manager |
| `arc-consensus` | 7,523 | 126 | DAG consensus, 2-round finality, stake tiers, slashing, cross-shard coordination, epoch transitions |
| `arc-bench` | 5,336 | — | 10 benchmark binaries (multinode, parallel, signed, soak, production, mixed, node, propose-verify, gpu-state) |
| `arc-gpu` | 3,810 | 37 | Metal MSL + WGSL Ed25519 batch verification, GPU account buffer, unified/managed memory, buffer pooling |
| `arc-net` | 2,355 | 24 | QUIC transport (quinn), shred propagation, XOR FEC, TX gossip, peer exchange, challenge-response auth |
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
- **21 transaction types** — transfers, settlements, staking, governance, bridge, channels, shard proofs
- **Zero-fee settlements** — AI agents settle for free

### State
- **DashMap** — lock-free concurrent reads/writes
- **Jellyfish Merkle Tree** — O(log n) inclusion + non-membership proofs
- **WAL persistence** — CRC32 integrity, LZ4 compression, crash recovery
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
cargo test --workspace --lib    # 1,050 tests
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

**Projected single-machine:** 300K-1.3M TPS (A100/H100 server)
**Projected multi-node:** 1B+ TPS (100 H100 nodes with sharding)

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
