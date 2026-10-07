Historical topology experiments: the LAN/large-machine projections below are not the deployment target. See REGIONAL-SWARM.md and regional CI budgets for the 7 Oct ordinary-node scope.

# Island runtime: recovered evidence and corrected projections

This report reuses the **Studio lab** measurements first published in
`962b640a6207f917289a10fd25c7e9e5129687d5` and the **CI runner** measurements
from that exact SHA's [island run](https://github.com/FerrumVir/arc-chain/actions/runs/37635536328).
No performance measurement was repeated for this recovery. The raw JSON files
are byte-preserved; `data/evidence-provenance.json` records their SHA-256 hashes.
The tables below are regenerated arithmetic over those measurements.

- Synthetic MLA + MoE models, stage processes on one host, loopback TCP;
  WAN delay, jitter and uplink are simulated. No real Kimi weights or LAN/TB/RDMA.
- The 54-case WAN sweep per measured host uses 32-token answers. Per-answer,
  overlapping-decode-window aggregate, and whole-run aggregate errors are all
  reported as `(measured - predicted) / predicted`; negative means overprediction.
  The window spans the last first-token to the first last-token completion;
  it is a finite-run proxy for steady state, not a production steady-load trial.
- K2 and K2.6 share the text shapes used in this arithmetic (61 layers, hidden
  7,168, 384 routed experts/top-8, expert width 2,048; see the checkpoint notes).
  The projection assumes research-6 kernel bandwidth, uniform expert routing,
  cross-sequence weight-read amortization and INT8 compressed MLA KV storage.
  The current runtime runs sequences separately and keeps an integer cache;
  these capacity/throughput assumptions are not measured runtime capabilities.
- Aggregate projections are optimistic modeled bounds; associated sequence
  and device counts are optimistic requirements. They are not guarantees.
  The report includes 4k/8k/32k context and 0.3 ms link sensitivities.
- One speculative draft means 1.85 expected output tokens per two-position
  verification pass (85% acceptance assumed, drafter cost assumed zero).
  Transfer now charges every position; no fixed 1.34 multiplier remains.
  No projected case here establishes 59 tok/s per answer.
- Wire budgeting includes a conservative activation + accumulated-record +
  framing envelope. Headers are per item; the actual return hop has no activation.
  This is not measured Kimi wire traffic; network-stack overhead is excluded.

Regenerate the section starting at `## Measured` and `data/projection.json`:

```sh
python3 scripts/arc_island/report.py docs/island/data/studio-kimi-mini.json docs/island/data/studio-small.json docs/island/data/ci-small-962b640a.json --out generated.md --json docs/island/data/projection.json
```

Recovery leaves the runtime and `StageCommit.logits/selected` contract unchanged.
The existing tests cover exact splits/batches, honest audits, WrongToken on
threads and processes, hidden-state tampering, a mutated process reveal, and
forced restart **between steps**. In-flight recovery, signed commitments,
authenticated request/reveal metadata and full Kimi execution remain outside
this local transport implementation.

## Measured

### Studio lab: macos aarch64, 24 logical CPUs

Model: synthetic MLA + MoE, 8 layers, d_model 512, 64 routed experts top-8, profile `arc.hf-deepseek-v3.mla-moe.i8-dyadic-row.i4g32-experts.q16.v1`; package blake3 `b1bbd3fa1d524d46…`. WAN delay line: spin.

Single process, no network: 71.95 tok/s decode (14.04 ms per position); engine effective weight bandwidth 1.087 GB/s (15.11 MB of active weights per token).

Boundary activations on the wire (lossless width): {'i16': 0, 'i32': 420, 'i64': 0, 'i8': 0} over 420 vectors; mean 2,048 B per vector vs 4096 B as i64.

**Hop latency** (ring round trip of a ping frame through every stage process, no compute; loopback TCP):

| stages | payload B | hops | ring median ms | ring p90 ms | per hop µs |
|---|---|---|---|---|---|
| 1 | 0 | 2 | 0.07 | 0.16 | 36.79 |
| 1 | 4,096 | 2 | 0.06 | 0.12 | 30.15 |
| 1 | 14,336 | 2 | 0.08 | 0.14 | 38.19 |
| 1 | 28,672 | 2 | 0.04 | 0.05 | 20.96 |
| 1 | 57,344 | 2 | 0.06 | 0.08 | 31.02 |
| 2 | 0 | 3 | 0.19 | 0.23 | 61.82 |
| 2 | 4,096 | 3 | 0.16 | 0.23 | 54.47 |
| 2 | 14,336 | 3 | 0.17 | 0.24 | 57.96 |
| 2 | 28,672 | 3 | 0.19 | 0.26 | 61.72 |
| 2 | 57,344 | 3 | 0.22 | 0.30 | 73.64 |
| 4 | 0 | 5 | 0.28 | 0.38 | 56.02 |
| 4 | 4,096 | 5 | 0.29 | 0.38 | 58.52 |
| 4 | 14,336 | 5 | 0.29 | 0.36 | 57.95 |
| 4 | 28,672 | 5 | 0.31 | 0.39 | 62.03 |
| 4 | 57,344 | 5 | 0.36 | 0.45 | 72.71 |

**Throughput, stage processes on one host** (decode tok/s per answer, mean; aggregate = generated / wall time):

| stages | G | B | tokens | per answer tok/s | aggregate tok/s | bit-exact |
|---|---|---|---|---|---|---|
| 1 | 1 | 1 | 96 | 78.68 | 61.79 | yes |
| 1 | 2 | 8 | 96 | 8.74 | 58.71 | yes |
| 2 | 1 | 1 | 96 | 48.37 | 38.84 | yes |
| 2 | 2 | 2 | 96 | 37.08 | 59.27 | yes |
| 2 | 2 | 8 | 96 | 9.44 | 58.79 | yes |
| 4 | 1 | 1 | 96 | 46.82 | 37.74 | yes |
| 4 | 4 | 4 | 96 | 27.27 | 86.52 | yes |
| 4 | 4 | 8 | 96 | 13.98 | 86.81 | yes |

### Studio lab: macos aarch64, 24 logical CPUs

Model: synthetic MLA + MoE, 8 layers, d_model 256, 16 routed experts top-4, profile `arc.hf-deepseek-v3.mla-moe.i8-dyadic-row.q16.v1`; package blake3 `59cccff13d80759a…`. WAN delay line: spin.

Single process, no network: 495.86 tok/s decode (2.01 ms per position); engine effective weight bandwidth 2.791 GB/s (5.63 MB of active weights per token).

Boundary activations on the wire (lossless width): {'i16': 0, 'i32': 532, 'i64': 0, 'i8': 0} over 532 vectors; mean 1,024 B per vector vs 2048 B as i64.

**Hop latency** (ring round trip of a ping frame through every stage process, no compute; loopback TCP):

| stages | payload B | hops | ring median ms | ring p90 ms | per hop µs |
|---|---|---|---|---|---|
| 1 | 0 | 2 | 0.06 | 0.08 | 27.98 |
| 1 | 2,048 | 2 | 0.06 | 0.06 | 28.23 |
| 1 | 14,336 | 2 | 0.03 | 0.06 | 13.58 |
| 1 | 28,672 | 2 | 0.03 | 0.04 | 16.44 |
| 1 | 57,344 | 2 | 0.05 | 0.06 | 25.50 |
| 2 | 0 | 3 | 0.09 | 0.11 | 31.62 |
| 2 | 2,048 | 3 | 0.05 | 0.10 | 15.42 |
| 2 | 14,336 | 3 | 0.04 | 0.05 | 13.79 |
| 2 | 28,672 | 3 | 0.05 | 0.07 | 17.86 |
| 2 | 57,344 | 3 | 0.08 | 0.09 | 25.51 |
| 4 | 0 | 5 | 0.15 | 0.17 | 30.82 |
| 4 | 2,048 | 5 | 0.06 | 0.07 | 12.43 |
| 4 | 14,336 | 5 | 0.07 | 0.08 | 13.67 |
| 4 | 28,672 | 5 | 0.08 | 0.11 | 16.32 |
| 4 | 57,344 | 5 | 0.13 | 0.15 | 25.91 |

**Throughput, stage processes on one host** (decode tok/s per answer, mean; aggregate = generated / wall time):

| stages | G | B | tokens | per answer tok/s | aggregate tok/s | bit-exact |
|---|---|---|---|---|---|---|
| 1 | 1 | 1 | 128 | 474.21 | 403.40 | yes |
| 1 | 2 | 8 | 128 | 58.58 | 408.19 | yes |
| 2 | 1 | 1 | 128 | 455.13 | 388.82 | yes |
| 2 | 2 | 2 | 128 | 274.32 | 469.20 | yes |
| 2 | 2 | 8 | 128 | 74.39 | 502.16 | yes |
| 4 | 1 | 1 | 128 | 433.59 | 366.85 | yes |
| 4 | 4 | 4 | 128 | 147.06 | 508.21 | yes |
| 4 | 4 | 8 | 128 | 76.06 | 522.16 | yes |

