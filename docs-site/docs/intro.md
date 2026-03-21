---
title: Introduction
sidebar_position: 1
slug: /
id: intro
---

# What is ARC Chain?

ARC Chain is a high-performance Layer 1 blockchain built from scratch in Rust. It is purpose-built for AI agent coordination with zero-fee settlements, DAG consensus, GPU-accelerated execution, and post-quantum cryptography.

**Not a fork. Not a copy. Every line is original.**

- **77,244 lines of Rust** across 11 crates
- **1,054 tests** passing, 0 failures
- **23 native transaction types** covering transfers, settlements, staking, governance, bridge, channels, shard proofs, and inference attestation

## Key Features

| Feature | Detail |
|---------|--------|
| **183K TPS** | Single-node peak on Apple M2 Ultra (CPU verify + Sequential execution) |
| **AI-Native** | Three inference tiers, on-chain agent registration, zero-fee agent settlements |
| **Zero-Fee Settlements** | AI agents settle for free via the `Settle` TX type (0x02) |
| **DAG Consensus** | Mysticeti-inspired, all validators propose in parallel, 2-round finality (~450ms) |
| **Post-Quantum** | Falcon-512 and ML-DSA signatures, Stwo Circle STARK proofs (no trusted setup) |
| **GPU Acceleration** | Metal + WGSL Ed25519 batch verification at 379K sigs/sec (13.68x over CPU) |
| **Dual VM** | WASM (Wasmer 6.0) + EVM (revm 19) smart contract runtime |
| **No-Burn Tokenomics** | Fixed 1.03B $ARC supply, 100% of fees distributed to validators and treasury |

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

## Crate Map

| Crate | LOC | Tests | What It Does |
|-------|-----|-------|-------------|
| `arc-types` | 14,490 | 264 | 23 transaction types, blocks, accounts, governance, staking, bridge, account abstraction, social recovery, inference attestation/challenge, state rent |
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

## Next Steps

- [Quickstart](./quickstart.md) -- build and run a node in minutes
- [Architecture](./architecture.md) -- deep dive into how each layer works
- [Agents Overview](./agents/overview.md) -- learn about on-chain AI agents (Synths)
- [RPC API](./rpc-api.md) -- full endpoint reference
