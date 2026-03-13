---
title: "Performance & Benchmarking"
sidebar_position: 8
slug: "/benchmarking"
---
# Performance and Benchmarking

ARC Chain includes a comprehensive benchmark suite (`arc-bench` crate) with multiple binaries targeting different performance dimensions.

---

## Quick Start

```bash
# Build everything in release mode (required for accurate results)
cargo build --release

# Run the main benchmark suite
cargo run --release -p arc-bench
```

Release mode is mandatory -- debug builds are 10-50x slower and produce misleading results.

---

## Benchmark Binaries

| Binary | Command | What It Measures |
|---|---|---|
| `arc-bench` | `cargo run --release -p arc-bench` | Full 5-phase scaling suite |
| `arc-bench-multinode` | `cargo run --release --bin arc-bench-multinode` | Real multi-node TPS via QUIC consensus |
| `arc-bench-propose-verify` | `cargo run --release --bin arc-bench-propose-verify` | Propose-verify pipeline throughput |
| `signed_bench` | `cargo run --release --bin signed_bench` | Ed25519 sign + verify + execute |
| `mixed_bench` | `cargo run --release --bin mixed_bench` | Mixed TX types (transfers, settles, deploys) |
| `soak_bench` | `cargo run --release --bin soak_bench` | Sustained load over time |
| `node_bench` | `cargo run --release --bin node_bench` | Full node pipeline (RPC + mempool + execution) |
| `parallel_bench` | `cargo run --release --bin parallel_bench` | Concurrent sender group throughput |
| `production_bench` | `cargo run --release --bin production_bench` | Combined production scenario |

---

## Main Benchmark Suite (arc-bench)

The main benchmark runs five phases sequentially:

### Phase 1: Single-Core Baseline

Sequential BLAKE3 hashing and single-sender execution. Establishes the floor.

- Hashes 2,000,000 transactions sequentially
- Measures raw BLAKE3 throughput (upper bound for TPS)
- Runs single-sender balance updates through StateDB

### Phase 2: Multi-Core Parallel Execution

Rayon-parallel execution with sender-sharded state using `DashMap`.

- Distributes transactions across all CPU cores
- Measures parallel execution with lock-free concurrent state access
- Scaling should be near-linear up to core count

### Phase 3: Compact Transactions

Uses the `CompactTransfer` format (fixed 250-byte layout) for maximum throughput.

- Reduces memory bandwidth vs standard 768-byte transactions
- Measures the impact of transaction size on TPS

### Phase 4: Simulated Multi-Node Cluster

Projects multi-node TPS based on single-node throughput (N nodes x single-node TPS).

### Phase 5: GPU Acceleration

Measures GPU compute shader throughput for batch operations.

- BLAKE3 batch hashing on GPU
- Ed25519 batch verification (Metal MSL or WGSL fallback)
- Commitment generation

---

## Multi-Node Benchmark

The `arc-bench-multinode` binary starts real `arc-node` instances in-process connected via QUIC transport with DAG consensus, and measures actual committed TPS through the full stack.

```bash
cargo run --release --bin arc-bench-multinode -- \
  --txs 100000 \
  --batch 1000 \
  --nodes 2
```

### CLI Flags

| Flag | Default | Description |
|---|---|---|
| `--txs` | `100000` | Total transactions to process across all nodes |
| `--batch` | `1000` | Batch size for mempool injection |
| `--nodes` | `2` | Number of validator nodes (2-4) |
| `--senders-per-node` | `50` | Funded sender accounts per node partition |
| `--warmup-blocks` | `3` | Warmup blocks before measurement starts |
| `--timeout-secs` | `300` | Max wait time for consensus to commit |

### What It Does

1. Starts N nodes with shared genesis, QUIC transport, DAG consensus
2. Pre-signs M transactions (Ed25519, deterministic keypairs)
3. Injects transactions into each node's mempool (partitioned by sender)
4. Waits for consensus to commit all transactions to state
5. Reports `TPS = committed transactions / wall-clock time`

---

## Propose-Verify Benchmark

Measures the bifurcated execution pipeline where a proposer executes transactions and verifiers validate state diffs.

```bash
cargo run --release --bin arc-bench-propose-verify
```

---

## Reference Results

### Apple M4 Pro (10 cores, 48 GB RAM)

