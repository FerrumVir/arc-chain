---
title: Benchmarks
sidebar_position: 7
id: benchmarks
---

# Benchmark Results

All benchmarks were run on real hardware with real cryptographic operations. No simulated or bypassed components.

## Hardware

| Machine | CPU | Memory | Notes |
|---------|-----|--------|-------|
| **M2 Ultra Mac Studio** | 24 cores (16P+8E) | 64 GB unified | Primary benchmark platform |
| **M4 MacBook Pro** | 10 cores | 16 GB unified | Previous benchmark platform |

## Measured Performance (M2 Ultra)

| Metric | Value | Conditions |
|--------|-------|------------|
| **Single-node peak TPS** | **183,000** | CPU verify + Sequential exec |
| **Multi-node sustained TPS** | **33,230** | 2 validators, real QUIC, real DAG consensus |
| **Peak TPS** | **350,000** | 1-second burst window |
| **Commit rate** | **100%** | 500K/500K transactions committed |
| **State lookups** | **22.3M/sec** | DashMap baseline |
| **GPU Ed25519 verify** | **379,000/sec** | Metal compute shader, 13.68x over CPU |
| **Ed25519 signing** | **82,800/sec** | Single-core, ed25519-dalek |
| **Finality** | **4.27s** | 2-round DAG commit |

### Execution Mode Comparison (M2 Ultra)

| Mode | TPS | Speedup | ETH-Weighted |
|------|-----|---------|-------------|
| CPU verify + Sequential exec | **183.0K** | 1.00x | 46.6K |
| CPU verify + Block-STM exec | 143.1K | 0.78x | 36.4K |
| CPU verify + Block-STM + Coalesce | 179.6K | 0.98x | 45.7K |
| GPU verify + Sequential exec | 96.9K | 0.53x | 24.7K |
| GPU verify + Block-STM exec | 115.9K | 0.63x | 29.5K |
| GPU verify + Block-STM + Coalesce | 121.5K | 0.66x | 30.9K |

Best single-node (weighted): **46.6K TPS** -- that is **3,104x faster** than Ethereum (~15 TPS).

### Previous Results (M4 MacBook Pro)

| Mode | TPS |
|------|-----|
| CPU verify + Sequential | 64.3K |
| CPU verify + Block-STM + Coalesce | 69.3K |
| Multi-node sustained (2 validators) | 27,000 |

## Running Benchmarks

### Multi-node TPS benchmark

The primary benchmark. Runs 2 validators with real QUIC transport, real DAG consensus, real Ed25519 signatures, and real state execution.

```bash
# Default: 100K transactions, 2 nodes
cargo run --release --bin arc-bench-multinode

# Custom parameters
cargo run --release --bin arc-bench-multinode -- \
    --txs 500000 \
    --nodes 2 \
    --senders-per-node 90 \
    --timeout-secs 120
```

Results are written to `benchmark-multinode-results.json`.

### Single-node parallel execution

Tests Sequential vs BlockSTM parallel execution modes:

```bash
cargo run --release --bin arc-bench-parallel
```

### GPU-resident state benchmark

Measures GPU-accelerated state lookups and transfers:

```bash
cargo run --release --bin arc-bench-gpu-state
```

### All available benchmarks

```bash
cargo run --release --bin arc-bench               # Basic single-node
cargo run --release --bin arc-bench-signed         # With real Ed25519 signing
cargo run --release --bin arc-bench-production     # Production-like workload
cargo run --release --bin arc-bench-mixed          # Mixed transaction types
cargo run --release --bin arc-bench-soak           # Long-running stability test
cargo run --release --bin arc-bench-propose-verify # Propose-verify pipeline
cargo run --release --bin arc-bench-node           # Full node benchmark
```

## What the Multi-Node Benchmark Measures

Every component in the benchmark is **real**, not simulated:

| Component | Status | Detail |
|-----------|--------|--------|
| QUIC transport | Real | Quinn-based, TLS encrypted, peer discovery |
| DAG consensus | Real | 2-round commit rule, stake-weighted quorum |
| Ed25519 signatures | Real | Every TX signed and verified (ed25519-dalek) |
| Mempool | Real | SegQueue FIFO, DashSet deduplication |
| State execution | Real | Balance checks, nonce validation, Merkle updates |
| Timing | Wall-clock | `std::time::Instant`, not CPU cycles |
| TX content | Real transfers | 1 ARC value, sequential nonces, distinct senders |

### What Is NOT Measured

- **Geographic latency** -- both nodes run on localhost
- **Disk I/O** -- state is in-memory DashMap (WAL exists but not exercised)
- **Smart contract execution** -- all transactions are simple transfers
- **Gossip overhead at scale** -- 2 nodes do not stress the gossip protocol
- **Adversarial conditions** -- no Byzantine validators or network partitions

## Scaling Projections

### Hardware Scaling (Single Shard)

| Hardware | Expected TPS | Multiplier |
|----------|-------------|------------|
| Apple M4 (measured) | 27,000 | 1x baseline |
| AMD EPYC 9654 (96 cores) | 150,000 - 200,000 | 6-8x |
| NVIDIA A100 (80GB) | 250,000 - 400,000 | 10-15x |
| NVIDIA H100 (80GB) | 400,000 - 600,000 | 15-22x |
| AMD MI300X (192GB) | 450,000 - 700,000 | 17-26x |

### Compound Scaling Projection

```
  Base:     27,000 TPS (M4 laptop, measured)
  x 8x     CPU cores (96 vs 10)
  x 2x     GPU batch sig verify
  x 3x     BlockSTM parallel execution
  ----------------------------------------
  ~1.3M TPS (single shard, single machine)
```

### Validator Sharding

Linear scaling via independent validator shards:

| Shards | M4 Laptop | A100 Server | H100 Cluster |
|--------|-----------|-------------|--------------|
| 1 | 27,000 | 300,000 | 500,000 |
| 4 | 95,000 | 1,050,000 | 1,760,000 |
| 16 | 380,000 | 4,200,000 | 7,000,000 |
| 64 | 1,500,000 | 16,800,000 | 28,000,000 |

### Multi-Node Propose-Verify Projections (ETH-weighted)

| Nodes | Projected TPS |
|-------|--------------|
| 10 | 419,100 |
| 50 | 1,980,000 |
| 100 | 3,720,000 |
| 500 | 16,300,000 |

## Comparison to Existing L1s

| Chain | Claimed TPS | Measured (Independent) |
|-------|------------|----------------------|
| Solana | 65,000 | 3,000 - 5,000 sustained |
| Sui | 297,000 | 10,000 - 20,000 sustained |
| Aptos | 160,000 | 12,000 - 15,000 sustained |
| NEAR | 100,000+ | 1,000 - 3,000 per shard |
| **ARC Chain** | **27,000** | **27,000 (measured)** |

ARC Chain's 27K TPS is an honest, measured number -- not a theoretical maximum. On equivalent server hardware, ARC Chain projects to match or exceed the measured throughput of existing high-performance L1s.
