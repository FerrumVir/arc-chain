# Assignment in the running node (S1-S9): what remains, and how

Status: design, 2026-09-22; trust model and v1 rules added after an
independent review the same day. The requirement: different nodes compute
different pieces of one query, assigned automatically from measured
capability. S10 (the original single-query comparison across machines) is
**FAIL** and stays FAIL until a multi-machine run passes; nothing below
changes that by itself.

## What exists

- `arc-assign` (pure, tested): signed capability leases and challenges,
  pessimistic link summaries, deterministic cost-aware placement, assignment
  certificates any validator can recompute, verification plans (spot rows,
  duplicate slices), a fair bounded queue.
- The native worker uses the fair queue: requesters take turns, expiry drops
  a request before it runs, a full queue is backpressure (S6; written, not
  compiled).
- `crates/arc-node/examples/assigned_partition_local.rs` drives the whole
  chain on real canonical-I8 rows on one host: leases, a challenge that
  refuses a false claim, placement over all 225 projections, a certificate
  recomputed before use, the partitioned backend with exact logits at every
  forward, the verification plan executed, and request-boundary reassignment
  when a worker leaves (`--churn`). Labelled local / in-process / simulated
  links (written, not run).
- The paid route that runs today is full replication: every committee
  member executes the whole request and payment needs a >2/3 stake
  certificate (P5, soak). It does not meet the different-pieces requirement.

## Trust model and v1 rules (review of 2026-09-22)

A native certificate means that members holding more than 2/3 of stake each
computed the same output. That is evidence only while their computations are
independent, so that a byzantine minority cannot make honest members agree on
a wrong answer. Partitioning removes that independence once members share
helpers. A worker that returns the same wrong rows to every member it serves
makes all of them compute the same wrong output. If those members, together
with any byzantine members, hold more than 2/3 of stake, the wrong output is
certified and paid.

Sampling does not close this. With the local harness's rule (100 per mille
duplicate slices, 2 spot rows per stage), one corrupted row in a 4,096-row
stage escapes a member's checks with probability 0.9 x (1 - 1/4096)^2 ≈ 90%,
and the independent checks of four members about 65% of the time. The
earlier design's "a worker is trusted only up to the sampling probability"
holds for the member's own execution; it does not hold for the certificate.

**Hard rule, whatever D19 decides:** a member's vote may count toward a
certificate only if every row its output depends on was computed by that
member, meaning its own machines under its own validator key, or was
recomputed by it in full. Rows from another validator or a community node
may feed only an answer that is labelled uncertified.

The options for the owner (D19), none implemented:

- (a) **Operator-local partitioning.** One validator spreads a query across
  its own machines, each holding a machine key delegated by the validator
  key. Certificates keep their meaning, consensus does not change, and the
  operator is paid as one signer. Different nodes compute different pieces
  of one query, but all the nodes belong to one operator. Community nodes
  add nothing to certified work.
- (b) **Fast path, replicated finality.** Any helpers, community nodes
  included, produce a fast answer that is shown as not yet certified. The
  certificate still comes from members that computed independently. Cost:
  full replication plus the helpers' work. Helpers are unpaid unless a new
  rule pays them.
