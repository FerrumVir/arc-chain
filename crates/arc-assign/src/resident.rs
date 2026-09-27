//! Private fixed-residency placement. Additive contract: no v1/v2 lease,
//! candidate, policy or certificate bytes change. Every primary row is owned
//! by exactly one configured worker; measured rates never move its boundary.
//! This is operator-trusted compute with canonical checks, not public proof.

use crate::certificate::{AssignmentCertificate, policy_hash_v2};
use crate::placement::{Candidate, Participant, Placement, Policy, Slice, Stage, StagePlan};
use crate::verify::{VerificationPlan, VerificationRule};
use crate::{Hash256, digest};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

pub const MAX_WORKERS: usize = 32;
pub const MAX_STAGES: usize = 4096;
pub const MAX_RESIDENT_BYTES: u64 = 1 << 30;
const POLICY_DOMAIN: &str = "ARC-private-fixed-resident-row-policy-v1";
const CERTIFICATE_DOMAIN: &str = "ARC-private-fixed-resident-row-certificate-v1";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResidentRange {
    pub stage: usize,
    pub row_start: u64,
    pub row_end: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkerResidency {
    pub worker: Hash256,
    pub manifest_hash: Hash256,
    pub ranges: Vec<ResidentRange>,
}

/// New authorization semantics: fixed remote ownership, no fallback, each
/// owner's first row checked canonically, configured seeded duplicates checked
/// canonically, and the existing seeded per-stage spot checks retained.
/// Canonical verification reads local artifact rows and consumes real CPU/I/O.
pub fn policy_hash(policy: &Policy, rule: &VerificationRule) -> Hash256 {
    digest(
        POLICY_DOMAIN,
        &(
            1u32,
            policy_hash_v2(policy, rule),
            "fixed-remote-owners-no-fallback;canonical-first-row-per-owner;canonical-seeded-duplicates;stage-spots",
        ),
    )
}

/// Validate complete ownership before connecting, without claiming readiness.
/// Initially each worker owns one nonempty interval of EVERY projection. This
/// intentionally excludes mixed layer bundles and replica/checker-only offers.
pub fn validate_layout(
    stages: &[Stage],
    residency: &[WorkerResidency],
    max_workers: usize,
) -> Result<(), String> {
    if stages.is_empty()
        || stages.len() > MAX_STAGES
        || residency.is_empty()
        || residency.len() > MAX_WORKERS
        || residency.len() > max_workers
        || max_workers > MAX_WORKERS
    {
        return Err("resident layout stage/worker bounds".into());
    }
    let mut stage_keys = BTreeSet::new();
    for stage in stages {
        if stage.rows == 0
            || stage.cols == 0
            || !stage_keys.insert((stage.layer, stage.tensor.as_str()))
        {
            return Err("resident layout has an empty or duplicate model stage".into());
        }
    }
    let mut workers = BTreeSet::new();
    let mut manifests = BTreeSet::new();
    for owner in residency {
        if owner.worker == Hash256::ZERO
            || owner.manifest_hash == Hash256::ZERO
            || !workers.insert(owner.worker.0)
            || !manifests.insert(owner.manifest_hash.0)
            || owner.ranges.len() != stages.len()
        {
            return Err("resident worker/manifest identity or stage count".into());
        }
        let mut seen = BTreeSet::new();
        let mut bytes = 0u128;
        for range in &owner.ranges {
            let stage = stages
                .get(range.stage)
                .ok_or("resident range names an unknown stage")?;
            if !seen.insert(range.stage)
                || range.row_start >= range.row_end
                || range.row_end > stage.rows
            {
                return Err("resident range is duplicate, empty or outside its projection".into());
            }
            let payload = u128::from(range.row_end - range.row_start)
                .checked_mul(u128::from(stage.cols) + 8)
                .ok_or("resident byte count overflow")?;
            bytes = bytes
                .checked_add(payload)
                .ok_or("resident byte count overflow")?;
        }
        if bytes > u128::from(MAX_RESIDENT_BYTES) {
            return Err("resident canonical payload exceeds 1 GiB".into());
        }
    }
    for (index, stage) in stages.iter().enumerate() {
        let mut ranges: Vec<_> = residency
            .iter()
            .map(|owner| {
                owner
                    .ranges
                    .iter()
                    .find(|range| range.stage == index)
                    .expect("stage coverage checked")
            })
            .collect();
        ranges.sort_by_key(|range| range.row_start);
        let mut next = 0;
        for range in ranges {
            if range.row_start != next {
                return Err("resident projection has a gap or overlap".into());
            }
            next = range.row_end;
        }
        if next != stage.rows {
            return Err("resident projection has an uncovered tail".into());
        }
    }
    Ok(())
}

pub fn place(
    stages: &[Stage],
    candidates: &[Candidate],
    residency: &[WorkerResidency],
    policy: &Policy,
) -> Result<Placement, String> {
    validate_layout(stages, residency, policy.max_workers)?;
    if policy.include_coordinator || policy.weight_bytes_per_element != 1 {
        return Err("resident placement requires canonical I8 remote-only execution".into());
    }
    let mut by_worker = BTreeMap::new();
    let mut transports = BTreeSet::new();
    for candidate in candidates {
        if by_worker.insert(candidate.worker.0, candidate).is_some()
            || !transports.insert(candidate.transport_id.as_str())
            || candidate.transport_id.is_empty()
        {
            return Err("resident candidates repeat a worker or transport identity".into());
        }
    }
    if by_worker.len() != residency.len() {
        return Err("every resident owner must be measured, connected and reservable".into());
    }
    for owner in residency {
        let candidate = by_worker
            .get(&owner.worker.0)
            .ok_or("a resident owner is unavailable")?;
        if candidate.macs_per_s == 0
            || candidate.link.bandwidth_bps == 0
            || candidate.max_concurrency == 0
            || candidate.link.measured_at > policy.now
            || candidate.link.failure_per_mille > policy.max_failure_per_mille
            || policy.now - candidate.link.measured_at > policy.max_link_age
            || (!policy.allow_simulated_links && candidate.link.simulated)
            || !candidate.resident_layers.is_empty()
            || candidate.resident_output
        {
            return Err(
                "resident owner lacks a usable observation or has mixed capabilities".into(),
            );
        }
        let bytes: u128 = owner
            .ranges
            .iter()
            .map(|range| {
                u128::from(range.row_end - range.row_start)
                    * (u128::from(stages[range.stage].cols) + 8)
            })
            .sum();
        if bytes > u128::from(candidate.ram_headroom_bytes) {
            return Err("resident owner exceeds its configured memory budget".into());
        }
    }
    let plans = stages
        .iter()
        .enumerate()
        .map(|(index, _)| {
            let mut slices: Vec<_> = residency
                .iter()
                .map(|owner| {
                    let range = owner
                        .ranges
                        .iter()
                        .find(|range| range.stage == index)
                        .expect("validated");
                    Slice {
                        participant: Participant::Worker(owner.worker),
                        row_start: range.row_start,
                        row_end: range.row_end,
                    }
                })
                .collect();
            slices.sort_by_key(|slice| slice.row_start);
            StagePlan {
                stage: index,
                slices,
            }
        })
        .collect();
    let mut workers: Vec<_> = residency.iter().map(|owner| owner.worker).collect();
    workers.sort_by_key(|worker| worker.0);
    Ok(Placement {
        workers,
        stages: plans,
        // Fixed ownership is a capacity decision, never a speed prediction.
        // Zero means unavailable; local canonical I/O/verification is unmeasured.
        predicted_token_us: 0,
        coordinator_only_token_us: 0,
    })
}

/// Independent audit envelope; its inner version 3 is deliberately unsupported
/// by the legacy AssignmentCertificate verifier. Never serialize this as v1/v2.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResidentCertificate {
    pub assignment: AssignmentCertificate,
    pub residency: Vec<WorkerResidency>,
}
impl ResidentCertificate {
    pub fn issue(
        mut assignment: AssignmentCertificate,
        mut residency: Vec<WorkerResidency>,
    ) -> Result<Self, String> {
        assignment.version = 3;
        assignment
            .candidates
            .sort_by_key(|candidate| candidate.worker.0);
        assignment.lease_digests.sort_by_key(|hash| hash.0);
        residency.sort_by_key(|owner| owner.worker.0);
        for owner in &mut residency {
            owner.ranges.sort_by_key(|range| range.stage);
        }
        if assignment.verification.duplicate_per_mille > 1000
            || assignment.verification.spot_rows_per_stage == 0
            || assignment.verification.spot_rows_per_stage > 64
        {
            return Err("resident verification rule is outside bounds".into());
        }
        assignment.placement = place(
            &assignment.stages,
            &assignment.candidates,
            &residency,
            &assignment.policy,
        )?;
        Ok(Self {
            assignment,
            residency,
        })
    }
    pub fn hash(&self) -> Hash256 {
        digest(CERTIFICATE_DOMAIN, self)
    }
    pub fn verify(&self, authorized: &Hash256) -> Result<(), String> {
        if self.assignment.version != 3
            || policy_hash(&self.assignment.policy, &self.assignment.verification) != *authorized
        {
            return Err("resident certificate version or unauthorized policy".into());
        }
        let rebuilt = Self::issue(self.assignment.clone(), self.residency.clone())?;
        if rebuilt != *self {
            return Err("resident certificate does not reproduce fixed ownership".into());
        }
        Ok(())
    }
    pub fn verification_plan(&self) -> VerificationPlan {
        let mut checks = crate::verify::plan(
            self.hash(),
            &self.assignment.placement,
            &self.assignment.verification,
        );
        // Disjoint owners cannot duplicate one another's rows. Under this new
        // policy every selected duplicate is computed by the canonical source.
        for duplicate in &mut checks.duplicates {
            duplicate.checker = Participant::Coordinator;
        }
        for stage in &self.assignment.placement.stages {
            for slice in &stage.slices {
                checks.spot_rows.push((stage.stage, slice.row_start));
            }
        }
        checks
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::link::LinkMeasurement;

    fn fixture() -> (AssignmentCertificate, Vec<WorkerResidency>) {
        let stages: Vec<_> = ["wq", "w_gate", "w_down", "lm_head"]
            .into_iter()
            .map(|tensor| Stage {
                layer: (tensor != "lm_head").then_some(0),
                tensor: tensor.into(),
                rows: 6,
                cols: 2,
            })
            .collect();
        let candidates = (1..=2)
            .map(|id| Candidate {
                worker: Hash256([id; 32]),
                transport_id: format!("owner-{id}"),
                macs_per_s: 1_000,
                ram_headroom_bytes: 1024,
                max_concurrency: 1,
                link: LinkMeasurement {
                    samples: 8,
                    rtt_median_us: 2,
                    rtt_p95_us: 3,
                    jitter_us: 1,
                    bandwidth_bps: 1000,
                    failure_per_mille: 0,
                    measured_at: 10,
                    simulated: false,
                },
                resident_layers: vec![],
                resident_output: false,
            })
            .collect();
        let residency = [(1, 0, 2), (2, 2, 6)]
            .into_iter()
            .map(|(id, start, end)| WorkerResidency {
                worker: Hash256([id; 32]),
                manifest_hash: Hash256([id + 10; 32]),
                ranges: (0..stages.len())
                    .map(|stage| ResidentRange {
                        stage,
                        row_start: start,
                        row_end: end,
                    })
                    .collect(),
            })
            .collect();
        (
            AssignmentCertificate {
                version: 3,
                request_id: Hash256([20; 32]),
                artifact_id: Hash256([21; 32]),
                execution_profile: "fixture-only".into(),
                epoch: 0,
                stages,
                coordinator_macs_per_s: 0,
                candidates,
                lease_digests: vec![],
                policy: Policy {
                    max_workers: 2,
                    max_link_age: 30,
                    now: 12,
                    max_failure_per_mille: 10,
                    allow_simulated_links: false,
                    input_element_bytes: 8,
                    output_element_bytes: 8,
                    weight_bytes_per_element: 1,
                    include_coordinator: false,
                },
                verification: VerificationRule {
                    duplicate_per_mille: 1000,
                    spot_rows_per_stage: 2,
                },
                placement: Placement {
                    workers: vec![],
                    stages: vec![],
                    predicted_token_us: 0,
                    coordinator_only_token_us: 0,
                },
            },
            residency,
        )
    }

    #[test]
    fn fixed_ownership_never_moves_with_rates_and_binds_a_distinct_policy() {
        let (base, owners) = fixture();
        let certificate = ResidentCertificate::issue(base.clone(), owners.clone()).unwrap();
        let authorization = policy_hash(&base.policy, &base.verification);
        certificate.verify(&authorization).unwrap();
        assert_ne!(
            authorization,
            policy_hash_v2(&base.policy, &base.verification)
        );
        assert!(
            certificate.assignment.verify(&authorization).is_err(),
            "old verifier must not reinterpret version3"
        );
        let mut faster = base.clone();
        faster.candidates[0].macs_per_s = 1_000_000_000;
        faster.policy.now += 1;
        assert_eq!(
            ResidentCertificate::issue(faster.clone(), owners.clone())
                .unwrap()
                .assignment
                .placement,
            certificate.assignment.placement
        );
        assert_eq!(
            policy_hash(&faster.policy, &faster.verification),
            authorization
        );
        let mut reordered = base.clone();
        reordered.candidates.reverse();
        let mut reversed = owners.clone();
        reversed.reverse();
        for owner in &mut reversed {
            owner.ranges.reverse();
        }
        assert_eq!(
            ResidentCertificate::issue(reordered, reversed).unwrap(),
            certificate
        );
        let mut changed = certificate.clone();
        changed.residency[1].ranges[0].row_start -= 1;
        assert!(changed.verify(&authorization).is_err());
        let mut changed = certificate.clone();
        changed.assignment.placement.stages[0].slices[0].row_end += 1;
        assert!(changed.verify(&authorization).is_err());
        let mut changed = certificate.clone();
        changed.assignment.verification.spot_rows_per_stage = 1;
        assert!(changed.verify(&authorization).is_err());
        let mut changed = certificate.clone();
        changed.residency[0].manifest_hash = Hash256([99; 32]);
        assert_ne!(
            changed.hash(),
            certificate.hash(),
            "every manifest pin is committed"
        );
        assert_eq!(
            certificate.assignment.placement.predicted_token_us, 0,
            "no local verification cost estimate is invented"
        );
    }

    #[test]
    fn unavailable_unique_owners_and_invalid_coverage_refuse_instead_of_repartitioning() {
        let (base, owners) = fixture();
        for mutation in 0..10 {
            let mut changed = base.clone();
            match mutation {
                0 => {
                    changed.candidates.pop();
                }
                1 => changed.candidates[1].macs_per_s = 0,
                2 => changed.candidates[1].link.measured_at = 100,
                3 => changed.policy.now = 100,
                4 => changed.candidates[1].link.bandwidth_bps = 0,
                5 => changed.candidates[1].max_concurrency = 0,
                6 => changed.candidates[1].ram_headroom_bytes = 1,
                7 => changed.candidates[1].link.simulated = true,
                8 => {
                    changed.candidates[1].transport_id = changed.candidates[0].transport_id.clone()
                }
                _ => changed.policy.include_coordinator = true,
            }
            assert!(
                ResidentCertificate::issue(changed, owners.clone()).is_err(),
                "mutation {mutation}"
            );
        }
        for mutation in 0..8 {
            let mut changed = owners.clone();
            match mutation {
                0 => changed[1].ranges[0].row_start += 1,
                1 => changed[1].ranges[0].row_start -= 1,
                2 => changed[1].ranges[0].row_end = 7,
                3 => {
                    changed[1].ranges.pop();
                } // output head is mandatory
                4 => changed[1].ranges[0].row_start = 6,
                5 => changed[1].worker = changed[0].worker,
                6 => changed[1].manifest_hash = changed[0].manifest_hash,
                _ => changed[1].ranges[3].stage = 0,
            }
            assert!(
                ResidentCertificate::issue(base.clone(), changed).is_err(),
                "mutation {mutation}"
            );
        }
        let mut huge = base.clone();
        huge.stages[0].cols = u64::MAX;
        assert!(ResidentCertificate::issue(huge, owners).is_err());
    }

    #[test]
    fn every_owner_is_spot_checked_and_all_selected_duplicates_use_canonical_rows() {
        let (base, owners) = fixture();
        let cert = ResidentCertificate::issue(base, owners).unwrap();
        let checks = cert.verification_plan();
        assert_eq!(checks.duplicates.len(), 8);
        assert!(
            checks
                .duplicates
                .iter()
                .all(|check| check.checker == Participant::Coordinator)
        );
        for stage in 0..4 {
            assert!(checks.spot_rows.contains(&(stage, 0)));
            assert!(checks.spot_rows.contains(&(stage, 2)));
            assert_eq!(
                checks.spot_rows.iter().filter(|(s, _)| *s == stage).count(),
                4
            );
        }
        let mut base = cert.assignment.clone();
        base.verification.duplicate_per_mille = 0;
        let cert = ResidentCertificate::issue(base, cert.residency).unwrap();
        assert!(cert.verification_plan().duplicates.is_empty());
        assert!(
            cert.verification_plan().spot_rows.contains(&(3, 2)),
            "zero duplicate fraction never removes per-owner checks, including output head"
        );
    }
}
