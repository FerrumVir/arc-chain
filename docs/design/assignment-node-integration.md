# Assignment in the running node (S1-S9): what remains, and how

Status: design, 2026-09-22. The requirement: different nodes compute
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

## What the running node still lacks, in dependency order

1. **Real link measurements (S2).** The validator transport is QUIC
   (`quinn`), which keeps a smoothed RTT and loss counters per connection.
   `PeerConnection` stores only the send stream; store the
   `quinn::Connection` too, expose a read-only snapshot
   (`rtt()`, `stats().path.lost_packets` / `sent_packets`) through the
   consensus manager, and summarise it with `link::summarize(…, simulated =
   false)`. Bandwidth needs an explicit probe (a bounded payload echoed by a
   member), rate-limited per peer.
2. **Lease exchange (S1).** Gossip each member's signed `CapabilityLease` on
   the validator mesh as a new bounded message, refreshed every N heights and
   expiring; accept only from frozen-committee members (`lease::validate`),
   latest nonce wins.
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
   QUIC transport with mutual validator-key authentication, replacing the
   SSH stdio sidecar. Workers stay stateless (the coordinator runs attention
   and holds the KV cache), which is what makes request-boundary and even
   per-call reassignment safe (S7).
6. **Job authorisation (S4, decision D19).** The job's `assignment_hash`
   names an allowlisted policy hash (`policy_hash(policy, verification)`);
   the coordinator issues a certificate per request; votes carry its hash;
   state recomputes it under the allowlisted policy before counting votes.
   This changes consensus: a new protocol minor version with an explicit
   activation, like D20.
7. **Payment for partitioned work.** Replication pays the certificate's
   signers. Partitioned work needs a settlement rule for slices (for example
   credits by rows computed and verified). Owner decision.
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
rows; remote row workers equal to local rows bit for bit, a worker leaving
mid-query aborting the query and the next placement omitting it; D19 votes
refused for a certificate that does not recompute; the S10 run itself.
