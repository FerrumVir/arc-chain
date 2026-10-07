# Island runtime: measured results and the Kimi K2 / K2.6 projection

Measured on **Studio lab** (Mac Studio, Apple M2 Ultra, 24 cores, macOS; 7 Oct 2026)
with `arc-island bench --kernel simd --threads 4 --stage-threads 4` (32 tokens per answer
on the emulated WAN). CI runner numbers are produced on every pull-request run by
`.github/workflows/island-runtime.yml` (job "bench", artifact `island-bench-ci`, also in
the job summary). Raw data: [`data/`](data/). Regenerate this report with
`python3 scripts/arc_island/report.py docs/island/data/studio-kimi-mini.json docs/island/data/studio-small.json`.

All measurements use synthetic MLA + MoE models: the Kimi K2 text architecture at small
width (`kimi-mini`: 8 layers, d 512, 64 experts top-8, INT4 g32 experts; `small`: 8
layers, d 256). Every stage process runs on one host, joined by loopback TCP; the WAN
rows emulate each hop in the sending process (delay line + bounded uplink). **No real
Kimi weights and no real LAN, Thunderbolt or internet link were used.** Everything under
"Projection" is arithmetic, not a measurement. Kimi K2.6 has the same text shapes as K2
(`docs/protocol/kimi-k26-checkpoint.md` §1: 61 layers, hidden 7,168, 384 experts top-8,
expert width 2,048), so the K2 arithmetic applies to K2.6.

## Reading

1. **Bit-exact everywhere.** Every run produced the single process's tokens, every logits
   hash and the hash at every layer boundary of every position. That covers 1, 2, 4 and 8
   stage processes, 1 to 128 concurrent sequences, and every WAN profile.
2. **Hop cost (Studio lab, loopback TCP).** About 16–18 µs per hop for a 28 KiB frame (a
   Kimi K2 boundary at i32) in the `small` run, and up to 62 µs in the `kimi-mini` run on
   the same host. The projection uses the larger. A real NIC, driver and GPU copies add
   more; research-6's 0.3 ms streaming hop is shown as a sensitivity.
3. **The pipeline model, checked on per answer and on aggregate.** Fed with the measured
   compute per position and hop cost, research-6's round-time model was checked against 54
   emulated-WAN runs (2/4/8 stages × 10/30/60 ms × 20/100 Mbit/s × depth 1/4/16, 32-token
   answers). Measured minus predicted:
   - **per answer:** median −3.0% (range −14.6% to −0.7%);
   - **aggregate at steady state** (while every sequence decodes): median −3.1% (−14.2% to −0.5%);
   - **aggregate over wall-clock time** (prefill and pipeline fill/drain included): median −7.7% (−19.3% to −4.2%).

   The model never under-predicts, so every projected aggregate below is an **upper bound**.
   An earlier version of this report quoted only the per-answer error. It also ran 6-token
   answers, where fill/drain cost 16–35% of aggregate. And its emulator charged the
   activation on the last hop, which in reality carries commitments only.
4. **The swarm tier is uplink-bound.** Every exact boundary is i32 (none of 532 measured
   vectors fits i16 or i8), so a Kimi token puts 28,672 B of activation on every stage's
   uplink. Accumulated commitments add up to 32 B × (61 + stages); with 26 stages that is
   +2,784 B. A 26-stage pipeline therefore cannot exceed about **79 tok/s aggregate at
   20 Mbit/s or 397 tok/s at 100 Mbit/s**. Measured on the emulated WAN at 8 stages,
   10 ms per hop and 128 sequences: 85 tok/s steady at 20 Mbit/s and 297 tok/s at 100 Mbit/s.
5. **The curve** (tables "Curve, measured" and "Curve: what one swarm pipeline needs").
   Projected for 26 RTX 5090-class stages at 100 Mbit/s and 4k context (an upper bound):

   | Per hop | Target aggregate | Sequences in flight | Per answer |
   |---|---|---|---|
   | 10 ms | 100 tok/s | 52 | 2.4 tok/s |
   | 10 ms | 200 tok/s | 130 | 1.6 tok/s |
   | 10 ms | 300 tok/s | 494 | 0.6 tok/s |
   | 60 ms | 100 tok/s | 234 | 0.45 tok/s |
   | 60 ms | 200 tok/s | 702 | 0.29 tok/s |

   - **Context matters.** At 8k context the 60 ms pipeline tops out at 153 tok/s; at 32k, at
     56 tok/s ("Context sensitivity").
   - **Per-sequence weight reads cost throughput.** This runtime reads each sequence's
     weights separately, which cuts the depth-16 aggregate from 292 to 262 tok/s at
     10 ms / 100 Mbit/s.
6. **One billion tokens a day on the swarm tier** (11.6k tok/s sustained, decode only, 100%
   utilisation) projects to at least about 1,040 RTX 5090-class devices on 100 Mbit/s
   uplinks, or about 4,000 on 20 Mbit/s. These are lower bounds on devices.
