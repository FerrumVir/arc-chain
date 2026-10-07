# Island runtime: measured results and the Kimi K2 projection

Measured on **Studio lab** (Mac Studio, Apple M2 Ultra, 24 cores, macOS; 7 Oct 2026)
with `arc-island bench --kernel simd --threads 4 --stage-threads 4`; the CI runner
numbers are produced on every pull-request run by `.github/workflows/island-runtime.yml`
(job "bench", artifact `island-bench-ci`, also printed in the job summary); the copy in
`data/ci-small.json` is from run 37621860068 (GitHub ubuntu-latest, 4 vCPU). Raw data:
[`data/`](data/). Regenerate this report with
`python3 scripts/arc_island/report.py docs/island/data/studio-kimi-mini.json docs/island/data/studio-small.json docs/island/data/ci-small.json`.

All numbers are from synthetic MLA + MoE models (the Kimi K2 text architecture at
small width: `kimi-mini` 8 layers, d 512, 64 experts top-8, INT4 g32 experts; `small`
8 layers, d 256). Every stage process on one host, joined by loopback TCP; the WAN rows
emulate each hop in the sending process (delay line + bounded uplink). **No real Kimi
weights, no real LAN, Thunderbolt or internet link was used.** Everything under
"Projection" is arithmetic, not a measurement.

## Reading

1. **Bit-exact everywhere.** Every run (1, 2, 4 and 8 stage processes; 1 to 128
   concurrent sequences; every WAN profile) produced the single process's tokens, every
   logits hash and the hash at every layer boundary of every position.
2. **Hop cost (Studio lab, loopback TCP): about 17–19 µs per hop** for a 28 KiB frame
   (a Kimi K2 boundary at i32), about 13–14 µs for 2–14 KiB. Zero-byte pings read about
   30 µs: they run first on a freshly started ring and include wake-up time. This is the
   runtime's own software cost; a real NIC adds wire time and driver latency, which was
   not measured. On the CI runner (4 vCPU) the same hop costs 36–39 µs; the projection uses
   the larger.
3. **The pipeline model holds.** Fed with the measured compute per position and hop cost,
   research-6's round-time model predicts the emulated-WAN per-answer speed with a median
   error of 3.9% over 54 runs (1.9% on the CI runner) (2/4/8 stages × 10/30/60 ms × 20/100 Mbit/s × depth 1/4/16).
4. **The swarm tier is uplink-bound.** Exact boundaries are i32 (every boundary of both
   synthetic models fits i32; none fits i16), so a Kimi K2 token puts 28 KiB on every
   stage's uplink. A pipeline therefore cannot exceed about **87 tok/s aggregate at
   20 Mbit/s uplinks or 436 tok/s at 100 Mbit/s**, whatever the schedule; per answer it
   runs below 1 tok/s near those ceilings (projection table). Measured with Kimi-sized
   frames on the emulated WAN: 8 stages, 10 ms, depth 16 (128 sequences) gave 54 tok/s
   aggregate at 20 Mbit/s and 205 tok/s at 100 Mbit/s (prefill included).
5. **One billion tokens a day on the swarm tier** (11.6k tok/s sustained) projects to about
   1,000 RTX 5090-class devices on 100 Mbit/s uplinks, or 3,800 on 20 Mbit/s, at 100%
   utilisation (decode only). ~130 community nodes would give a few hundred to ~1.5k tok/s
   at best, if they had the GPUs. Lossless activation compression (or exact INT8
   boundaries in a future profile) moves this ceiling directly.
6. **Islands.** Pipeline parallelism does not speed up one answer: 2–4 M3 Ultras project to
   about 20 tok/s per answer (memory-bound, as research-6 §2.5), with aggregate 74–147 tok/s
   at 8 sequences per stage. A 26× RTX 5090 LAN pipeline projects to about 47 tok/s per answer, about
   63 with one exact speculative draft, and 2.3k tok/s aggregate at depth 8. ≥59 tok/s per
   answer needs either that GPU island with speculation or tensor/expert parallelism over
   RDMA. Both assume kernels at research-6's calibrated memory bandwidths.
