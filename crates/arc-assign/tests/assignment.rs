//! S9: synthetic assignment scenarios - no network, no model.

use arc_assign::certificate::{AssignmentCertificate, CertificateError, policy_hash};
use arc_assign::lease::{
    self, CapabilityLease, ChallengeResult, LeaseBody, LeaseError, LeaseRequirements,
    OP_ROW_PROJECTION_I8,
};
use arc_assign::link::{LinkMeasurement, Probe, summarize};
use arc_assign::placement::{
    Candidate, Participant, Placement, PlacementError, Policy, Stage, covers_exactly, place,
};
use arc_assign::queue::{Call, FairQueue, Next, QueueError};
use arc_assign::verify::{VerificationRule, plan};
use arc_crypto::{Hash256, KeyPair, hash_bytes};
use std::collections::BTreeSet;

fn key(i: u8) -> KeyPair {
    KeyPair::from_ed25519_secret_bytes(&hash_bytes(&[b'w', i]).0)
}

fn body(k: &KeyPair) -> LeaseBody {
    LeaseBody {
        version: 1,
        worker: k.address(),
        transport_id: format!("ssh-ed25519:{}", k.address()),
        artifact_id: hash_bytes(b"artifact"),
        execution_profile: "arc.gguf-llama.i8-per-row.rope-interleaved.v1".into(),
        backend: "cpu-neon".into(),
        kernel_set: hash_bytes(b"kernels"),
        operators: [OP_ROW_PROJECTION_I8.to_string()].into_iter().collect(),
        ram_headroom_bytes: 8 << 30,
        claimed_macs_per_s: 2_000_000_000,
        max_concurrency: 4,
        warm_rows: vec![],
        issued_at_height: 10,
        expires_at_height: 1_000,
        nonce: 1,
    }
}

fn members(keys: &[&KeyPair]) -> BTreeSet<[u8; 32]> {
    keys.iter().map(|k| k.address().0).collect()
}

#[test]
fn a_lease_is_used_only_when_signed_current_and_for_this_model() {
    let k = key(1);
    let m = members(&[&k]);
    let req = LeaseRequirements {
        members: &m,
        artifact_id: hash_bytes(b"artifact"),
        execution_profile: "arc.gguf-llama.i8-per-row.rope-interleaved.v1",
        operator: OP_ROW_PROJECTION_I8,
        height: 100,
    };
    let good = CapabilityLease::sign(body(&k), &k).unwrap();
    lease::validate(&good, &req).unwrap();

    // Signed by someone else for this worker.
    let forged = CapabilityLease::sign(body(&k), &key(2)).unwrap();
    assert_eq!(lease::validate(&forged, &req), Err(LeaseError::Signature));
    // Not a committee member.
    let outsider = key(3);
    let foreign = CapabilityLease::sign(body(&outsider), &outsider).unwrap();
    assert_eq!(lease::validate(&foreign, &req), Err(LeaseError::NotMember));
    // Another model; a missing operator; expired.
    let mut b = body(&k);
    b.artifact_id = hash_bytes(b"other");
    assert_eq!(lease::validate(&CapabilityLease::sign(b, &k).unwrap(), &req), Err(LeaseError::WrongModel));
    let mut b = body(&k);
    b.operators.clear();
    assert!(matches!(
        lease::validate(&CapabilityLease::sign(b, &k).unwrap(), &req),
        Err(LeaseError::MissingOperator(_))
    ));
    let mut b = body(&k);
    b.expires_at_height = 100;
    assert!(matches!(
        lease::validate(&CapabilityLease::sign(b, &k).unwrap(), &req),
        Err(LeaseError::Expired { .. })
    ));
}

