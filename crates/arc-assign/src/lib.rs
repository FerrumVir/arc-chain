//! Single-query work assignment for ARC (checklist S1-S8).
//!
//! Pure functions over signed inputs, no I/O: a coordinator gathers
//! capability leases from workers and measures its links to them; this crate
//! validates the leases, decides a placement (which workers compute which
//! rows of each projection - or none, when distributing would be slower),
//! binds that decision into an assignment certificate any validator can
//! recompute, and plans the redundancy and spot checks that verify the
//! partitioned work. Design: `docs/design/single-query-distribution.md`.
//!
//! The partition route this serves is tensor-row parallelism with the
//! coordinator holding attention and the KV cache, so workers are stateless
//! across calls. That is what makes per-call reassignment safe and redundancy
//! exactly checkable.

pub mod book;
pub mod certificate;
pub mod lease;
pub mod link;
pub mod placement;
pub mod queue;
pub mod reservation;
pub mod verify;

pub use arc_crypto::Hash256;

/// A validator's address (its key's hash).
pub type Address = Hash256;

/// Domain-separated hash of a serialisable value.
pub(crate) fn digest<T: serde::Serialize>(domain: &str, value: &T) -> Hash256 {
    let mut hasher = blake3::Hasher::new_derive_key(domain);
    hasher.update(&bincode::serialize(value).expect("in-memory value serialises"));
    Hash256(*hasher.finalize().as_bytes())
}
