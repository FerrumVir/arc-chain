# ENG-9: network latency engineering for sharded inference

Status: draft, branch `agent/builder/ARC-71`. Code and tests only. Nothing here
is wired into `arc-node`, consensus, validators or the public
`/inference/run_sharded` pipeline. Everything runs on loopback.

Per-answer speed on a sharded model is set by one token's trip through every
stage. This change builds the parts that trip runs over and measures them:

| Part | Where | What it does |
|---|---|---|
| Transport trait | `crates/arc-inference/src/stage_net/wire.rs` | `StageSink` / `StageSource`: send and receive whole framed messages. Implemented for persistent tuned TCP (`TcpStageSink`, `TcpStageSource`: one connection per hop for the pipeline's life, `TCP_NODELAY`, 4 MiB socket buffers, one `write_all` per message from a buffer reused for encoding) and for in-process channels. |
| Wire format | same | 40-byte message header plus a 64-byte header per sequence. Each entry carries its per-stage commitment, so any stage can be re-verified on its own. Hostile lengths are bounded before allocation. |
| Exact activation codec | `stage_net/codec.rs` | Zigzag bit-packing of the `i64` residual stream in blocks of 128 values, one width byte per block. Lossless for every input, `i64::MIN` and `i64::MAX` included. Falls back to raw `i64` when packing would not save space. The commitment is BLAKE3 over the canonical little-endian `i64` bytes, identical to `distributed::serialize_activations`, so the codec never changes a digest. |
| Overlap | `stage_net/pipeline.rs` | Each stage runs a reader thread, a compute loop and a sender thread. The next micro-batch computes while the previous one is on the wire. |
| Micro-batched ring | same | Many sequences in flight, optionally grouped per message. Tokens and every per-stage commitment are independent of the split, grouping, codec, transport and overlap. Tests and the benchmark check this against a local, transport-free reference. |
| WAN simulation | `stage_net/shaper.rs` | Per hop: one-way delay of RTT/2, seeded jitter, and a bounded uplink that the sender's queue waits behind. Delivery is first-in first-out. It calibrates the host's timers at start (see *Timer accuracy*). |
| Cost model | `stage_net/cost.rs` | Research-7 §2.1: `t_pass = Σ compute + Σ_hops (r/2 + o + bytes·8/uplink)`, plus an aggregate cap from the busiest stage. Unit-tested against research-7's §2.3 table. |
| Placement optimizer | `stage_net/placement.rs` | Steps below. |
| Benchmark | `crates/arc-inference/src/bin/arc_wan_bench.rs` | Everything below, as a binary (`cargo run --release -p arc-inference --bin arc_wan_bench`). |
| CI | `.github/workflows/stage-net-bench.yml` | Runs the bench on `ubuntu-latest` for pull requests that touch these files. Tables go to the job summary; JSON and Markdown go to the `stage-net-bench` artifact. |

The placement optimizer works in five steps:

1. **Regional cells.** Complete-linkage clustering of the RTT matrix, cut at the cell diameter (research-7 §3.5 and §6.2). A forward pass never leaves its cell unless no cell can hold the model, and that case is flagged `cross_cluster`.
2. **Candidate stages.** Every node is a candidate stage. For MoE models, nodes within 1 ms of each other also form a LAN-island stage that splits each layer's routed experts into contiguous slices by memory.
3. **Search.** Every disjoint set of up to 8 stages is scored. Each extra hop costs `r/2 + o + bytes/uplink`, so fewer, faster stages win by construction.
4. **Ring order.** The ring order is exact (Held-Karp), then every rotation and direction is scored.
5. **Layer slices.** Per-answer placement fills the fastest stages first. Aggregate placement balances stage time.

## For ENG-6 (island runtime, ARC-68)

ENG-6 plugs in at two traits. `StageSink`/`StageSource` is the transport: a
multi-process runtime connects each hop with `TcpStageSink::connect(addr, …)`
and `TcpStageSource::accept(&listener, …)`, or with a future RDMA or
Thunderbolt implementation. `StageCompute` is a stage's forward pass;
`IntegerSlice` is the dense engine's, and the MoE/MLA engine adds its own.
`run_stage` is the per-stage loop (reader, compute, sender, commitments). The
benchmark runs stages as threads of one process over real loopback sockets;
measuring separate processes and machines is ENG-6's acceptance test.

## Results, Studio lab (Mac Studio M2 Ultra, 24 threads, 7 Oct 2026)

Command: `arc_wan_bench --label "Studio lab (Mac Studio M2 Ultra)" --model small`.
Setup:
- Synthetic integer model: vocab 2,048, d_model 1,024, 8 heads, d_ff 2,816, 16 layers (`CachedIntegerModel::synthetic`).
- Each hop carries a Kimi-width frame: the real hidden state, tiled to 7,168 values.
- Jitter ±1 ms; uplink 50 Mb/s on shaped hops.
- Prompt 4 tokens, 12 generated per sequence.

**Every run below was bit-exact.** Tokens and every per-stage commitment
matched the transport-free reference, and tokens matched across 1, 2 and 4
stages.

**A. Per-answer speed, 1 sequence in flight**

| Stages | Hop RTT ms | Pass p50 ms | tok/s per answer | Research-7 model ms (o = 0) | Implied o per hop ms |
|---|---|---|---|---|---|
| 1 | — | 51.01 | 19.6 | 50.47 | — |
| 2 | 0 (loopback) | 50.54 | 19.7 | 51.05 | −0.25 |
| 2 | 10 | 68.29 | 14.6 | 67.65 | 0.32 |
| 2 | 30 | 83.63 | 12.0 | 83.77 | −0.07 |
| 2 | 60 | 114.51 | 8.8 | 113.09 | 0.71 |
| 4 | 0 (loopback) | 45.29 | 22.5 | 44.37 | 0.23 |
| 4 | 10 | 89.96 | 11.1 | 88.53 | 0.36 |
| 4 | 30 | 129.14 | 7.7 | 128.22 | 0.23 |
| 4 | 60 | 186.30 | 5.4 | 184.63 | 0.42 |

**Per-hop breakdown, 4 stages at 30 ms (p50, ms)**

| Hop | Bytes/msg | Bits/value | Serialize (commit + encode) | Transfer | Queue | Deserialize (decode + verify) | Next stage compute |
|---|---|---|---|---|---|---|---|
| 0→1 | 45,072 | 50.2 | 0.031 | 22.039 | 0.029 | 0.022 | 11.265 |
| 1→2 | 45,856 | 51.0 | 0.031 | 22.760 | 0.026 | 0.022 | 11.632 |
| 2→3 | 45,856 | 51.1 | 0.032 | 22.567 | 0.035 | 0.022 | 11.626 |
| 3→driver | 104 | — | 0.001 | 15.069 | 0.032 | 0.001 | — |

Each activation hop's transfer is 15 ms one-way, plus 7.3 ms for 45.9 KB on a
50 Mb/s uplink, plus about 0.2 ms of transport. On loopback with no shaping, a
hop's transfer is 0.12 ms p50.

**B. Exact codec, 2 stages, 30 ms, 50 Mb/s**

| Codec | Bytes/msg | Bits/value | Transfer p50 ms | Pass p50 ms |
|---|---|---|---|---|
| raw `i64` | 57,448 | 64.0 | 24.00 | 80.81 |
| exact bit-pack | 45,856 | 51.0 | 22.06 | 80.42 |

On the `tiny` synthetic model (d_model 128) the same codec reached 27.1
bits/value: 24,352 bytes against 57,448 raw. At 10 ms and 50 Mb/s that cut hop
transfer from 13.98 to 8.66 ms. Bits/value depends on the activations; real
model activations have not been measured here (see *Not done*).

**C. Swarm throughput, 4 stages, 30 ms, 50 Mb/s**

| In flight | Micro-batch | Overlap | Aggregate tok/s | Per-answer tok/s |
|---|---|---|---|---|
| 1 | 1 | off / on | 8.0 / 7.9 | 8.0 / 7.9 |
| 4 | 1 | off / on | 31.3 / 32.4 | 8.2 / 8.3 |
| 8 | 1 | off / on | 56.0 / 62.5 | 7.5 / 8.2 |
| 16 | 1 | off / on | 55.9 / **92.3** | 3.8 / 6.2 |
| 16 | 4 | on | 51.8 | 3.4 |

### What the numbers say

- **The research-7 model holds.** With measured compute and codec cost, and the injected RTT and uplink, the model lands within 0.7 ms per hop of every measured pass.
- **The transport's own cost is small.** Measured per-hop overhead is o ≈ 0.23 ms (median, loopback), against research-7's 1 ms WAN default and 0.3 ms "optimized" figure. Serialize plus deserialize is about 0.05 ms for a 7,168-value frame.
- **Network time is RTT and uplink.** On a WAN hop, what remains is RTT/2 and the uplink term, and only placement and fewer hops reduce those.
- **Overlap matters once the ring is full.** At 16 sequences in flight on 4 stages, aggregate rose from 55.9 to 92.3 tok/s (+65%). With 1 sequence in flight it changes nothing.
- **Grouping sequences into one message did not help on this engine.** 51.8 against 92.3 tok/s. The engine runs each sequence on its own, so a group only delays the stages behind it. Grouping pays once the engine batches matmuls (research-4 E4).
- **The exact boundary is wider than research-7's 2 bytes/value.** On the synthetic models it measured 27–51 bits/value. Research-7 budgets 2 bytes/value (an INT16 boundary), but the boundary carries the residual stream, which the next stage adds into at full precision, so it cannot be cut to INT16 without changing digests.

## Kimi-K2.6-shaped placement (PROJECTION, not a measurement)

The bench's section D runs the optimizer on a K2.6-shaped model and a
14-node population.

Model inputs:
- Shape: 61 layers, hidden 7,168, 384 experts, 582 GB with INT4 g32 experts, about 22.6 GB read per token (ENG-5's `docs/protocol/kimi-k26-checkpoint.md`).
- Boundary bytes per token: the codec's measured bits/value applied to 7,168 values.

Network inputs:
- RTTs: research-7's DC-to-DC medians plus 4.6 ms FTTH access per home.
- Per-hop overhead o: measured in the same run.

Assumption:
- Per-layer decode time assumes **0.6 of peak memory bandwidth**. ARC's integer engine does not reach this today.

With the Studio-lab inputs:

| Scenario | Plan | Pass ms | Per-answer tok/s |
|---|---|---|---|
| Two 512 GB Macs on one LAN (0.3 ms) | 1 island stage, experts 0..192 / 192..384 | 36.9 | 27.1 |
| Home WAN only (London) | 2 stages, 43 + 18 layers | 63.0 | 15.9 |

**59 tok/s per answer needs a pass of at most 16.9 ms.** No WAN pipeline gets
there: the network time alone is at least `S · r/2`. The only path in this
model is compute-bound islands with more bandwidth per layer (more members
splitting the experts) or speculation that commits several tokens per pass
(research-7 §2.2). Network work keeps WAN hops at `r/2` plus 0.2–0.3 ms, but it
cannot remove `r/2`.

## Timer accuracy

The WAN shaper must wait accurately. On the Studio-lab host the agent's process
tree runs at background QoS, and macOS coalesces its timers: a 1 ms
`thread::sleep` took 9 ms and a 5 ms sleep took about 35 ms. The shaper times a
1 ms sleep once at start. If the sleep overshoots, it yield-spins the whole
wait; otherwise it sleeps and spins only the last 250 µs. The bench prints
which mode ran and the measured sleep. Delays now land within about 0.3 ms of
the target, for example 8.63 ms measured against 8.9 ms injected.

## Not done, and why

- **QUIC transport.** Not built. One persistent ordered stream per hop gains nothing from QUIC's stream multiplexing, and QUIC would pull a tokio runtime and TLS into the engine crate. The trait leaves room for it, or for RDMA.
- **Separate processes and machines.** Stages here are threads joined by real loopback sockets. Multi-process and multi-host runs belong to the ENG-6 runtime, which plugs into these traits.
- **Codec bits/value on real model weights.** Only synthetic activations were measured. The model files on the Studio sit in a node data directory, which the task rules keep off-limits.
- **MoE/MLA compute.** The bench's stage compute is the dense integer engine. The optimizer models MoE islands, and the MoE/MLA engine (ENG-5) plugs in through `StageCompute`.
- **Speculative decoding** is out of scope here. It is the lever the projection points to for 59 tok/s per answer.
