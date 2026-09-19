# ARC Chain V2: inference and release contract, draft 1

September 19, 2026. Engineering contract for local implementation and isolated tests. This document does not activate a protocol, authorize a production restart, or certify Byzantine safety. V2 is the product plan name; the recovered chain currently uses protocol 3. Keep existing paid-inference and consensus guards in place until the replacement passes its gates.

## Decisions that unblock implementation

**Separate execution from payment.** An HTTP result, shard majority, cache hit or community-worker reply is execution evidence. Payment requires a state-validated native result and a canonical receipt. Community rewards and caller-funded native escrow are separate economic paths. A result cannot receive both rewards for the same billable work unless an explicit economic rule authorizes it.

**Freeze identity and semantics before dispatch.** Define a versioned job commitment over chain/genesis identity, recovery epoch and validator-set identity, requester and nonce, immutable model bytes, operator/arithmetic profile, tokenizer/config, input commitment, generation parameters, output/resource limits, absolute expiry, assignment commitment and quoted maximum payment. Bind all fields into the caller's signed request using one canonical encoding. Display names, URLs, hardware tiers and model filenames are not identities. The state layer recomputes the commitment and rejects unknown versions, oversized fields and unsupported profiles before changing balances.

**Use authenticated identity for every replica count.** One admitted validator key has one vote in a given assignment/step, regardless of aliases, origins or reconnects. Two different keys sharing a display name remain distinct. Authentication alone does not make arbitrary keys independent: the assignment must bind an eligible, Sybil-resistant registry and its version. Routing changes never change membership or quorum.

**Start with fixed membership, without a new random committee.** The controlled paid-inference candidate uses the entire frozen active validator set as the verification authority. Capability checks may make a job unavailable; they cannot silently reduce that authority. Require strictly more than two thirds of its total active stake for the same complete result commitment, under the explicit assumption that Byzantine stake is less than one third. Verify weights from the frozen registry and each signature under the chain's admitted signature scheme. With six equally weighted validators this means five matching validators. This is a proposed result-attestation policy, not a proof of model execution or a replacement for chain consensus. Do not reuse the legacy caller-selected committee size, request-id-derived selection, majority of survivors, or an unverified VRF claim.

**Do not promise cheap verification.** Every result attestor must independently validate all operations it attests to, with exact inputs/model/profile/output. A coordinator's transcript hash alone does not establish correctness. Initial isolated settlement tests can use complete-model recomputation as a conservative reference. Useful computation can be partitioned in the executor, but this reference duplicates verification work and cannot close the efficient work-sharing gate. A production partition certificate needs either independently checked partition results under a justified fault model or a validated proof system covering the actual integer operators. Selecting arbitrary small committees per partition does not inherit the network-wide stake assumption. Any cheaper verification design is a separately reviewed change, with compute and communication measured explicitly.

## Model and assignment interface

The package commitment covers architecture/operator graph, weights/chunks, tensor shapes, tokenizer/config, execution profile, partition interfaces and resource bounds. The integer profile defines quantization scales/zero points, rounding, accumulator width, overflow, normalization and sampling. INT weights alone are insufficient. Start with a pinned production artifact on an existing conformant backend; unrecognized operators/backends remain unavailable.

A worker advertises an authenticated capability record and expiring capacity lease. The scheduler uses measured throughput, free memory, warm weights, queue load and worker-to-worker network measurements. The assignment identifies each distinct useful partition, its dependencies, eligible executors/verifiers and reserved resources. Larger devices may receive larger shares. The common interface supports model and backend adapters without promising that every device can execute every model.

Within an active request, partition boundaries and arithmetic are immutable. Cancellation, output limits and backpressure apply to every child computation. A retry has an assignment epoch and must restore or replay the correct KV state; a cold replica must not continue a warmed stream. Reassignment that changes paid membership requires a specified state transition or a new request after refund. The first path may fail and refund rather than attempt unsafe midstream reassignment.

The early benchmark must record different useful slices for one query, output equality, compute time, data transfer, synchronization, verification and end-to-end latency separately. Local threads demonstrate local arithmetic only; they cannot certify WAN performance. A synthetic matmul is a feasibility gate, not production model acceptance.

## Atomic request lifecycle

