# Island runtime: measured results and the Kimi K2 projection

Measured on **Studio lab** (Mac Studio, Apple M2 Ultra, 24 cores, macOS; 7 Oct 2026)
with `arc-island bench --kernel simd --threads 4 --stage-threads 4`; the CI runner
numbers are produced on every pull-request run by `.github/workflows/island-runtime.yml`
(job "bench", artifact `island-bench-ci`, also printed in the job summary). Raw data:
[`data/`](data/). Regenerate this report with
`python3 scripts/arc_island/report.py docs/island/data/studio-kimi-mini.json docs/island/data/studio-small.json`.

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
   not measured.
3. **The pipeline model holds.** Fed with the measured compute per position and hop cost,
   research-6's round-time model predicts the emulated-WAN per-answer speed with a median
   error of 3.9% over 54 runs (2/4/8 stages × 10/30/60 ms × 20/100 Mbit/s × depth 1/4/16).
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
   at 8 sequences per stage. A 26× RTX 5090 LAN pipeline projects to 48 tok/s per answer, about
   63 with one exact speculative draft, and 2.3k tok/s aggregate at depth 8. ≥59 tok/s per
   answer needs either that GPU island with speculation or tensor/expert parallelism over
   RDMA. Both assume kernels at research-6's calibrated memory bandwidths.
7. **The engine, not the network, is the gap.** ARC's integer engine reads its active weights
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

## Bit-exactness

Every island run above was compared with the single process (tokens, every logits hash, the hash at every layer boundary of every position): **all identical**.

## Projection for Kimi K2 (not a measurement)

Measured software cost per hop used below: 18.3 µs (Studio lab, loopback TCP, 28 KiB frame; the larger of the measured hosts). A real NIC adds wire time: 22.9 µs on 10 GbE, 2.9 µs on TB5 (80 Gb/s).

**Islands (T1), pipeline parallel, PROJECTION** — per answer and aggregate tok/s; concurrency = stages × depth (one micro-batch per stage); the last column applies research-6 §2.7's 1.34× for one exact speculative draft (Kimi K2 has no MTP head; needs a drafter).

| devices | stages | link | depth | concurrent | per answer tok/s | aggregate tok/s | with 1 draft | KV fits |
|---|---|---|---|---|---|---|---|---|
| M3 Ultra 512 GB | 2 | TB5 | 1 | 1 | 20.43 | 20.43 | 27.38 | yes |
| M3 Ultra 512 GB | 2 | TB5 | 8 | 16 | 4.61 | 73.77 | – | yes |
| M3 Ultra 512 GB | 2 | TB5 | 32 | 64 | 1.57 | 100.56 | – | yes |
| M3 Ultra 512 GB | 4 | TB5 | 1 | 1 | 20.41 | 20.41 | 27.35 | yes |
| M3 Ultra 512 GB | 4 | TB5 | 8 | 32 | 4.61 | 147.51 | – | yes |
| M3 Ultra 512 GB | 4 | TB5 | 32 | 128 | 1.57 | 201.11 | – | yes |
| M3 Ultra 512 GB | 2 | 10 GbE | 1 | 1 | 20.41 | 20.41 | 27.35 | yes |
| M3 Ultra 512 GB | 2 | 10 GbE | 8 | 16 | 4.61 | 73.76 | – | yes |
| M3 Ultra 512 GB | 2 | 10 GbE | 32 | 64 | 1.57 | 100.56 | – | yes |
| RTX 5090 32 GB | 26 | 25 GbE | 1 | 1 | 47.98 | 47.98 | 64.30 | yes |
| RTX 5090 32 GB | 26 | 25 GbE | 8 | 208 | 11.12 | 2,312.22 | – | yes |
| RTX 5090 32 GB | 26 | 25 GbE | 32 | 832 | 3.81 | 3,168.12 | – | yes |

**Swarm pipeline across homes (T2-batch), PROJECTION** — G = stages micro-batches of `depth`; uplink ceiling = uplink ÷ (28 KiB × 8): every token's activation crosses every stage's uplink, so no schedule can exceed it. KV fits = the concurrent sequences' MLA cache at 4096 tokens of context (34.3 KiB per token, spread over the stages) fits beside the device's share of the 582 GB of weights.

