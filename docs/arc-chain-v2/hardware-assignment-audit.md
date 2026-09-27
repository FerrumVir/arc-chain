# Hardware and automatic assignment audit

Audit basis: `main` at `f616705`, 2026-09-19. Read-only source review; no
network writes, hardware probes, or repository changes.

## Verdict

The implemented path targets a constrained, operator-provisioned INT8
pipeline with authenticated shard announcements and bounded fail-closed
execution. This source review does not establish end-to-end correctness, and
the requested universal hardware/model/network-agnostic autoassignment is not
implemented. “Auto” currently means selecting from declared
metadata or an already announced topology; it does not discover arbitrary
compute, benchmark it, or safely reassign stateful work after failure.

## What is implemented

- The deterministic chain planner (`crates/arc-node/src/planner.rs:65-180`)
  sorts requests and keys, chooses fixed five-layer buckets, uses a coarse
  100 MB/layer RAM heuristic, and picks top-k nodes by remaining advertised
  RAM. `CapacityAdvertisementBody` contains only RAM, VRAM, bandwidth, uptime,
  stake, and region (`crates/arc-types/src/transaction.rs:1435-1449`). It does
  not require model/profile support, measured throughput, RTT, or load. A
  repository-wide search found no runtime caller of `compute_assignment`; this
  proposal path is currently an isolated planner/test module.
- `/shards/join` is an unauthenticated advisory gap finder. It returns a range
  but explicitly does not mutate topology (`crates/arc-node/src/rpc.rs:
  16072-16216`). Startup calls it with hard-coded 32 layers, canonical INT8,
  `gpu_tier=0`, and locally detected RAM (`crates/arc-node/src/main.rs:
  5663-5688`). `/shards/auto_plan` is also advisory and uses proportional RAM
  plus a 1.5 GPU bonus; its source says GPU detection is TODO
  (`crates/arc-inference/src/distributed.rs:173-228`, `rpc.rs:15978-16015`).
- Community workers are whole-prompt workers, not layer/tensor participants
  (`rpc.rs:12231-12255`). Registration and claim enforce signed identity,
  exact model id, and canonical INT8 (`rpc.rs:11970-12078,14411-14554`), with
  90-second heartbeat expiry. Dispatch is a FIFO queue with first compatible
  claimant; it has no measured CPU/GPU capacity, throughput, RTT, or bandwidth
  placement policy (`rpc.rs:5915-6048,14484-14545`).
- Shard announcements are substantially better authenticated: the holder signs
  the exact validator destination/domain, and the receiver validates the
  signer, profile, artifact id, routable origin, and destination identity
  (`rpc.rs:11420-11544`). The pipeline filters exact model/profile, removes
  stubs, and orders replicas by local EWMA (`rpc.rs:1075-1275` and
  `812-865`). This is local latency-aware replica ordering, not network-wide
  placement. Assembly deduplicates by self-chosen `node_name` rather than
  validator key (`rpc.rs:1127-1137`).

## Missing safety and capability coverage

- No live RTT/bandwidth-aware assignment, CPU/GPU benchmark, concurrency/load
  signal, model-family compatibility matrix, or verified hardware capability.
- `distributed.rs:31-44,231-275` describes layer and MoE expert assignments;
  the dense executor is pipeline-parallel. There is no implemented tensor
  parallel partition/all-reduce capability negotiation. `expert_indices` is
  metadata, not proof that an execution route supports it.
- Registry TTL removes stale announcements, but there is no runtime planner
  that creates replacement assignments. A mid-stream shard failure cannot
  hand off to a cold replica because KV history is not replayed; the code
  explicitly fails with `kv_cache_out_of_sync` (`rpc.rs:9337-9395`). Cleanup
  evicts request KV state (`rpc.rs:10017-10090`). No checkpointed KV transfer,
  resume token, or durable assignment epoch exists for this route.
- `/inference/auto` chooses sharded, local, or community by availability
  booleans (`rpc.rs:16221-16310`), not predicted cost or hardware fit.

## Closure criteria

Add a signed, finalized capability lease binding validator identity, artifact
and profile, memory headroom, kernels/parallel mode, measured throughput,
concurrency, RTT/bandwidth samples, and expiry. Make placement replayable and
model/profile aware, dedupe by validator key, and persist assignment epochs.
Add replacement planning plus KV checkpoint/replay or only reassign at a
request boundary. Add explicit tensor-parallel capability and collective
protocol if tensor splitting is desired. Gate payment on the resulting
assignment certificate and fixed quorum. Until these exist, advertise the
system as authenticated pipeline replication with declared capacity, not
universal automatic hardware assignment.