#[test]
fn claimed_capacity_is_replaced_by_the_measured_one_or_refused() {
    let k = key(1);
    let lease = CapabilityLease::sign(body(&k), &k).unwrap();
    // Answered correctly at 1.8e9 MAC/s against a 2e9 claim: accepted at the
    // measured rate.
    let honest = ChallengeResult { macs: 1_800_000_000, elapsed_us: 1_000_000, correct: true };
    assert_eq!(lease::check_challenge(&lease, &honest, 80), Ok(1_800_000_000));
    // Half the claim: dishonest capacity.
    let slow = ChallengeResult { macs: 1_000_000_000, elapsed_us: 1_000_000, correct: true };
    assert!(matches!(
        lease::check_challenge(&lease, &slow, 80),
        Err(LeaseError::DishonestCapacity { .. })
    ));
    // Fast but wrong is refused outright.
    let wrong = ChallengeResult { macs: 4_000_000_000, elapsed_us: 1_000_000, correct: false };
    assert!(lease::check_challenge(&lease, &wrong, 80).is_err());
}

#[test]
fn a_link_is_summarised_pessimistically() {
    let probes: Vec<Probe> = (1..=20)
        .map(|i| Probe { rtt_us: i * 100, bytes: 1_000_000, transfer_us: 1_000 + i * 10, failed: i == 20 })
        .collect();
    let m = summarize(&probes, 7, false).unwrap();
    assert_eq!(m.samples, 20);
    assert_eq!(m.failure_per_mille, 50);
    assert_eq!(m.rtt_median_us, 1_000);
    assert!(m.rtt_p95_us >= 1_800);
    assert_eq!(m.bandwidth_bps, 1_000_000 * 1_000_000 / (1_000 + 190), "slowest bulk probe");
    assert!(summarize(&[Probe { rtt_us: 1, bytes: 0, transfer_us: 0, failed: true }], 7, false).is_none());
}

fn link(rtt_us: u64, bandwidth_bps: u64) -> LinkMeasurement {
    LinkMeasurement {
        samples: 32,
        rtt_median_us: rtt_us,
        rtt_p95_us: rtt_us,
        jitter_us: 0,
        bandwidth_bps,
        failure_per_mille: 0,
        measured_at: 100,
        simulated: false,
    }
}

fn candidate(i: u8, rate: u64, link: LinkMeasurement) -> Candidate {
    let k = key(i);
    Candidate {
        worker: k.address(),
        transport_id: format!("t{i}"),
        macs_per_s: rate,
        ram_headroom_bytes: 64 << 30,
        max_concurrency: 4,
        link,
    }
}

fn policy() -> Policy {
    Policy {
        max_workers: 8,
        max_link_age: 50,
        now: 110,
        max_failure_per_mille: 20,
        allow_simulated_links: false,
        input_element_bytes: 8,
        output_element_bytes: 8,
        weight_bytes_per_element: 1,
        include_coordinator: true,
    }
}

/// A 7B-like layer's projections: rows x cols per token.
fn stages() -> Vec<Stage> {
    let mut s = Vec::new();
    for layer in 0..4u32 {
        for (tensor, rows, cols) in [("wq", 4096, 4096), ("wk", 4096, 4096), ("wv", 4096, 4096),
                                     ("wo", 4096, 4096), ("w_gate", 11008, 4096),
                                     ("w_up", 11008, 4096), ("w_down", 4096, 11008)] {
            s.push(Stage { layer: Some(layer), tensor: tensor.into(), rows, cols });
        }
    }
    s
}

fn rows_of(p: &Placement, who: &Participant) -> u64 {
    p.stages.iter().flat_map(|s| &s.slices).filter(|s| &s.participant == who).map(|s| s.row_end - s.row_start).sum()
}

#[test]
fn unequal_workers_get_unequal_slices_on_a_fast_network() {
    let fast = link(50, 10_000_000_000);
    let cands = vec![candidate(1, 4_000_000_000, fast), candidate(2, 1_000_000_000, fast)];
    let p = place(&stages(), 1_000_000_000, &cands, &policy()).unwrap();
    assert!(covers_exactly(&p, &stages()));
    assert_eq!(p.workers.len(), 2, "both help on a fast network");
    assert!(p.predicted_token_us < p.coordinator_only_token_us);
    let fast_rows = rows_of(&p, &Participant::Worker(key(1).address()));
    let slow_rows = rows_of(&p, &Participant::Worker(key(2).address()));
    assert!(fast_rows > 3 * slow_rows, "shares follow measured rate: {fast_rows} vs {slow_rows}");
}

