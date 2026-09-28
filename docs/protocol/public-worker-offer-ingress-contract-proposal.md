# Public worker offer ingress: contract proposal

**Status: proposal only.** No node/RPC/transport integration is implemented by this change. Public worker offers do not execute work, create assignments, or establish payment or settlement entitlement.

## Current boundary

`arc-assign::public_worker` defines a signed offer and a bounded durable nonce-admission journal, but a repository-wide caller search finds no caller outside that module. `PublicWorkerOfferAdmissionBook::admit` verifies a caller-supplied context and persists replay provenance plus the signed-offer digest; it does not persist the offer body or queue it for scheduling. It returns success only after the append is synced and reread. The book has a default capacity of 4,096, a hard maximum of 16,384, never evicts entries, and documents that rollback to an earlier valid journal prefix cannot be detected without an independent monotonic anchor.

The existing `/community/register` path is a separate HTTP worker registry with a bounded JSON body and signed route/coordinator/recovery-domain envelope. Its registry is in-memory and TTL-based. P2P receive handling admits only authenticated validator identities before reading messages, so public workers cannot safely be added as peers through that path without a distinct peer-authorization contract. `/native-inference/context` exposes the active chain/recovery domain, context commitment, height, and allowed `(model, profile, generation, assignment)` tuples, but does not expose authoritative kernel and bundle hashes.

`PublicWorkerExecutionBindingV1` defines a versioned, domain-separated commitment over chain genesis, recovery epoch, coordinator audience, and artifact/profile/generation/kernel/bundle hashes. Its `requirements()` method derives offer-verification context only after exact equality with an independently supplied identity snapshot. That snapshot must come from recovered node state, validated package identity, and pinned kernel/row-bundle manifests. This is an operator-pinned local binding, not a consensus authorization, paid assignment, or proof of a loaded artifact by itself. Public protocol 3 currently has no consensus field committing the complete tuple; private protocol-4 inference context is not a substitute and must not be required for public-chain ingress.

## Proposed ingress shape, contingent on the decisions below

Use a new default-off HTTP endpoint, `POST /community/public-worker/offers`, rather than changing validator-only P2P admission. Accept one strict JSON `PublicWorkerOffer` body with a dedicated 64 KiB request-body ceiling. An enabled deployment would require an operator-selected private admission directory and an explicit immutable public-worker execution binding; no implicit directory or enablement default is allowed. Keep this endpoint separate from the existing community registration and reward routes.

Before parsing, enforce the byte ceiling. Reject unknown fields and malformed or oversized offers. Derive coordinator, chain genesis, recovery epoch, current height, and the active execution tuple set from the node’s own state; never accept these values from the request. Require the offer’s coordinator and chain/recovery values to match that state, resolve every execution commitment against one explicitly defined active binding, verify its worker signature and validity interval, then durably record admission before returning an acknowledgement. Map journal busy/full/corrupt/uncertain states to refusal; do not turn them into success or evict replay state.

The response must state only what happened. If the implementation records only nonce consumption, return `nonce_consumed` and do not imply the worker is available to a scheduler. If the product wants an offer to survive restart as pending supply, persist the complete signed offer and pending status durably before returning `offer_recorded`; the current digest-only replay journal is insufficient for that promise. In either case, admission is not execution, work verification, a paid assignment, a reward, or settlement.

## Decisions required before wiring a caller

1. **Binding provenance:** define and pin the operator-owned public-worker execution config that supplies the complete V1 binding, including kernel and row-bundle manifest identities. Protocol 3 does not consensus-commit this tuple. Do not infer that `public_binding` means either the private protocol-4 context commitment or an assignment hash.
2. **Acknowledgement semantics:** choose whether admission consumes a nonce only or creates restart-resumable pending supply. If pending supply is intended, specify a durable offer-body/queue format and the atomic relationship to replay admission.
3. **Lifetime and capacity:** choose the retention/rotation policy for a journal that never evicts, including when an exhausted or expired book can be replaced without reopening replay within the active chain/recovery epoch.
4. **Rollback threat model:** decide whether ordinary persistent storage and fail-closed loss handling suffice, or whether replay protection must detect restoration of an earlier valid journal prefix via an independent monotonic anchor.
5. **Enablement:** specify which chain modes and operator configuration may enable the endpoint. Until approved, it remains absent/default-off and does not alter existing community or paid inference behavior.

These are protocol/product boundaries, not implementation details. The ingress caller should be added only after they are resolved and covered by endpoint-level bounded-decoding, signature/context mismatch, durable-before-ack, restart, replay, capacity, corruption, and default-off tests.