**Emulated WAN, Studio lab** (every stage's uplink shaped: one-way delay ± 10% jitter, bounded uplink; 28672 B of activation per position on every hop but the last (a Kimi K2 boundary at i32; commitments ride on top); G = stages micro-batches of `depth` sequences; 32 tokens per answer). Predicted = research-6 §2.6 round-time model fed with this run's measured compute per position and hop overhead; it predicts steady state.

Model error (measured − predicted) ÷ predicted, over 54 runs:
- per answer: median -3.0%, range -14.6% to -0.7%;
- aggregate, steady state (tokens while every sequence decodes): median -3.1%, range -14.2% to -0.5%;
- aggregate, wall clock (prefill and pipeline fill/drain included): median -7.7%, range -19.3% to -4.2%.

| stages | one-way ms | uplink Mbit/s | depth | B | per answer tok/s | predicted | aggregate steady tok/s | aggregate wall tok/s | predicted aggregate | bit-exact |
|---|---|---|---|---|---|---|---|---|---|---|
| 2 | 10.00 | 20.00 | 1 | 2 | 28.79 | 29.84 | 57.73 | 54.77 | 59.68 | yes |
| 2 | 10.00 | 20.00 | 4 | 8 | 10.75 | 10.90 | 86.73 | 82.43 | 87.19 | yes |
| 2 | 10.00 | 20.00 | 16 | 32 | 2.69 | 2.72 | 86.67 | 83.23 | 87.19 | yes |
| 2 | 10.00 | 100.00 | 1 | 2 | 39.88 | 41.09 | 79.11 | 76.69 | 82.19 | yes |
| 2 | 10.00 | 100.00 | 4 | 8 | 23.83 | 26.85 | 190.16 | 183.00 | 214.84 | yes |
| 2 | 10.00 | 100.00 | 16 | 32 | 9.64 | 11.26 | 309.59 | 294.61 | 360.17 | yes |
| 2 | 30.00 | 20.00 | 1 | 2 | 13.35 | 13.60 | 26.59 | 25.54 | 27.21 | yes |
| 2 | 30.00 | 20.00 | 4 | 8 | 8.46 | 8.78 | 67.83 | 64.56 | 70.21 | yes |
| 2 | 30.00 | 20.00 | 16 | 32 | 2.69 | 2.72 | 86.67 | 82.68 | 87.19 | yes |
| 2 | 30.00 | 100.00 | 1 | 2 | 15.40 | 15.54 | 30.47 | 29.77 | 31.09 | yes |
| 2 | 30.00 | 100.00 | 4 | 8 | 12.51 | 12.95 | 99.33 | 96.07 | 103.58 | yes |
| 2 | 30.00 | 100.00 | 16 | 32 | 6.99 | 7.76 | 224.49 | 213.40 | 248.36 | yes |
| 2 | 60.00 | 20.00 | 1 | 2 | 7.40 | 7.49 | 14.64 | 14.30 | 14.98 | yes |
| 2 | 60.00 | 20.00 | 4 | 8 | 5.56 | 5.75 | 44.49 | 42.48 | 45.99 | yes |
| 2 | 60.00 | 20.00 | 16 | 32 | 2.69 | 2.72 | 86.74 | 81.93 | 87.19 | yes |
| 2 | 60.00 | 100.00 | 1 | 2 | 7.94 | 8.04 | 15.66 | 15.34 | 16.09 | yes |
| 2 | 60.00 | 100.00 | 4 | 8 | 7.07 | 7.29 | 56.14 | 54.27 | 58.29 | yes |
| 2 | 60.00 | 100.00 | 16 | 32 | 4.96 | 5.30 | 157.32 | 152.96 | 169.45 | yes |
| 4 | 10.00 | 20.00 | 1 | 4 | 12.72 | 13.08 | 51.03 | 48.13 | 52.30 | yes |
| 4 | 10.00 | 20.00 | 4 | 16 | 5.16 | 5.38 | 83.47 | 77.59 | 86.15 | yes |
| 4 | 10.00 | 20.00 | 16 | 64 | 1.33 | 1.36 | 86.09 | 80.52 | 87.19 | yes |
| 4 | 10.00 | 100.00 | 1 | 4 | 19.50 | 20.43 | 77.22 | 74.74 | 81.70 | yes |
| 4 | 10.00 | 100.00 | 4 | 16 | 11.64 | 13.22 | 186.35 | 176.55 | 211.58 | yes |
| 4 | 10.00 | 100.00 | 16 | 64 | 4.76 | 5.49 | 306.54 | 287.59 | 351.11 | yes |
| 4 | 30.00 | 20.00 | 1 | 4 | 6.29 | 6.39 | 24.88 | 24.11 | 25.56 | yes |
| 4 | 30.00 | 20.00 | 4 | 16 | 3.66 | 3.76 | 58.70 | 55.08 | 60.21 | yes |
| 4 | 30.00 | 20.00 | 16 | 64 | 1.33 | 1.36 | 86.10 | 80.02 | 87.19 | yes |
| 4 | 30.00 | 100.00 | 1 | 4 | 7.67 | 7.75 | 30.14 | 29.53 | 31.02 | yes |
| 4 | 30.00 | 100.00 | 4 | 16 | 6.24 | 6.43 | 98.62 | 95.53 | 102.81 | yes |
| 4 | 30.00 | 100.00 | 16 | 64 | 3.36 | 3.81 | 215.01 | 205.12 | 244.01 | yes |
| 4 | 60.00 | 20.00 | 1 | 4 | 3.57 | 3.62 | 14.12 | 13.67 | 14.47 | yes |
| 4 | 60.00 | 20.00 | 4 | 16 | 2.54 | 2.59 | 40.69 | 38.75 | 41.48 | yes |
| 4 | 60.00 | 20.00 | 16 | 64 | 1.17 | 1.22 | 75.57 | 70.53 | 77.79 | yes |
| 4 | 60.00 | 100.00 | 1 | 4 | 3.97 | 4.02 | 15.62 | 15.31 | 16.07 | yes |
| 4 | 60.00 | 100.00 | 4 | 16 | 3.56 | 3.63 | 56.03 | 54.72 | 58.05 | yes |
| 4 | 60.00 | 100.00 | 16 | 64 | 2.47 | 2.62 | 157.35 | 150.99 | 167.42 | yes |
| 8 | 10.00 | 20.00 | 1 | 8 | 6.00 | 6.16 | 48.11 | 45.16 | 49.25 | yes |
| 8 | 10.00 | 20.00 | 4 | 32 | 2.34 | 2.44 | 75.56 | 70.09 | 78.18 | yes |
| 8 | 10.00 | 20.00 | 16 | 128 | 0.66 | 0.68 | 85.40 | 78.86 | 87.19 | yes |
| 8 | 10.00 | 100.00 | 1 | 8 | 9.84 | 10.18 | 77.53 | 75.15 | 81.47 | yes |
| 8 | 10.00 | 100.00 | 4 | 32 | 6.01 | 6.56 | 192.84 | 182.55 | 209.99 | yes |
| 8 | 10.00 | 100.00 | 16 | 128 | 2.31 | 2.71 | 297.43 | 279.92 | 346.74 | yes |
| 8 | 30.00 | 20.00 | 1 | 8 | 3.07 | 3.10 | 24.34 | 23.31 | 24.81 | yes |
| 8 | 30.00 | 20.00 | 4 | 32 | 1.70 | 1.76 | 54.53 | 51.16 | 56.21 | yes |
| 8 | 30.00 | 20.00 | 16 | 128 | 0.61 | 0.64 | 79.39 | 73.51 | 82.22 | yes |
| 8 | 30.00 | 100.00 | 1 | 8 | 3.78 | 3.87 | 29.79 | 29.14 | 30.98 | yes |
| 8 | 30.00 | 100.00 | 4 | 32 | 3.10 | 3.20 | 99.16 | 94.90 | 102.44 | yes |
| 8 | 30.00 | 100.00 | 16 | 128 | 1.69 | 1.89 | 216.37 | 205.44 | 241.90 | yes |
| 8 | 60.00 | 20.00 | 1 | 8 | 1.76 | 1.78 | 13.88 | 13.49 | 14.22 | yes |
| 8 | 60.00 | 20.00 | 4 | 32 | 1.22 | 1.24 | 38.89 | 36.76 | 39.54 | yes |
| 8 | 60.00 | 20.00 | 16 | 128 | 0.54 | 0.56 | 69.29 | 64.44 | 71.24 | yes |
| 8 | 60.00 | 100.00 | 1 | 8 | 1.99 | 2.01 | 15.68 | 15.32 | 16.06 | yes |
| 8 | 60.00 | 100.00 | 4 | 32 | 1.78 | 1.81 | 56.17 | 54.40 | 57.93 | yes |
| 8 | 60.00 | 100.00 | 16 | 128 | 1.19 | 1.30 | 151.75 | 145.04 | 166.42 | yes |

**Curve, measured on the emulated WAN (Studio lab)**: the smallest swept configuration (2/4/8 stages × depth 1/4/16, G = stages) whose steady-state aggregate reached each target, with Kimi-sized activations on the wire. Synthetic model, so compute per stage is small; the network and the uplink set these numbers.

| one-way ms | uplink Mbit/s | target tok/s | stages | depth | concurrent | per answer tok/s | aggregate tok/s |
|---|---|---|---|---|---|---|---|
| 10.00 | 20.00 | 25 | 2 | 1 | 2 | 28.79 | 57.73 |
| 10.00 | 20.00 | 50 | 2 | 1 | 2 | 28.79 | 57.73 |
| 10.00 | 20.00 | 100 | – | – | – | – | not reached (best 87) |
| 10.00 | 20.00 | 150 | – | – | – | – | not reached (best 87) |
| 10.00 | 20.00 | 200 | – | – | – | – | not reached (best 87) |
| 10.00 | 100.00 | 25 | 2 | 1 | 2 | 39.88 | 79.11 |
| 10.00 | 100.00 | 50 | 2 | 1 | 2 | 39.88 | 79.11 |
| 10.00 | 100.00 | 100 | 2 | 4 | 8 | 23.83 | 190.16 |
| 10.00 | 100.00 | 150 | 2 | 4 | 8 | 23.83 | 190.16 |
| 10.00 | 100.00 | 200 | 2 | 16 | 32 | 9.64 | 309.59 |
| 30.00 | 20.00 | 25 | 2 | 1 | 2 | 13.35 | 26.59 |
| 30.00 | 20.00 | 50 | 2 | 4 | 8 | 8.46 | 67.83 |
| 30.00 | 20.00 | 100 | – | – | – | – | not reached (best 87) |
| 30.00 | 20.00 | 150 | – | – | – | – | not reached (best 87) |
| 30.00 | 20.00 | 200 | – | – | – | – | not reached (best 87) |
| 30.00 | 100.00 | 25 | 2 | 1 | 2 | 15.40 | 30.47 |
| 30.00 | 100.00 | 50 | 2 | 4 | 8 | 12.51 | 99.33 |
| 30.00 | 100.00 | 100 | 2 | 16 | 32 | 6.99 | 224.49 |
| 30.00 | 100.00 | 150 | 2 | 16 | 32 | 6.99 | 224.49 |
| 30.00 | 100.00 | 200 | 2 | 16 | 32 | 6.99 | 224.49 |
| 60.00 | 20.00 | 25 | 2 | 4 | 8 | 5.56 | 44.49 |
| 60.00 | 20.00 | 50 | 2 | 16 | 32 | 2.69 | 86.74 |
| 60.00 | 20.00 | 100 | – | – | – | – | not reached (best 87) |
| 60.00 | 20.00 | 150 | – | – | – | – | not reached (best 87) |
| 60.00 | 20.00 | 200 | – | – | – | – | not reached (best 87) |
| 60.00 | 100.00 | 25 | 2 | 4 | 8 | 7.07 | 56.14 |
| 60.00 | 100.00 | 50 | 2 | 4 | 8 | 7.07 | 56.14 |
| 60.00 | 100.00 | 100 | 2 | 16 | 32 | 4.96 | 157.32 |
| 60.00 | 100.00 | 150 | 2 | 16 | 32 | 4.96 | 157.32 |
| 60.00 | 100.00 | 200 | – | – | – | – | not reached (best 157) |

### CI runner (GitHub ubuntu-latest): linux x86_64, 4 logical CPUs

Model: synthetic MLA + MoE, 8 layers, d_model 256, 16 routed experts top-4, profile `arc.hf-deepseek-v3.mla-moe.i8-dyadic-row.q16.v1`; package blake3 `59cccff13d80759a…`. WAN delay line: sleep.

Single process, no network: 155.83 tok/s decode (6.37 ms per position); engine effective weight bandwidth 0.877 GB/s (5.63 MB of active weights per token).

Boundary activations on the wire (lossless width): {'i16': 0, 'i32': 532, 'i64': 0, 'i8': 0} over 532 vectors; mean 1,024 B per vector vs 2048 B as i64.

**Hop latency** (ring round trip of a ping frame through every stage process, no compute; loopback TCP):

| stages | payload B | hops | ring median ms | ring p90 ms | per hop µs |
|---|---|---|---|---|---|
| 1 | 0 | 2 | 0.11 | 0.11 | 53.71 |
| 1 | 2,048 | 2 | 0.11 | 0.12 | 56.37 |
| 1 | 14,336 | 2 | 0.12 | 0.13 | 61.09 |
| 1 | 28,672 | 2 | 0.13 | 0.14 | 66.37 |
| 1 | 57,344 | 2 | 0.15 | 0.16 | 76.11 |
| 2 | 0 | 3 | 0.15 | 0.16 | 51.25 |
| 2 | 2,048 | 3 | 0.16 | 0.17 | 54.09 |
| 2 | 14,336 | 3 | 0.18 | 0.18 | 59.44 |
| 2 | 28,672 | 3 | 0.19 | 0.19 | 63.58 |
| 2 | 57,344 | 3 | 0.21 | 0.22 | 71.62 |
| 4 | 0 | 5 | 0.25 | 0.26 | 50.02 |
| 4 | 2,048 | 5 | 0.26 | 0.26 | 51.34 |
| 4 | 14,336 | 5 | 0.28 | 0.29 | 55.85 |
| 4 | 28,672 | 5 | 0.30 | 0.31 | 60.85 |
| 4 | 57,344 | 5 | 0.35 | 0.36 | 69.99 |

**Throughput, stage processes on one host** (decode tok/s per answer, mean; aggregate = generated / wall time):

| stages | G | B | tokens | per answer tok/s | aggregate tok/s | bit-exact |
|---|---|---|---|---|---|---|
| 1 | 1 | 1 | 128 | 149.41 | 124.80 | yes |
| 1 | 2 | 8 | 128 | 19.34 | 134.09 | yes |
| 2 | 1 | 1 | 128 | 155.16 | 130.48 | yes |
| 2 | 2 | 2 | 128 | 115.88 | 195.27 | yes |
| 2 | 2 | 8 | 128 | 28.71 | 194.18 | yes |
| 4 | 1 | 1 | 128 | 171.15 | 145.33 | yes |
| 4 | 4 | 4 | 128 | 84.20 | 278.95 | yes |
| 4 | 4 | 8 | 128 | 40.18 | 265.81 | yes |

**Emulated WAN, CI runner (GitHub ubuntu-latest)** (every stage's uplink shaped: one-way delay ± 10% jitter, bounded uplink; 28672 B of activation per position on every hop but the last (a Kimi K2 boundary at i32; commitments ride on top); G = stages micro-batches of `depth` sequences; 32 tokens per answer). Predicted = research-6 §2.6 round-time model fed with this run's measured compute per position and hop overhead; it predicts steady state.

Model error (measured − predicted) ÷ predicted, over 54 runs:
- per answer: median -2.2%, range -12.9% to 0.1%;
- aggregate, steady state (tokens while every sequence decodes): median -2.5%, range -13.2% to -0.3%;
- aggregate, wall clock (prefill and pipeline fill/drain included): median -6.8%, range -17.2% to -3.2%.

| stages | one-way ms | uplink Mbit/s | depth | B | per answer tok/s | predicted | aggregate steady tok/s | aggregate wall tok/s | predicted aggregate | bit-exact |
|---|---|---|---|---|---|---|---|---|---|---|
| 2 | 10.00 | 20.00 | 1 | 2 | 25.55 | 26.34 | 51.24 | 48.80 | 52.68 | yes |
| 2 | 10.00 | 20.00 | 4 | 8 | 10.60 | 10.90 | 85.53 | 80.48 | 87.19 | yes |
| 2 | 10.00 | 20.00 | 16 | 32 | 2.69 | 2.72 | 86.91 | 82.72 | 87.19 | yes |
| 2 | 10.00 | 100.00 | 1 | 2 | 34.73 | 34.74 | 68.77 | 66.94 | 69.47 | yes |
| 2 | 10.00 | 100.00 | 4 | 8 | 17.16 | 18.26 | 137.13 | 131.64 | 146.05 | yes |
| 2 | 10.00 | 100.00 | 16 | 32 | 5.82 | 6.30 | 186.90 | 177.62 | 201.59 | yes |
| 2 | 30.00 | 20.00 | 1 | 2 | 12.61 | 12.83 | 25.03 | 24.30 | 25.65 | yes |
| 2 | 30.00 | 20.00 | 4 | 8 | 7.42 | 7.61 | 59.69 | 56.46 | 60.85 | yes |
| 2 | 30.00 | 20.00 | 16 | 32 | 2.69 | 2.72 | 86.70 | 81.93 | 87.19 | yes |
| 2 | 30.00 | 100.00 | 1 | 2 | 14.40 | 14.54 | 28.61 | 27.65 | 29.08 | yes |
| 2 | 30.00 | 100.00 | 4 | 8 | 10.56 | 10.55 | 83.62 | 81.69 | 84.41 | yes |
| 2 | 30.00 | 100.00 | 16 | 32 | 4.83 | 5.03 | 154.16 | 148.80 | 161.02 | yes |
| 2 | 60.00 | 20.00 | 1 | 2 | 7.20 | 7.25 | 14.26 | 13.89 | 14.50 | yes |
| 2 | 60.00 | 20.00 | 4 | 8 | 5.18 | 5.22 | 41.50 | 39.59 | 41.78 | yes |
| 2 | 60.00 | 20.00 | 16 | 32 | 2.42 | 2.47 | 78.06 | 73.73 | 78.91 | yes |
| 2 | 60.00 | 100.00 | 1 | 2 | 7.73 | 7.76 | 15.26 | 14.93 | 15.53 | yes |
| 2 | 60.00 | 100.00 | 4 | 8 | 6.35 | 6.46 | 50.16 | 48.99 | 51.69 | yes |
| 2 | 60.00 | 100.00 | 16 | 32 | 3.84 | 3.86 | 122.42 | 118.29 | 123.68 | yes |
| 4 | 10.00 | 20.00 | 1 | 4 | 11.97 | 12.34 | 47.99 | 45.38 | 49.37 | yes |
| 4 | 10.00 | 20.00 | 4 | 16 | 4.72 | 4.92 | 76.28 | 71.08 | 78.68 | yes |
| 4 | 10.00 | 20.00 | 16 | 64 | 1.33 | 1.36 | 86.17 | 80.16 | 87.19 | yes |
| 4 | 10.00 | 100.00 | 1 | 4 | 18.14 | 18.69 | 71.63 | 69.82 | 74.77 | yes |
| 4 | 10.00 | 100.00 | 4 | 16 | 10.02 | 10.72 | 159.06 | 153.60 | 171.58 | yes |
| 4 | 10.00 | 100.00 | 16 | 64 | 3.45 | 3.96 | 220.25 | 210.00 | 253.71 | yes |
| 4 | 30.00 | 20.00 | 1 | 4 | 6.12 | 6.21 | 24.21 | 23.48 | 24.84 | yes |
| 4 | 30.00 | 20.00 | 4 | 16 | 3.45 | 3.53 | 55.40 | 52.19 | 56.47 | yes |
| 4 | 30.00 | 20.00 | 16 | 64 | 1.25 | 1.29 | 80.65 | 75.05 | 82.83 | yes |
| 4 | 30.00 | 100.00 | 1 | 4 | 7.40 | 7.49 | 29.10 | 28.55 | 29.96 | yes |
| 4 | 30.00 | 100.00 | 4 | 16 | 5.65 | 5.77 | 89.34 | 86.90 | 92.35 | yes |
| 4 | 30.00 | 100.00 | 16 | 64 | 2.78 | 3.01 | 177.97 | 168.95 | 192.62 | yes |
| 4 | 60.00 | 20.00 | 1 | 4 | 3.53 | 3.56 | 13.98 | 13.53 | 14.23 | yes |
| 4 | 60.00 | 20.00 | 4 | 16 | 2.46 | 2.48 | 39.22 | 37.23 | 39.67 | yes |
| 4 | 60.00 | 20.00 | 16 | 64 | 1.09 | 1.12 | 70.71 | 66.00 | 71.70 | yes |
| 4 | 60.00 | 100.00 | 1 | 4 | 3.88 | 3.94 | 15.27 | 14.96 | 15.78 | yes |
| 4 | 60.00 | 100.00 | 4 | 16 | 3.36 | 3.41 | 52.91 | 51.67 | 54.56 | yes |
| 4 | 60.00 | 100.00 | 16 | 64 | 2.13 | 2.21 | 135.88 | 130.37 | 141.51 | yes |
| 8 | 10.00 | 20.00 | 1 | 8 | 5.85 | 5.98 | 46.76 | 44.14 | 47.86 | yes |
| 8 | 10.00 | 20.00 | 4 | 32 | 2.26 | 2.34 | 73.01 | 67.76 | 74.92 | yes |
| 8 | 10.00 | 20.00 | 16 | 128 | 0.66 | 0.68 | 85.39 | 78.65 | 87.19 | yes |
| 8 | 10.00 | 100.00 | 1 | 8 | 9.44 | 9.72 | 74.19 | 72.37 | 77.73 | yes |
| 8 | 10.00 | 100.00 | 4 | 32 | 5.48 | 5.88 | 175.58 | 166.55 | 188.01 | yes |
| 8 | 10.00 | 100.00 | 16 | 128 | 2.05 | 2.28 | 263.03 | 247.29 | 291.37 | yes |
| 8 | 30.00 | 20.00 | 1 | 8 | 3.01 | 3.06 | 23.84 | 22.98 | 24.45 | yes |
| 8 | 30.00 | 20.00 | 4 | 32 | 1.67 | 1.70 | 53.53 | 50.21 | 54.51 | yes |
| 8 | 30.00 | 20.00 | 16 | 128 | 0.59 | 0.61 | 76.94 | 71.23 | 78.68 | yes |
| 8 | 30.00 | 100.00 | 1 | 8 | 3.74 | 3.80 | 29.29 | 28.74 | 30.43 | yes |
| 8 | 30.00 | 100.00 | 4 | 32 | 2.96 | 3.03 | 93.54 | 90.67 | 96.91 | yes |
| 8 | 30.00 | 100.00 | 16 | 128 | 1.54 | 1.67 | 197.24 | 187.04 | 213.58 | yes |
| 8 | 60.00 | 20.00 | 1 | 8 | 1.74 | 1.76 | 13.79 | 13.35 | 14.11 | yes |
| 8 | 60.00 | 20.00 | 4 | 32 | 1.19 | 1.21 | 37.94 | 36.25 | 38.69 | yes |
| 8 | 60.00 | 20.00 | 16 | 128 | 0.52 | 0.54 | 67.27 | 62.69 | 68.56 | yes |
| 8 | 60.00 | 100.00 | 1 | 8 | 1.96 | 1.99 | 15.35 | 15.12 | 15.91 | yes |
| 8 | 60.00 | 100.00 | 4 | 32 | 1.73 | 1.75 | 54.42 | 53.12 | 56.12 | yes |
| 8 | 60.00 | 100.00 | 16 | 128 | 1.14 | 1.19 | 145.56 | 139.13 | 152.51 | yes |

**Curve, measured on the emulated WAN (CI runner (GitHub ubuntu-latest))**: the smallest swept configuration (2/4/8 stages × depth 1/4/16, G = stages) whose steady-state aggregate reached each target, with Kimi-sized activations on the wire. Synthetic model, so compute per stage is small; the network and the uplink set these numbers.

| one-way ms | uplink Mbit/s | target tok/s | stages | depth | concurrent | per answer tok/s | aggregate tok/s |
|---|---|---|---|---|---|---|---|
| 10.00 | 20.00 | 25 | 2 | 1 | 2 | 25.55 | 51.24 |
| 10.00 | 20.00 | 50 | 2 | 1 | 2 | 25.55 | 51.24 |
| 10.00 | 20.00 | 100 | – | – | – | – | not reached (best 87) |
| 10.00 | 20.00 | 150 | – | – | – | – | not reached (best 87) |
| 10.00 | 20.00 | 200 | – | – | – | – | not reached (best 87) |
| 10.00 | 100.00 | 25 | 2 | 1 | 2 | 34.73 | 68.77 |
| 10.00 | 100.00 | 50 | 2 | 1 | 2 | 34.73 | 68.77 |
| 10.00 | 100.00 | 100 | 2 | 4 | 8 | 17.16 | 137.13 |
| 10.00 | 100.00 | 150 | 4 | 4 | 16 | 10.02 | 159.06 |
| 10.00 | 100.00 | 200 | 4 | 16 | 64 | 3.45 | 220.25 |
| 30.00 | 20.00 | 25 | 2 | 1 | 2 | 12.61 | 25.03 |
| 30.00 | 20.00 | 50 | 2 | 4 | 8 | 7.42 | 59.69 |
| 30.00 | 20.00 | 100 | – | – | – | – | not reached (best 87) |
| 30.00 | 20.00 | 150 | – | – | – | – | not reached (best 87) |
| 30.00 | 20.00 | 200 | – | – | – | – | not reached (best 87) |
| 30.00 | 100.00 | 25 | 2 | 1 | 2 | 14.40 | 28.61 |
| 30.00 | 100.00 | 50 | 2 | 4 | 8 | 10.56 | 83.62 |
| 30.00 | 100.00 | 100 | 2 | 16 | 32 | 4.83 | 154.16 |
| 30.00 | 100.00 | 150 | 2 | 16 | 32 | 4.83 | 154.16 |
| 30.00 | 100.00 | 200 | – | – | – | – | not reached (best 197) |
| 60.00 | 20.00 | 25 | 2 | 4 | 8 | 5.18 | 41.50 |
| 60.00 | 20.00 | 50 | 2 | 16 | 32 | 2.42 | 78.06 |
| 60.00 | 20.00 | 100 | – | – | – | – | not reached (best 78) |
| 60.00 | 20.00 | 150 | – | – | – | – | not reached (best 78) |
| 60.00 | 20.00 | 200 | – | – | – | – | not reached (best 78) |
| 60.00 | 100.00 | 25 | 2 | 4 | 8 | 6.35 | 50.16 |
| 60.00 | 100.00 | 50 | 2 | 4 | 8 | 6.35 | 50.16 |
| 60.00 | 100.00 | 100 | 2 | 16 | 32 | 3.84 | 122.42 |
| 60.00 | 100.00 | 150 | – | – | – | – | not reached (best 146) |
| 60.00 | 100.00 | 200 | – | – | – | – | not reached (best 146) |

## Bit-exactness

Every island run above was compared with the single process (tokens, every logits hash, the hash at every layer boundary of every position): **all identical**.

## Projection for Kimi K2 / K2.6 (not a measurement)

Everything below is arithmetic, not a measurement, for **Kimi K2**; K2.6 has the same text shapes (`docs/protocol/kimi-k26-checkpoint.md` §1), so it applies to the requested K2.6 unchanged. No Kimi weights and no real network were run.

Inputs: the measured software cost per hop, 62.2 µs (CI runner (GitHub ubuntu-latest), loopback TCP, 28 KiB frame; the larger of the measured hosts); research-6's memory bandwidths (456 GB/s M3 Ultra, 1,108 GB/s RTX 5090), which ARC's engine does not reach (see the measured host-specific bandwidths above); batched weight reads across a micro-batch's sequences, which this runtime does not have (the swarm table also shows per-sequence reads); and **4096 tokens of context per sequence** (sensitivity table below).

**Aggregates are optimistic.** The same model, fed with measured costs, put the emulated-WAN steady-state aggregate error at a median -2.8%, and wall-clock aggregate error (with prefill and fill/drain) at a median -7.4%, using (measured − predicted) / predicted. Read every projected aggregate below as an upper bound.

**Islands (T1), pipeline parallel, PROJECTION**. Concurrency = stages × depth (one micro-batch per stage). "0.3 ms hop" replaces the measured loopback hop with research-6's streaming-transport hop (a real NIC, driver and GPU copies). "1 draft" is the model's verify pass over 2 positions (one draft token) at 1.85 tokens accepted per pass — DeepSeek-V3's MTP acceptance, ASSUMED: Kimi has no MTP head; acceptance is unknown and draft generation cost is assumed zero. Transfer scales with the number of positions, including the two-position verify pass. Full-record wire envelopes are charged on every hop, conservatively including the return hop.

| devices | stages | link | depth | concurrent | per answer tok/s | at 0.3 ms hop | 1 draft | aggregate tok/s | KV fits |
|---|---|---|---|---|---|---|---|---|---|
| M3 Ultra 512 GB | 2 | TB5 | 1 | 1 | 20.39 | 20.20 | 24.81 | 20.39 | yes |
| M3 Ultra 512 GB | 2 | TB5 | 8 | 16 | 4.61 | – | – | 73.73 | yes |
| M3 Ultra 512 GB | 2 | TB5 | 32 | 64 | 1.57 | – | – | 100.52 | yes |
| M3 Ultra 512 GB | 4 | TB5 | 1 | 1 | 20.34 | 19.95 | 24.76 | 20.34 | yes |
| M3 Ultra 512 GB | 4 | TB5 | 8 | 32 | 4.60 | – | – | 147.33 | yes |
| M3 Ultra 512 GB | 4 | TB5 | 32 | 128 | 1.57 | – | – | 200.94 | yes |
| M3 Ultra 512 GB | 2 | 10 GbE | 1 | 1 | 20.38 | 20.18 | 24.78 | 20.38 | yes |
| M3 Ultra 512 GB | 2 | 10 GbE | 8 | 16 | 4.60 | – | – | 73.61 | yes |
| M3 Ultra 512 GB | 2 | 10 GbE | 32 | 64 | 1.57 | – | – | 100.30 | yes |
| RTX 5090 32 GB | 26 | 25 GbE | 1 | 1 | 45.43 | 35.47 | 56.43 | 45.43 | yes |
| RTX 5090 32 GB | 26 | 25 GbE | 8 | 208 | 10.75 | – | – | 2,236.80 | yes |
| RTX 5090 32 GB | 26 | 25 GbE | 32 | 832 | 3.68 | – | – | 3,058.41 | yes |

**Swarm pipeline across homes (T2-batch), PROJECTION**. G = stages micro-batches of `depth`. Uplink modeled payload cap = uplink ÷ (bytes per position × 8), using a conservative envelope: activation 28,672 B + hashes 32 × (61 + stages) + stage headers 17 × stages + head logits/token 36 B + item fields 39 B + length-prefix/Step header 17 B per position. Early hops carry fewer commitments; the return hop has no activation. This is a model capacity estimate, not an exact wire trace; TLS/IP overhead is excluded. "Per-sequence reads" = the same with every sequence reading its own weights (this runtime today). KV fits = the MLA cache at 4096 tokens of context fits beside the device's share of the 582 GB of weights.

| devices | stages | one-way ms | uplink Mbit/s | depth | concurrent | per answer tok/s | aggregate tok/s | per-sequence reads | uplink ceiling tok/s | KV fits |
|---|---|---|---|---|---|---|---|---|---|---|
| RTX 5090 32 GB | 26 | 10 | 20 | 1 | 26 | 1.66 | 43.21 | 43.21 | 78.15 | yes |
| RTX 5090 32 GB | 26 | 10 | 20 | 4 | 104 | 0.63 | 65.32 | 64.13 | 78.15 | yes |
| RTX 5090 32 GB | 26 | 10 | 20 | 16 | 416 | 0.18 | 75.13 | 72.96 | 78.15 | yes |
| RTX 5090 32 GB | 26 | 10 | 20 | 64 | 1,664 | 0.05 | 78.15 | 75.56 | 78.15 | no (9.2 > 4.6 GB) |
| RTX 5090 32 GB | 26 | 10 | 100 | 1 | 26 | 2.89 | 75.20 | 75.20 | 390.75 | yes |
| RTX 5090 32 GB | 26 | 10 | 100 | 4 | 104 | 1.76 | 182.93 | 173.90 | 390.75 | yes |
| RTX 5090 32 GB | 26 | 10 | 100 | 16 | 416 | 0.69 | 288.49 | 258.82 | 390.75 | yes |
| RTX 5090 32 GB | 26 | 10 | 100 | 64 | 1,664 | 0.21 | 350.54 | 294.80 | 390.75 | no (9.2 > 4.6 GB) |
| RTX 5090 32 GB | 26 | 30 | 20 | 1 | 26 | 0.89 | 23.18 | 23.18 | 78.15 | yes |
| RTX 5090 32 GB | 26 | 30 | 20 | 4 | 104 | 0.47 | 49.24 | 48.56 | 78.15 | yes |
| RTX 5090 32 GB | 26 | 30 | 20 | 16 | 416 | 0.17 | 68.68 | 66.86 | 78.15 | yes |
| RTX 5090 32 GB | 26 | 30 | 20 | 64 | 1,664 | 0.05 | 76.87 | 73.81 | 78.15 | no (9.2 > 4.6 GB) |
| RTX 5090 32 GB | 26 | 30 | 100 | 1 | 26 | 1.16 | 30.03 | 30.03 | 390.75 | yes |
| RTX 5090 32 GB | 26 | 30 | 100 | 4 | 104 | 0.92 | 95.54 | 93.02 | 390.75 | yes |
| RTX 5090 32 GB | 26 | 30 | 100 | 16 | 416 | 0.51 | 212.03 | 195.55 | 390.75 | yes |
| RTX 5090 32 GB | 26 | 30 | 100 | 64 | 1,664 | 0.19 | 315.93 | 269.94 | 390.75 | no (9.2 > 4.6 GB) |
| RTX 5090 32 GB | 26 | 60 | 20 | 1 | 26 | 0.53 | 13.67 | 13.67 | 78.15 | yes |
| RTX 5090 32 GB | 26 | 60 | 20 | 4 | 104 | 0.35 | 35.96 | 35.60 | 78.15 | yes |
| RTX 5090 32 GB | 26 | 60 | 20 | 16 | 416 | 0.15 | 60.85 | 59.41 | 78.15 | yes |
| RTX 5090 32 GB | 26 | 60 | 20 | 64 | 1,664 | 0.04 | 74.20 | 71.34 | 78.15 | no (9.2 > 4.6 GB) |
| RTX 5090 32 GB | 26 | 60 | 100 | 1 | 26 | 0.61 | 15.80 | 15.80 | 390.75 | yes |
| RTX 5090 32 GB | 26 | 60 | 100 | 4 | 104 | 0.54 | 55.66 | 54.79 | 390.75 | yes |
| RTX 5090 32 GB | 26 | 60 | 100 | 16 | 416 | 0.36 | 151.71 | 143.09 | 390.75 | yes |
| RTX 5090 32 GB | 26 | 60 | 100 | 64 | 1,664 | 0.17 | 275.18 | 239.62 | 390.75 | no (9.2 > 4.6 GB) |
| Mac 64 GB (M4 Pro) | 12 | 10 | 20 | 1 | 12 | 2.47 | 29.69 | 29.69 | 79.86 | yes |
| Mac 64 GB (M4 Pro) | 12 | 10 | 20 | 4 | 48 | 0.96 | 46.11 | 38.26 | 79.86 | yes |
| Mac 64 GB (M4 Pro) | 12 | 10 | 20 | 16 | 192 | 0.29 | 55.50 | 41.23 | 79.86 | yes |
| Mac 64 GB (M4 Pro) | 12 | 10 | 20 | 64 | 768 | 0.08 | 65.28 | 42.05 | 79.86 | no (9.2 > 2.5 GB) |
| Mac 64 GB (M4 Pro) | 12 | 10 | 100 | 1 | 12 | 3.40 | 40.81 | 40.81 | 399.31 | yes |
| Mac 64 GB (M4 Pro) | 12 | 10 | 100 | 4 | 48 | 1.67 | 79.97 | 58.97 | 399.31 | yes |
| Mac 64 GB (M4 Pro) | 12 | 10 | 100 | 16 | 192 | 0.59 | 113.18 | 66.35 | 399.31 | yes |
| Mac 64 GB (M4 Pro) | 12 | 10 | 100 | 64 | 768 | 0.21 | 162.96 | 68.50 | 399.31 | no (9.2 > 2.5 GB) |
| Mac 64 GB (M4 Pro) | 12 | 30 | 20 | 1 | 12 | 1.55 | 18.63 | 18.63 | 79.86 | yes |
| Mac 64 GB (M4 Pro) | 12 | 30 | 20 | 4 | 48 | 0.78 | 37.47 | 32.11 | 79.86 | yes |
| Mac 64 GB (M4 Pro) | 12 | 30 | 20 | 16 | 192 | 0.27 | 51.90 | 39.21 | 79.86 | yes |
| Mac 64 GB (M4 Pro) | 12 | 30 | 20 | 64 | 768 | 0.08 | 63.97 | 41.50 | 79.86 | no (9.2 > 2.5 GB) |
| Mac 64 GB (M4 Pro) | 12 | 30 | 100 | 1 | 12 | 1.87 | 22.47 | 22.47 | 399.31 | yes |
| Mac 64 GB (M4 Pro) | 12 | 30 | 100 | 4 | 48 | 1.19 | 57.13 | 45.54 | 399.31 | yes |
| Mac 64 GB (M4 Pro) | 12 | 30 | 100 | 16 | 192 | 0.52 | 99.15 | 61.27 | 399.31 | yes |
| Mac 64 GB (M4 Pro) | 12 | 30 | 100 | 64 | 768 | 0.20 | 155.07 | 67.06 | 399.31 | no (9.2 > 2.5 GB) |
| Mac 64 GB (M4 Pro) | 12 | 60 | 20 | 1 | 12 | 1.00 | 11.95 | 11.95 | 79.86 | yes |
| Mac 64 GB (M4 Pro) | 12 | 60 | 20 | 4 | 48 | 0.61 | 29.25 | 25.88 | 79.86 | yes |
| Mac 64 GB (M4 Pro) | 12 | 60 | 20 | 16 | 192 | 0.25 | 47.30 | 36.53 | 79.86 | yes |
| Mac 64 GB (M4 Pro) | 12 | 60 | 20 | 64 | 768 | 0.08 | 62.11 | 40.71 | 79.86 | no (9.2 > 2.5 GB) |
| Mac 64 GB (M4 Pro) | 12 | 60 | 100 | 1 | 12 | 1.12 | 13.42 | 13.42 | 399.31 | yes |
| Mac 64 GB (M4 Pro) | 12 | 60 | 100 | 4 | 48 | 0.83 | 39.99 | 33.95 | 399.31 | yes |
| Mac 64 GB (M4 Pro) | 12 | 60 | 100 | 16 | 192 | 0.44 | 83.61 | 54.96 | 399.31 | yes |
| Mac 64 GB (M4 Pro) | 12 | 60 | 100 | 64 | 768 | 0.19 | 144.56 | 65.02 | 399.31 | no (9.2 > 2.5 GB) |

**Context sensitivity, PROJECTION**: the most aggregate one swarm pipeline reaches before its KV cache runs out, by tokens of context held per sequence (agentic traffic is prompt-heavy, so 4k is optimistic).

| devices | stages | one-way ms | uplink Mbit/s | 4k context | 8k context | 32k context |
|---|---|---|---|---|---|---|
| RTX 5090 32 GB | 26 | 10 | 20 | 77 (832 seqs, 0.09/answer) | 75 (416 seqs, 0.18/answer) | 65 (104 seqs, 0.63/answer) |
| RTX 5090 32 GB | 26 | 10 | 100 | 324 (832 seqs, 0.39/answer) | 288 (416 seqs, 0.69/answer) | 183 (104 seqs, 1.76/answer) |
| RTX 5090 32 GB | 26 | 30 | 20 | 74 (832 seqs, 0.09/answer) | 69 (416 seqs, 0.17/answer) | 49 (104 seqs, 0.47/answer) |
| RTX 5090 32 GB | 26 | 30 | 100 | 269 (832 seqs, 0.32/answer) | 212 (416 seqs, 0.51/answer) | 96 (104 seqs, 0.92/answer) |
| RTX 5090 32 GB | 26 | 60 | 20 | 69 (832 seqs, 0.08/answer) | 61 (416 seqs, 0.15/answer) | 36 (104 seqs, 0.35/answer) |
| RTX 5090 32 GB | 26 | 60 | 100 | 215 (832 seqs, 0.26/answer) | 152 (416 seqs, 0.36/answer) | 56 (104 seqs, 0.54/answer) |
| Mac 64 GB (M4 Pro) | 12 | 10 | 20 | 56 (204 seqs, 0.27/answer) | 51 (96 seqs, 0.54/answer) | 39 (24 seqs, 1.62/answer) |
| Mac 64 GB (M4 Pro) | 12 | 10 | 100 | 115 (204 seqs, 0.56/answer) | 97 (96 seqs, 1.01/answer) | 60 (24 seqs, 2.51/answer) |
| Mac 64 GB (M4 Pro) | 12 | 30 | 20 | 52 (204 seqs, 0.26/answer) | 46 (96 seqs, 0.47/answer) | 28 (24 seqs, 1.17/answer) |
| Mac 64 GB (M4 Pro) | 12 | 30 | 100 | 101 (204 seqs, 0.50/answer) | 78 (96 seqs, 0.81/answer) | 38 (24 seqs, 1.57/answer) |
| Mac 64 GB (M4 Pro) | 12 | 60 | 20 | 48 (204 seqs, 0.24/answer) | 39 (96 seqs, 0.41/answer) | 20 (24 seqs, 0.82/answer) |
| Mac 64 GB (M4 Pro) | 12 | 60 | 100 | 86 (204 seqs, 0.42/answer) | 60 (96 seqs, 0.63/answer) | 24 (24 seqs, 1.00/answer) |

**Curve: what one swarm pipeline needs to reach an aggregate, PROJECTION** — the smallest micro-batch depth (G = stages micro-batches) whose projected aggregate reaches the target with the KV cache fitting (4096 tokens of context, assumed), and the per-answer speed at that point. Upper bounds: the sequence counts are lower bounds (see the model error above), and batched weight reads are assumed. Two stage counts per device class: the memory minimum (with KV headroom) and about twice that. More stages give more KV room, so more sequences can be in flight, but each answer is slower and the uplink ceiling stays the same: every stage's uplink carries every token.

| devices | stages | one-way ms | uplink Mbit/s | target tok/s | depth | concurrent | per answer tok/s | aggregate tok/s |
|---|---|---|---|---|---|---|---|---|
| RTX 5090 32 GB | 26 | 10 | 20 | 50 | 2 | 52 | 1.07 | 55.79 |
| RTX 5090 32 GB | 26 | 10 | 20 | 100 | – | – | – | no: uplink ceiling 78 |
| RTX 5090 32 GB | 26 | 10 | 20 | 200 | – | – | – | no: uplink ceiling 78 |
| RTX 5090 32 GB | 26 | 10 | 20 | 300 | – | – | – | no: uplink ceiling 78 |
| RTX 5090 32 GB | 26 | 10 | 20 | 400 | – | – | – | no: uplink ceiling 78 |
| RTX 5090 32 GB | 26 | 10 | 100 | 50 | 1 | 26 | 2.89 | 75.20 |
| RTX 5090 32 GB | 26 | 10 | 100 | 100 | 2 | 52 | 2.38 | 123.75 |
| RTX 5090 32 GB | 26 | 10 | 100 | 200 | 5 | 130 | 1.56 | 202.37 |
| RTX 5090 32 GB | 26 | 10 | 100 | 300 | 20 | 520 | 0.58 | 300.99 |
| RTX 5090 32 GB | 26 | 10 | 100 | 400 | – | – | – | no: uplink ceiling 391 |
| RTX 5090 32 GB | 26 | 30 | 20 | 50 | 5 | 130 | 0.41 | 53.24 |
| RTX 5090 32 GB | 26 | 30 | 20 | 100 | – | – | – | no: uplink ceiling 78 |
| RTX 5090 32 GB | 26 | 30 | 20 | 200 | – | – | – | no: uplink ceiling 78 |
| RTX 5090 32 GB | 26 | 30 | 20 | 300 | – | – | – | no: uplink ceiling 78 |
| RTX 5090 32 GB | 26 | 30 | 20 | 400 | – | – | – | no: uplink ceiling 78 |
| RTX 5090 32 GB | 26 | 30 | 100 | 50 | 2 | 52 | 1.06 | 55.31 |
| RTX 5090 32 GB | 26 | 30 | 100 | 100 | 5 | 130 | 0.86 | 111.84 |
| RTX 5090 32 GB | 26 | 30 | 100 | 200 | 14 | 364 | 0.55 | 200.19 |
| RTX 5090 32 GB | 26 | 30 | 100 | 300 | – | – | – | no: KV memory runs out first |
| RTX 5090 32 GB | 26 | 30 | 100 | 400 | – | – | – | no: uplink ceiling 391 |
| RTX 5090 32 GB | 26 | 60 | 20 | 50 | 9 | 234 | 0.22 | 51.55 |
| RTX 5090 32 GB | 26 | 60 | 20 | 100 | – | – | – | no: uplink ceiling 78 |
| RTX 5090 32 GB | 26 | 60 | 20 | 200 | – | – | – | no: uplink ceiling 78 |
| RTX 5090 32 GB | 26 | 60 | 20 | 300 | – | – | – | no: uplink ceiling 78 |
| RTX 5090 32 GB | 26 | 60 | 20 | 400 | – | – | – | no: uplink ceiling 78 |
| RTX 5090 32 GB | 26 | 60 | 100 | 50 | 4 | 104 | 0.54 | 55.66 |
| RTX 5090 32 GB | 26 | 60 | 100 | 100 | 9 | 234 | 0.45 | 104.66 |
| RTX 5090 32 GB | 26 | 60 | 100 | 200 | 28 | 728 | 0.28 | 202.68 |
| RTX 5090 32 GB | 26 | 60 | 100 | 300 | – | – | – | no: KV memory runs out first |
| RTX 5090 32 GB | 26 | 60 | 100 | 400 | – | – | – | no: uplink ceiling 391 |
| RTX 5090 32 GB | 44 | 10 | 20 | 50 | 2 | 88 | 0.62 | 54.86 |
| RTX 5090 32 GB | 44 | 10 | 20 | 100 | – | – | – | no: uplink ceiling 76 |
| RTX 5090 32 GB | 44 | 10 | 20 | 200 | – | – | – | no: uplink ceiling 76 |
| RTX 5090 32 GB | 44 | 10 | 20 | 300 | – | – | – | no: uplink ceiling 76 |
| RTX 5090 32 GB | 44 | 10 | 20 | 400 | – | – | – | no: uplink ceiling 76 |
| RTX 5090 32 GB | 44 | 10 | 100 | 50 | 1 | 44 | 1.74 | 76.40 |
| RTX 5090 32 GB | 44 | 10 | 100 | 100 | 2 | 88 | 1.43 | 125.80 |
| RTX 5090 32 GB | 44 | 10 | 100 | 200 | 5 | 220 | 0.94 | 205.80 |
| RTX 5090 32 GB | 44 | 10 | 100 | 300 | 19 | 836 | 0.36 | 301.75 |
| RTX 5090 32 GB | 44 | 10 | 100 | 400 | – | – | – | no: uplink ceiling 380 |
| RTX 5090 32 GB | 44 | 30 | 20 | 50 | 5 | 220 | 0.24 | 52.25 |
| RTX 5090 32 GB | 44 | 30 | 20 | 100 | – | – | – | no: uplink ceiling 76 |
| RTX 5090 32 GB | 44 | 30 | 20 | 200 | – | – | – | no: uplink ceiling 76 |
| RTX 5090 32 GB | 44 | 30 | 20 | 300 | – | – | – | no: uplink ceiling 76 |
| RTX 5090 32 GB | 44 | 30 | 20 | 400 | – | – | – | no: uplink ceiling 76 |
| RTX 5090 32 GB | 44 | 30 | 100 | 50 | 2 | 88 | 0.63 | 55.71 |
| RTX 5090 32 GB | 44 | 30 | 100 | 100 | 5 | 220 | 0.51 | 112.88 |
| RTX 5090 32 GB | 44 | 30 | 100 | 200 | 14 | 616 | 0.33 | 202.18 |
| RTX 5090 32 GB | 44 | 30 | 100 | 300 | 50 | 2,200 | 0.14 | 300.49 |
| RTX 5090 32 GB | 44 | 30 | 100 | 400 | – | – | – | no: uplink ceiling 380 |
| RTX 5090 32 GB | 44 | 60 | 20 | 50 | 9 | 396 | 0.13 | 50.58 |
| RTX 5090 32 GB | 44 | 60 | 20 | 100 | – | – | – | no: uplink ceiling 76 |
| RTX 5090 32 GB | 44 | 60 | 20 | 200 | – | – | – | no: uplink ceiling 76 |
| RTX 5090 32 GB | 44 | 60 | 20 | 300 | – | – | – | no: uplink ceiling 76 |
| RTX 5090 32 GB | 44 | 60 | 20 | 400 | – | – | – | no: uplink ceiling 76 |
| RTX 5090 32 GB | 44 | 60 | 100 | 50 | 4 | 176 | 0.32 | 55.94 |
| RTX 5090 32 GB | 44 | 60 | 100 | 100 | 9 | 396 | 0.27 | 105.35 |
| RTX 5090 32 GB | 44 | 60 | 100 | 200 | 27 | 1,188 | 0.17 | 200.38 |
| RTX 5090 32 GB | 44 | 60 | 100 | 300 | 93 | 4,092 | 0.07 | 300.64 |
| RTX 5090 32 GB | 44 | 60 | 100 | 400 | – | – | – | no: uplink ceiling 380 |
| Mac 64 GB (M4 Pro) | 12 | 10 | 20 | 50 | 7 | 84 | 0.60 | 50.48 |
| Mac 64 GB (M4 Pro) | 12 | 10 | 20 | 100 | – | – | – | no: uplink ceiling 80 |
| Mac 64 GB (M4 Pro) | 12 | 10 | 20 | 200 | – | – | – | no: uplink ceiling 80 |
| Mac 64 GB (M4 Pro) | 12 | 10 | 20 | 300 | – | – | – | no: uplink ceiling 80 |
| Mac 64 GB (M4 Pro) | 12 | 10 | 20 | 400 | – | – | – | no: uplink ceiling 80 |
| Mac 64 GB (M4 Pro) | 12 | 10 | 100 | 50 | 2 | 24 | 2.51 | 60.35 |
| Mac 64 GB (M4 Pro) | 12 | 10 | 100 | 100 | 10 | 120 | 0.85 | 102.35 |
| Mac 64 GB (M4 Pro) | 12 | 10 | 100 | 200 | – | – | – | no: KV memory runs out first |
| Mac 64 GB (M4 Pro) | 12 | 10 | 100 | 300 | – | – | – | no: KV memory runs out first |
| Mac 64 GB (M4 Pro) | 12 | 10 | 100 | 400 | – | – | – | no: uplink ceiling 399 |
| Mac 64 GB (M4 Pro) | 12 | 30 | 20 | 50 | 13 | 156 | 0.32 | 50.12 |
| Mac 64 GB (M4 Pro) | 12 | 30 | 20 | 100 | – | – | – | no: uplink ceiling 80 |
| Mac 64 GB (M4 Pro) | 12 | 30 | 20 | 200 | – | – | – | no: uplink ceiling 80 |
| Mac 64 GB (M4 Pro) | 12 | 30 | 20 | 300 | – | – | – | no: uplink ceiling 80 |
| Mac 64 GB (M4 Pro) | 12 | 30 | 20 | 400 | – | – | – | no: uplink ceiling 80 |
| Mac 64 GB (M4 Pro) | 12 | 30 | 100 | 50 | 4 | 48 | 1.19 | 57.13 |
| Mac 64 GB (M4 Pro) | 12 | 30 | 100 | 100 | 17 | 204 | 0.50 | 101.02 |
| Mac 64 GB (M4 Pro) | 12 | 30 | 100 | 200 | – | – | – | no: KV memory runs out first |
| Mac 64 GB (M4 Pro) | 12 | 30 | 100 | 300 | – | – | – | no: KV memory runs out first |
| Mac 64 GB (M4 Pro) | 12 | 30 | 100 | 400 | – | – | – | no: uplink ceiling 399 |
| Mac 64 GB (M4 Pro) | 12 | 60 | 20 | 50 | – | – | – | no: KV memory runs out first |
| Mac 64 GB (M4 Pro) | 12 | 60 | 20 | 100 | – | – | – | no: uplink ceiling 80 |
| Mac 64 GB (M4 Pro) | 12 | 60 | 20 | 200 | – | – | – | no: uplink ceiling 80 |
| Mac 64 GB (M4 Pro) | 12 | 60 | 20 | 300 | – | – | – | no: uplink ceiling 80 |
| Mac 64 GB (M4 Pro) | 12 | 60 | 20 | 400 | – | – | – | no: uplink ceiling 80 |
| Mac 64 GB (M4 Pro) | 12 | 60 | 100 | 50 | 6 | 72 | 0.72 | 51.55 |
| Mac 64 GB (M4 Pro) | 12 | 60 | 100 | 100 | – | – | – | no: KV memory runs out first |
| Mac 64 GB (M4 Pro) | 12 | 60 | 100 | 200 | – | – | – | no: KV memory runs out first |
| Mac 64 GB (M4 Pro) | 12 | 60 | 100 | 300 | – | – | – | no: KV memory runs out first |
| Mac 64 GB (M4 Pro) | 12 | 60 | 100 | 400 | – | – | – | no: uplink ceiling 399 |
| Mac 64 GB (M4 Pro) | 24 | 10 | 20 | 50 | 3 | 72 | 0.71 | 50.77 |
| Mac 64 GB (M4 Pro) | 24 | 10 | 20 | 100 | – | – | – | no: uplink ceiling 78 |
| Mac 64 GB (M4 Pro) | 24 | 10 | 20 | 200 | – | – | – | no: uplink ceiling 78 |
| Mac 64 GB (M4 Pro) | 24 | 10 | 20 | 300 | – | – | – | no: uplink ceiling 78 |
| Mac 64 GB (M4 Pro) | 24 | 10 | 20 | 400 | – | – | – | no: uplink ceiling 78 |
| Mac 64 GB (M4 Pro) | 24 | 10 | 100 | 50 | 1 | 24 | 2.24 | 53.82 |
| Mac 64 GB (M4 Pro) | 24 | 10 | 100 | 100 | 3 | 72 | 1.40 | 100.85 |
| Mac 64 GB (M4 Pro) | 24 | 10 | 100 | 200 | 40 | 960 | 0.21 | 200.83 |
| Mac 64 GB (M4 Pro) | 24 | 10 | 100 | 300 | – | – | – | no: KV memory runs out first |
| Mac 64 GB (M4 Pro) | 24 | 10 | 100 | 400 | – | – | – | no: uplink ceiling 392 |
| Mac 64 GB (M4 Pro) | 24 | 30 | 20 | 50 | 7 | 168 | 0.30 | 50.08 |
| Mac 64 GB (M4 Pro) | 24 | 30 | 20 | 100 | – | – | – | no: uplink ceiling 78 |
| Mac 64 GB (M4 Pro) | 24 | 30 | 20 | 200 | – | – | – | no: uplink ceiling 78 |
| Mac 64 GB (M4 Pro) | 24 | 30 | 20 | 300 | – | – | – | no: uplink ceiling 78 |
| Mac 64 GB (M4 Pro) | 24 | 30 | 20 | 400 | – | – | – | no: uplink ceiling 78 |
| Mac 64 GB (M4 Pro) | 24 | 30 | 100 | 50 | 3 | 72 | 0.84 | 60.31 |
| Mac 64 GB (M4 Pro) | 24 | 30 | 100 | 100 | 8 | 192 | 0.54 | 104.51 |
| Mac 64 GB (M4 Pro) | 24 | 30 | 100 | 200 | 55 | 1,320 | 0.15 | 200.74 |
| Mac 64 GB (M4 Pro) | 24 | 30 | 100 | 300 | – | – | – | no: KV memory runs out first |
| Mac 64 GB (M4 Pro) | 24 | 30 | 100 | 400 | – | – | – | no: uplink ceiling 392 |
| Mac 64 GB (M4 Pro) | 24 | 60 | 20 | 50 | 13 | 312 | 0.16 | 50.18 |
| Mac 64 GB (M4 Pro) | 24 | 60 | 20 | 100 | – | – | – | no: uplink ceiling 78 |
| Mac 64 GB (M4 Pro) | 24 | 60 | 20 | 200 | – | – | – | no: uplink ceiling 78 |
| Mac 64 GB (M4 Pro) | 24 | 60 | 20 | 300 | – | – | – | no: uplink ceiling 78 |
| Mac 64 GB (M4 Pro) | 24 | 60 | 20 | 400 | – | – | – | no: uplink ceiling 78 |
| Mac 64 GB (M4 Pro) | 24 | 60 | 100 | 50 | 5 | 120 | 0.46 | 55.16 |
| Mac 64 GB (M4 Pro) | 24 | 60 | 100 | 100 | 14 | 336 | 0.30 | 102.33 |
| Mac 64 GB (M4 Pro) | 24 | 60 | 100 | 200 | 73 | 1,752 | 0.11 | 200.88 |
| Mac 64 GB (M4 Pro) | 24 | 60 | 100 | 300 | – | – | – | no: KV memory runs out first |
| Mac 64 GB (M4 Pro) | 24 | 60 | 100 | 400 | – | – | – | no: uplink ceiling 392 |

**One billion tokens a day (11,574 tok/s sustained), swarm tier, PROJECTION, lower bound on devices** (the deepest micro-batch depth whose KV fits at 4096 tokens; batched weight reads; decode tokens only, no prefill; 100% utilisation):

| devices | stages | one-way ms | uplink Mbit/s | depth | concurrent | per answer tok/s | aggregate per pipeline | pipelines | devices |
|---|---|---|---|---|---|---|---|---|---|
| RTX 5090 32 GB | 26 | 10 | 20 | 16 | 416 | 0.18 | 75.13 | 155 | 4,030 |
| RTX 5090 32 GB | 26 | 10 | 100 | 16 | 416 | 0.69 | 288.49 | 41 | 1,066 |
| RTX 5090 32 GB | 26 | 30 | 20 | 16 | 416 | 0.17 | 68.68 | 169 | 4,394 |
| RTX 5090 32 GB | 26 | 30 | 100 | 16 | 416 | 0.51 | 212.03 | 55 | 1,430 |
| RTX 5090 32 GB | 26 | 60 | 20 | 16 | 416 | 0.15 | 60.85 | 191 | 4,966 |
| RTX 5090 32 GB | 26 | 60 | 100 | 16 | 416 | 0.36 | 151.71 | 77 | 2,002 |
| Mac 64 GB (M4 Pro) | 12 | 10 | 20 | 16 | 192 | 0.29 | 55.50 | 209 | 2,508 |
| Mac 64 GB (M4 Pro) | 12 | 10 | 100 | 16 | 192 | 0.59 | 113.18 | 103 | 1,236 |
| Mac 64 GB (M4 Pro) | 12 | 30 | 20 | 16 | 192 | 0.27 | 51.90 | 224 | 2,688 |
| Mac 64 GB (M4 Pro) | 12 | 30 | 100 | 16 | 192 | 0.52 | 99.15 | 117 | 1,404 |
| Mac 64 GB (M4 Pro) | 12 | 60 | 20 | 16 | 192 | 0.25 | 47.30 | 245 | 2,940 |
| Mac 64 GB (M4 Pro) | 12 | 60 | 100 | 16 | 192 | 0.44 | 83.61 | 139 | 1,668 |