#[test]
fn a_slow_network_means_the_coordinator_works_alone() {
    // Stage A's measured WAN: 161 ms round trips. No worker can pay that 28
    // times per token and win.
    let wan = link(161_000, 12_500_000);
    let cands = vec![candidate(1, 8_000_000_000, wan), candidate(2, 8_000_000_000, wan)];
    let p = place(&stages(), 1_000_000_000, &cands, &policy()).unwrap();
    assert!(p.workers.is_empty(), "adding workers would be slower");
    assert_eq!(p.predicted_token_us, p.coordinator_only_token_us);
    assert!(covers_exactly(&p, &stages()));
}

#[test]
fn memory_caps_a_workers_share_and_the_rest_goes_elsewhere() {
    let fast = link(50, 10_000_000_000);
    let mut small = candidate(1, 8_000_000_000, fast);
    let total: u64 = stages().iter().map(|s| s.rows * s.cols).sum();
    small.ram_headroom_bytes = total / 10; // holds a tenth of the rows at most
    let p = place(&stages(), 1_000_000_000, &[small], &policy()).unwrap();
    let rows = rows_of(&p, &Participant::Worker(key(1).address()));
    let all_rows: u64 = stages().iter().map(|s| s.rows).sum();
    assert!(rows * 10 <= all_rows + stages().len() as u64, "{rows} of {all_rows}");
    assert!(covers_exactly(&p, &stages()));
}

#[test]
fn stale_failing_and_simulated_links_are_not_used() {
    let mut stale = link(50, 10_000_000_000);
    stale.measured_at = 10;
    let mut failing = link(50, 10_000_000_000);
    failing.failure_per_mille = 300;
    let mut simulated = link(50, 10_000_000_000);
    simulated.simulated = true;
    let cands = vec![candidate(1, 8_000_000_000, stale), candidate(2, 8_000_000_000, failing),
                     candidate(3, 8_000_000_000, simulated)];
    let p = place(&stages(), 1_000_000_000, &cands, &policy()).unwrap();
    assert!(p.workers.is_empty());
    let mut allow = policy();
    allow.allow_simulated_links = true;
    let p = place(&stages(), 1_000_000_000, &cands, &allow).unwrap();
    assert_eq!(p.workers, vec![key(3).address()], "only when simulated links are allowed");
}

#[test]
fn placement_is_the_same_whatever_order_the_inputs_arrive_in() {
    let fast = link(50, 10_000_000_000);
    let mut cands: Vec<Candidate> = (1..=5).map(|i| candidate(i, i as u64 * 1_000_000_000, fast)).collect();
    let a = place(&stages(), 1_000_000_000, &cands, &policy()).unwrap();
    cands.reverse();
    cands.swap(0, 2);
    let b = place(&stages(), 1_000_000_000, &cands, &policy()).unwrap();
    assert_eq!(a, b);
    // One validator presenting two offers counts once.
    let mut doubled = cands.clone();
    let mut again = candidate(5, 5_000_000_000, fast);
    again.transport_id = "second transport".into();
    doubled.push(again);
    let c = place(&stages(), 1_000_000_000, &doubled, &policy()).unwrap();
    assert_eq!(c.workers.len(), a.workers.len());
}

#[test]
fn without_the_coordinator_the_workers_must_hold_every_row() {
    let fast = link(50, 10_000_000_000);
    let mut tiny = candidate(1, 4_000_000_000, fast);
    tiny.ram_headroom_bytes = 1 << 20;
    let mut p = policy();
    p.include_coordinator = false;
    assert_eq!(place(&stages(), 0, &[tiny], &p), Err(PlacementError::Infeasible));
    let roomy = candidate(2, 4_000_000_000, fast);
    let placed = place(&stages(), 0, &[roomy], &p).unwrap();
    assert!(covers_exactly(&placed, &stages()));
}

fn rule() -> VerificationRule {
    VerificationRule { duplicate_per_mille: 250, spot_rows_per_stage: 2 }
}