7. **Islands, per answer.** Pipeline parallelism does not speed up one answer.
   - **2–4 M3 Ultras:** about 20 tok/s per answer.
   - **26× RTX 5090 LAN pipeline:** 45.5 tok/s with the measured loopback hop, 35.5 tok/s
     with research-6's 0.3 ms hop.
   - **One exact speculative draft:** 56.9 tok/s for the 5090 island and 24.8 for 2× M3
     Ultra. The model runs a 2-position verify pass at 1.85 tokens accepted per pass,
     DeepSeek-V3's MTP rate. This is an assumption: Kimi has no MTP head, and the drafter's
     acceptance and cost are not modelled.
   - **No projected configuration here reaches ≥59 tok/s per answer.**
8. **The engine, not the network, is the gap.** ARC's integer engine reads its active
   weights at 1.1–2.8 GB/s on the Studio (4 threads, SIMD kernel). Every projection
   assumes 456–1,108 GB/s.

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

## Bit-exactness

Every island run above was compared with the single process (tokens, every logits hash, the hash at every layer boundary of every position): **all identical**.

## Projection for Kimi K2 / K2.6 (not a measurement)

Everything below is arithmetic, not a measurement, for **Kimi K2**; K2.6 has the same text shapes (`docs/protocol/kimi-k26-checkpoint.md` §1), so it applies to the requested K2.6 unchanged. No Kimi weights and no real network were run.

Inputs: the measured software cost per hop, 61.9 µs (Studio lab, loopback TCP, 28 KiB frame; the larger of the measured hosts); research-6's memory bandwidths (456 GB/s M3 Ultra, 1,108 GB/s RTX 5090), which ARC's engine does not reach (1.4–2.7 GB/s measured); batched weight reads across a micro-batch's sequences, which this runtime does not have (the swarm table also shows per-sequence reads); and **4096 tokens of context per sequence** (sensitivity table below).

**Aggregates are optimistic.** The same model, fed with measured costs, put the emulated-WAN steady-state aggregate at a median -3.1% from measured, and wall-clock aggregate (with prefill and fill/drain) at a median -7.7%. Read every projected aggregate below as an upper bound.

**Islands (T1), pipeline parallel, PROJECTION**. Concurrency = stages × depth (one micro-batch per stage). "0.3 ms hop" replaces the measured loopback hop with research-6's streaming-transport hop (a real NIC, driver and GPU copies). "1 draft" is the model's verify pass over 2 positions (one draft token) at 1.85 tokens accepted per pass — DeepSeek-V3's MTP acceptance, ASSUMED: Kimi has no MTP head, and the drafter's acceptance and cost are not modelled.

| devices | stages | link | depth | concurrent | per answer tok/s | at 0.3 ms hop | 1 draft | aggregate tok/s | KV fits |
|---|---|---|---|---|---|---|---|---|---|
| M3 Ultra 512 GB | 2 | TB5 | 1 | 1 | 20.39 | 20.20 | 24.81 | 20.39 | yes |
| M3 Ultra 512 GB | 2 | TB5 | 8 | 16 | 4.61 | – | – | 73.74 | yes |
| M3 Ultra 512 GB | 2 | TB5 | 32 | 64 | 1.57 | – | – | 100.55 | yes |
| M3 Ultra 512 GB | 4 | TB5 | 1 | 1 | 20.34 | 19.95 | 24.77 | 20.34 | yes |
| M3 Ultra 512 GB | 4 | TB5 | 8 | 32 | 4.61 | – | – | 147.39 | yes |
| M3 Ultra 512 GB | 4 | TB5 | 32 | 128 | 1.57 | – | – | 201.06 | yes |
| M3 Ultra 512 GB | 2 | 10 GbE | 1 | 1 | 20.38 | 20.18 | 24.80 | 20.38 | yes |
| M3 Ultra 512 GB | 2 | 10 GbE | 8 | 16 | 4.61 | – | – | 73.73 | yes |
| M3 Ultra 512 GB | 2 | 10 GbE | 32 | 64 | 1.57 | – | – | 100.54 | yes |
| RTX 5090 32 GB | 26 | 25 GbE | 1 | 1 | 45.46 | 35.48 | 56.91 | 45.46 | yes |
| RTX 5090 32 GB | 26 | 25 GbE | 8 | 208 | 10.98 | – | – | 2,282.87 | yes |
| RTX 5090 32 GB | 26 | 25 GbE | 32 | 832 | 3.79 | – | – | 3,154.23 | yes |

**Swarm pipeline across homes (T2-batch), PROJECTION**. G = stages micro-batches of `depth`. Uplink ceiling = uplink ÷ (bytes per position × 8), with the activation (28,672 B) plus accumulated commitments (up to 32 B × (61 + stages)) on the busiest uplink: every token crosses every stage's uplink, so no schedule can exceed it. "Per-sequence reads" = the same with every sequence reading its own weights (this runtime today). KV fits = the MLA cache at 4096 tokens of context fits beside the device's share of the 582 GB of weights.