7. **The curve, read the other way** (tables "Curve, measured" and "Curve: what one swarm
   pipeline needs"). With 26 RTX 5090-class stages, 10 ms per hop and 100 Mbit/s uplinks, the
   projection reaches 100 tok/s aggregate with 52 sequences in flight at 2.4 tok/s per answer,
   200 tok/s with 130 at 1.6, and 300 tok/s with 416 at 0.7; 400 tok/s is not reachable before
   the KV cache runs out. At 60 ms per hop the same pipeline needs 234 sequences for 100 tok/s
   (0.46 tok/s per answer). At 20 Mbit/s nothing passes 87 tok/s. Doubling the stage count adds
   KV room (more sequences in flight) but not throughput past the uplink ceiling, and every
   answer gets slower.
8. **The engine, not the network, is the gap.** ARC's integer engine reads its active weights
   at 2.2–2.7 GB/s on the Studio (4 threads, SIMD kernel) against the 456 GB/s the M3 Ultra
   projection assumes. The island runtime adds microseconds per hop; the per-token compute
   is two orders of magnitude from the projection's assumption.

## Measured

### Studio lab: macos aarch64, 24 logical CPUs

Model: synthetic MLA + MoE, 8 layers, d_model 512, 64 routed experts top-8, profile `arc.hf-deepseek-v3.mla-moe.i8-dyadic-row.i4g32-experts.q16.v1`; package blake3 `b1bbd3fa1d524d46…`. WAN delay line: spin.

Single process, no network: 144.19 tok/s decode (6.95 ms per position); engine effective weight bandwidth 2.178 GB/s (15.11 MB of active weights per token).

Boundary activations on the wire (lossless width): {'i16': 0, 'i32': 420, 'i64': 0, 'i8': 0} over 420 vectors; mean 2,048 B per vector vs 4096 B as i64.

**Hop latency** (ring round trip of a ping frame through every stage process, no compute; loopback TCP):

| stages | payload B | hops | ring median ms | ring p90 ms | per hop µs |
|---|---|---|---|---|---|
| 1 | 0 | 2 | 0.09 | 0.11 | 43.31 |
| 1 | 4,096 | 2 | 0.08 | 0.09 | 38.21 |
| 1 | 14,336 | 2 | 0.03 | 0.06 | 13.83 |
| 1 | 28,672 | 2 | 0.04 | 0.06 | 20.79 |
| 1 | 57,344 | 2 | 0.06 | 0.09 | 31.31 |
| 2 | 0 | 3 | 0.09 | 0.10 | 29.17 |
| 2 | 4,096 | 3 | 0.04 | 0.09 | 13.36 |
| 2 | 14,336 | 3 | 0.04 | 0.06 | 14.11 |
| 2 | 28,672 | 3 | 0.06 | 0.07 | 18.58 |
| 2 | 57,344 | 3 | 0.08 | 0.10 | 25.22 |
| 4 | 0 | 5 | 0.15 | 0.16 | 29.28 |
| 4 | 4,096 | 5 | 0.06 | 0.08 | 12.85 |
| 4 | 14,336 | 5 | 0.07 | 0.08 | 13.58 |
| 4 | 28,672 | 5 | 0.08 | 0.11 | 16.40 |
| 4 | 57,344 | 5 | 0.13 | 0.15 | 26.12 |

**Throughput, stage processes on one host** (decode tok/s per answer, mean; aggregate = generated / wall time):

| stages | G | B | tokens | per answer tok/s | aggregate tok/s | bit-exact |
|---|---|---|---|---|---|---|
| 1 | 1 | 1 | 96 | 133.02 | 107.10 | yes |
| 1 | 2 | 8 | 96 | 18.00 | 120.58 | yes |
| 2 | 1 | 1 | 96 | 152.87 | 122.26 | yes |
| 2 | 2 | 2 | 96 | 101.93 | 165.51 | yes |
| 2 | 2 | 8 | 96 | 25.54 | 161.24 | yes |
| 4 | 1 | 1 | 96 | 144.83 | 116.21 | yes |
| 4 | 4 | 4 | 96 | 62.16 | 201.51 | yes |
| 4 | 4 | 8 | 96 | 31.25 | 201.54 | yes |

### Studio lab: macos aarch64, 24 logical CPUs

Model: synthetic MLA + MoE, 8 layers, d_model 256, 16 routed experts top-4, profile `arc.hf-deepseek-v3.mla-moe.i8-dyadic-row.q16.v1`; package blake3 `59cccff13d80759a…`. WAN delay line: spin.

Single process, no network: 472.81 tok/s decode (2.10 ms per position); engine effective weight bandwidth 2.661 GB/s (5.63 MB of active weights per token).

Boundary activations on the wire (lossless width): {'i16': 0, 'i32': 532, 'i64': 0, 'i8': 0} over 532 vectors; mean 1,024 B per vector vs 2048 B as i64.

**Hop latency** (ring round trip of a ping frame through every stage process, no compute; loopback TCP):

| stages | payload B | hops | ring median ms | ring p90 ms | per hop µs |
|---|---|---|---|---|---|
| 1 | 0 | 2 | 0.06 | 0.08 | 28.62 |
| 1 | 2,048 | 2 | 0.06 | 0.07 | 29.40 |
| 1 | 14,336 | 2 | 0.03 | 0.04 | 13.85 |
| 1 | 28,672 | 2 | 0.03 | 0.04 | 16.71 |
| 1 | 57,344 | 2 | 0.05 | 0.07 | 25.65 |
| 2 | 0 | 3 | 0.10 | 0.11 | 32.24 |
| 2 | 2,048 | 3 | 0.04 | 0.10 | 13.90 |
| 2 | 14,336 | 3 | 0.04 | 0.05 | 13.79 |
| 2 | 28,672 | 3 | 0.06 | 0.07 | 18.72 |
| 2 | 57,344 | 3 | 0.08 | 0.10 | 27.44 |
| 4 | 0 | 5 | 0.16 | 0.17 | 31.78 |
| 4 | 2,048 | 5 | 0.06 | 0.07 | 12.79 |
| 4 | 14,336 | 5 | 0.07 | 0.09 | 13.55 |
| 4 | 28,672 | 5 | 0.09 | 0.11 | 17.84 |
| 4 | 57,344 | 5 | 0.13 | 0.17 | 26.98 |

**Throughput, stage processes on one host** (decode tok/s per answer, mean; aggregate = generated / wall time):

| stages | G | B | tokens | per answer tok/s | aggregate tok/s | bit-exact |
|---|---|---|---|---|---|---|
| 1 | 1 | 1 | 128 | 490.76 | 414.24 | yes |
| 1 | 2 | 8 | 128 | 54.92 | 392.25 | yes |
| 2 | 1 | 1 | 128 | 431.73 | 365.68 | yes |
| 2 | 2 | 2 | 128 | 274.47 | 473.00 | yes |
| 2 | 2 | 8 | 128 | 73.13 | 497.14 | yes |
| 4 | 1 | 1 | 128 | 424.46 | 361.96 | yes |
| 4 | 4 | 4 | 128 | 136.54 | 471.87 | yes |
| 4 | 4 | 8 | 128 | 73.76 | 507.86 | yes |

**Emulated WAN, Studio lab** (every stage's uplink shaped: one-way delay ± 10% jitter, bounded uplink; 28672 B per position on the wire = a Kimi K2 boundary at i32; G = stages micro-batches of `depth` sequences). Predicted = research-6 §2.6 round-time model fed with this run's measured compute per position and hop overhead. Median |error| per answer: 3.9%.

| stages | one-way ms | uplink Mbit/s | depth | B | per answer tok/s | predicted | aggregate tok/s | predicted | bit-exact |
|---|---|---|---|---|---|---|---|---|---|
| 2 | 10.00 | 20.00 | 1 | 2 | 21.62 | 22.18 | 34.62 | 44.37 | yes |
| 2 | 10.00 | 20.00 | 4 | 8 | 7.99 | 8.32 | 50.34 | 66.56 | yes |
| 2 | 10.00 | 20.00 | 16 | 32 | 2.24 | 2.38 | 56.62 | 76.07 | yes |
| 2 | 10.00 | 100.00 | 1 | 2 | 35.64 | 37.42 | 59.38 | 74.83 | yes |
| 2 | 10.00 | 100.00 | 4 | 8 | 19.75 | 21.37 | 129.66 | 170.95 | yes |
| 2 | 10.00 | 100.00 | 16 | 32 | 6.87 | 7.87 | 180.47 | 251.82 | yes |
| 2 | 30.00 | 20.00 | 1 | 2 | 11.82 | 11.75 | 19.59 | 23.51 | yes |
| 2 | 30.00 | 20.00 | 4 | 8 | 6.25 | 6.24 | 39.58 | 49.94 | yes |
| 2 | 30.00 | 20.00 | 16 | 32 | 2.09 | 2.17 | 52.39 | 69.46 | yes |
| 2 | 30.00 | 100.00 | 1 | 2 | 14.62 | 14.99 | 24.87 | 29.97 | yes |
| 2 | 30.00 | 100.00 | 4 | 8 | 11.05 | 11.52 | 73.43 | 92.17 | yes |
| 2 | 30.00 | 100.00 | 16 | 32 | 5.28 | 5.99 | 138.82 | 191.53 | yes |
| 2 | 60.00 | 20.00 | 1 | 2 | 6.79 | 6.89 | 10.84 | 13.79 | yes |
| 2 | 60.00 | 20.00 | 4 | 8 | 4.51 | 4.54 | 27.73 | 36.33 | yes |
| 2 | 60.00 | 20.00 | 16 | 32 | 1.86 | 1.92 | 46.66 | 61.46 | yes |
| 2 | 60.00 | 100.00 | 1 | 2 | 7.72 | 7.89 | 13.14 | 15.78 | yes |
| 2 | 60.00 | 100.00 | 4 | 8 | 6.73 | 6.81 | 44.32 | 54.50 | yes |
| 2 | 60.00 | 100.00 | 16 | 32 | 4.09 | 4.40 | 108.48 | 140.92 | yes |
| 4 | 10.00 | 20.00 | 1 | 4 | 11.04 | 11.36 | 34.23 | 45.43 | yes |
| 4 | 10.00 | 20.00 | 4 | 16 | 4.11 | 4.31 | 49.63 | 68.97 | yes |
| 4 | 10.00 | 20.00 | 16 | 64 | 1.15 | 1.24 | 55.71 | 79.24 | yes |
| 4 | 10.00 | 100.00 | 1 | 4 | 18.93 | 19.47 | 62.64 | 77.90 | yes |
| 4 | 10.00 | 100.00 | 4 | 16 | 10.66 | 11.74 | 135.73 | 187.83 | yes |
| 4 | 10.00 | 100.00 | 16 | 64 | 3.91 | 4.53 | 194.50 | 290.23 | yes |
| 4 | 30.00 | 20.00 | 1 | 4 | 5.91 | 5.95 | 19.10 | 23.80 | yes |
| 4 | 30.00 | 20.00 | 4 | 16 | 3.15 | 3.21 | 38.62 | 51.28 | yes |
| 4 | 30.00 | 20.00 | 16 | 64 | 1.06 | 1.13 | 51.41 | 72.10 | yes |
| 4 | 30.00 | 100.00 | 1 | 4 | 7.46 | 7.61 | 24.89 | 30.45 | yes |
| 4 | 30.00 | 100.00 | 4 | 16 | 5.82 | 6.05 | 76.90 | 96.86 | yes |
| 4 | 30.00 | 100.00 | 16 | 64 | 2.95 | 3.33 | 151.23 | 212.97 | yes |
| 4 | 60.00 | 20.00 | 1 | 4 | 3.43 | 3.47 | 11.23 | 13.89 | yes |
| 4 | 60.00 | 20.00 | 4 | 16 | 2.09 | 2.31 | 27.15 | 37.04 | yes |
| 4 | 60.00 | 20.00 | 16 | 64 | 0.94 | 0.99 | 44.08 | 63.51 | yes |
| 4 | 60.00 | 100.00 | 1 | 4 | 3.97 | 3.98 | 13.26 | 15.91 | yes |
| 4 | 60.00 | 100.00 | 4 | 16 | 3.40 | 3.51 | 45.51 | 56.10 | yes |
| 4 | 60.00 | 100.00 | 16 | 64 | 2.02 | 2.38 | 107.50 | 152.19 | yes |
| 8 | 10.00 | 20.00 | 1 | 8 | 5.63 | 5.75 | 34.03 | 45.98 | yes |
| 8 | 10.00 | 20.00 | 4 | 32 | 2.05 | 2.20 | 48.76 | 70.24 | yes |
| 8 | 10.00 | 20.00 | 16 | 128 | 0.56 | 0.63 | 53.84 | 80.92 | yes |
| 8 | 10.00 | 100.00 | 1 | 8 | 9.37 | 9.94 | 54.73 | 79.52 | yes |
| 8 | 10.00 | 100.00 | 4 | 32 | 5.66 | 6.17 | 141.71 | 197.58 | yes |
| 8 | 10.00 | 100.00 | 16 | 128 | 2.13 | 2.45 | 204.69 | 314.20 | yes |
| 8 | 30.00 | 20.00 | 1 | 8 | 2.96 | 2.99 | 18.95 | 23.95 | yes |
| 8 | 30.00 | 20.00 | 4 | 32 | 1.58 | 1.62 | 38.09 | 51.99 | yes |
| 8 | 30.00 | 20.00 | 16 | 128 | 0.53 | 0.57 | 50.85 | 73.49 | yes |
| 8 | 30.00 | 100.00 | 1 | 8 | 3.77 | 3.84 | 25.28 | 30.70 | yes |
| 8 | 30.00 | 100.00 | 4 | 32 | 3.00 | 3.11 | 77.56 | 99.39 | yes |
| 8 | 30.00 | 100.00 | 16 | 128 | 1.59 | 1.76 | 157.73 | 225.60 | yes |
| 8 | 60.00 | 20.00 | 1 | 8 | 1.72 | 1.74 | 11.33 | 13.94 | yes |
| 8 | 60.00 | 20.00 | 4 | 32 | 1.15 | 1.17 | 28.48 | 37.40 | yes |
| 8 | 60.00 | 20.00 | 16 | 128 | 0.48 | 0.50 | 45.82 | 64.59 | yes |
| 8 | 60.00 | 100.00 | 1 | 8 | 1.97 | 2.00 | 13.22 | 15.98 | yes |
| 8 | 60.00 | 100.00 | 4 | 32 | 1.74 | 1.78 | 45.81 | 56.94 | yes |
| 8 | 60.00 | 100.00 | 16 | 128 | 1.19 | 1.24 | 119.42 | 158.54 | yes |

**Curve, measured on the emulated WAN (Studio lab)**: the smallest swept configuration (2/4/8 stages × depth 1/4/16, G = stages) whose aggregate reached each target, with Kimi-sized frames on the wire. Synthetic model, so compute per stage is small; the network and the uplink set these numbers.

| one-way ms | uplink Mbit/s | target tok/s | stages | depth | concurrent | per answer tok/s | aggregate tok/s |
|---|---|---|---|---|---|---|---|
| 10.00 | 20.00 | 25 | 2 | 1 | 2 | 21.62 | 34.62 |
| 10.00 | 20.00 | 50 | 2 | 4 | 8 | 7.99 | 50.34 |
| 10.00 | 20.00 | 100 | – | – | – | – | not reached (best 57) |
| 10.00 | 20.00 | 150 | – | – | – | – | not reached (best 57) |
| 10.00 | 20.00 | 200 | – | – | – | – | not reached (best 57) |
| 10.00 | 100.00 | 25 | 2 | 1 | 2 | 35.64 | 59.38 |
| 10.00 | 100.00 | 50 | 2 | 1 | 2 | 35.64 | 59.38 |
| 10.00 | 100.00 | 100 | 2 | 4 | 8 | 19.75 | 129.66 |
| 10.00 | 100.00 | 150 | 2 | 16 | 32 | 6.87 | 180.47 |
| 10.00 | 100.00 | 200 | 8 | 16 | 128 | 2.13 | 204.69 |
| 30.00 | 20.00 | 25 | 2 | 4 | 8 | 6.25 | 39.58 |
| 30.00 | 20.00 | 50 | 2 | 16 | 32 | 2.09 | 52.39 |
| 30.00 | 20.00 | 100 | – | – | – | – | not reached (best 52) |
| 30.00 | 20.00 | 150 | – | – | – | – | not reached (best 52) |
| 30.00 | 20.00 | 200 | – | – | – | – | not reached (best 52) |
| 30.00 | 100.00 | 25 | 2 | 4 | 8 | 11.05 | 73.43 |
| 30.00 | 100.00 | 50 | 2 | 4 | 8 | 11.05 | 73.43 |
| 30.00 | 100.00 | 100 | 2 | 16 | 32 | 5.28 | 138.82 |
| 30.00 | 100.00 | 150 | 4 | 16 | 64 | 2.95 | 151.23 |
| 30.00 | 100.00 | 200 | – | – | – | – | not reached (best 158) |
| 60.00 | 20.00 | 25 | 2 | 4 | 8 | 4.51 | 27.73 |
| 60.00 | 20.00 | 50 | – | – | – | – | not reached (best 47) |
| 60.00 | 20.00 | 100 | – | – | – | – | not reached (best 47) |
| 60.00 | 20.00 | 150 | – | – | – | – | not reached (best 47) |
| 60.00 | 20.00 | 200 | – | – | – | – | not reached (best 47) |
| 60.00 | 100.00 | 25 | 2 | 4 | 8 | 6.73 | 44.32 |
| 60.00 | 100.00 | 50 | 2 | 16 | 32 | 4.09 | 108.48 |
| 60.00 | 100.00 | 100 | 2 | 16 | 32 | 4.09 | 108.48 |
| 60.00 | 100.00 | 150 | – | – | – | – | not reached (best 119) |
| 60.00 | 100.00 | 200 | – | – | – | – | not reached (best 119) |

### CI runner (GitHub ubuntu-latest): linux x86_64, 4 logical CPUs

Model: synthetic MLA + MoE, 8 layers, d_model 256, 16 routed experts top-4, profile `arc.hf-deepseek-v3.mla-moe.i8-dyadic-row.q16.v1`; package blake3 `59cccff13d80759a…`. WAN delay line: sleep.

Single process, no network: 251.95 tok/s decode (3.94 ms per position); engine effective weight bandwidth 1.418 GB/s (5.63 MB of active weights per token).

Boundary activations on the wire (lossless width): {'i16': 0, 'i32': 532, 'i64': 0, 'i8': 0} over 532 vectors; mean 1,024 B per vector vs 2048 B as i64.

**Hop latency** (ring round trip of a ping frame through every stage process, no compute; loopback TCP):

| stages | payload B | hops | ring median ms | ring p90 ms | per hop µs |
|---|---|---|---|---|---|
| 1 | 0 | 2 | 0.05 | 0.06 | 26.96 |
| 1 | 2,048 | 2 | 0.06 | 0.06 | 28.60 |
| 1 | 14,336 | 2 | 0.07 | 0.07 | 33.71 |
| 1 | 28,672 | 2 | 0.07 | 0.08 | 36.38 |
| 1 | 57,344 | 2 | 0.09 | 0.10 | 45.50 |
| 2 | 0 | 3 | 0.09 | 0.09 | 29.86 |
| 2 | 2,048 | 3 | 0.10 | 0.10 | 31.78 |
| 2 | 14,336 | 3 | 0.11 | 0.11 | 35.00 |
| 2 | 28,672 | 3 | 0.12 | 0.12 | 38.57 |
| 2 | 57,344 | 3 | 0.14 | 0.15 | 46.51 |
| 4 | 0 | 5 | 0.14 | 0.15 | 28.97 |
| 4 | 2,048 | 5 | 0.15 | 0.16 | 30.24 |
| 4 | 14,336 | 5 | 0.17 | 0.18 | 34.42 |
| 4 | 28,672 | 5 | 0.19 | 0.20 | 37.87 |
| 4 | 57,344 | 5 | 0.22 | 0.24 | 44.77 |

**Throughput, stage processes on one host** (decode tok/s per answer, mean; aggregate = generated / wall time):

| stages | G | B | tokens | per answer tok/s | aggregate tok/s | bit-exact |
|---|---|---|---|---|---|---|
| 1 | 1 | 1 | 128 | 230.82 | 195.77 | yes |
| 1 | 2 | 8 | 128 | 30.60 | 214.87 | yes |
| 2 | 1 | 1 | 128 | 237.36 | 200.95 | yes |
| 2 | 2 | 2 | 128 | 176.31 | 293.07 | yes |
| 2 | 2 | 8 | 128 | 46.32 | 305.87 | yes |
| 4 | 1 | 1 | 128 | 228.90 | 193.17 | yes |
| 4 | 4 | 4 | 128 | 106.30 | 360.27 | yes |
| 4 | 4 | 8 | 128 | 56.07 | 370.69 | yes |

**Emulated WAN, CI runner (GitHub ubuntu-latest)** (every stage's uplink shaped: one-way delay ± 10% jitter, bounded uplink; 28672 B per position on the wire = a Kimi K2 boundary at i32; G = stages micro-batches of `depth` sequences). Predicted = research-6 §2.6 round-time model fed with this run's measured compute per position and hop overhead. Median |error| per answer: 1.9%.

| stages | one-way ms | uplink Mbit/s | depth | B | per answer tok/s | predicted | aggregate tok/s | predicted | bit-exact |
|---|---|---|---|---|---|---|---|---|---|
| 2 | 10.00 | 20.00 | 1 | 2 | 20.95 | 21.30 | 33.80 | 42.59 | yes |
| 2 | 10.00 | 20.00 | 4 | 8 | 7.74 | 7.84 | 48.25 | 62.70 | yes |
| 2 | 10.00 | 20.00 | 16 | 32 | 2.17 | 2.22 | 54.49 | 71.08 | yes |
| 2 | 10.00 | 100.00 | 1 | 2 | 34.63 | 34.96 | 58.11 | 69.91 | yes |
| 2 | 10.00 | 100.00 | 4 | 8 | 18.32 | 18.45 | 118.34 | 147.60 | yes |
| 2 | 10.00 | 100.00 | 16 | 32 | 5.69 | 6.39 | 150.91 | 204.38 | yes |
| 2 | 30.00 | 20.00 | 1 | 2 | 11.44 | 11.50 | 18.65 | 23.00 | yes |
| 2 | 30.00 | 20.00 | 4 | 8 | 5.89 | 5.97 | 37.58 | 47.73 | yes |
| 2 | 30.00 | 20.00 | 16 | 32 | 2.01 | 2.04 | 50.50 | 65.28 | yes |
| 2 | 30.00 | 100.00 | 1 | 2 | 14.19 | 14.58 | 24.29 | 29.15 | yes |
| 2 | 30.00 | 100.00 | 4 | 8 | 10.54 | 10.62 | 71.06 | 84.92 | yes |
| 2 | 30.00 | 100.00 | 16 | 32 | 4.93 | 5.09 | 128.52 | 162.79 | yes |
| 2 | 60.00 | 20.00 | 1 | 2 | 6.86 | 6.80 | 11.44 | 13.61 | yes |
| 2 | 60.00 | 20.00 | 4 | 8 | 4.31 | 4.39 | 28.16 | 35.15 | yes |
| 2 | 60.00 | 20.00 | 16 | 32 | 1.81 | 1.82 | 45.36 | 58.16 | yes |
| 2 | 60.00 | 100.00 | 1 | 2 | 7.50 | 7.78 | 12.87 | 15.55 | yes |
| 2 | 60.00 | 100.00 | 4 | 8 | 6.47 | 6.49 | 43.41 | 51.88 | yes |
| 2 | 60.00 | 100.00 | 16 | 32 | 3.81 | 3.90 | 100.01 | 124.72 | yes |
| 4 | 10.00 | 20.00 | 1 | 4 | 11.00 | 11.11 | 33.85 | 44.46 | yes |
| 4 | 10.00 | 20.00 | 4 | 16 | 4.01 | 4.18 | 48.58 | 66.83 | yes |
| 4 | 10.00 | 20.00 | 16 | 64 | 1.13 | 1.19 | 54.53 | 76.44 | yes |
| 4 | 10.00 | 100.00 | 1 | 4 | 18.32 | 18.77 | 60.05 | 75.09 | yes |
| 4 | 10.00 | 100.00 | 4 | 16 | 10.26 | 10.80 | 129.94 | 172.74 | yes |
| 4 | 10.00 | 100.00 | 16 | 64 | 3.57 | 4.00 | 178.87 | 255.95 | yes |
| 4 | 30.00 | 20.00 | 1 | 4 | 5.86 | 5.88 | 18.93 | 23.53 | yes |
| 4 | 30.00 | 20.00 | 4 | 16 | 3.10 | 3.13 | 37.89 | 50.09 | yes |
| 4 | 30.00 | 20.00 | 16 | 64 | 1.05 | 1.09 | 50.57 | 69.77 | yes |
| 4 | 30.00 | 100.00 | 1 | 4 | 7.30 | 7.50 | 24.57 | 30.01 | yes |
| 4 | 30.00 | 100.00 | 4 | 16 | 5.69 | 5.79 | 75.29 | 92.69 | yes |
| 4 | 30.00 | 100.00 | 16 | 64 | 2.84 | 3.03 | 146.11 | 193.91 | yes |
| 4 | 60.00 | 20.00 | 1 | 4 | 3.41 | 3.45 | 11.24 | 13.79 | yes |
| 4 | 60.00 | 20.00 | 4 | 16 | 2.25 | 2.28 | 28.43 | 36.41 | yes |
| 4 | 60.00 | 20.00 | 16 | 64 | 0.94 | 0.96 | 45.60 | 61.70 | yes |
| 4 | 60.00 | 100.00 | 1 | 4 | 3.88 | 3.95 | 13.18 | 15.79 | yes |
| 4 | 60.00 | 100.00 | 4 | 16 | 3.37 | 3.42 | 44.79 | 54.68 | yes |
| 4 | 60.00 | 100.00 | 16 | 64 | 2.14 | 2.22 | 110.03 | 142.21 | yes |
| 8 | 10.00 | 20.00 | 1 | 8 | 5.58 | 5.68 | 34.02 | 45.45 | yes |
| 8 | 10.00 | 20.00 | 4 | 32 | 2.05 | 2.16 | 48.69 | 69.10 | yes |
| 8 | 10.00 | 20.00 | 16 | 128 | 0.57 | 0.62 | 54.53 | 79.43 | yes |
| 8 | 10.00 | 100.00 | 1 | 8 | 9.52 | 9.75 | 62.26 | 77.97 | yes |
| 8 | 10.00 | 100.00 | 4 | 32 | 5.68 | 5.90 | 138.25 | 188.81 | yes |
| 8 | 10.00 | 100.00 | 16 | 128 | 2.08 | 2.29 | 201.13 | 292.90 | yes |
| 8 | 30.00 | 20.00 | 1 | 8 | 2.95 | 2.98 | 18.83 | 23.81 | yes |
| 8 | 30.00 | 20.00 | 4 | 32 | 1.58 | 1.60 | 37.77 | 51.36 | yes |
| 8 | 30.00 | 20.00 | 16 | 128 | 0.53 | 0.56 | 50.53 | 72.26 | yes |
| 8 | 30.00 | 100.00 | 1 | 8 | 3.75 | 3.81 | 25.00 | 30.46 | yes |
| 8 | 30.00 | 100.00 | 4 | 32 | 2.96 | 3.04 | 76.67 | 97.12 | yes |
| 8 | 30.00 | 100.00 | 16 | 128 | 1.59 | 1.68 | 158.02 | 214.40 | yes |
| 8 | 60.00 | 20.00 | 1 | 8 | 1.71 | 1.74 | 11.28 | 13.89 | yes |
| 8 | 60.00 | 20.00 | 4 | 32 | 1.15 | 1.16 | 28.51 | 37.08 | yes |
| 8 | 60.00 | 20.00 | 16 | 128 | 0.48 | 0.50 | 45.31 | 63.64 | yes |
| 8 | 60.00 | 100.00 | 1 | 8 | 1.96 | 1.99 | 13.19 | 15.92 | yes |
| 8 | 60.00 | 100.00 | 4 | 32 | 1.72 | 1.76 | 45.57 | 56.19 | yes |
| 8 | 60.00 | 100.00 | 16 | 128 | 1.15 | 1.19 | 116.15 | 152.93 | yes |

**Curve, measured on the emulated WAN (CI runner (GitHub ubuntu-latest))**: the smallest swept configuration (2/4/8 stages × depth 1/4/16, G = stages) whose aggregate reached each target, with Kimi-sized frames on the wire. Synthetic model, so compute per stage is small; the network and the uplink set these numbers.

| one-way ms | uplink Mbit/s | target tok/s | stages | depth | concurrent | per answer tok/s | aggregate tok/s |
|---|---|---|---|---|---|---|---|
| 10.00 | 20.00 | 25 | 2 | 1 | 2 | 20.95 | 33.80 |
| 10.00 | 20.00 | 50 | 2 | 16 | 32 | 2.17 | 54.49 |
| 10.00 | 20.00 | 100 | – | – | – | – | not reached (best 55) |
| 10.00 | 20.00 | 150 | – | – | – | – | not reached (best 55) |
| 10.00 | 20.00 | 200 | – | – | – | – | not reached (best 55) |
| 10.00 | 100.00 | 25 | 2 | 1 | 2 | 34.63 | 58.11 |
| 10.00 | 100.00 | 50 | 2 | 1 | 2 | 34.63 | 58.11 |
| 10.00 | 100.00 | 100 | 2 | 4 | 8 | 18.32 | 118.34 |
| 10.00 | 100.00 | 150 | 2 | 16 | 32 | 5.69 | 150.91 |
| 10.00 | 100.00 | 200 | 8 | 16 | 128 | 2.08 | 201.13 |
| 30.00 | 20.00 | 25 | 2 | 4 | 8 | 5.89 | 37.58 |
| 30.00 | 20.00 | 50 | 2 | 16 | 32 | 2.01 | 50.50 |
| 30.00 | 20.00 | 100 | – | – | – | – | not reached (best 51) |
| 30.00 | 20.00 | 150 | – | – | – | – | not reached (best 51) |
| 30.00 | 20.00 | 200 | – | – | – | – | not reached (best 51) |
| 30.00 | 100.00 | 25 | 2 | 4 | 8 | 10.54 | 71.06 |
| 30.00 | 100.00 | 50 | 2 | 4 | 8 | 10.54 | 71.06 |
| 30.00 | 100.00 | 100 | 2 | 16 | 32 | 4.93 | 128.52 |
| 30.00 | 100.00 | 150 | 8 | 16 | 128 | 1.59 | 158.02 |
| 30.00 | 100.00 | 200 | – | – | – | – | not reached (best 158) |
| 60.00 | 20.00 | 25 | 2 | 4 | 8 | 4.31 | 28.16 |
| 60.00 | 20.00 | 50 | – | – | – | – | not reached (best 46) |
| 60.00 | 20.00 | 100 | – | – | – | – | not reached (best 46) |
| 60.00 | 20.00 | 150 | – | – | – | – | not reached (best 46) |
| 60.00 | 20.00 | 200 | – | – | – | – | not reached (best 46) |
| 60.00 | 100.00 | 25 | 2 | 4 | 8 | 6.47 | 43.41 |
| 60.00 | 100.00 | 50 | 2 | 16 | 32 | 3.81 | 100.01 |
| 60.00 | 100.00 | 100 | 2 | 16 | 32 | 3.81 | 100.01 |
| 60.00 | 100.00 | 150 | – | – | – | – | not reached (best 116) |
| 60.00 | 100.00 | 200 | – | – | – | – | not reached (best 116) |

## Bit-exactness

Every island run above was compared with the single process (tokens, every logits hash, the hash at every layer boundary of every position): **all identical**.

## Projection for Kimi K2 (not a measurement)

Measured software cost per hop used below: 38.2 µs (CI runner (GitHub ubuntu-latest), loopback TCP, 28 KiB frame; the larger of the measured hosts). A real NIC adds wire time: 22.9 µs on 10 GbE, 2.9 µs on TB5 (80 Gb/s).

**Islands (T1), pipeline parallel, PROJECTION** — per answer and aggregate tok/s; concurrency = stages × depth (one micro-batch per stage); the last column applies research-6 §2.7's 1.34× for one exact speculative draft (Kimi K2 has no MTP head; needs a drafter).

| devices | stages | link | depth | concurrent | per answer tok/s | aggregate tok/s | with 1 draft | KV fits |
|---|---|---|---|---|---|---|---|---|
| M3 Ultra 512 GB | 2 | TB5 | 1 | 1 | 20.41 | 20.41 | 27.35 | yes |
| M3 Ultra 512 GB | 2 | TB5 | 8 | 16 | 4.61 | 73.76 | – | yes |
| M3 Ultra 512 GB | 2 | TB5 | 32 | 64 | 1.57 | 100.56 | – | yes |
| M3 Ultra 512 GB | 4 | TB5 | 1 | 1 | 20.38 | 20.38 | 27.31 | yes |
| M3 Ultra 512 GB | 4 | TB5 | 8 | 32 | 4.61 | 147.46 | – | yes |
| M3 Ultra 512 GB | 4 | TB5 | 32 | 128 | 1.57 | 201.09 | – | yes |
| M3 Ultra 512 GB | 2 | 10 GbE | 1 | 1 | 20.40 | 20.40 | 27.33 | yes |
| M3 Ultra 512 GB | 2 | 10 GbE | 8 | 16 | 4.61 | 73.74 | – | yes |
| M3 Ultra 512 GB | 2 | 10 GbE | 32 | 64 | 1.57 | 100.55 | – | yes |
| RTX 5090 32 GB | 26 | 25 GbE | 1 | 1 | 46.82 | 46.82 | 62.74 | yes |
| RTX 5090 32 GB | 26 | 25 GbE | 8 | 208 | 11.05 | 2,298.97 | – | yes |
| RTX 5090 32 GB | 26 | 25 GbE | 32 | 832 | 3.80 | 3,161.88 | – | yes |

**Swarm pipeline across homes (T2-batch), PROJECTION** — G = stages micro-batches of `depth`; uplink ceiling = uplink ÷ (28 KiB × 8): every token's activation crosses every stage's uplink, so no schedule can exceed it. KV fits = the concurrent sequences' MLA cache at 4096 tokens of context (34.3 KiB per token, spread over the stages) fits beside the device's share of the 582 GB of weights.

| devices | stages | one-way ms | uplink Mbit/s | depth | concurrent | per answer tok/s | aggregate tok/s | uplink ceiling tok/s | KV fits |
|---|---|---|---|---|---|---|---|---|---|
| RTX 5090 32 GB | 26 | 10 | 20 | 1 | 26 | 1.73 | 44.88 | 87.19 | yes |
| RTX 5090 32 GB | 26 | 10 | 20 | 4 | 104 | 0.66 | 69.11 | 87.19 | yes |
| RTX 5090 32 GB | 26 | 10 | 20 | 16 | 416 | 0.19 | 80.17 | 87.19 | yes |
| RTX 5090 32 GB | 26 | 10 | 20 | 64 | 1,664 | 0.05 | 84.31 | 87.19 | no (9.2 > 4.6 GB) |
| RTX 5090 32 GB | 26 | 10 | 100 | 1 | 26 | 2.93 | 76.30 | 435.97 | yes |
| RTX 5090 32 GB | 26 | 10 | 100 | 4 | 104 | 1.82 | 188.90 | 435.97 | yes |
| RTX 5090 32 GB | 26 | 10 | 100 | 16 | 416 | 0.73 | 303.23 | 435.97 | yes |
| RTX 5090 32 GB | 26 | 10 | 100 | 64 | 1,664 | 0.22 | 372.39 | 435.97 | no (9.2 > 4.6 GB) |
| RTX 5090 32 GB | 26 | 30 | 20 | 1 | 26 | 0.91 | 23.65 | 87.19 | yes |
| RTX 5090 32 GB | 26 | 30 | 20 | 4 | 104 | 0.49 | 51.36 | 87.19 | yes |
| RTX 5090 32 GB | 26 | 30 | 20 | 16 | 416 | 0.18 | 72.87 | 87.19 | yes |
| RTX 5090 32 GB | 26 | 30 | 20 | 64 | 1,664 | 0.05 | 82.15 | 87.19 | no (9.2 > 4.6 GB) |
| RTX 5090 32 GB | 26 | 30 | 100 | 1 | 26 | 1.16 | 30.21 | 435.97 | yes |
| RTX 5090 32 GB | 26 | 30 | 100 | 4 | 104 | 0.93 | 97.15 | 435.97 | yes |
| RTX 5090 32 GB | 26 | 30 | 100 | 16 | 416 | 0.53 | 219.88 | 435.97 | yes |
| RTX 5090 32 GB | 26 | 30 | 100 | 64 | 1,664 | 0.20 | 333.57 | 435.97 | no (9.2 > 4.6 GB) |
| RTX 5090 32 GB | 26 | 60 | 20 | 1 | 26 | 0.53 | 13.83 | 87.19 | yes |
| RTX 5090 32 GB | 26 | 60 | 20 | 4 | 104 | 0.36 | 37.08 | 87.19 | yes |
| RTX 5090 32 GB | 26 | 60 | 20 | 16 | 416 | 0.15 | 64.11 | 87.19 | yes |
| RTX 5090 32 GB | 26 | 60 | 20 | 64 | 1,664 | 0.05 | 79.10 | 87.19 | no (9.2 > 4.6 GB) |
| RTX 5090 32 GB | 26 | 60 | 100 | 1 | 26 | 0.61 | 15.85 | 435.97 | yes |
| RTX 5090 32 GB | 26 | 60 | 100 | 4 | 104 | 0.54 | 56.20 | 435.97 | yes |
| RTX 5090 32 GB | 26 | 60 | 100 | 16 | 416 | 0.37 | 155.69 | 435.97 | yes |
| RTX 5090 32 GB | 26 | 60 | 100 | 64 | 1,664 | 0.17 | 288.47 | 435.97 | no (9.2 > 4.6 GB) |
| Mac 64 GB (M4 Pro) | 12 | 10 | 20 | 1 | 12 | 2.48 | 29.72 | 87.19 | yes |
| Mac 64 GB (M4 Pro) | 12 | 10 | 20 | 4 | 48 | 0.96 | 46.14 | 87.19 | yes |
| Mac 64 GB (M4 Pro) | 12 | 10 | 20 | 16 | 192 | 0.29 | 55.53 | 87.19 | yes |
| Mac 64 GB (M4 Pro) | 12 | 10 | 20 | 64 | 768 | 0.09 | 65.32 | 87.19 | no (9.2 > 2.5 GB) |
| Mac 64 GB (M4 Pro) | 12 | 10 | 100 | 1 | 12 | 3.40 | 40.85 | 435.97 | yes |
| Mac 64 GB (M4 Pro) | 12 | 10 | 100 | 4 | 48 | 1.67 | 80.02 | 435.97 | yes |
| Mac 64 GB (M4 Pro) | 12 | 10 | 100 | 16 | 192 | 0.59 | 113.22 | 435.97 | yes |
| Mac 64 GB (M4 Pro) | 12 | 10 | 100 | 64 | 768 | 0.21 | 163.02 | 435.97 | no (9.2 > 2.5 GB) |
| Mac 64 GB (M4 Pro) | 12 | 30 | 20 | 1 | 12 | 1.55 | 18.64 | 87.19 | yes |
| Mac 64 GB (M4 Pro) | 12 | 30 | 20 | 4 | 48 | 0.78 | 37.49 | 87.19 | yes |
| Mac 64 GB (M4 Pro) | 12 | 30 | 20 | 16 | 192 | 0.27 | 51.93 | 87.19 | yes |
| Mac 64 GB (M4 Pro) | 12 | 30 | 20 | 64 | 768 | 0.08 | 64.01 | 87.19 | no (9.2 > 2.5 GB) |
| Mac 64 GB (M4 Pro) | 12 | 30 | 100 | 1 | 12 | 1.87 | 22.48 | 435.97 | yes |
| Mac 64 GB (M4 Pro) | 12 | 30 | 100 | 4 | 48 | 1.19 | 57.15 | 435.97 | yes |
| Mac 64 GB (M4 Pro) | 12 | 30 | 100 | 16 | 192 | 0.52 | 99.19 | 435.97 | yes |
| Mac 64 GB (M4 Pro) | 12 | 30 | 100 | 64 | 768 | 0.20 | 155.12 | 435.97 | no (9.2 > 2.5 GB) |
| Mac 64 GB (M4 Pro) | 12 | 60 | 20 | 1 | 12 | 1.00 | 11.95 | 87.19 | yes |
| Mac 64 GB (M4 Pro) | 12 | 60 | 20 | 4 | 48 | 0.61 | 29.26 | 87.19 | yes |
| Mac 64 GB (M4 Pro) | 12 | 60 | 20 | 16 | 192 | 0.25 | 47.32 | 87.19 | yes |
| Mac 64 GB (M4 Pro) | 12 | 60 | 20 | 64 | 768 | 0.08 | 62.15 | 87.19 | no (9.2 > 2.5 GB) |
| Mac 64 GB (M4 Pro) | 12 | 60 | 100 | 1 | 12 | 1.12 | 13.43 | 435.97 | yes |
| Mac 64 GB (M4 Pro) | 12 | 60 | 100 | 4 | 48 | 0.83 | 40.00 | 435.97 | yes |
| Mac 64 GB (M4 Pro) | 12 | 60 | 100 | 16 | 192 | 0.44 | 83.63 | 435.97 | yes |
| Mac 64 GB (M4 Pro) | 12 | 60 | 100 | 64 | 768 | 0.19 | 144.61 | 435.97 | no (9.2 > 2.5 GB) |

**Curve: what one swarm pipeline needs to reach an aggregate, PROJECTION** — the smallest micro-batch depth (G = stages micro-batches) whose projected aggregate reaches the target with the KV cache fitting (4096 tokens of context), and the per-answer speed at that point. Two stage counts per device class: the memory minimum (with KV headroom) and about twice that. More stages give more KV room, so more sequences can be in flight, but each answer is slower and the uplink ceiling stays the same: every stage's uplink carries every token.

| devices | stages | one-way ms | uplink Mbit/s | target tok/s | depth | concurrent | per answer tok/s | aggregate tok/s |
|---|---|---|---|---|---|---|---|---|
| RTX 5090 32 GB | 26 | 10 | 20 | 50 | 2 | 52 | 1.13 | 58.56 |
| RTX 5090 32 GB | 26 | 10 | 20 | 100 | – | – | – | no: uplink ceiling 87 |
| RTX 5090 32 GB | 26 | 10 | 20 | 200 | – | – | – | no: uplink ceiling 87 |
| RTX 5090 32 GB | 26 | 10 | 20 | 300 | – | – | – | no: uplink ceiling 87 |
| RTX 5090 32 GB | 26 | 10 | 20 | 400 | – | – | – | no: uplink ceiling 87 |
| RTX 5090 32 GB | 26 | 10 | 100 | 50 | 1 | 26 | 2.93 | 76.30 |
| RTX 5090 32 GB | 26 | 10 | 100 | 100 | 2 | 52 | 2.43 | 126.55 |
| RTX 5090 32 GB | 26 | 10 | 100 | 200 | 5 | 130 | 1.61 | 209.66 |
| RTX 5090 32 GB | 26 | 10 | 100 | 300 | 16 | 416 | 0.73 | 303.23 |
| RTX 5090 32 GB | 26 | 10 | 100 | 400 | – | – | – | no: KV memory runs out first |
| RTX 5090 32 GB | 26 | 30 | 20 | 50 | 4 | 104 | 0.49 | 51.36 |
| RTX 5090 32 GB | 26 | 30 | 20 | 100 | – | – | – | no: uplink ceiling 87 |
| RTX 5090 32 GB | 26 | 30 | 20 | 200 | – | – | – | no: uplink ceiling 87 |
| RTX 5090 32 GB | 26 | 30 | 20 | 300 | – | – | – | no: uplink ceiling 87 |
| RTX 5090 32 GB | 26 | 30 | 20 | 400 | – | – | – | no: uplink ceiling 87 |
| RTX 5090 32 GB | 26 | 30 | 100 | 50 | 2 | 52 | 1.07 | 55.86 |
| RTX 5090 32 GB | 26 | 30 | 100 | 100 | 5 | 130 | 0.88 | 114.03 |
| RTX 5090 32 GB | 26 | 30 | 100 | 200 | 13 | 338 | 0.59 | 200.12 |
| RTX 5090 32 GB | 26 | 30 | 100 | 300 | – | – | – | no: KV memory runs out first |
| RTX 5090 32 GB | 26 | 30 | 100 | 400 | – | – | – | no: KV memory runs out first |
| RTX 5090 32 GB | 26 | 60 | 20 | 50 | 8 | 208 | 0.25 | 51.54 |
| RTX 5090 32 GB | 26 | 60 | 20 | 100 | – | – | – | no: uplink ceiling 87 |
| RTX 5090 32 GB | 26 | 60 | 20 | 200 | – | – | – | no: uplink ceiling 87 |
| RTX 5090 32 GB | 26 | 60 | 20 | 300 | – | – | – | no: uplink ceiling 87 |
| RTX 5090 32 GB | 26 | 60 | 20 | 400 | – | – | – | no: uplink ceiling 87 |
| RTX 5090 32 GB | 26 | 60 | 100 | 50 | 4 | 104 | 0.54 | 56.20 |
| RTX 5090 32 GB | 26 | 60 | 100 | 100 | 9 | 234 | 0.46 | 106.56 |
| RTX 5090 32 GB | 26 | 60 | 100 | 200 | 26 | 676 | 0.30 | 202.51 |
| RTX 5090 32 GB | 26 | 60 | 100 | 300 | – | – | – | no: KV memory runs out first |
| RTX 5090 32 GB | 26 | 60 | 100 | 400 | – | – | – | no: KV memory runs out first |
| RTX 5090 32 GB | 44 | 10 | 20 | 50 | 2 | 88 | 0.67 | 59.40 |
| RTX 5090 32 GB | 44 | 10 | 20 | 100 | – | – | – | no: uplink ceiling 87 |
| RTX 5090 32 GB | 44 | 10 | 20 | 200 | – | – | – | no: uplink ceiling 87 |
| RTX 5090 32 GB | 44 | 10 | 20 | 300 | – | – | – | no: uplink ceiling 87 |
| RTX 5090 32 GB | 44 | 10 | 20 | 400 | – | – | – | no: uplink ceiling 87 |
| RTX 5090 32 GB | 44 | 10 | 100 | 50 | 1 | 44 | 1.78 | 78.19 |
| RTX 5090 32 GB | 44 | 10 | 100 | 100 | 2 | 88 | 1.48 | 130.53 |
| RTX 5090 32 GB | 44 | 10 | 100 | 200 | 5 | 220 | 0.99 | 218.43 |
| RTX 5090 32 GB | 44 | 10 | 100 | 300 | 13 | 572 | 0.53 | 303.22 |
| RTX 5090 32 GB | 44 | 10 | 100 | 400 | – | – | – | no: KV memory runs out first |
| RTX 5090 32 GB | 44 | 30 | 20 | 50 | 4 | 176 | 0.29 | 51.90 |
| RTX 5090 32 GB | 44 | 30 | 20 | 100 | – | – | – | no: uplink ceiling 87 |
| RTX 5090 32 GB | 44 | 30 | 20 | 200 | – | – | – | no: uplink ceiling 87 |
| RTX 5090 32 GB | 44 | 30 | 20 | 300 | – | – | – | no: uplink ceiling 87 |
| RTX 5090 32 GB | 44 | 30 | 20 | 400 | – | – | – | no: uplink ceiling 87 |
| RTX 5090 32 GB | 44 | 30 | 100 | 50 | 2 | 88 | 0.64 | 56.62 |
| RTX 5090 32 GB | 44 | 30 | 100 | 100 | 5 | 220 | 0.53 | 116.58 |
| RTX 5090 32 GB | 44 | 30 | 100 | 200 | 13 | 572 | 0.36 | 206.76 |
| RTX 5090 32 GB | 44 | 30 | 100 | 300 | 35 | 1,540 | 0.19 | 300.08 |
| RTX 5090 32 GB | 44 | 30 | 100 | 400 | – | – | – | no: KV memory runs out first |
| RTX 5090 32 GB | 44 | 60 | 20 | 50 | 8 | 352 | 0.15 | 52.01 |
| RTX 5090 32 GB | 44 | 60 | 20 | 100 | – | – | – | no: uplink ceiling 87 |
| RTX 5090 32 GB | 44 | 60 | 20 | 200 | – | – | – | no: uplink ceiling 87 |
| RTX 5090 32 GB | 44 | 60 | 20 | 300 | – | – | – | no: uplink ceiling 87 |
| RTX 5090 32 GB | 44 | 60 | 20 | 400 | – | – | – | no: uplink ceiling 87 |
| RTX 5090 32 GB | 44 | 60 | 100 | 50 | 4 | 176 | 0.32 | 56.84 |
| RTX 5090 32 GB | 44 | 60 | 100 | 100 | 9 | 396 | 0.27 | 108.54 |
| RTX 5090 32 GB | 44 | 60 | 100 | 200 | 24 | 1,056 | 0.19 | 200.09 |
| RTX 5090 32 GB | 44 | 60 | 100 | 300 | 67 | 2,948 | 0.10 | 300.74 |
| RTX 5090 32 GB | 44 | 60 | 100 | 400 | – | – | – | no: KV memory runs out first |
| Mac 64 GB (M4 Pro) | 12 | 10 | 20 | 50 | 7 | 84 | 0.60 | 50.51 |
| Mac 64 GB (M4 Pro) | 12 | 10 | 20 | 100 | – | – | – | no: uplink ceiling 87 |
| Mac 64 GB (M4 Pro) | 12 | 10 | 20 | 200 | – | – | – | no: uplink ceiling 87 |
| Mac 64 GB (M4 Pro) | 12 | 10 | 20 | 300 | – | – | – | no: uplink ceiling 87 |
| Mac 64 GB (M4 Pro) | 12 | 10 | 20 | 400 | – | – | – | no: uplink ceiling 87 |
| Mac 64 GB (M4 Pro) | 12 | 10 | 100 | 50 | 2 | 24 | 2.52 | 60.40 |
| Mac 64 GB (M4 Pro) | 12 | 10 | 100 | 100 | 10 | 120 | 0.85 | 102.39 |
| Mac 64 GB (M4 Pro) | 12 | 10 | 100 | 200 | – | – | – | no: KV memory runs out first |
| Mac 64 GB (M4 Pro) | 12 | 10 | 100 | 300 | – | – | – | no: KV memory runs out first |
| Mac 64 GB (M4 Pro) | 12 | 10 | 100 | 400 | – | – | – | no: KV memory runs out first |
| Mac 64 GB (M4 Pro) | 12 | 30 | 20 | 50 | 13 | 156 | 0.32 | 50.15 |
| Mac 64 GB (M4 Pro) | 12 | 30 | 20 | 100 | – | – | – | no: uplink ceiling 87 |
| Mac 64 GB (M4 Pro) | 12 | 30 | 20 | 200 | – | – | – | no: uplink ceiling 87 |
| Mac 64 GB (M4 Pro) | 12 | 30 | 20 | 300 | – | – | – | no: uplink ceiling 87 |
| Mac 64 GB (M4 Pro) | 12 | 30 | 20 | 400 | – | – | – | no: uplink ceiling 87 |
| Mac 64 GB (M4 Pro) | 12 | 30 | 100 | 50 | 4 | 48 | 1.19 | 57.15 |
| Mac 64 GB (M4 Pro) | 12 | 30 | 100 | 100 | 17 | 204 | 0.50 | 101.05 |
| Mac 64 GB (M4 Pro) | 12 | 30 | 100 | 200 | – | – | – | no: KV memory runs out first |
| Mac 64 GB (M4 Pro) | 12 | 30 | 100 | 300 | – | – | – | no: KV memory runs out first |
| Mac 64 GB (M4 Pro) | 12 | 30 | 100 | 400 | – | – | – | no: KV memory runs out first |
| Mac 64 GB (M4 Pro) | 12 | 60 | 20 | 50 | – | – | – | no: KV memory runs out first |
| Mac 64 GB (M4 Pro) | 12 | 60 | 20 | 100 | – | – | – | no: uplink ceiling 87 |
| Mac 64 GB (M4 Pro) | 12 | 60 | 20 | 200 | – | – | – | no: uplink ceiling 87 |
| Mac 64 GB (M4 Pro) | 12 | 60 | 20 | 300 | – | – | – | no: uplink ceiling 87 |
| Mac 64 GB (M4 Pro) | 12 | 60 | 20 | 400 | – | – | – | no: uplink ceiling 87 |
| Mac 64 GB (M4 Pro) | 12 | 60 | 100 | 50 | 6 | 72 | 0.72 | 51.56 |
| Mac 64 GB (M4 Pro) | 12 | 60 | 100 | 100 | – | – | – | no: KV memory runs out first |
| Mac 64 GB (M4 Pro) | 12 | 60 | 100 | 200 | – | – | – | no: KV memory runs out first |
| Mac 64 GB (M4 Pro) | 12 | 60 | 100 | 300 | – | – | – | no: KV memory runs out first |
| Mac 64 GB (M4 Pro) | 12 | 60 | 100 | 400 | – | – | – | no: KV memory runs out first |
| Mac 64 GB (M4 Pro) | 24 | 10 | 20 | 50 | 3 | 72 | 0.73 | 52.82 |
| Mac 64 GB (M4 Pro) | 24 | 10 | 20 | 100 | – | – | – | no: uplink ceiling 87 |
| Mac 64 GB (M4 Pro) | 24 | 10 | 20 | 200 | – | – | – | no: uplink ceiling 87 |
| Mac 64 GB (M4 Pro) | 24 | 10 | 20 | 300 | – | – | – | no: uplink ceiling 87 |
| Mac 64 GB (M4 Pro) | 24 | 10 | 20 | 400 | – | – | – | no: uplink ceiling 87 |
| Mac 64 GB (M4 Pro) | 24 | 10 | 100 | 50 | 1 | 24 | 2.26 | 54.33 |
| Mac 64 GB (M4 Pro) | 24 | 10 | 100 | 100 | 3 | 72 | 1.42 | 102.50 |
| Mac 64 GB (M4 Pro) | 24 | 10 | 100 | 200 | 35 | 840 | 0.24 | 200.92 |
| Mac 64 GB (M4 Pro) | 24 | 10 | 100 | 300 | 157 | 3,768 | 0.08 | 300.35 |
| Mac 64 GB (M4 Pro) | 24 | 10 | 100 | 400 | – | – | – | no: KV memory runs out first |
| Mac 64 GB (M4 Pro) | 24 | 30 | 20 | 50 | 7 | 168 | 0.31 | 52.06 |
| Mac 64 GB (M4 Pro) | 24 | 30 | 20 | 100 | – | – | – | no: uplink ceiling 87 |
| Mac 64 GB (M4 Pro) | 24 | 30 | 20 | 200 | – | – | – | no: uplink ceiling 87 |
| Mac 64 GB (M4 Pro) | 24 | 30 | 20 | 300 | – | – | – | no: uplink ceiling 87 |
| Mac 64 GB (M4 Pro) | 24 | 30 | 20 | 400 | – | – | – | no: uplink ceiling 87 |
| Mac 64 GB (M4 Pro) | 24 | 30 | 100 | 50 | 3 | 72 | 0.85 | 60.89 |
| Mac 64 GB (M4 Pro) | 24 | 30 | 100 | 100 | 8 | 192 | 0.55 | 106.22 |
| Mac 64 GB (M4 Pro) | 24 | 30 | 100 | 200 | 50 | 1,200 | 0.17 | 201.01 |
| Mac 64 GB (M4 Pro) | 24 | 30 | 100 | 300 | 178 | 4,272 | 0.07 | 300.20 |
| Mac 64 GB (M4 Pro) | 24 | 30 | 100 | 400 | – | – | – | no: KV memory runs out first |
| Mac 64 GB (M4 Pro) | 24 | 60 | 20 | 50 | 12 | 288 | 0.18 | 51.02 |
| Mac 64 GB (M4 Pro) | 24 | 60 | 20 | 100 | – | – | – | no: uplink ceiling 87 |
| Mac 64 GB (M4 Pro) | 24 | 60 | 20 | 200 | – | – | – | no: uplink ceiling 87 |
| Mac 64 GB (M4 Pro) | 24 | 60 | 20 | 300 | – | – | – | no: uplink ceiling 87 |
| Mac 64 GB (M4 Pro) | 24 | 60 | 20 | 400 | – | – | – | no: uplink ceiling 87 |
| Mac 64 GB (M4 Pro) | 24 | 60 | 100 | 50 | 5 | 120 | 0.46 | 55.63 |
| Mac 64 GB (M4 Pro) | 24 | 60 | 100 | 100 | 13 | 312 | 0.32 | 100.07 |
| Mac 64 GB (M4 Pro) | 24 | 60 | 100 | 200 | 67 | 1,608 | 0.12 | 200.56 |
| Mac 64 GB (M4 Pro) | 24 | 60 | 100 | 300 | – | – | – | no: KV memory runs out first |
| Mac 64 GB (M4 Pro) | 24 | 60 | 100 | 400 | – | – | – | no: KV memory runs out first |

**One billion tokens a day (11,574 tok/s sustained), swarm tier, PROJECTION** (the deepest micro-batch depth whose KV fits; decode tokens only, no prefill; 100% utilisation):

| devices | stages | one-way ms | uplink Mbit/s | depth | concurrent | per answer tok/s | aggregate per pipeline | pipelines | devices |
|---|---|---|---|---|---|---|---|---|---|
| RTX 5090 32 GB | 26 | 10 | 20 | 16 | 416 | 0.19 | 80.17 | 145 | 3,770 |
| RTX 5090 32 GB | 26 | 10 | 100 | 16 | 416 | 0.73 | 303.23 | 39 | 1,014 |
| RTX 5090 32 GB | 26 | 30 | 20 | 16 | 416 | 0.18 | 72.87 | 159 | 4,134 |
| RTX 5090 32 GB | 26 | 30 | 100 | 16 | 416 | 0.53 | 219.88 | 53 | 1,378 |
| RTX 5090 32 GB | 26 | 60 | 20 | 16 | 416 | 0.15 | 64.11 | 181 | 4,706 |
| RTX 5090 32 GB | 26 | 60 | 100 | 16 | 416 | 0.37 | 155.69 | 75 | 1,950 |
| Mac 64 GB (M4 Pro) | 12 | 10 | 20 | 16 | 192 | 0.29 | 55.53 | 209 | 2,508 |
| Mac 64 GB (M4 Pro) | 12 | 10 | 100 | 16 | 192 | 0.59 | 113.22 | 103 | 1,236 |
| Mac 64 GB (M4 Pro) | 12 | 30 | 20 | 16 | 192 | 0.27 | 51.93 | 223 | 2,676 |
| Mac 64 GB (M4 Pro) | 12 | 30 | 100 | 16 | 192 | 0.52 | 99.19 | 117 | 1,404 |
| Mac 64 GB (M4 Pro) | 12 | 60 | 20 | 16 | 192 | 0.25 | 47.32 | 245 | 2,940 |
| Mac 64 GB (M4 Pro) | 12 | 60 | 100 | 16 | 192 | 0.44 | 83.63 | 139 | 1,668 |
