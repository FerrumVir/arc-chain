# Single-query distributed work sharing and automatic assignment (S1-S11)

Status: design + implementation plan, 2026-09-21. Grounded in what exists:
`arc-inference::tensor_parallel` (stateless row workers, exact row coverage,
pinned SSH stdio transport, a two-machine exact-logits proof that was 34x
slower over WAN), `arc-node::planner` (a deterministic planner with no runtime
caller) and advisory `/shards/*` endpoints. Stage A's verdict stands: exact
tensor-row parallelism is round-trip bound (>= 65 sequential exchanges per
token), so over WAN it cannot be faster than one machine. This design makes
assignment real, safe and honest about when distribution helps - including
choosing NOT to distribute.

## Partition route (S5)

Tensor-row parallelism per projection, as proven: every worker owns disjoint
output rows of a canonical-I8 matrix and returns them for one call; the
coordinator (the executing validator) keeps embeddings, attention, norms,
the KV cache and sampling. Consequences that the rest of the design relies
on:

- workers are STATELESS across calls - no KV, no position - so a worker can
  be replaced between any two projection calls without mixing cache
  histories (S7), and any two workers holding the same rows return identical
  integers (exact arithmetic), which makes redundancy checkable (S8);
- per token, every projection is a round trip, so per-token latency is at
  least `stages x RTT`; the placement model below prices that in.

Layer (pipeline) partitioning stays the route for models that do not fit one
host; it does not reduce single-query latency and is out of scope here.

## Capability leases (S1)

A worker states what it can do in a `CapabilityLease`, signed by its
validator key over a domain-separated hash:

- identity: validator address, pinned transport id;
- what: artifact id, execution profile, backend + kernel-set digest,
  supported operators (`row-projection-i8`), resident rows (warm) per tensor;
- how much: RAM headroom, measured row throughput per tensor shape class,
  max concurrent calls;
- when: issued/expiry heights, nonce.

Validation: signature; signer is a frozen committee member (dedupe by that
key, never a node name - S4); artifact/profile match; unexpired; operators
cover the job. Claimed throughput is not believed: the coordinator issues a
challenge projection with a known answer and times it; a worker whose
measured rate is below its claim by more than the tolerance is refused
(dishonest capacity). Leases are refreshed before expiry; an expired lease
takes the worker out of placement.

## Network measurement (S2)

Per coordinator-worker link: RTT (median, p95), jitter, bandwidth, failure
rate, from probe exchanges over the pinned transport. Measurements carry
their sample count and time; placement refuses links measured too long ago.
Simulated conditions (added latency, loss, bandwidth caps) are applied at
the transport adapter in tests and labelled as simulated in evidence.

## Placement (S3)

Deterministic function of (model shape, valid leases, fresh links, policy):

```
per token, for a candidate worker set W with row shares s_w:
  compute(W)  = max_w ( rows(s_w) * cols / rate_w )           per stage
  comm(W)     = max_w ( rtt_w + bytes(s_w) / bandwidth_w )     per stage
  T(W)        = stages * (compute(W) + comm(W)) + cold_load(W)
choose W (including the empty set = run locally) minimising T,
shares proportional to measured rates (unequal slices), each within the
worker's RAM headroom and concurrency.
```

It therefore chooses FEWER workers when communication dominates, and none
when the coordinator alone is fastest - which, on today's measured WAN, is
always. Ties break by validator address, so every node computes the same
placement from the same inputs (deterministic replay, S4).

## Assignment certificate (S4)

`AssignmentCertificate` binds: the job (request id), artifact, profile, the
placement (per stage: row ranges -> validator + transport id), the lease
digests and link-measurement digest it was computed from, the assignment
epoch, and the verification rule. Its hash is the evidence a validator
attaches to its execution; any validator can recompute the placement from
the bound inputs and refuse a certificate that does not reproduce. The
on-chain job's `assignment_hash` names the POLICY (placement algorithm +
verification rule version), which the activation allowlists; concrete
worker sets are per execution. Worker compensation is outside the native
settlement today and must never be created by a free, cached or degraded
result.

## Verification of partitioned work (S8)

- Redundancy: a deterministic, seeded subset of stages (seed = hash of the
  request id and certificate) is computed by two workers; any difference is
  proof of a fault (exact arithmetic), the stage is recomputed locally, the
  worker is excluded for the epoch and the evidence kept.
- Spot checks: a seeded sample of rows is recomputed by the coordinator.
- Cost: redundancy fraction r adds r x worker compute; spot fraction s adds
  s x local compute. Trust remaining: the coordinator is trusted for its own
  execution - which is exactly what the committee's per-validator execution
  and supermajority vote already covers - and a worker is trusted only up to
  the sampling probability of catching it.

## Queues and churn (S6, S7)

A bounded, fair queue per worker across requests (round-robin by request,
per-request deadline, cancellation, capacity reservations from the lease's
max concurrency). A failed or slow call is retried on another worker holding
the same rows, or computed locally; because workers are stateless this never
touches KV state. A worker that fails repeatedly is dropped for the epoch
and the placement recomputed at the next request boundary.

## Tests (S9) and measurement (S10, S11)

Pure-function tests with synthetic leases/links: mixed speed and memory,
unsupported operators/profiles, dishonest capacity, contention, cold/warm,
slow and failed links, dropout, cancellation, replay, duplicate and stale
responses. Measurement: 1/2/4/8 row workers as separate processes on ONE
host (labelled as such - they share cores, so no speedup is expected from
this configuration and none will be claimed), exact-logits equivalence to the
single-process forward, compute-only vs fully verified TTFT/decode, total
duplicated work; the existing two-machine WAN result is reused, not rerun.
The primary performance gate stays FAIL unless a measured configuration
beats the single-machine baseline.

## Implementation map

- new crate `arc-assign` (pure, no I/O): leases, links, placement,
  certificate, verification plan, queue - with the tests above;
- `arc-inference::tensor_parallel`: consume a certificate's row ranges,
  execute redundancy/spot checks, stateless reassignment;
- `arc-node`: lease exchange over the validator transport, challenge
  benchmarking, link probes, runtime scheduler state, `/assignment/*`
  read-only views.
