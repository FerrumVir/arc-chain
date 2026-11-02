//! Stage 0 cryptographic foundation for ARC's validator plan: a dormant library.
//!
//! Nothing in `arc-node`, `arc-consensus` or any other workspace crate imports
//! this crate yet. It exists so the primitives can be reviewed, tested against
//! public test vectors and attacked in CI before a later PR wires them into
//! consensus. The modules are:
//!
//! - [`ecvrf`]: RFC 9381 ECVRF with the `ECVRF-EDWARDS25519-SHA512-TAI`
//!   ciphersuite (key generation, prove, verify, proof-to-hash), the
//!   unique-output VRF that replaces the signature-hash construction in
//!   `arc-crypto::vrf` once the wiring PR lands;
//! - [`leader`]: the fixed, domain-separated byte layout of the VRF input
//!   `alpha` for leader election and committee sampling;
//! - [`bls_pop`]: BLS12-381 proof of possession (IETF BLS signature draft,
//!   PoP scheme, `min_pk`) for validator key registration, with an
//!   [`EnrolledBlsKey`] type that only exists after the proof verified.
//!
//! `crates/arc-vrf/README.md` records the crate evaluation, the audit facts of
//! every dependency, the test inventory and the follow-ups.
#![forbid(unsafe_code)]
#![deny(missing_docs)]

pub mod bls_pop;
pub mod ecvrf;
pub mod leader;

pub use bls_pop::{
    BlsPopError, BlsPublicKey, BlsSecretKey, BlsSignature, EnrolledBlsKey, ProofOfPossession,
};
pub use ecvrf::{Output, Proof, PublicKey, SecretKey, VrfError};
pub use leader::{Purpose, VrfInput};
