# Batch 3 contract bounds

Added an early certificate vote-count guard before hashing, allocation, or
signature work. Empty certificates and certificates larger than the supplied
committee now return `InvalidCertificateBounds`; existing member, duplicate,
output, and signature checks remain unchanged. Focused tests cover empty and
oversized certificates, rejection of a nonmember requester vote, admission of
a requester who is an actual committee member with refund/reward coalesced
into one credit, stake-total overflow, and the existing zero nonce path.

Validation:

```text
cargo test --locked -p arc-types inference_contract
7 passed, 0 failed
rustfmt --edition 2024 crates/arc-types/src/inference_contract.rs
git diff --check -- crates/arc-types/src/inference_contract.rs
```

The module remains pure and inactive: no decoder, transaction body, RPC,
state, WAL, or exactly-once semantics were added.