- (c) **Verified cross-operator partitioning.** Every helper slice is
  recomputed by an independent party, or proven. This is a new trust
  assumption ("no two helpers collude" is weaker than "less than 1/3 of
  stake is byzantine"). Research, not v1.

Recommendation: (a) for v1, because it keeps what a certificate means; (b)
as the product route for community nodes; not (c). This is the owner's call,
because it decides in what sense TJ's different-pieces-of-one-query
requirement is met. S10 stays FAIL under every option until a multi-machine
run passes.

Answers to the review, per point (the rest of this document follows them):

1. **Reservations.** A placed slice holds one of the worker's
   `max_concurrency` call slots, keyed (worker, request), from placement
   until the member's execution ends: finalized, refund-eligible
   (`height + 1 >= expires_at`), aborted or reassigned. Placement sees
   free slots, not the lease maximum. Expired reservations are swept every
   height. Reassignment moves a reservation in one step, and a committee
   change drops every reservation for a departed worker. The ledger never
   holds more than the sum of the current leases' slots. The requester's
   escrow is untouched: partitioning is how one member computes its own
   vote, and the request still settles only through the certificate or the
   refund. On the worker, the fair queue's hard capacity stays the backstop:
   a refused call is computed by the member itself.
2. **Verification in the node.** The member runs the plan during its own
   execution. It recomputes the spot rows from its own copy of the rows and
   sends each duplicate slice to its checker. On a `Fault`, the slice is
   recomputed locally before the output is used, so the output stays exact.
   The worker is excluded from that member's placements for the rest of the
   epoch, and the finding (both digests and the certificate hash) is kept,
   one per worker per epoch. v1 has no payment effect and no on-chain
   penalty; slashing is an owner decision. Under (a) the plan guards a
   member against its own faulty machines, not against other parties.
3. **Coordinator trust.** Nobody chooses a coordinator: every member
   coordinates its own execution and is trusted for its own vote, as today.
   Placing a share on itself is expected (`include_coordinator`). The
   measured inputs are already bound into the certificate: measured rates,
   capped at `min(measured, claimed)` by `check_challenge`, and link
   figures. Recomputing a certificate therefore proves that the placement
   follows from the bound inputs under the allowlisted policy. It does not
   prove the measurements true. The certificate is the member's audit
   record, not a consensus input, under (a) and (b).
4. **Reassignment (v1: fall back per call, never abort by default).** A
   failed, timed-out or refused call is recomputed by the member, or by
   another of its machines that holds those rows. Workers are stateless, so
   no KV state is touched. After `k` consecutive failures the worker is
   dropped for the epoch and the next request's placement omits it. If no
   participant holds the rows, the member abstains: it casts no vote, nothing
   is paid for partial slices, and the request settles through the other
   members' certificate or refunds after expiry.
5. **Bounded state (proposed bounds; each count exported as a gauge that
   `growth.py` can fit).**
   - leases: one per committee member, evicted at expiry and on committee
     change
   - challenges: one in flight and one result per member per epoch, cleared
     at epoch change
   - links: the last 32 probe samples per member, plus one counter snapshot
     per connection generation
   - reservations: at most the sum of lease slots, swept every height
   - certificates: one per request in flight (the worker queue's capacity),
     then a ring of the last 256 for evidence
   - findings and exclusions: one per member per epoch
6. **D19 and mixed versions.** Under (a) and (b), votes and certificates do
   not change, so no activation is needed. Partitioning is invisible to
   consensus, because the output is bit-identical by exact arithmetic. If a
   later version carries an assignment-certificate hash in votes, that is a
   new vote layout, and old nodes cannot read it. The gossip decoder
   (`arc-net` `deserialize_message`, an exact decode) refuses the extra bytes.
   Inside a block the encoding is positional, so an old binary fails to
   decode the new layout or decodes it wrong. Ignoring the field and
   counting the vote is therefore not an option, and neither is running
   mixed versions. It needs an activation record like D20's: the record
   carries the selection rule and the assignment-policy allowlist as
   separate versioned fields, both take effect at the record's height, every
   node upgrades before that height, and a binary that does not know a field
   refuses the record rather than ignoring it.
7. **Measurement.** In quinn 0.11.11 (quinn-proto 0.11.17), `Connection::rtt()`
   is one smoothed estimate, and `stats().path.lost_packets` and
   `sent_packets` are cumulative for the life of a connection. Loss is
   therefore computed from deltas between snapshots of the same connection
   generation; a reconnect starts a new window rather than producing a
   negative or oversized delta. p95 RTT and bandwidth come from explicit
   timed probes, not from the smoothed estimate. The probe responder has the
   challenge's limits (members only, bounded payload, one probe per peer per
   epoch) and answers with less than it received. The prober pays for the
   bytes, so a probe cannot be used for amplification.
8. **Layer split over WAN (decision line).** Row partitioning needs at least
   65 sequential round trips per token, so v1 limits it to a LAN (S10). A
   layer (pipeline) split costs about one hop per stage boundary per token.
   It is the only split that could be usable across a WAN. It does not beat
   one machine on latency, but it serves models that do not fit one host.
   It needs stateful workers (KV per layer), so reassignment is
   request-boundary only. Recorded as an owner option, not v1. The WAN limit
   of row partitioning is therefore a chosen tradeoff.

## Written under option (a), 2026-09-22 (not compiled; parse-checked)

The node side of option (a) exists as source, pending compilation after the soak.
- `arc-inference`:
  - `try_generate_v2_with_backend`: generation v2 token by token on a projection backend.
  - `VerifiedPartitionBackend`: per-call fallback to local rows, duplicate and spot checks, and every fallback and fault reported to a sink.
  - Symmetric row-frame codecs and `serve_row_frames`.
  - The `tensor_row_model_worker` example: a machine holding the whole artifact serves any assignment, which dynamic placement needs; the row-file sidecar holds one fixed slice.
- `arc-assign`:
  - `book::Offer` and `candidates_from`, so the operator's configured machines and other validators' leases feed one placement path.
  - Reservations and the bounded books.
- `arc-node`:
  - `row_cohort`: the `--native-row-workers` JSON config of the operator's machines (SSH with pinned host keys).
  - Per-epoch challenges this node recomputes.
  - Probes from each real call's timing.
  - Per request: placement, a reservation, a certificate, and a run on the verified backend. The request runs locally when placement says this node alone is faster, or when anything prevents a placed run.
  - Exclusion after a fault or three consecutive failures.
  - A 256-record ring behind the read-only `GET /assignment/cohort`.
- The canonical executor runs through the cohort via `NativeExecutor::execute_at`, which the worker now calls with the chain height.

After an independent review the same day (all uncompiled):
- Responses now carry their own frame magic (`ARCTR001`), so a request frame, or a worker that echoes one, is never read as a response. The row-file sidecar example changed with it.
- The SSH remote command is quoted with POSIX `'\''`; the old quoting let an apostrophe end an argument's quoting.
- ssh runs with no configuration file (`-F none`), no global known-hosts file and no host-key updates, so the pin is exclusive.
- An excluded machine's slices are reported as `Skipped`, so none moves silently.
- A machine's connection closes itself on any failure; it is now reconnected at its next measurement.
- Measurement:
  - A warm-up call under `startup_timeout_ms` absorbs the remote model load.
  - Eight one-row pings give the round trip, and a wide call gives the bandwidth. A real call's time mixes compute and queueing into the round trip, so it no longer counts as a probe.
  - The challenge's compute time excludes the round trip.
  - Stale links are refreshed between epochs.
- Reservations are released by a drop guard on every exit path.
- `/assignment/cohort` answers loopback callers only. The production nginx allowlist ends in `return 404` and does not list it.

A second review of those fixes followed (still uncompiled):
- Machines are connected and measured on a background thread, so neither startup nor a paid request waits on a connect or a model load. A request uses whatever has been measured so far.
- A closed connection's slices are reported as `Skipped` and are not counted as failures. Before this, one timeout became three failures within a single token.
- An unreachable machine has no refused challenge on record, so it takes the epoch's challenge once it answers.
- A successful reconnect lifts an exclusion for failures, never one for a fault.
- The challenge is 4,096 rows, and neither it nor the bandwidth probe subtracts a round trip it does not clearly exceed, so jitter cannot inflate a rate. The bandwidth probe takes the median of three wide calls.
- known_hosts must be a plain absolute path, checked when the config loads.
- ssh adds `CheckHostIP=no`, a 10 s connect timeout and keepalives.
- `is_open()` is a flag, so the operator view never waits on a call.
- The view also refuses proxied requests: the repo's `deploy/nginx.conf` forwards `/rpc/` from 127.0.0.1, and now denies `/rpc/assignment/` as well.

A third review, of the threading, followed (still uncompiled):
- Each machine is measured on its own thread and recorded as soon as it finishes, so one slow machine delays no other. The recording uses the latest height.
- A drop guard clears a machine's in-flight mark however its measurement ends, even when no thread could be created.
- A challenge that was never sent is never recorded as refused.
- A challenge answered for an epoch that has since ended is dropped.
- An open machine that missed the epoch's challenge takes it at the next request.
- A reconnect lifts an exclusion for failures only if the machine is actually excluded, only in the epoch the measurement started in, and at most once per machine per epoch. A reconnect of a machine that is not excluded leaves its failure count alone, so a machine that answers pings but fails its real calls still reaches an exclusion.
- A final review added: probing stops once the session closes, so calls that send nothing are not failed probes; a round whose challenge could not be prepared delays the next challenge attempt by a refresh interval; the `Host` check is exact (brackets for IPv6, digits only after the colon); a refusal is logged only when the book recorded one; and a poisoned lock reports `Closed`.
- The challenge's input and expected answer are computed once per round, not once per machine.
- A poisoned call lock now reads as closed, and the child is still killed when the worker is dropped.
- Call deadlines are clamped to one hour.
- The operator view also refuses browser requests (`Origin`, `Sec-Fetch-*`) and any `Host` that is not loopback, which covers DNS rebinding.
- Known limit: a refresh of an open, placed machine shares its SSH session with that machine's paid calls, so a paid call can wait behind one short measurement call.

Not written: the validator-mesh lease gossip and QUIC probes (options b and c), D19, and payment for other operators' work. S10 stays FAIL until a LAN run on real machines (E3) shows distinct slices, exact output and a measured comparison with local execution. The coordinator's own share still copies its rows per call (`rows_of`), which S11 should replace with a row-range kernel entry before any timing is quoted.

## What the running node still lacks, in dependency order

1. **Real link measurements (S2).** The validator transport is QUIC
   (`quinn`), which keeps a smoothed RTT and loss counters per connection.
   `PeerConnection` stores only the send stream; store the
   `quinn::Connection` too, and expose through the consensus manager a
   read-only snapshot per connection generation: `stats().path.lost_packets`
   and `sent_packets` for windowed loss, and `rtt()` only as a coarse
   smoothed figure. RTT samples and bandwidth come from explicit timed
   probes: a bounded payload sent to the member, which answers with a short
   timed acknowledgement, rate-limited per peer (point 7). Summarise them
   with `link::summarize(…, simulated = false)`.
2. **Lease exchange (S1).** Gossip each member's signed `CapabilityLease` on
   the validator mesh as a new bounded message, refreshed every N heights and
   expiring; accept only from frozen-committee members (`lease::validate`),
   latest nonce wins. That is the form for (b) and (c). Under (a) the
   exchange is between one operator's machines, and a lease is valid only
   under a delegation from that validator's key, which `lease::validate`
   does not check yet.
3. **Challenges (S1).** A coordinator-signed challenge (tensor, rows, input
   seed, deadline) sent to a member, answered with the rows' hash and the
   elapsed time; the coordinator recomputes the rows from its own copy of the
   artifact and replaces the claimed rate with the measured one
   (`check_challenge`). Members only, one challenge per peer per epoch: a
   public challenge endpoint would be a free compute-exhaustion vector.
4. **Placement inputs (S3).** Model transfer cost for a member without warm
   rows (weight bytes / measured bandwidth, once), warm rows from the lease,
   queue depth from the lease, and the request's deadline from `expires_at`.
5. **Remote row workers (S5).** Row projection requests over the validator
   QUIC transport with mutual authentication (under (a), a machine key
   delegated by the validator key), replacing the SSH stdio sidecar. Workers
   stay stateless (the coordinator runs attention and holds the KV cache),
   which is what makes per-call fallback safe (S7, point 4 above).
6. **Job authorisation (S4, decision D19).** Under (a) or (b), none in
   consensus: the member keeps its certificate as evidence behind a
   read-only `/assignment/*` view, and votes are unchanged (point 6 above).
   Only a design in which votes carry the certificate hash changes
   consensus. Then the job's `assignment_hash` names an allowlisted
   `policy_hash(policy, verification)`, state recomputes the certificate
   before counting votes, and an activation record like D20's is needed.
7. **Payment for partitioned work.** Replication pays the certificate's
   signers. Under (a) the operator is paid as one signer and pays its own
   machines. Paying helpers from other operators (b, c) needs a new
   settlement rule, for example credits for rows computed and verified. It
   must never pay for a free, cached or degraded result. Owner decision.
8. **The S10 run.** At least two real hosts on a LAN (Stage A measured WAN
   RTT that makes exact row partitioning 34x slower than local): distinct
   slices per host, exact combined output against the unpartitioned model,
   measured TTFT and decode against local, and duplicated work accounted.
   Needs hardware (E3) and, for anything beyond LAN, an authorised WAN run
   (E4).

## Tests each step needs

Link snapshot vs a loopback pair with injected delay; lease gossip refusing a
non-member, an expired and a replayed lease; a challenge answered wrongly or
slowly refused; placement moving rows away from a slow link and toward warm
rows; remote row workers equal to local rows bit for bit; a worker leaving
mid-query has its remaining calls computed by the member, the output still
exact, and the next placement omitting it (the member abstains only when no
participant holds those rows); reservations released on finalize, refund
eligibility, abort and reassignment, and never over the lease slots; a spot
or duplicate fault recomputed locally, the worker excluded for the epoch;
loss windows across a reconnect; every bounded collection at its bound; D19
votes refused for a certificate that does not recompute (only if votes carry
it); the S10 run itself.
