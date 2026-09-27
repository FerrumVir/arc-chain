//! Assignment certificates (S4): the placement for one job, bound to the
//! exact inputs it was computed from, so any validator can recompute it.

use crate::placement::{Candidate, Placement, PlacementError, Policy, Stage, place};
use crate::verify::VerificationRule;
use crate::{Hash256, digest};
use serde::{Deserialize, Serialize};

pub const CERTIFICATE_DOMAIN: &str = "ARC-assign-certificate-v1";
pub const POLICY_DOMAIN: &str = "ARC-assign-policy-v1";
pub const CERTIFICATE_DOMAIN_V2: &str = "ARC-assign-certificate-v2";
pub const POLICY_DOMAIN_V2: &str = "ARC-assign-policy-v2";

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

/// Stable authorization for native private-row execution. Unlike v1, the
/// observation height is not an authorization setting. Certificates still
/// record it and use it to reject stale links when placement is recomputed.
/// The coordinator mode explicitly commits whether local fallback is allowed.
/// This is a new contract, never a reinterpretation of a published v1 hash.
pub fn policy_hash_v2(policy: &Policy, verification: &VerificationRule) -> Hash256 {
    let Policy {
        max_workers,
        max_link_age,
        now: _,
        max_failure_per_mille,
        allow_simulated_links,
        input_element_bytes,
        output_element_bytes,
        weight_bytes_per_element,
        include_coordinator,
    } = policy;
    let mode = if *include_coordinator {
        "private-row-resident-with-local-fallback-v1"
    } else {
        "private-row-remote-only-strict-v1"
    };
    digest(
        POLICY_DOMAIN_V2,
        &(
            2u32,
            mode,
            max_workers,
            max_link_age,
            max_failure_per_mille,
            allow_simulated_links,
            input_element_bytes,
            output_element_bytes,
            weight_bytes_per_element,
            include_coordinator,
            verification,
        ),
    )
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

    /// Same placement algorithm and observation record as v1, under the
    /// explicitly stable v2 authorization and certificate domains.
    #[allow(clippy::too_many_arguments)]
    pub fn issue_v2(
        request_id: Hash256,
        artifact_id: Hash256,
        execution_profile: &str,
        epoch: u64,
        stages: Vec<Stage>,
        coordinator_macs_per_s: u64,
        candidates: Vec<Candidate>,
        lease_digests: Vec<Hash256>,
        policy: Policy,
        verification: VerificationRule,
    ) -> Result<Self, PlacementError> {
        let mut certificate = Self::issue(
            request_id,
            artifact_id,
            execution_profile,
            epoch,
            stages,
            coordinator_macs_per_s,
            candidates,
            lease_digests,
            policy,
            verification,
        )?;
        certificate.version = 2;
        Ok(certificate)
    }

    pub fn hash(&self) -> Hash256 {
        let domain = if self.version == 2 {
            CERTIFICATE_DOMAIN_V2
        } else {
            CERTIFICATE_DOMAIN
        };
        digest(domain, self)
    }

    /// Recompute the placement from the bound inputs and require that it is
    /// the one recorded, under the policy the job authorised.
    pub fn verify(&self, authorised_policy: &Hash256) -> Result<(), CertificateError> {
        let policy = match self.version {
            1 => policy_hash(&self.policy, &self.verification),
            2 => policy_hash_v2(&self.policy, &self.verification),
            other => return Err(CertificateError::Version(other)),
        };
        if policy != *authorised_policy {
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