| devices | stages | one-way ms | uplink Mbit/s | depth | concurrent | per answer tok/s | aggregate tok/s | per-sequence reads | uplink ceiling tok/s | KV fits |
|---|---|---|---|---|---|---|---|---|---|---|
| RTX 5090 32 GB | 26 | 10 | 20 | 1 | 26 | 1.68 | 43.60 | 43.60 | 79.48 | yes |
| RTX 5090 32 GB | 26 | 10 | 20 | 4 | 104 | 0.64 | 66.21 | 64.99 | 79.48 | yes |
| RTX 5090 32 GB | 26 | 10 | 20 | 16 | 416 | 0.18 | 76.31 | 74.07 | 79.48 | yes |
| RTX 5090 32 GB | 26 | 10 | 20 | 64 | 1,664 | 0.05 | 79.48 | 76.75 | 79.48 | no (9.2 > 4.6 GB) |
| RTX 5090 32 GB | 26 | 10 | 100 | 1 | 26 | 2.90 | 75.44 | 75.44 | 397.38 | yes |
| RTX 5090 32 GB | 26 | 10 | 100 | 4 | 104 | 1.77 | 184.31 | 175.15 | 397.38 | yes |
| RTX 5090 32 GB | 26 | 10 | 100 | 16 | 416 | 0.70 | 291.95 | 261.60 | 397.38 | yes |
| RTX 5090 32 GB | 26 | 10 | 100 | 64 | 1,664 | 0.21 | 355.66 | 298.42 | 397.38 | no (9.2 > 4.6 GB) |
| RTX 5090 32 GB | 26 | 30 | 20 | 1 | 26 | 0.90 | 23.29 | 23.29 | 79.48 | yes |
| RTX 5090 32 GB | 26 | 30 | 20 | 4 | 104 | 0.48 | 49.74 | 49.05 | 79.48 | yes |
| RTX 5090 32 GB | 26 | 30 | 20 | 16 | 416 | 0.17 | 69.67 | 67.79 | 79.48 | yes |
| RTX 5090 32 GB | 26 | 30 | 20 | 64 | 1,664 | 0.05 | 78.11 | 74.95 | 79.48 | no (9.2 > 4.6 GB) |
| RTX 5090 32 GB | 26 | 30 | 100 | 1 | 26 | 1.16 | 30.07 | 30.07 | 397.38 | yes |
| RTX 5090 32 GB | 26 | 30 | 100 | 4 | 104 | 0.92 | 95.92 | 93.38 | 397.38 | yes |
| RTX 5090 32 GB | 26 | 30 | 100 | 16 | 416 | 0.51 | 213.89 | 197.14 | 397.38 | yes |
| RTX 5090 32 GB | 26 | 30 | 100 | 64 | 1,664 | 0.19 | 320.09 | 272.96 | 397.38 | no (9.2 > 4.6 GB) |
| RTX 5090 32 GB | 26 | 60 | 20 | 1 | 26 | 0.53 | 13.71 | 13.71 | 79.48 | yes |
| RTX 5090 32 GB | 26 | 60 | 20 | 4 | 104 | 0.35 | 36.23 | 35.86 | 79.48 | yes |
| RTX 5090 32 GB | 26 | 60 | 20 | 16 | 416 | 0.15 | 61.62 | 60.15 | 79.48 | yes |
| RTX 5090 32 GB | 26 | 60 | 20 | 64 | 1,664 | 0.05 | 75.35 | 72.41 | 79.48 | no (9.2 > 4.6 GB) |
| RTX 5090 32 GB | 26 | 60 | 100 | 1 | 26 | 0.61 | 15.81 | 15.81 | 397.38 | yes |
| RTX 5090 32 GB | 26 | 60 | 100 | 4 | 104 | 0.54 | 55.79 | 54.92 | 397.38 | yes |
| RTX 5090 32 GB | 26 | 60 | 100 | 16 | 416 | 0.37 | 152.67 | 143.93 | 397.38 | yes |
| RTX 5090 32 GB | 26 | 60 | 100 | 64 | 1,664 | 0.17 | 278.33 | 242.00 | 397.38 | no (9.2 > 4.6 GB) |
| Mac 64 GB (M4 Pro) | 12 | 10 | 20 | 1 | 12 | 2.48 | 29.78 | 29.78 | 80.62 | yes |
| Mac 64 GB (M4 Pro) | 12 | 10 | 20 | 4 | 48 | 0.97 | 46.34 | 38.42 | 80.62 | yes |
| Mac 64 GB (M4 Pro) | 12 | 10 | 20 | 16 | 192 | 0.29 | 55.84 | 41.42 | 80.62 | yes |
| Mac 64 GB (M4 Pro) | 12 | 10 | 20 | 64 | 768 | 0.09 | 65.74 | 42.24 | 80.62 | no (9.2 > 2.5 GB) |
| Mac 64 GB (M4 Pro) | 12 | 10 | 100 | 1 | 12 | 3.40 | 40.85 | 40.85 | 403.12 | yes |
| Mac 64 GB (M4 Pro) | 12 | 10 | 100 | 4 | 48 | 1.67 | 80.10 | 59.05 | 403.12 | yes |
| Mac 64 GB (M4 Pro) | 12 | 10 | 100 | 16 | 192 | 0.59 | 113.46 | 66.45 | 403.12 | yes |
| Mac 64 GB (M4 Pro) | 12 | 10 | 100 | 64 | 768 | 0.21 | 163.54 | 68.60 | 403.12 | no (9.2 > 2.5 GB) |
| Mac 64 GB (M4 Pro) | 12 | 30 | 20 | 1 | 12 | 1.56 | 18.66 | 18.66 | 80.62 | yes |
| Mac 64 GB (M4 Pro) | 12 | 30 | 20 | 4 | 48 | 0.78 | 37.62 | 32.23 | 80.62 | yes |
| Mac 64 GB (M4 Pro) | 12 | 30 | 20 | 16 | 192 | 0.27 | 52.19 | 39.38 | 80.62 | yes |
| Mac 64 GB (M4 Pro) | 12 | 30 | 20 | 64 | 768 | 0.08 | 64.42 | 41.69 | 80.62 | no (9.2 > 2.5 GB) |
| Mac 64 GB (M4 Pro) | 12 | 30 | 100 | 1 | 12 | 1.87 | 22.48 | 22.48 | 403.12 | yes |
| Mac 64 GB (M4 Pro) | 12 | 30 | 100 | 4 | 48 | 1.19 | 57.20 | 45.59 | 403.12 | yes |
| Mac 64 GB (M4 Pro) | 12 | 30 | 100 | 16 | 192 | 0.52 | 99.37 | 61.35 | 403.12 | yes |
| Mac 64 GB (M4 Pro) | 12 | 30 | 100 | 64 | 768 | 0.20 | 155.59 | 67.16 | 403.12 | no (9.2 > 2.5 GB) |
| Mac 64 GB (M4 Pro) | 12 | 60 | 20 | 1 | 12 | 1.00 | 11.96 | 11.96 | 80.62 | yes |
| Mac 64 GB (M4 Pro) | 12 | 60 | 20 | 4 | 48 | 0.61 | 29.34 | 25.95 | 80.62 | yes |
| Mac 64 GB (M4 Pro) | 12 | 60 | 20 | 16 | 192 | 0.25 | 47.54 | 36.67 | 80.62 | yes |
| Mac 64 GB (M4 Pro) | 12 | 60 | 20 | 64 | 768 | 0.08 | 62.53 | 40.89 | 80.62 | no (9.2 > 2.5 GB) |
| Mac 64 GB (M4 Pro) | 12 | 60 | 100 | 1 | 12 | 1.12 | 13.43 | 13.43 | 403.12 | yes |
| Mac 64 GB (M4 Pro) | 12 | 60 | 100 | 4 | 48 | 0.83 | 40.03 | 33.97 | 403.12 | yes |
| Mac 64 GB (M4 Pro) | 12 | 60 | 100 | 16 | 192 | 0.44 | 83.76 | 55.02 | 403.12 | yes |
| Mac 64 GB (M4 Pro) | 12 | 60 | 100 | 64 | 768 | 0.19 | 145.01 | 65.11 | 403.12 | no (9.2 > 2.5 GB) |

