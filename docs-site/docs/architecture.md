---
title: Architecture
sidebar_position: 3
id: architecture
---

# Architecture

ARC Chain is structured as 11 Rust crates, each responsible for a distinct layer of the blockchain stack. This page covers the major subsystems: consensus, execution, state management, networking, cryptography, and GPU acceleration.

## Layer Overview

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

## Crate Breakdown

| Crate | LOC | Tests | What It Does |
|-------|-----|-------|-------------|
| `arc-types` | 14,490 | 264 | 24 transaction types, blocks, accounts, governance, staking, bridge, account abstraction, social recovery, inference attestation/challenge, state rent |
| `arc-state` | 13,203 | 147 | DashMap state DB, Jellyfish Merkle Tree, segmented WAL with auto-rotate, adaptive BlockSTM parallel execution, GPU-resident state cache, JMT auto-pruning, receipt pruning, state rent, state sync |
| `arc-crypto` | 11,680 | 220 | Ed25519, Secp256k1, BLS12-381, BLAKE3, Falcon-512 (post-quantum), ML-DSA, VRF, threshold crypto, Pedersen commitments, Stwo STARK prover |
| `arc-vm` | 8,439 | 145 | Wasmer WASM runtime, revm EVM interpreter, gas metering, host imports, 11 precompiles, AI inference oracle |
| `arc-node` | 8,424 | 61 | Pipelined block production, adaptive execution, RPC server (30 HTTP + ETH JSON-RPC), consensus manager, STARK proof gen, DA erasure coding, encrypted mempool |
| `arc-consensus` | 7,971 | 137 | DAG consensus, 2-round finality, beacon chain shard coordinator, validator roles, slashing, cross-shard coordination, epoch transitions |
| `arc-bench` | 5,336 | -- | 10 benchmark binaries |
| `arc-gpu` | 3,810 | 37 | Metal MSL + WGSL Ed25519 batch verification (379K sigs/sec), GPU account buffer, unified/managed memory, buffer pooling |
| `arc-net` | 2,355 | 26 | QUIC transport (quinn), shred propagation, XOR FEC, TX gossip, peer exchange, challenge-response auth |
| `arc-mempool` | 876 | 17 | Lock-free SegQueue FIFO, DashSet deduplication, encrypted mempool (BLS threshold) |
| `arc-cli` | 660 | -- | Command-line client: keygen, RPC queries, transaction submission |

## DAG Consensus (`arc-consensus`)

Unlike linear blockchains where one leader proposes at a time, ARC Chain uses a **directed acyclic graph (DAG)** where all validators propose blocks concurrently. The design is inspired by Mysticeti.

### Commit Rule

A block B in round R is finalized when:

1. A "certifier" block C in round R+1 references B as a parent
2. C is referenced by blocks in round R+2 with combined stake >= 2f+1

This gives **two-round finality** with no leader election overhead. Measured finality is approximately 450ms at 150ms per round.

### Validator Roles and Stake Tiers

| Role | Min Stake | Fee Share | Responsibilities |
|------|-----------|-----------|-----------------|
| **Observer** | 50,000 ARC | 15% | Monitor network, attest to block validity |
| **Verifier** | 500,000 ARC | 25% | Validate transactions, check state transitions |
| **Proposer** | 5,000,000 ARC | 40% | Produce blocks, run full execution |

**VRF proposer selection** ensures verifiable random leader rotation each round.

### Slashing

Equivocation (double-proposing in the same round) results in automatic stake reduction. Progressive penalties: 10% / 20% / 30% by tier. Validators below the Spark threshold are ejected.

### MEV Protection

An encrypted mempool using BLS threshold commit-reveal prevents front-running. Transactions are encrypted before inclusion and only decrypted after the block is committed.

## State Management (`arc-state`)

### DashMap

Lock-free concurrent reads and writes. The primary in-memory state store for accounts, balances, nonces, and contract storage. Achieves 22.3M lookups/sec on M2 Ultra.

### Jellyfish Merkle Tree (JMT)

Provides O(log n) inclusion and non-membership proofs for light clients. Features:

- Incremental updates with domain-separated BLAKE3 hashing
- Auto-pruning every 100 blocks (keeps 1,000 versions)
- Non-membership proofs via empty-slot and different-key verification

### Write-Ahead Log (WAL)

Segmented WAL with:
- Auto-rotate at 256 MB
- CRC32 integrity checks
- LZ4 compression
- Checkpoint and replay for crash recovery
- Pruning after snapshots

### BlockSTM Parallel Execution

Optimistic parallel transaction execution with sender-sharded conflict detection. The node auto-selects between Sequential and BlockSTM modes depending on workload characteristics.

## GPU Acceleration (`arc-gpu`)

### Ed25519 Batch Verification

GPU-accelerated signature verification using Metal (Apple) and WGSL (cross-platform) compute shaders:

- **379,000 verifications/sec** on M2 Ultra (13.68x over CPU)
- Branchless Shamir's trick with 4-entry LUT, zero SIMD divergence
- Buffer pool with async dispatch for non-blocking GPU submission
- Portable to CUDA, Vulkan, DirectX 12, and WebGPU via wgpu