| devices | stages | one-way ms | uplink Mbit/s | depth | concurrent | per answer tok/s | aggregate tok/s | uplink ceiling tok/s | KV fits |
|---|---|---|---|---|---|---|---|---|---|
| RTX 5090 32 GB | 26 | 10 | 20 | 1 | 26 | 1.73 | 44.92 | 87.19 | yes |
| RTX 5090 32 GB | 26 | 10 | 20 | 4 | 104 | 0.66 | 69.14 | 87.19 | yes |
| RTX 5090 32 GB | 26 | 10 | 20 | 16 | 416 | 0.19 | 80.18 | 87.19 | yes |
| RTX 5090 32 GB | 26 | 10 | 20 | 64 | 1,664 | 0.05 | 84.32 | 87.19 | no (9.2 > 4.6 GB) |
| RTX 5090 32 GB | 26 | 10 | 100 | 1 | 26 | 2.94 | 76.42 | 435.97 | yes |
| RTX 5090 32 GB | 26 | 10 | 100 | 4 | 104 | 1.82 | 189.08 | 435.97 | yes |
| RTX 5090 32 GB | 26 | 10 | 100 | 16 | 416 | 0.73 | 303.34 | 435.97 | yes |
| RTX 5090 32 GB | 26 | 10 | 100 | 64 | 1,664 | 0.22 | 372.43 | 435.97 | no (9.2 > 4.6 GB) |
| RTX 5090 32 GB | 26 | 30 | 20 | 1 | 26 | 0.91 | 23.66 | 87.19 | yes |
| RTX 5090 32 GB | 26 | 30 | 20 | 4 | 104 | 0.49 | 51.38 | 87.19 | yes |
| RTX 5090 32 GB | 26 | 30 | 20 | 16 | 416 | 0.18 | 72.88 | 87.19 | yes |
| RTX 5090 32 GB | 26 | 30 | 20 | 64 | 1,664 | 0.05 | 82.15 | 87.19 | no (9.2 > 4.6 GB) |
| RTX 5090 32 GB | 26 | 30 | 100 | 1 | 26 | 1.16 | 30.22 | 435.97 | yes |
| RTX 5090 32 GB | 26 | 30 | 100 | 4 | 104 | 0.93 | 97.19 | 435.97 | yes |
| RTX 5090 32 GB | 26 | 30 | 100 | 16 | 416 | 0.53 | 219.94 | 435.97 | yes |
| RTX 5090 32 GB | 26 | 30 | 100 | 64 | 1,664 | 0.20 | 333.61 | 435.97 | no (9.2 > 4.6 GB) |
| RTX 5090 32 GB | 26 | 60 | 20 | 1 | 26 | 0.53 | 13.84 | 87.19 | yes |
| RTX 5090 32 GB | 26 | 60 | 20 | 4 | 104 | 0.36 | 37.09 | 87.19 | yes |
| RTX 5090 32 GB | 26 | 60 | 20 | 16 | 416 | 0.15 | 64.12 | 87.19 | yes |
| RTX 5090 32 GB | 26 | 60 | 20 | 64 | 1,664 | 0.05 | 79.11 | 87.19 | no (9.2 > 4.6 GB) |
| RTX 5090 32 GB | 26 | 60 | 100 | 1 | 26 | 0.61 | 15.85 | 435.97 | yes |
| RTX 5090 32 GB | 26 | 60 | 100 | 4 | 104 | 0.54 | 56.22 | 435.97 | yes |
| RTX 5090 32 GB | 26 | 60 | 100 | 16 | 416 | 0.37 | 155.72 | 435.97 | yes |
| RTX 5090 32 GB | 26 | 60 | 100 | 64 | 1,664 | 0.17 | 288.49 | 435.97 | no (9.2 > 4.6 GB) |
| Mac 64 GB (M4 Pro) | 12 | 10 | 20 | 1 | 12 | 2.48 | 29.73 | 87.19 | yes |
| Mac 64 GB (M4 Pro) | 12 | 10 | 20 | 4 | 48 | 0.96 | 46.15 | 87.19 | yes |
| Mac 64 GB (M4 Pro) | 12 | 10 | 20 | 16 | 192 | 0.29 | 55.54 | 87.19 | yes |
| Mac 64 GB (M4 Pro) | 12 | 10 | 20 | 64 | 768 | 0.09 | 65.32 | 87.19 | no (9.2 > 2.5 GB) |
| Mac 64 GB (M4 Pro) | 12 | 10 | 100 | 1 | 12 | 3.41 | 40.89 | 435.97 | yes |
| Mac 64 GB (M4 Pro) | 12 | 10 | 100 | 4 | 48 | 1.67 | 80.05 | 435.97 | yes |
| Mac 64 GB (M4 Pro) | 12 | 10 | 100 | 16 | 192 | 0.59 | 113.24 | 435.97 | yes |
| Mac 64 GB (M4 Pro) | 12 | 10 | 100 | 64 | 768 | 0.21 | 163.03 | 435.97 | no (9.2 > 2.5 GB) |
| Mac 64 GB (M4 Pro) | 12 | 30 | 20 | 1 | 12 | 1.55 | 18.65 | 87.19 | yes |
| Mac 64 GB (M4 Pro) | 12 | 30 | 20 | 4 | 48 | 0.78 | 37.50 | 87.19 | yes |
| Mac 64 GB (M4 Pro) | 12 | 30 | 20 | 16 | 192 | 0.27 | 51.93 | 87.19 | yes |
| Mac 64 GB (M4 Pro) | 12 | 30 | 20 | 64 | 768 | 0.08 | 64.02 | 87.19 | no (9.2 > 2.5 GB) |
| Mac 64 GB (M4 Pro) | 12 | 30 | 100 | 1 | 12 | 1.87 | 22.49 | 435.97 | yes |
| Mac 64 GB (M4 Pro) | 12 | 30 | 100 | 4 | 48 | 1.19 | 57.17 | 435.97 | yes |
| Mac 64 GB (M4 Pro) | 12 | 30 | 100 | 16 | 192 | 0.52 | 99.20 | 435.97 | yes |
| Mac 64 GB (M4 Pro) | 12 | 30 | 100 | 64 | 768 | 0.20 | 155.13 | 435.97 | no (9.2 > 2.5 GB) |
| Mac 64 GB (M4 Pro) | 12 | 60 | 20 | 1 | 12 | 1.00 | 11.96 | 87.19 | yes |
| Mac 64 GB (M4 Pro) | 12 | 60 | 20 | 4 | 48 | 0.61 | 29.27 | 87.19 | yes |
| Mac 64 GB (M4 Pro) | 12 | 60 | 20 | 16 | 192 | 0.25 | 47.32 | 87.19 | yes |
| Mac 64 GB (M4 Pro) | 12 | 60 | 20 | 64 | 768 | 0.08 | 62.15 | 87.19 | no (9.2 > 2.5 GB) |
| Mac 64 GB (M4 Pro) | 12 | 60 | 100 | 1 | 12 | 1.12 | 13.43 | 435.97 | yes |
| Mac 64 GB (M4 Pro) | 12 | 60 | 100 | 4 | 48 | 0.83 | 40.01 | 435.97 | yes |
| Mac 64 GB (M4 Pro) | 12 | 60 | 100 | 16 | 192 | 0.44 | 83.64 | 435.97 | yes |
| Mac 64 GB (M4 Pro) | 12 | 60 | 100 | 64 | 768 | 0.19 | 144.61 | 435.97 | no (9.2 > 2.5 GB) |

