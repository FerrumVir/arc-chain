# Regional swarm runtime (ENG-6)

The deployment target is many ordinary community nodes in a measured-RTT region.
A 16 GB node is a normal member; big-memory machines are optional. About 600 GB
of K2.6 weights and roughly 40+ stages/expert groups are **planning assumptions
from TJ's 7 Oct scope correction**, not a demonstrated capacity fit. KV,
activation logs, runtime overhead and redundancy need additional capacity.
ENG-7 owns fitting and discovery. This runtime does not contact live ARC nodes.

The legacy `island` paths and `arc-island` executable remain compatible.
`RESULTS.md` is historical topology evidence, not the current deployment target.
No Kimi speed claim is made. The synthetic profile's existing golden digests
are unchanged.

## Node churn and heterogeneous service times

`replica.rs` adds a stage relay compatible with the existing ring. Configure
one relay for each stage and ordered replica endpoints holding the same stage.
Each replica session owns fresh sequence caches. The relay preserves accepted
inputs and hashes of their entire responses. If a connection fails or times out,
it connects the next replica, checks its pinned config/range/weight identity,
replays accepted frames and checks every response byte, then resends the pending
frame. A reply lost after computation is safe: the failed session has no route
to forward duplicate outputs into the ring. A divergent replay is rejected;
exhausted replicas fail closed. No digest is relaxed for churn or slow nodes.

`DeadlineTcpTransport` bounds connection, read and write waits. Endpoints must
be numeric addresses (discovery resolves DNS beforehand). Configure each
relay's timeout for its stage's measured compute and RTT. Stages progress
independently and the existing micro-batch scheduler overlaps streams across
them. Uneven cuts and arbitrary replica ordering allow heterogeneous nodes;
placement and automatic deadline tuning belong to ENG-7/ENG-9.

Example command contracts (addresses supplied by consent-gated formation):

```text
arc-island replica --package stage.arcspkg --listen HOST:PORT
arc-island relay --listen RELAY:PORT --next NEXT:PORT \
  --replicas PRIMARY:PORT,SPARE:PORT --stage-id PINNED_HEX \
  --timeout-ms 1000 --journal-mib 256
```

The replica hello exposes `stage_id`; formation must pin the expected identity
from verified package bytes before use. Identity is a consistency check, not
peer authentication or proof that a malicious worker ran those weights. Existing
commitment audits remain necessary. `--sessions`, `--drop-reply-at` and
`--reply-delay-ms` support finite process tests and synthetic churn/heterogeneity.

Current recovery limits: this is warm activation replay, not hot KV mirroring.
The relay and its journal must survive. The journal is bounded and refuses work
before overflow; it is not silently truncated. Relay/coordinator crash recovery,
journal compaction and durable replicated logs are not implemented. The relay
adds request/response traffic; the direct-ring regional budget does not measure
that extra RPC cost. TCP endpoints are a lab protocol, without public-internet
authentication/encryption or abuse controls; deployment remains future work.

## Integration interfaces

- **ENG-8:** `Frame::Tree { id, prefix, nodes }` traverses every stage once.
  Topologically ordered `TreeNode`s name one token and either the live prefix
  or an earlier node as parent. Each stage forks the corresponding KV locally,
  returns per-node boundary/logits commitments and sampler selections, and drops
  temporary branches. The prefix is unchanged even on error. Accepted paths are
  committed later through ordinary `Step` frames. Proposal/acceptance policy,
  fused tree attention, prefix-cache sharing and tree memory admission belong to
  ENG-8; this initial implementation clones caches and caps trees at 4096 nodes.
  Use `Coordinator::forward_batch` to establish a live prefix,
  `Coordinator::verify_tree` to verify it and `close_sequences` to release it;
  these synchronous calls require no other frames in flight.
- **ENG-9:** `Transport`/`Listener`/`Link` remain the frame transport boundary.
  `ReplicaConnector`/`StageSession::exchange` expose recovery sessions for alternate
  transports. Every connect must yield empty KV. Relays occupy independent ring
  stages, so a slow stage applies backpressure without globally serializing all
  streams. Transport/fused compute optimizations must preserve response bytes.
- **ENG-1:** `Step { items }` and `Schedule` carry batched independent streams.
  No cross-sequence fused GEMM is introduced. Tests compare a sequence alone and
  among neighbours, including multi-item frames through replica failover.
- **ENG-7:** `RemoteExperts::connect_placed` accepts an explicit expert-ID to
  device-ID map, with modulo placement retained as a compatibility default.
  `--expert-owners` exposes it for `stage` processes. Partial sums stay exact i128.
  Current ownership maps apply across the stage's layers; expert RPCs remain
  sequential and their WAN cost is not included in the direct pipeline budget.
  Expert servers still open layer packages; this change does not create selective
  expert-only package files or prove a real Kimi memory fit on 16 GB hardware.

## Evidence and reproducibility

Targeted tests cover every contiguous partition of a four-layer model in three
formats, a 40-stage process pipeline, tampered-stage/token rejection, arbitrary
expert ownership, and draft siblings/grandchildren compared to separate exact
paths. Replica tests lose a reply after execution, reject altered replay, and
continue byte-identically after each of three stages fails over between separate
TCP processes. Injected response delays exercise heterogeneous speeds.

CI `.github/workflows/island-runtime.yml` measures 40 separate stage processes
at **assumed** regional RTTs of 5, 10 and 20 ms, with 1 and 40 streams, 100 Mbit/s
uplinks and Kimi-width activation padding. RTT is divided by two for each
one-way shaped hop. The coordinator-to-first-stage handoff is local; 40 shaped
hops include the return. Compute is synthetic tiny MLA/MoE, not Kimi weights.

```sh
arc-island synth --shape regional-40 --out regional.arcspkg
arc-island regional-bench --package regional.arcspkg --out regional.json --threads 1
python3 scripts/arc_island/regional_report.py regional.json regional.md
```

Every cell checks tokens, logits hashes and all layer-boundary digests against
the whole model. JSON records per-stage forward time, positions, outgoing data
frames/bytes and measured hop residence (queueing, propagation, timer overshoot,
send). It separates configured assumptions from measurements. The report gives
per-output-token compute, network residence, overlap lower bound, residual and
elapsed wall budgets; totals include prefill. Aggregate wall time includes
pipeline fill/drain; per-answer decode excludes the first token. Summed service
minus wall bounds overlap; this is not a trace proving the precise critical path.

The attached local report uses a **debug build on the shared Studio host**;
other targeted checks ran on the host during part of this measurement. CI uses
release builds and uploads `regional-ci.json` and `regional-ci.md` in the existing
`island-bench-ci` artifact. Neither result establishes the ≥59 tok/s Kimi target.
Real community nodes, real model weights, the combined speculation/batching path,
and replica/expert WAN latency measurements remain outstanding.
