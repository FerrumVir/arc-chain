//! Capability leases (S1): what a worker can do, signed, expiring, and
//! checked against a challenge rather than believed.

use crate::{Address, Hash256, digest};
use arc_crypto::{KeyPair, Signature};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

pub const LEASE_DOMAIN: &str = "ARC-assign-capability-lease-v1";

/// The operator a row worker performs: one canonical-I8 row projection.
pub const OP_ROW_PROJECTION_I8: &str = "row-projection-i8";

/// What a worker offers, signed by its validator key.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LeaseBody {
    pub version: u32,
    /// The worker's validator identity: the only key placements dedupe on.
    pub worker: Address,
    /// Pinned transport identity (e.g. host key fingerprint), not an address.
    pub transport_id: String,
    pub artifact_id: Hash256,
    pub execution_profile: String,
    pub backend: String,
    pub kernel_set: Hash256,
    pub operators: BTreeSet<String>,
    /// Bytes of RAM the worker can devote to resident rows.
    pub ram_headroom_bytes: u64,
    /// Measured canonical-I8 row throughput: multiply-accumulates per second.
    pub claimed_macs_per_s: u64,
    pub max_concurrency: u32,
    /// Rows already resident per tensor key (warm), by name.
    pub warm_rows: Vec<(String, u64)>,
    /// Layers this worker holds weights for, as half-open `[start, end)`
    /// ranges. Empty means the whole model. Signed with the rest of the
    /// body, so a worker cannot overstate what it can serve.
    #[serde(default)]
    pub resident_layers: Vec<(u32, u32)>,
    pub issued_at_height: u64,
    pub expires_at_height: u64,
    pub nonce: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CapabilityLease {
    pub body: LeaseBody,
    pub signature: Signature,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum LeaseError {
    #[error("lease signature does not verify for its worker")]
    Signature,
    #[error("lease worker is not a member of the frozen committee")]
    NotMember,
    #[error("lease is for another artifact or execution profile")]
    WrongModel,
    #[error("lease does not offer operator {0}")]
    MissingOperator(String),
    #[error("lease expired at height {expired}, now {now}")]
    Expired { expired: u64, now: u64 },
    #[error("lease is not yet valid")]
    NotYetValid,
    #[error("lease version {0} is not supported")]
    Version(u32),
    #[error("measured {measured} MAC/s is below the claimed {claimed} beyond tolerance")]
    DishonestCapacity { claimed: u64, measured: u64 },
}

impl LeaseBody {
    pub fn transcript(&self) -> Hash256 {
        digest(LEASE_DOMAIN, self)
    }
}

impl CapabilityLease {
    pub fn sign(body: LeaseBody, key: &KeyPair) -> Result<Self, arc_crypto::SignatureError> {
        let signature = key.sign(&body.transcript())?;
        Ok(Self { body, signature })
    }

    /// Hash identifying this exact signed lease, for binding into a
    /// certificate.
    pub fn digest(&self) -> Hash256 {
        digest(LEASE_DOMAIN, &(&self.body, &self.signature))
    }
}

/// What a lease must satisfy to be used for a job.
pub struct LeaseRequirements<'a> {
    /// Frozen committee members, by address bytes.
    pub members: &'a BTreeSet<[u8; 32]>,
    pub artifact_id: Hash256,
    pub execution_profile: &'a str,
    pub operator: &'a str,
    pub height: u64,
}

/// Validate a lease for a job. Capacity is NOT trusted here; see
/// [`check_challenge`].
pub fn validate(lease: &CapabilityLease, req: &LeaseRequirements<'_>) -> Result<(), LeaseError> {
    let b = &lease.body;
    if b.version != 1 {
        return Err(LeaseError::Version(b.version));
    }
    lease
        .signature
        .verify(&b.transcript(), &b.worker)
        .map_err(|_| LeaseError::Signature)?;
    if !req.members.contains(&b.worker.0) {
        return Err(LeaseError::NotMember);
    }
    if b.artifact_id != req.artifact_id || b.execution_profile != req.execution_profile {
        return Err(LeaseError::WrongModel);
    }
    if !b.operators.contains(req.operator) {
        return Err(LeaseError::MissingOperator(req.operator.to_string()));
    }
    if req.height < b.issued_at_height {
        return Err(LeaseError::NotYetValid);
    }
    if req.height >= b.expires_at_height {
        return Err(LeaseError::Expired {
            expired: b.expires_at_height,
            now: req.height,
        });
    }
    Ok(())
}

/// A timed challenge projection with a known answer: `macs` multiply-
/// accumulates answered correctly in `elapsed_us`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChallengeResult {
    pub macs: u64,
    pub elapsed_us: u64,
    pub correct: bool,
}

impl ChallengeResult {
    pub fn measured_macs_per_s(&self) -> u64 {
        if self.elapsed_us == 0 {
            return 0;
        }
        ((self.macs as u128 * 1_000_000) / self.elapsed_us as u128) as u64
    }
}

/// Accept a lease's claimed capacity only if a challenge measured at least
/// `min_fraction_percent` of it, with a correct answer. Returns the rate the
/// placement should use: the MEASURED one, never the claim.
pub fn check_challenge(
    lease: &CapabilityLease,
    challenge: &ChallengeResult,
    min_fraction_percent: u64,
) -> Result<u64, LeaseError> {
    let measured = challenge.measured_macs_per_s();
    let claimed = lease.body.claimed_macs_per_s;
    if !challenge.correct
        || (measured as u128) * 100 < (claimed as u128) * (min_fraction_percent as u128)
    {
        return Err(LeaseError::DishonestCapacity { claimed, measured });
    }
    Ok(measured.min(claimed))
}