**One billion tokens a day (11,574 tok/s sustained), swarm tier, PROJECTION** (the deepest micro-batch depth whose KV fits; decode tokens only, no prefill; 100% utilisation):

| devices | stages | one-way ms | uplink Mbit/s | depth | concurrent | per answer tok/s | aggregate per pipeline | pipelines | devices |
|---|---|---|---|---|---|---|---|---|---|
| RTX 5090 32 GB | 26 | 10 | 20 | 16 | 416 | 0.19 | 80.18 | 145 | 3,770 |
| RTX 5090 32 GB | 26 | 10 | 100 | 16 | 416 | 0.73 | 303.34 | 39 | 1,014 |
| RTX 5090 32 GB | 26 | 30 | 20 | 16 | 416 | 0.18 | 72.88 | 159 | 4,134 |
| RTX 5090 32 GB | 26 | 30 | 100 | 16 | 416 | 0.53 | 219.94 | 53 | 1,378 |
| RTX 5090 32 GB | 26 | 60 | 20 | 16 | 416 | 0.15 | 64.12 | 181 | 4,706 |
| RTX 5090 32 GB | 26 | 60 | 100 | 16 | 416 | 0.37 | 155.72 | 75 | 1,950 |
| Mac 64 GB (M4 Pro) | 12 | 10 | 20 | 16 | 192 | 0.29 | 55.54 | 209 | 2,508 |
| Mac 64 GB (M4 Pro) | 12 | 10 | 100 | 16 | 192 | 0.59 | 113.24 | 103 | 1,236 |
| Mac 64 GB (M4 Pro) | 12 | 30 | 20 | 16 | 192 | 0.27 | 51.93 | 223 | 2,676 |
| Mac 64 GB (M4 Pro) | 12 | 30 | 100 | 16 | 192 | 0.52 | 99.20 | 117 | 1,404 |
| Mac 64 GB (M4 Pro) | 12 | 60 | 20 | 16 | 192 | 0.25 | 47.32 | 245 | 2,940 |
| Mac 64 GB (M4 Pro) | 12 | 60 | 100 | 16 | 192 | 0.44 | 83.64 | 139 | 1,668 |
