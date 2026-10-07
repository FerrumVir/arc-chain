//! ARC-AC v0: finding, forming and running Kimi-class islands and regional
//! swarms on community machines.
//!
//! A 1T-class model (Kimi K2.6, 582 GB with INT4 experts) fits on no single
//! community machine. This crate decides which opted-in machines serve one
//! copy together, and how:
//!
//! - [`device`]: the device descriptor, built from the Proof Kit's
//!   `arc.proof-result.v1` island facts (PR #149) plus measured link
//!   statistics, and the owner's consent (#138's `compute_consent` model).
//! - [`model`]: per-layer weight, active-byte and KV accounting.
//! - [`fit`]: capacity fitting, heterogeneity-aware contiguous layer
//!   partitioning, and ring ordering.
//! - [`form`]: tiers T0–T2 and the formation algorithm: single devices, then
//!   Thunderbolt/LAN islands, then metro → zone → region swarms; warm spares.
//! - [`perf`]: projected per-answer tok/s (with and without speculative
//!   decoding), batching depth, aggregate tok/s.
//! - [`lifecycle`]: form → qualify (golden digest) → serve → degrade/promote
//!   spare → dissolve, driven by heartbeats and consent changes.
//! - [`selftest`]: the deterministic qualification self-test.
//! - [`admission`]: per-request admission control.
//! - [`sim`]: the offline capacity simulator over a synthetic inventory.
//!
//! Sources: research-6 ("Auto-clustering consumer devices to serve 1T-class
//! models", §2, §3, §6) and research-7 ("Adaptive hierarchical clustering",
//! §2–§4). Model figures come from `docs/protocol/kimi-k26-checkpoint.md`
//! (PR #156).
//!
//! **Determinism.** Formation, partitioning, spare choice and lifecycle use
//! integers only (bytes, microseconds, MB/s), so anyone recomputing a
//! formation from the same inputs gets the same islands. Projections are
//! `f64` and are reporting only: nothing in formation reads them.
//!
//! **Dormant.** Nothing in the node depends on this crate, and it has no
//! network code. It changes no protocol behaviour.

pub mod admission;
pub mod device;
pub mod fit;
pub mod form;
pub mod lifecycle;
pub mod model;
pub mod perf;
pub mod selftest;
pub mod sim;