**Context sensitivity, PROJECTION**: the most aggregate one swarm pipeline reaches before its KV cache runs out, by tokens of context held per sequence (agentic traffic is prompt-heavy, so 4k is optimistic).

| devices | stages | one-way ms | uplink Mbit/s | 4k context | 8k context | 32k context |
|---|---|---|---|---|---|---|
| RTX 5090 32 GB | 26 | 10 | 20 | 79 (832 seqs, 0.09/answer) | 76 (416 seqs, 0.18/answer) | 66 (104 seqs, 0.64/answer) |
| RTX 5090 32 GB | 26 | 10 | 100 | 328 (832 seqs, 0.39/answer) | 292 (416 seqs, 0.70/answer) | 184 (104 seqs, 1.77/answer) |
| RTX 5090 32 GB | 26 | 30 | 20 | 75 (832 seqs, 0.09/answer) | 70 (416 seqs, 0.17/answer) | 50 (104 seqs, 0.48/answer) |
| RTX 5090 32 GB | 26 | 30 | 100 | 272 (832 seqs, 0.33/answer) | 214 (416 seqs, 0.51/answer) | 96 (104 seqs, 0.92/answer) |
| RTX 5090 32 GB | 26 | 60 | 20 | 70 (832 seqs, 0.08/answer) | 62 (416 seqs, 0.15/answer) | 36 (104 seqs, 0.35/answer) |
| RTX 5090 32 GB | 26 | 60 | 100 | 217 (832 seqs, 0.26/answer) | 153 (416 seqs, 0.37/answer) | 56 (104 seqs, 0.54/answer) |
| Mac 64 GB (M4 Pro) | 12 | 10 | 20 | 56 (204 seqs, 0.28/answer) | 52 (96 seqs, 0.54/answer) | 39 (24 seqs, 1.62/answer) |
| Mac 64 GB (M4 Pro) | 12 | 10 | 100 | 115 (204 seqs, 0.56/answer) | 97 (96 seqs, 1.01/answer) | 60 (24 seqs, 2.52/answer) |
| Mac 64 GB (M4 Pro) | 12 | 30 | 20 | 53 (204 seqs, 0.26/answer) | 46 (96 seqs, 0.48/answer) | 28 (24 seqs, 1.17/answer) |
| Mac 64 GB (M4 Pro) | 12 | 30 | 100 | 101 (204 seqs, 0.50/answer) | 78 (96 seqs, 0.82/answer) | 38 (24 seqs, 1.57/answer) |
| Mac 64 GB (M4 Pro) | 12 | 60 | 20 | 48 (204 seqs, 0.24/answer) | 39 (96 seqs, 0.41/answer) | 20 (24 seqs, 0.82/answer) |
| Mac 64 GB (M4 Pro) | 12 | 60 | 100 | 86 (204 seqs, 0.42/answer) | 61 (96 seqs, 0.63/answer) | 24 (24 seqs, 1.00/answer) |