### GPU-Resident State Cache

Account data formatted as 128-byte aligned `GpuAccountRepr` structs in GPU unified memory. On Apple Silicon, the GPU buffer and DashMap share the same physical memory pages. This enables batch compute shaders (BlockSTM execution, Merkle hashing) to access account state at approximately 2 TB/s unified memory bandwidth.

| Backend | Platform | Status |
|---------|----------|--------|
| Metal | macOS, iOS | Implemented + tested (native MSL shaders) |
| Vulkan | Linux, Windows, Android | Supported via wgpu (WGSL shaders) |
| CUDA | NVIDIA GPUs (A100, H100) | Supported via wgpu Vulkan backend |
| DirectX 12 | Windows | Supported via wgpu |
| WebGPU | Browsers | Supported via wgpu |

## Execution (`arc-vm`)

### Dual Runtime

- **WASM** (Wasmer 6.0) -- host imports for `balance_of`, `transfer`, `storage_get/set`, `emit_event`, `block_height`, `caller`. Deterministic execution with metered gas.
- **EVM** (revm 19) -- full EVM opcode execution with gas translation. Solidity contracts compile and deploy natively.

### Precompiles

11 precompiles at fixed addresses:

| Address | Precompile | Purpose |
|---------|-----------|---------|
| 0x01 | BLAKE3 | Fast hashing |
| 0x02 | Ed25519 | Signature verification |
| 0x03 | VRF | Verifiable random function |
| 0x04 | Oracle | Price feed reads |
| 0x05 | Merkle | Merkle proof verification |
| 0x06 | BlockInfo | Current block metadata |
| 0x07 | Identity | Identity precompile |
| 0x08 | Falcon-512 | Post-quantum signature verification |
| 0x09 | ZK-verify | STARK proof verification |
| 0x0A | AI-inference | On-chain model inference (Tier 1) |
| 0x0B | BLS-verify | BLS signature verification |

### STARK Proof Generation

Per-block STARK proof using the Stwo Circle STARK prover (M31 field). A 22-constraint transfer AIR covers balance conservation, nonce monotonicity, and signature binding. Recursive proof composition is supported. Proofs are compressed with RLE + dictionary compression.

## Networking (`arc-net`)

- **QUIC** (quinn) -- multiplexed streams, TLS 1.3, 0-RTT reconnect
- **Shred propagation** -- 1,280-byte chunks with XOR erasure coding (50% redundancy, single-shred recovery)
- **TX gossip** -- batched, stake-weighted fan-out with DashSet deduplication
- **Peer Exchange (PEX)** -- automatic peer discovery on a 60-second broadcast interval
- **Challenge-response auth** -- Ed25519 signed nonce + genesis hash binding

## Cryptographic Stack (`arc-crypto`)

| Algorithm | Library | Purpose |
|-----------|---------|---------|
| Ed25519 | ed25519-dalek | Primary transaction signing (118K sigs/sec) |
| Secp256k1 | k256 | Ethereum-compatible operations (MetaMask, bridges) |
| BLS12-381 | blst | Aggregate validator signatures (N sigs -> 1 verify) |
| BLAKE3 | blake3 | Domain-separated hashing, GPU-accelerated |
| Falcon-512 | pqcrypto | Post-quantum signatures (NIST selected) |
| ML-DSA | pqcrypto | Post-quantum digital signatures (FIPS 204) |
| Stwo STARK | stwo | Zero-knowledge proofs, no trusted setup |
| Pedersen | custom | Homomorphic commitments, privacy-preserving |

## Transaction Types (23 total)

### Core Protocol (16 types)

| Code | Type | Gas |
|------|------|-----|
| 0x01 | Transfer | 21,000 |
| 0x02 | Settle (zero-fee agent settlement) | 25,000 |
| 0x03 | Swap | 30,000 |
| 0x04 | Escrow | 35,000 |
| 0x05 | Stake | 25,000 |
| 0x06 | WasmCall | 21,000 + exec |
| 0x07 | MultiSig | 35,000 |
| 0x08 | DeployContract | 53,000 |
| 0x09 | RegisterAgent | 30,000 |
| 0x0a | JoinValidator | 30,000 |
| 0x0b | LeaveValidator | 25,000 |
| 0x0c | ClaimRewards | 25,000 |
| 0x0d | UpdateStake | 25,000 |
| 0x0e | Governance | 50,000 |
| 0x0f | BridgeLock | 50,000 |
| 0x10 | BridgeMint | 50,000 |

### L1 Native Scaling (5 types)

| Code | Type | Gas |
|------|------|-----|
| 0x11 | BatchSettle (bilateral netting) | 30,000 + 500/entry |
| 0x12 | ChannelOpen | 40,000 |
| 0x13 | ChannelClose | 35,000 |
| 0x14 | ChannelDispute | 50,000 |
| 0x15 | ShardProof (STARK verification) | 60,000 |

### Inference (2 types)

| Code | Type | Gas |
|------|------|-----|
| 0x16 | InferenceAttestation | 30,000 |
| 0x17 | InferenceChallenge | 50,000 |