1. **Admit:** validate signature/domain, nonce, canonical request identity, model/profile registration, complete assignment, limits, expiry, balance and escrow nonexistence before mutation. Reserve funds and persist the complete request atomically. One shared semantic validator must cover signed RPC, mempool admission and block application; state remains authoritative.
2. **Execute:** move through queued/executing states using durable job IDs and bounded queues. These observations are not consensus state or payment. Workers sign commitments that bind the authorized job, assignment, output token encoding and all required execution evidence.
3. **Vote:** validate membership, frozen weight, unique signer, complete binding and timely inclusion. Reject votes at or after expiry. Honest signers persist their decision before emitting a signature and never sign conflicting output commitments for the same job/epoch after restart.
4. **Finalize:** before expiry, require the fixed matching threshold and the exact matching bounded output blob. Store output by commitment, never accept the first arbitrary attached blob as the winning output. All credits, escrow debit, receipt and terminal state commit together. Validate checked arithmetic and conservation; no saturating credit that silently destroys value. Fix the pricing/distribution schedule in the authorized request/protocol contract before implementing payments.
5. **Refund:** at or after expiry, permit deterministic refund through an idempotent state transition. An observed HTTP timeout alone cannot refund. Refund the unused reserved funds according to the explicit fee contract; execution failures cannot invent a penalty. If finalize and refund compete at the boundary, canonical inclusion height decides using the rules above. A halted chain cannot guarantee a wall-clock refund.
6. **Recover:** replay or snapshot restore must reproduce balances, receipts, assignments, votes, outputs, job indexes and anti-equivocation records. No payment occurs twice, including after a crash between durable writes. Atomicity must be established in the persistence design, not assumed from several consecutive WAL appends.

The current legacy bodies and state apply code do not satisfy this contract. Add a new explicit versioned format and activation rule; do not reinterpret historical transaction bytes or modify recovered state by silently enabling old variants. Historical result lookup stays readable.

## Finality, persistence and activation

An inference certificate asserts execution agreement. A committed-block certificate asserts the chain accepted the state transition. Clients must not confuse them. A new finality transcript must bind chain/recovery domain, validator-set version, block height/hash and state/receipt commitment. Sign only after the specified consensus commit condition and persist anti-equivocation state. Existing DAG signatures are not signatures over this transcript.

The current all-validator recovery guard remains unchanged. The view-change/leader-skip protocol, lock/commit proof, finality-signing lifecycle and durable snapshot transaction boundary need their own focused design and adversarial simulation before activation. Naming a two-thirds threshold is not a sufficient consensus design.

Snapshot format must bind schema, chain/recovery identity, committed height/hash, state root, WAL cursor, all economic/index state and signing safety records. Write and fsync a new snapshot before atomically publishing it; retain a valid prior snapshot and required WAL tail. Restore must reject incompatible/corrupt data and recover interrupted writes deterministically. No production snapshot pruning before recovery equivalence tests.

Activation requires an exact future protocol identifier/height, supported client matrix and migration artifact selected after isolated integration. These remain unset in draft 1. Existing protocol-3 nodes must never accept new inference state without the activation rule. Rollback limits must account for irreversible state-format/protocol changes.

## Compatibility and acceptance matrix

| Surface | Current behavior to preserve | Required acceptance before release |
|---|---|---|
| Protocol-3 paid ingress | Disabled; signed alternative ingress cannot bypass it | New versioned request, shared admission, negative/replay vectors and state conservation |
| Free/sharded HTTP | Explicit execution evidence and degraded/unavailable states | No implied receipt/payment; unique authenticated identities; exact profile and complete coverage |
| Integer model | Explicit pinned artifact/profile | Cross-backend production-model vectors, quality acceptance and measured resource fit |
| Work sharing | Existing ordered layers and pipelined prefill | Real useful partitions, equivalent output, measured 1/2/4/8-worker comparison where available |
| Explorer/app | Read recovered data with boundaries | Real transfer/address journey; native finalized/refunded receipt; separate community-reward receipt |
| Finality | Export unavailable until proper signatures exist | Independent transcript verification, fault/restart simulation and safe activation |
| Persistence | Existing WAL recovery | Atomic economic transition, crash tests, snapshot/replay equivalence and bounded restart |
| Distribution | Local builds are preliminary evidence | Signed native packages, clean install/update tests on advertised platforms, deployed provenance and soak |

## Immediate implementation boundaries

Batch 1 can land prepared dependency/explorer fixes, authenticated replica deduplication, truthful product state handling, focused acceptance tests and native packaging preflight. These changes do not need paid-inference activation. Freeze wire encoding, economic schedule and atomic persistence rules before adding settlement transactions. Model-performance experiments and product fixes proceed independently. Draft 1 narrows the decisions and records remaining design gates; it is not a claim that all protocol work is finished.

Source anchors inspected for this draft: `crates/arc-node/src/rpc.rs` paid-ingress guard and `assemble_pipeline`; `crates/arc-state/src/lib.rs` legacy request/vote/finalize, committee derivation and WAL mutations; `crates/arc-types/src/transaction.rs` legacy Tier-1 bodies; `crates/arc-consensus/src/lib.rs` fail-closed finality export. Runtime baseline is `f616705d6058b75543f6a894dfb7254410055453` plus the prepared seven-file patch. Archived design prose is not used as implementation evidence.