fn issue() -> AssignmentCertificate {
    let fast = link(50, 10_000_000_000);
    let cands = vec![candidate(1, 4_000_000_000, fast), candidate(2, 2_000_000_000, fast)];
    AssignmentCertificate::issue(
        hash_bytes(b"request"),
        hash_bytes(b"artifact"),
        "arc.gguf-llama.i8-per-row.rope-interleaved.v1",
        3,
        stages(),
        1_000_000_000,
        cands,
        vec![hash_bytes(b"lease-1"), hash_bytes(b"lease-2")],
        policy(),
        rule(),
    )
    .unwrap()
}

#[test]
fn any_validator_can_recompute_a_certificate_and_refuse_a_forged_one() {
    let cert = issue();
    let authorised = policy_hash(&policy(), &rule());
    cert.verify(&authorised).unwrap();

    let mut forged = cert.clone();
    forged.placement.stages[0].slices.swap(0, 1);
    assert_eq!(forged.verify(&authorised), Err(CertificateError::PlacementMismatch));
    let mut inflated = cert.clone();
    inflated.candidates[0].macs_per_s *= 10; // claims its worker is faster
    assert_eq!(inflated.verify(&authorised), Err(CertificateError::PlacementMismatch));
    let other_policy = policy_hash(&policy(), &VerificationRule { duplicate_per_mille: 0, spot_rows_per_stage: 0 });
    assert_eq!(cert.verify(&other_policy), Err(CertificateError::WrongPolicy));
    assert_ne!(cert.hash(), forged.hash());
}

#[test]
fn the_verification_plan_is_reproducible_and_never_checks_a_slice_with_itself() {
    let cert = issue();
    let seed = cert.hash();
    let a = plan(seed, &cert.placement, &cert.verification);
    let b = plan(seed, &cert.placement, &cert.verification);
    assert_eq!(a, b, "the same seed gives the same plan");
    assert_ne!(a, plan(hash_bytes(b"another seed"), &cert.placement, &cert.verification));
    let all = plan(seed, &cert.placement, &VerificationRule { duplicate_per_mille: 1000, spot_rows_per_stage: 1 });
    for d in &all.duplicates {
        let slice = &cert.placement.stages[d.stage].slices[d.slice];
        assert_ne!(d.checker, slice.participant, "a slice is never checked by its own worker");
    }
    let worker_slices = cert.placement.stages.iter().flat_map(|s| &s.slices)
        .filter(|s| s.participant != Participant::Coordinator).count();
    assert_eq!(all.duplicates.len(), worker_slices, "1000 per mille checks every worker slice");
    for (stage, row) in &all.spot_rows {
        assert!(*row < stages()[*stage].rows);
    }
}

fn call(req: u8, n: u8, deadline: u64) -> Call {
    Call { request: hash_bytes(&[b'r', req]), call_id: hash_bytes(&[b'c', req, n]), deadline }
}

#[test]
fn the_queue_is_fair_bounded_and_honours_deadlines_and_cancellation() {
    let mut q = FairQueue::new(5);
    for n in 0..3 {
        q.push(call(1, n, 100)).unwrap();
    }
    q.push(call(2, 0, 100)).unwrap();
    q.push(call(3, 0, 5)).unwrap();
    assert_eq!(q.push(call(4, 0, 100)), Err(QueueError::Full(5)));

    // Past request 3's deadline: its call is expired, not run.
    match q.next(10) {
        Next::Expired(calls) => assert_eq!(calls, vec![call(3, 0, 5)]),
        other => panic!("{other:?}"),
    }
    // Requests alternate: 1, 2, then 1 again - request 1's three calls do
    // not all go first.
    let order: Vec<Hash256> = (0..3)
        .map(|_| match q.next(10) {
            Next::Run(c) => c.request,
            other => panic!("{other:?}"),
        })
        .collect();
    assert_eq!(order, vec![hash_bytes(&[b'r', 1]), hash_bytes(&[b'r', 2]), hash_bytes(&[b'r', 1])]);
    assert_eq!(q.cancel(&hash_bytes(&[b'r', 1])), 1);
    assert_eq!(q.next(10), Next::Idle);
    assert!(q.is_empty());
}