| Benchmark | Result |
|---|---|
| Single-core BLAKE3 hashing | ~12M hashes/sec |
| Multi-core parallel execution | ~4.3M TPS |
| Compact transactions (250 bytes) | ~6M TPS |
| Signed transaction throughput (CPU, Rayon) | ~235K verifications/sec |
| GPU Ed25519 verification (Metal MSL) | ~121K verifications/sec |
| STARK proving (CPU, per block) | 450us - 4.2ms |

### Key Metrics

- **BLAKE3 hash TPS**: Raw hashing throughput (theoretical upper bound)
- **Execute TPS**: State execution (balance changes, nonce updates). The realistic single-node number.
- **Signed TPS**: Includes Ed25519 signature verification + execution. Production-accurate.
- **GPU TPS**: GPU-accelerated operations. Depends on hardware.
- **Multi-node TPS**: Real consensus-committed throughput across N nodes.

---

## GPU Ed25519 Verification

ARC Chain offloads Ed25519 signature verification to the GPU on supported hardware.

### Architecture

- **Metal MSL shader** (`crates/arc-gpu/src/ed25519_verify.metal`): Native hardware u64, branchless Shamir's trick
- **WGSL fallback** (`crates/arc-gpu/src/ed25519_verify.wgsl`): Cross-platform WebGPU
- **4-entry LUT**: Indexed by `(a_bit + 2*b_bit)` for zero SIMD divergence
- **Buffer pool**: Pre-allocated GPU buffers reused via `queue.write_buffer()`
- **SigVerifyCache**: `DashMap` pre-verification cache

### Performance Notes

- Metal MSL shader is preferred when available (Apple Silicon)
- The GPU path achieves ~121K verifications/sec (was 37K before branchless optimization)
- CPU parallel (Rayon) achieves ~235K v/s for comparison -- CPU is faster for this workload due to GPU dispatch overhead
- GPU becomes advantageous for very large batches where dispatch overhead is amortized

---

## STARK Proving

ARC Chain uses the Stwo prover (Circle STARK over M31 field) for zero-knowledge proofs.

### Run STARK Tests

```bash
cargo test -p arc-crypto --features stwo-prover
```

This runs 232 additional tests covering the 22-constraint AIR, inner-circuit recursion, and verified proof generation.

### Proving Performance

| Operation | Time |
|---|---|
| Single block proof (CPU) | 450us - 4.2ms |
| Recursive proof composition | ~8ms |

---

## Environment Variables

| Variable | Default | Description |
|---|---|---|
| `RUST_LOG` | `info` | Log verbosity (`debug` for per-block stats) |
| `RAYON_NUM_THREADS` | all cores | Number of parallel worker threads |

### Constrained Core Count

```bash
# Benchmark with only 4 cores
RAYON_NUM_THREADS=4 cargo run --release -p arc-bench
```

---

## Interpreting Results

### What to Look For

1. **Single-core baseline** -- Sets the floor. If this is low, check that you are running in `--release` mode.

2. **Multi-core scaling** -- Should be near-linear up to core count. Sub-linear scaling indicates contention in `DashMap` or other shared state.

3. **Signed vs unsigned** -- Signature verification typically adds 2-3x overhead. This is the gap between "execute TPS" and "signed TPS."

4. **GPU vs CPU** -- For Ed25519 verification, CPU parallel is often faster due to GPU dispatch overhead. GPU becomes advantageous only at very large batch sizes.

5. **Multi-node** -- Real consensus overhead (QUIC transport, DAG consensus, PEX) reduces throughput vs single-node. A 2-node benchmark should achieve 60-80% of single-node TPS.

### Common Issues

| Symptom | Cause | Fix |
|---|---|---|
| Very low TPS (&lt;10K) | Debug build | Use `--release` |
| GPU benchmarks show 0 | No GPU detected | Check `wgpu` drivers; fallback to CPU |
| Multi-node hangs | Port conflict | Ensure unique `--p2p-port` per node |
| Scaling plateaus early | Thread contention | Profile with `RUST_LOG=debug` |

---

## HTML Dashboard

The main benchmark (`arc-bench`) generates results that can be viewed in the HTML dashboard at `crates/arc-bench/src/dashboard.html`. Open it in a browser after running benchmarks to see interactive charts.

## Criterion Micro-Benchmarks

For statistical micro-benchmarks with warmup, iterations, and confidence intervals:

```bash
cargo bench
```

Results are written to `target/criterion/` as HTML reports.