**Curve: what one swarm pipeline needs to reach an aggregate, PROJECTION** — the smallest micro-batch depth (G = stages micro-batches) whose projected aggregate reaches the target with the KV cache fitting (4096 tokens of context, assumed), and the per-answer speed at that point. Upper bounds: the sequence counts are lower bounds (see the model error above), and batched weight reads are assumed. Two stage counts per device class: the memory minimum (with KV headroom) and about twice that. More stages give more KV room, so more sequences can be in flight, but each answer is slower and the uplink ceiling stays the same: every stage's uplink carries every token.

| devices | stages | one-way ms | uplink Mbit/s | target tok/s | depth | concurrent | per answer tok/s | aggregate tok/s |
|---|---|---|---|---|---|---|---|---|
| RTX 5090 32 GB | 26 | 10 | 20 | 50 | 2 | 52 | 1.09 | 56.44 |
| RTX 5090 32 GB | 26 | 10 | 20 | 100 | – | – | – | no: uplink ceiling 79 |
| RTX 5090 32 GB | 26 | 10 | 20 | 200 | – | – | – | no: uplink ceiling 79 |
| RTX 5090 32 GB | 26 | 10 | 20 | 300 | – | – | – | no: uplink ceiling 79 |
| RTX 5090 32 GB | 26 | 10 | 20 | 400 | – | – | – | no: uplink ceiling 79 |
| RTX 5090 32 GB | 26 | 10 | 100 | 50 | 1 | 26 | 2.90 | 75.44 |
| RTX 5090 32 GB | 26 | 10 | 100 | 100 | 2 | 52 | 2.39 | 124.38 |
| RTX 5090 32 GB | 26 | 10 | 100 | 200 | 5 | 130 | 1.57 | 204.07 |
| RTX 5090 32 GB | 26 | 10 | 100 | 300 | 19 | 494 | 0.61 | 301.93 |
| RTX 5090 32 GB | 26 | 10 | 100 | 400 | – | – | – | no: uplink ceiling 397 |
| RTX 5090 32 GB | 26 | 30 | 20 | 50 | 5 | 130 | 0.41 | 53.82 |
| RTX 5090 32 GB | 26 | 30 | 20 | 100 | – | – | – | no: uplink ceiling 79 |
| RTX 5090 32 GB | 26 | 30 | 20 | 200 | – | – | – | no: uplink ceiling 79 |
| RTX 5090 32 GB | 26 | 30 | 20 | 300 | – | – | – | no: uplink ceiling 79 |
| RTX 5090 32 GB | 26 | 30 | 20 | 400 | – | – | – | no: uplink ceiling 79 |
| RTX 5090 32 GB | 26 | 30 | 100 | 50 | 2 | 52 | 1.07 | 55.43 |
| RTX 5090 32 GB | 26 | 30 | 100 | 100 | 5 | 130 | 0.86 | 112.36 |
| RTX 5090 32 GB | 26 | 30 | 100 | 200 | 14 | 364 | 0.55 | 201.86 |
| RTX 5090 32 GB | 26 | 30 | 100 | 300 | – | – | – | no: KV memory runs out first |
| RTX 5090 32 GB | 26 | 30 | 100 | 400 | – | – | – | no: uplink ceiling 397 |
| RTX 5090 32 GB | 26 | 60 | 20 | 50 | 9 | 234 | 0.22 | 52.10 |
| RTX 5090 32 GB | 26 | 60 | 20 | 100 | – | – | – | no: uplink ceiling 79 |
| RTX 5090 32 GB | 26 | 60 | 20 | 200 | – | – | – | no: uplink ceiling 79 |
| RTX 5090 32 GB | 26 | 60 | 20 | 300 | – | – | – | no: uplink ceiling 79 |
| RTX 5090 32 GB | 26 | 60 | 20 | 400 | – | – | – | no: uplink ceiling 79 |
| RTX 5090 32 GB | 26 | 60 | 100 | 50 | 4 | 104 | 0.54 | 55.79 |
| RTX 5090 32 GB | 26 | 60 | 100 | 100 | 9 | 234 | 0.45 | 105.12 |
| RTX 5090 32 GB | 26 | 60 | 100 | 200 | 27 | 702 | 0.29 | 200.97 |
| RTX 5090 32 GB | 26 | 60 | 100 | 300 | – | – | – | no: KV memory runs out first |
| RTX 5090 32 GB | 26 | 60 | 100 | 400 | – | – | – | no: uplink ceiling 397 |
| RTX 5090 32 GB | 44 | 10 | 20 | 50 | 2 | 88 | 0.63 | 55.86 |
| RTX 5090 32 GB | 44 | 10 | 20 | 100 | – | – | – | no: uplink ceiling 78 |
| RTX 5090 32 GB | 44 | 10 | 20 | 200 | – | – | – | no: uplink ceiling 78 |
| RTX 5090 32 GB | 44 | 10 | 20 | 300 | – | – | – | no: uplink ceiling 78 |
| RTX 5090 32 GB | 44 | 10 | 20 | 400 | – | – | – | no: uplink ceiling 78 |
| RTX 5090 32 GB | 44 | 10 | 100 | 50 | 1 | 44 | 1.75 | 76.78 |
| RTX 5090 32 GB | 44 | 10 | 100 | 100 | 2 | 88 | 1.44 | 126.85 |
| RTX 5090 32 GB | 44 | 10 | 100 | 200 | 5 | 220 | 0.95 | 208.62 |
| RTX 5090 32 GB | 44 | 10 | 100 | 300 | 17 | 748 | 0.40 | 301.57 |
| RTX 5090 32 GB | 44 | 10 | 100 | 400 | – | – | – | no: uplink ceiling 390 |
| RTX 5090 32 GB | 44 | 30 | 20 | 50 | 5 | 220 | 0.24 | 53.16 |
| RTX 5090 32 GB | 44 | 30 | 20 | 100 | – | – | – | no: uplink ceiling 78 |
| RTX 5090 32 GB | 44 | 30 | 20 | 200 | – | – | – | no: uplink ceiling 78 |
| RTX 5090 32 GB | 44 | 30 | 20 | 300 | – | – | – | no: uplink ceiling 78 |
| RTX 5090 32 GB | 44 | 30 | 20 | 400 | – | – | – | no: uplink ceiling 78 |
| RTX 5090 32 GB | 44 | 30 | 100 | 50 | 2 | 88 | 0.64 | 55.92 |
| RTX 5090 32 GB | 44 | 30 | 100 | 100 | 5 | 220 | 0.52 | 113.72 |
| RTX 5090 32 GB | 44 | 30 | 100 | 200 | 14 | 616 | 0.33 | 204.90 |
| RTX 5090 32 GB | 44 | 30 | 100 | 300 | 46 | 2,024 | 0.15 | 301.17 |
| RTX 5090 32 GB | 44 | 30 | 100 | 400 | – | – | – | no: uplink ceiling 390 |
| RTX 5090 32 GB | 44 | 60 | 20 | 50 | 9 | 396 | 0.13 | 51.43 |
| RTX 5090 32 GB | 44 | 60 | 20 | 100 | – | – | – | no: uplink ceiling 78 |
| RTX 5090 32 GB | 44 | 60 | 20 | 200 | – | – | – | no: uplink ceiling 78 |
| RTX 5090 32 GB | 44 | 60 | 20 | 300 | – | – | – | no: uplink ceiling 78 |
| RTX 5090 32 GB | 44 | 60 | 20 | 400 | – | – | – | no: uplink ceiling 78 |
| RTX 5090 32 GB | 44 | 60 | 100 | 50 | 4 | 176 | 0.32 | 56.15 |
| RTX 5090 32 GB | 44 | 60 | 100 | 100 | 9 | 396 | 0.27 | 106.09 |
| RTX 5090 32 GB | 44 | 60 | 100 | 200 | 27 | 1,188 | 0.17 | 203.06 |
| RTX 5090 32 GB | 44 | 60 | 100 | 300 | 85 | 3,740 | 0.08 | 300.47 |
| RTX 5090 32 GB | 44 | 60 | 100 | 400 | – | – | – | no: uplink ceiling 390 |
| Mac 64 GB (M4 Pro) | 12 | 10 | 20 | 50 | 7 | 84 | 0.60 | 50.76 |
| Mac 64 GB (M4 Pro) | 12 | 10 | 20 | 100 | – | – | – | no: uplink ceiling 81 |
| Mac 64 GB (M4 Pro) | 12 | 10 | 20 | 200 | – | – | – | no: uplink ceiling 81 |
| Mac 64 GB (M4 Pro) | 12 | 10 | 20 | 300 | – | – | – | no: uplink ceiling 81 |
| Mac 64 GB (M4 Pro) | 12 | 10 | 20 | 400 | – | – | – | no: uplink ceiling 81 |
| Mac 64 GB (M4 Pro) | 12 | 10 | 100 | 50 | 2 | 24 | 2.52 | 60.43 |
| Mac 64 GB (M4 Pro) | 12 | 10 | 100 | 100 | 9 | 108 | 0.93 | 100.16 |
| Mac 64 GB (M4 Pro) | 12 | 10 | 100 | 200 | – | – | – | no: KV memory runs out first |
| Mac 64 GB (M4 Pro) | 12 | 10 | 100 | 300 | – | – | – | no: KV memory runs out first |
| Mac 64 GB (M4 Pro) | 12 | 10 | 100 | 400 | – | – | – | no: KV memory runs out first |
| Mac 64 GB (M4 Pro) | 12 | 30 | 20 | 50 | 13 | 156 | 0.32 | 50.39 |
| Mac 64 GB (M4 Pro) | 12 | 30 | 20 | 100 | – | – | – | no: uplink ceiling 81 |
| Mac 64 GB (M4 Pro) | 12 | 30 | 20 | 200 | – | – | – | no: uplink ceiling 81 |
| Mac 64 GB (M4 Pro) | 12 | 30 | 20 | 300 | – | – | – | no: uplink ceiling 81 |
| Mac 64 GB (M4 Pro) | 12 | 30 | 20 | 400 | – | – | – | no: uplink ceiling 81 |
| Mac 64 GB (M4 Pro) | 12 | 30 | 100 | 50 | 4 | 48 | 1.19 | 57.20 |
| Mac 64 GB (M4 Pro) | 12 | 30 | 100 | 100 | 17 | 204 | 0.50 | 101.24 |
| Mac 64 GB (M4 Pro) | 12 | 30 | 100 | 200 | – | – | – | no: KV memory runs out first |
| Mac 64 GB (M4 Pro) | 12 | 30 | 100 | 300 | – | – | – | no: KV memory runs out first |
| Mac 64 GB (M4 Pro) | 12 | 30 | 100 | 400 | – | – | – | no: KV memory runs out first |
| Mac 64 GB (M4 Pro) | 12 | 60 | 20 | 50 | – | – | – | no: KV memory runs out first |
| Mac 64 GB (M4 Pro) | 12 | 60 | 20 | 100 | – | – | – | no: uplink ceiling 81 |
| Mac 64 GB (M4 Pro) | 12 | 60 | 20 | 200 | – | – | – | no: uplink ceiling 81 |
| Mac 64 GB (M4 Pro) | 12 | 60 | 20 | 300 | – | – | – | no: uplink ceiling 81 |
| Mac 64 GB (M4 Pro) | 12 | 60 | 20 | 400 | – | – | – | no: uplink ceiling 81 |
| Mac 64 GB (M4 Pro) | 12 | 60 | 100 | 50 | 6 | 72 | 0.72 | 51.61 |
| Mac 64 GB (M4 Pro) | 12 | 60 | 100 | 100 | – | – | – | no: KV memory runs out first |
| Mac 64 GB (M4 Pro) | 12 | 60 | 100 | 200 | – | – | – | no: KV memory runs out first |
| Mac 64 GB (M4 Pro) | 12 | 60 | 100 | 300 | – | – | – | no: KV memory runs out first |
| Mac 64 GB (M4 Pro) | 12 | 60 | 100 | 400 | – | – | – | no: KV memory runs out first |
| Mac 64 GB (M4 Pro) | 24 | 10 | 20 | 50 | 3 | 72 | 0.71 | 51.27 |
| Mac 64 GB (M4 Pro) | 24 | 10 | 20 | 100 | – | – | – | no: uplink ceiling 80 |
| Mac 64 GB (M4 Pro) | 24 | 10 | 20 | 200 | – | – | – | no: uplink ceiling 80 |
| Mac 64 GB (M4 Pro) | 24 | 10 | 20 | 300 | – | – | – | no: uplink ceiling 80 |
| Mac 64 GB (M4 Pro) | 24 | 10 | 20 | 400 | – | – | – | no: uplink ceiling 80 |
| Mac 64 GB (M4 Pro) | 24 | 10 | 100 | 50 | 1 | 24 | 2.25 | 53.93 |
| Mac 64 GB (M4 Pro) | 24 | 10 | 100 | 100 | 3 | 72 | 1.41 | 101.25 |
| Mac 64 GB (M4 Pro) | 24 | 10 | 100 | 200 | 38 | 912 | 0.22 | 200.05 |
| Mac 64 GB (M4 Pro) | 24 | 10 | 100 | 300 | 178 | 4,272 | 0.07 | 300.14 |
| Mac 64 GB (M4 Pro) | 24 | 10 | 100 | 400 | – | – | – | no: uplink ceiling 398 |
| Mac 64 GB (M4 Pro) | 24 | 30 | 20 | 50 | 7 | 168 | 0.30 | 50.57 |
| Mac 64 GB (M4 Pro) | 24 | 30 | 20 | 100 | – | – | – | no: uplink ceiling 80 |
| Mac 64 GB (M4 Pro) | 24 | 30 | 20 | 200 | – | – | – | no: uplink ceiling 80 |
| Mac 64 GB (M4 Pro) | 24 | 30 | 20 | 300 | – | – | – | no: uplink ceiling 80 |
| Mac 64 GB (M4 Pro) | 24 | 30 | 20 | 400 | – | – | – | no: uplink ceiling 80 |
| Mac 64 GB (M4 Pro) | 24 | 30 | 100 | 50 | 3 | 72 | 0.84 | 60.45 |
| Mac 64 GB (M4 Pro) | 24 | 30 | 100 | 100 | 8 | 192 | 0.55 | 104.93 |
| Mac 64 GB (M4 Pro) | 24 | 30 | 100 | 200 | 53 | 1,272 | 0.16 | 200.03 |
| Mac 64 GB (M4 Pro) | 24 | 30 | 100 | 300 | – | – | – | no: KV memory runs out first |
| Mac 64 GB (M4 Pro) | 24 | 30 | 100 | 400 | – | – | – | no: uplink ceiling 398 |
| Mac 64 GB (M4 Pro) | 24 | 60 | 20 | 50 | 13 | 312 | 0.16 | 50.67 |
| Mac 64 GB (M4 Pro) | 24 | 60 | 20 | 100 | – | – | – | no: uplink ceiling 80 |
| Mac 64 GB (M4 Pro) | 24 | 60 | 20 | 200 | – | – | – | no: uplink ceiling 80 |
| Mac 64 GB (M4 Pro) | 24 | 60 | 20 | 300 | – | – | – | no: uplink ceiling 80 |
| Mac 64 GB (M4 Pro) | 24 | 60 | 20 | 400 | – | – | – | no: uplink ceiling 80 |
| Mac 64 GB (M4 Pro) | 24 | 60 | 100 | 50 | 5 | 120 | 0.46 | 55.27 |
| Mac 64 GB (M4 Pro) | 24 | 60 | 100 | 100 | 14 | 336 | 0.31 | 102.74 |
| Mac 64 GB (M4 Pro) | 24 | 60 | 100 | 200 | 71 | 1,704 | 0.12 | 200.37 |
| Mac 64 GB (M4 Pro) | 24 | 60 | 100 | 300 | – | – | – | no: KV memory runs out first |
| Mac 64 GB (M4 Pro) | 24 | 60 | 100 | 400 | – | – | – | no: uplink ceiling 398 |

