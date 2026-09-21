//! Assignment certificates (S4): the placement for one job, bound to the
//! exact inputs it was computed from, so any validator can recompute it.

use crate::placement::{Candidate, Placement, PlacementError, Policy, Stage, place};
use crate::verify::VerificationRule;
use crate::{Hash256, digest};
use serde::{Deserialize, Serialize};

pub const CERTIFICATE_DOMAIN: &str = "ARC-assign-certificate-v1";
pub const POLICY_DOMAIN: &str = "ARC-assign-policy-v1";

/// Everything the placement was computed from, and the placement.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AssignmentCertificate {
    pub version: u32,
    pub request_id: Hash256,
    pub artifact_id: Hash256,
    pub execution_profile: String,
    pub epoch: u64,
    pub stages: Vec<Stage>,
    pub coordinator_macs_per_s: u64,
    /// The validated candidates (measured rates, links), in canonical
    /// order, with the digests of the signed leases they came from.
    pub candidates: Vec<Candidate>,
    pub lease_digests: Vec<Hash256>,
    pub policy: Policy,
    pub verification: VerificationRule,
    pub placement: Placement,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CertificateError {
    #[error("unsupported certificate version {0}")]
    Version(u32),
    #[error("the bound inputs do not reproduce the placement")]
    PlacementMismatch,
    #[error("the bound inputs cannot be placed: {0}")]
    Placement(#[from] PlacementError),
    #[error("the certificate names a policy the job did not authorise")]
    WrongPolicy,
}

/// The policy hash a job's `assignment_hash` names: the placement algorithm
/// version, its policy parameters and the verification rule. The activation
/// allowlists this, not a particular worker set.
pub fn policy_hash(policy: &Policy, verification: &VerificationRule) -> Hash256 {
    digest(POLICY_DOMAIN, &(1u32, policy, verification))
}

impl AssignmentCertificate {
    /// Compute a placement and bind it with its inputs.
    #[allow(clippy::too_many_arguments)]
    pub fn issue(
        request_id: Hash256,
        artifact_id: Hash256,
        execution_profile: &str,
        epoch: u64,
        stages: Vec<Stage>,
        coordinator_macs_per_s: u64,
        mut candidates: Vec<Candidate>,
        mut lease_digests: Vec<Hash256>,
        policy: Policy,
        verification: VerificationRule,
    ) -> Result<Self, PlacementError> {
        candidates.sort_by_key(|c| c.worker.0);
        lease_digests.sort_by_key(|d| d.0);
        let placement = place(&stages, coordinator_macs_per_s, &candidates, &policy)?;
        Ok(Self {
            version: 1,
            request_id,
            artifact_id,
            execution_profile: execution_profile.to_string(),
            epoch,
            stages,
            coordinator_macs_per_s,
            candidates,
            lease_digests,
            policy,
            verification,
            placement,
        })
    }

    pub fn hash(&self) -> Hash256 {
        digest(CERTIFICATE_DOMAIN, self)
    }

    /// Recompute the placement from the bound inputs and require that it is
    /// the one recorded, under the policy the job authorised.
    pub fn verify(&self, authorised_policy: &Hash256) -> Result<(), CertificateError> {
        if self.version != 1 {
            return Err(CertificateError::Version(self.version));
        }
        if policy_hash(&self.policy, &self.verification) != *authorised_policy {
            return Err(CertificateError::WrongPolicy);
        }
        let again = place(
            &self.stages,
            self.coordinator_macs_per_s,
            &self.candidates,
            &self.policy,
        )?;
        if again != self.placement {
            return Err(CertificateError::PlacementMismatch);
        }
        Ok(())
    }
}