**One billion tokens a day (11,574 tok/s sustained), swarm tier, PROJECTION, lower bound on devices** (the deepest micro-batch depth whose KV fits at 4096 tokens; batched weight reads; decode tokens only, no prefill; 100% utilisation):

| devices | stages | one-way ms | uplink Mbit/s | depth | concurrent | per answer tok/s | aggregate per pipeline | pipelines | devices |
|---|---|---|---|---|---|---|---|---|---|
| RTX 5090 32 GB | 26 | 10 | 20 | 16 | 416 | 0.18 | 76.31 | 152 | 3,952 |
| RTX 5090 32 GB | 26 | 10 | 100 | 16 | 416 | 0.70 | 291.95 | 40 | 1,040 |
| RTX 5090 32 GB | 26 | 30 | 20 | 16 | 416 | 0.17 | 69.67 | 167 | 4,342 |
| RTX 5090 32 GB | 26 | 30 | 100 | 16 | 416 | 0.51 | 213.89 | 55 | 1,430 |
| RTX 5090 32 GB | 26 | 60 | 20 | 16 | 416 | 0.15 | 61.62 | 188 | 4,888 |
| RTX 5090 32 GB | 26 | 60 | 100 | 16 | 416 | 0.37 | 152.67 | 76 | 1,976 |
| Mac 64 GB (M4 Pro) | 12 | 10 | 20 | 16 | 192 | 0.29 | 55.84 | 208 | 2,496 |
| Mac 64 GB (M4 Pro) | 12 | 10 | 100 | 16 | 192 | 0.59 | 113.46 | 103 | 1,236 |
| Mac 64 GB (M4 Pro) | 12 | 30 | 20 | 16 | 192 | 0.27 | 52.19 | 222 | 2,664 |
| Mac 64 GB (M4 Pro) | 12 | 30 | 100 | 16 | 192 | 0.52 | 99.37 | 117 | 1,404 |
| Mac 64 GB (M4 Pro) | 12 | 60 | 20 | 16 | 192 | 0.25 | 47.54 | 244 | 2,928 |
| Mac 64 GB (M4 Pro) | 12 | 60 | 100 | 16 | 192 | 0.44 | 83.76 | 139 | 1,668 |
