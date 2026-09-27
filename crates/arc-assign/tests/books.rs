//! Reservations, windowed loss and the bounded coordinator books (S1, S2, S6,
//! S7): synthetic, no network, no model.

use arc_assign::book::{
    self, ChallengeBook, ChallengeError, ChallengeState, Exclusion, Exclusions, LeaseBook, Offer,
    Offered, ProbeBook,
};
use arc_assign::certificate::{AssignmentCertificate, policy_hash};
use arc_assign::lease::{
    CapabilityLease, LeaseBody, LeaseError, LeaseRequirements, OP_ROW_PROJECTION_I8,
};
use arc_assign::link::{PathCounters, Probe, windowed_loss_per_mille};
use arc_assign::placement::{Participant, Policy, Stage};
use arc_assign::reservation::{MAX_SLOTS_PER_WORKER, ReservationError, ReservationLedger};
use arc_assign::verify::{Finding, VerificationRule, compare};
use arc_crypto::{Hash256, KeyPair, hash_bytes};
use std::collections::BTreeSet;

const PROFILE: &str = "arc.gguf-llama.i8-per-row.rope-interleaved.v1";

fn key(i: u8) -> KeyPair {
    KeyPair::from_ed25519_secret_bytes(&hash_bytes(&[b'w', i]).0)
}

fn worker(i: u8) -> Hash256 {
    key(i).address()
}

fn request(i: u8) -> Hash256 {
    hash_bytes(&[b'r', i])
}

fn body(k: &KeyPair, nonce: u64) -> LeaseBody {
    LeaseBody {
        version: 1,
        worker: k.address(),
        transport_id: format!("quic:{}", k.address()),
        artifact_id: hash_bytes(b"artifact"),
        execution_profile: PROFILE.into(),
        backend: "cpu-neon".into(),
        kernel_set: hash_bytes(b"kernels"),
        operators: [OP_ROW_PROJECTION_I8.to_string()].into_iter().collect(),
        ram_headroom_bytes: 8 << 30,
        claimed_macs_per_s: 2_000_000_000,
        max_concurrency: 4,
        warm_rows: vec![],
        resident_layers: vec![],
        issued_at_height: 10,
        expires_at_height: 1_000,
        nonce,
    }
}

fn signed(i: u8, nonce: u64) -> CapabilityLease {
    let k = key(i);
    CapabilityLease::sign(body(&k, nonce), &k).unwrap()
}

fn members(ids: &[u8]) -> BTreeSet<[u8; 32]> {
    ids.iter().map(|i| worker(*i).0).collect()
}

fn requirements(members: &BTreeSet<[u8; 32]>, height: u64) -> LeaseRequirements<'_> {
    LeaseRequirements {
        members,
        artifact_id: hash_bytes(b"artifact"),
        execution_profile: PROFILE,
        operator: OP_ROW_PROJECTION_I8,
        height,
    }
}

fn probe(rtt_us: u64) -> Probe {
    Probe {
        rtt_us,
        bytes: 1_000_000,
        transfer_us: 1_000,
        failed: false,
    }
}

#[test]
fn a_worker_is_never_double_booked_and_every_ending_releases_its_slot() {
    let mut ledger = ReservationLedger::new();
    ledger.set_capacity(worker(1), 2);
    ledger
        .reserve_all(&[worker(1)], request(1), 100, 10)
        .unwrap();
    ledger
        .reserve_all(&[worker(1)], request(2), 100, 10)
        .unwrap();
    assert_eq!(ledger.free(&worker(1)), 0);
    assert_eq!(
        ledger.reserve_all(&[worker(1)], request(3), 100, 10),
        Err(ReservationError::NoFreeSlot)
    );
    // Asking again for a slot already held takes no second one.
    ledger
        .reserve_all(&[worker(1)], request(1), 100, 10)
        .unwrap();
    assert_eq!(ledger.len(), 2);

    // Finalized or abandoned: released.
    assert_eq!(ledger.release(&request(1)), 1);
    assert_eq!(ledger.free(&worker(1)), 1);
    // Refund-eligible (height + 1 >= expires_at): swept, and not before.
    assert_eq!(ledger.sweep(98), 0);
    assert_eq!(ledger.sweep(99), 1);
    assert!(ledger.is_empty());
    // A request that is already refund-eligible cannot reserve.
    assert_eq!(
        ledger.reserve_all(&[worker(1)], request(4), 100, 99),
        Err(ReservationError::AlreadyExpired)
    );
}

#[test]
fn a_placement_reserves_on_every_worker_or_on_none() {
    let mut ledger = ReservationLedger::new();
    ledger.set_capacity(worker(1), 1);
    ledger.set_capacity(worker(2), 1);
    ledger
        .reserve_all(&[worker(2)], request(9), 100, 10)
        .unwrap();
    // worker 2 is full, so the pair fails and worker 1 keeps its slot.
    assert_eq!(
        ledger.reserve_all(&[worker(1), worker(2)], request(1), 100, 10),
        Err(ReservationError::NoFreeSlot)
    );
    assert_eq!(ledger.free(&worker(1)), 1);
    // A worker the ledger does not know is refused the same way.
    assert_eq!(
        ledger.reserve_all(&[worker(1), worker(3)], request(1), 100, 10),
        Err(ReservationError::UnknownWorker)
    );
    assert_eq!(ledger.free(&worker(1)), 1);
    assert_eq!(ledger.len(), 1);
}

#[test]
fn reassignment_moves_a_slot_in_one_step() {
    let mut ledger = ReservationLedger::new();
    ledger.set_capacity(worker(1), 1);
    ledger.set_capacity(worker(2), 1);
    ledger
        .reserve_all(&[worker(1)], request(1), 100, 10)
        .unwrap();
    ledger
        .reserve_all(&[worker(2)], request(2), 100, 10)
        .unwrap();
    // The target is full: nothing moves.
    assert_eq!(
        ledger.reassign(&request(1), &worker(1), worker(2)),
        Err(ReservationError::NoFreeSlot)
    );
    assert_eq!(ledger.free(&worker(1)), 0);
    ledger.release(&request(2));
    ledger.reassign(&request(1), &worker(1), worker(2)).unwrap();
    assert_eq!(ledger.free(&worker(1)), 1);
    assert_eq!(ledger.free(&worker(2)), 0);
    assert_eq!(ledger.len(), 1);
    assert_eq!(
        ledger.reassign(&request(7), &worker(1), worker(2)),
        Err(ReservationError::NotHeld)
    );
}

#[test]
fn a_lease_cannot_claim_unbounded_slots_and_a_departed_worker_frees_everything() {
    let mut ledger = ReservationLedger::new();
    ledger.set_capacity(worker(1), u32::MAX);
    assert_eq!(ledger.free(&worker(1)), MAX_SLOTS_PER_WORKER);
    for r in 0..3 {
        ledger
            .reserve_all(&[worker(1)], request(r), 100, 10)
            .unwrap();
    }
    assert_eq!(ledger.remove_worker(&worker(1)), 3);
    assert_eq!(ledger.free(&worker(1)), 0);
    assert!(ledger.is_empty());
    assert_eq!(ledger.workers(), 0);
}

#[test]
fn a_lowered_capacity_keeps_the_holds_taken_and_blocks_new_ones() {
    let mut ledger = ReservationLedger::new();
    ledger.set_capacity(worker(1), 3);
    for r in 0..3 {
        ledger
            .reserve_all(&[worker(1)], request(r), 100, 10)
            .unwrap();
    }
    ledger.set_capacity(worker(1), 1);
    assert_eq!(ledger.len(), 3);
    assert_eq!(ledger.free(&worker(1)), 0);
    ledger.release(&request(0));
    ledger.release(&request(1));
    assert_eq!(
        ledger.free(&worker(1)),
        0,
        "one hold still uses the one slot"
    );
    ledger.release(&request(2));
    assert_eq!(ledger.free(&worker(1)), 1);
}

#[test]
fn loss_is_measured_per_connection_window_and_never_goes_negative() {
    let at = |generation, sent_packets, lost_packets| PathCounters {
        generation,
        sent_packets,
        lost_packets,
    };
    assert_eq!(
        windowed_loss_per_mille(&at(1, 1_000, 10), &at(1, 2_000, 30)),
        Some(20)
    );
    // A reconnect is a new window, not a delta.
    assert_eq!(
        windowed_loss_per_mille(&at(1, 1_000, 10), &at(2, 50, 0)),
        None
    );
    // Counters that went backwards, or a window with nothing sent.
    assert_eq!(
        windowed_loss_per_mille(&at(1, 1_000, 10), &at(1, 900, 10)),
        None
    );
    assert_eq!(
        windowed_loss_per_mille(&at(1, 1_000, 10), &at(1, 1_000, 12)),
        None
    );
    // Losses of packets sent before the window are capped.
    assert_eq!(
        windowed_loss_per_mille(&at(1, 1_000, 0), &at(1, 1_010, 50)),
        Some(1000)
    );
}

#[test]
fn the_probe_book_keeps_recent_probes_and_the_worse_of_probe_and_transport_loss() {
    let mut probes = ProbeBook::new(false);
    for at in 1..=40u64 {
        probes.record_probe(&worker(1), at, probe(100 + at));
    }
    assert_eq!(probes.len(), book::MAX_PROBES_PER_WORKER);
    let link = probes.link(&worker(1)).unwrap();
    assert_eq!(link.samples as usize, book::MAX_PROBES_PER_WORKER);
    assert_eq!(link.measured_at, 40, "measured when the newest probe ran");
    assert_eq!(link.failure_per_mille, 0);
    assert!(!link.simulated);

    let counters = |generation, sent_packets, lost_packets| PathCounters {
        generation,
        sent_packets,
        lost_packets,
    };
    assert_eq!(
        probes.record_counters(&worker(1), counters(1, 1_000, 0)),
        None
    );
    assert_eq!(
        probes.record_counters(&worker(1), counters(1, 2_000, 100)),
        Some(100)
    );
    assert_eq!(probes.link(&worker(1)).unwrap().failure_per_mille, 100);
    // After a reconnect the old connection's loss no longer counts.
    assert_eq!(probes.record_counters(&worker(1), counters(2, 10, 0)), None);
    assert_eq!(probes.link(&worker(1)).unwrap().failure_per_mille, 0);

    // A worker whose every probe failed has no link.
    probes.record_probe(
        &worker(2),
        5,
        Probe {
            failed: true,
            ..probe(100)
        },
    );
    assert!(probes.link(&worker(2)).is_none());
    // Leaving the member set drops everything about the worker.
    assert_eq!(probes.retain_members(&members(&[1])), 1);
    assert_eq!(probes.len(), book::MAX_PROBES_PER_WORKER);
}

#[test]
fn the_lease_book_keeps_one_current_lease_per_member_and_ignores_replays() {
    let set = members(&[1, 2]);
    let req = requirements(&set, 100);
    let mut leases = LeaseBook::new();
    assert_eq!(leases.offer(signed(1, 1), &req), Ok(Offered::Accepted));
    assert_eq!(
        leases.offer(signed(1, 1), &req),
        Ok(Offered::Stale),
        "replayed"
    );
    assert_eq!(leases.offer(signed(1, 2), &req), Ok(Offered::Accepted));
    assert_eq!(
        leases.offer(signed(1, 1), &req),
        Ok(Offered::Stale),
        "reordered"
    );
    assert_eq!(leases.get(&worker(1)).unwrap().body.nonce, 2);
    // Not a member: refused, and the book does not grow.
    assert_eq!(leases.offer(signed(3, 1), &req), Err(LeaseError::NotMember));
    assert_eq!(leases.len(), 1);

    assert_eq!(leases.offer(signed(2, 1), &req), Ok(Offered::Accepted));
    // Worker 2 leaves the committee; then worker 1's lease expires.
    assert_eq!(leases.retain_current(&members(&[1]), 500), [worker(2)]);
    assert_eq!(leases.retain_current(&members(&[1]), 1_000), [worker(1)]);
    assert!(leases.is_empty());
}

#[test]
fn a_worker_is_challenged_once_per_epoch() {
    let mut challenges = ChallengeBook::new(3);
    challenges.issue(&worker(1), 50).unwrap();
    assert_eq!(
        challenges.issue(&worker(1), 60),
        Err(ChallengeError::AlreadyChallenged(3))
    );
    assert_eq!(
        challenges.answer(&worker(1), Ok(1_800_000_000), 40),
        Ok(ChallengeState::Measured {
            macs_per_s: 1_800_000_000
        })
    );
    assert_eq!(challenges.measured(&worker(1)), Some(1_800_000_000));
    // Answered once: a second answer has nothing in flight to answer.
    assert_eq!(
        challenges.answer(&worker(1), Ok(1), 41),
        Err(ChallengeError::NotInFlight)
    );

    // A correct answer after the deadline is still a refusal.
    challenges.issue(&worker(2), 50).unwrap();
    assert_eq!(
        challenges.answer(&worker(2), Ok(1_800_000_000), 51),
        Ok(ChallengeState::Refused)
    );
    // An unanswered challenge expires into a refusal.
    challenges.issue(&worker(3), 50).unwrap();
    assert_eq!(challenges.expire(50), 0);
    assert_eq!(challenges.expire(51), 1);
    assert_eq!(challenges.state(&worker(3)), Some(ChallengeState::Refused));

    // The next epoch starts clean.
    assert_eq!(challenges.begin_epoch(4), 3);
    assert_eq!(challenges.epoch(), 4);
    challenges.issue(&worker(2), 90).unwrap();
}

#[test]
fn a_fault_or_repeated_failures_exclude_a_worker_until_the_next_epoch() {
    let certificate = hash_bytes(b"certificate");
    let mut exclusions = Exclusions::new(3);
    let same = compare(
        0,
        Participant::Worker(worker(1)),
        hash_bytes(b"a"),
        hash_bytes(b"a"),
    );
    assert_eq!(same, Finding::Agrees);
    assert_eq!(exclusions.fault(certificate, &same), None);
    // A finding against the coordinator excludes nobody.
    let own = compare(
        0,
        Participant::Coordinator,
        hash_bytes(b"a"),
        hash_bytes(b"b"),
    );
    assert_eq!(exclusions.fault(certificate, &own), None);

    let wrong = compare(
        2,
        Participant::Worker(worker(1)),
        hash_bytes(b"a"),
        hash_bytes(b"b"),
    );
    assert_eq!(exclusions.fault(certificate, &wrong), Some(worker(1)));
    assert!(exclusions.is_excluded(&worker(1)));
    let later = compare(
        5,
        Participant::Worker(worker(1)),
        hash_bytes(b"c"),
        hash_bytes(b"d"),
    );
    exclusions.fault(hash_bytes(b"later"), &later);
    assert_eq!(
        exclusions.get(&worker(1)),
        Some(&Exclusion::Fault {
            certificate,
            stage: 2,
            expected_digest: hash_bytes(b"a"),
            found_digest: hash_bytes(b"b"),
        }),
        "the first fault of the epoch is the evidence kept"
    );

    // Failures count only while consecutive.
    assert!(!exclusions.call_failed(&worker(2)));
    assert!(!exclusions.call_failed(&worker(2)));
    exclusions.call_succeeded(&worker(2));
    assert!(!exclusions.call_failed(&worker(2)));
    assert!(!exclusions.call_failed(&worker(2)));
    assert!(exclusions.call_failed(&worker(2)));
    assert_eq!(
        exclusions.get(&worker(2)),
        Some(&Exclusion::Failures(book::FAILURES_BEFORE_EXCLUSION))
    );

    // A reconnect lifts an exclusion for failures, never one for a fault.
    assert!(exclusions.forgive_failures(&worker(2)));
    assert!(!exclusions.is_excluded(&worker(2)));
    assert!(!exclusions.forgive_failures(&worker(1)));
    assert!(exclusions.is_excluded(&worker(1)));

    assert_eq!(exclusions.begin_epoch(4), 1);
    assert!(!exclusions.is_excluded(&worker(1)));
    assert!(exclusions.is_empty());
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

#[test]
fn only_workers_with_every_input_become_candidates_and_the_certificate_recomputes() {
    let set = members(&[1, 2, 3, 4, 5]);
    let req = requirements(&set, 100);
    let mut leases = LeaseBook::new();
    let mut challenges = ChallengeBook::new(3);
    let mut probes = ProbeBook::new(false);
    let mut ledger = ReservationLedger::new();
    let mut exclusions = Exclusions::new(3);
    for i in 1..=5u8 {
        leases.offer(signed(i, 1), &req).unwrap();
        ledger.set_capacity(worker(i), 4);
        if i != 2 {
            challenges.issue(&worker(i), 50).unwrap();
            // Measured above the 2e9 claim: the claim caps it.
            challenges
                .answer(&worker(i), Ok(3_000_000_000), 40)
                .unwrap();
        }
        if i != 3 {
            probes.record_probe(&worker(i), 100, probe(50));
        }
    }
    // Worker 2 has no measured rate, worker 3 no probe, worker 4 is
    // excluded, worker 5 has no free slot.
    exclusions.fault(
        hash_bytes(b"earlier"),
        &compare(
            0,
            Participant::Worker(worker(4)),
            hash_bytes(b"a"),
            hash_bytes(b"b"),
        ),
    );
    ledger.set_capacity(worker(5), 1);
    ledger
        .reserve_all(&[worker(5)], request(9), 500, 100)
        .unwrap();
    ledger
        .reserve_all(&[worker(1)], request(8), 500, 100)
        .unwrap();

    let (candidates, digests) =
        book::candidates(&req, &leases, &challenges, &probes, &ledger, &exclusions);
    assert_eq!(candidates.len(), 1);
    let only = &candidates[0];
    assert_eq!(only.worker, worker(1));
    assert_eq!(
        only.macs_per_s, 2_000_000_000,
        "the measured rate, capped at the claim"
    );
    assert_eq!(
        only.max_concurrency, 3,
        "free slots, not the lease's maximum"
    );
    assert_eq!(only.link.measured_at, 100);
    assert_eq!(digests, [signed(1, 1).digest()]);

    // An expired lease drops out without being evicted first.
    let late = requirements(&set, 1_000);
    assert!(
        book::candidates(&late, &leases, &challenges, &probes, &ledger, &exclusions)
            .0
            .is_empty()
    );

    // What the books produce is exactly what a certificate binds.
    let stages = vec![Stage {
        layer: Some(0),
        tensor: "wq".into(),
        rows: 4096,
        cols: 4096,
    }];
    let rule = VerificationRule {
        duplicate_per_mille: 250,
        spot_rows_per_stage: 2,
    };
    let certificate = AssignmentCertificate::issue(
        request(1),
        hash_bytes(b"artifact"),
        PROFILE,
        3,
        stages,
        1_000_000_000,
        candidates,
        digests,
        policy(),
        rule,
    )
    .unwrap();
    certificate.verify(&policy_hash(&policy(), &rule)).unwrap();
}

#[test]
fn an_operator_offer_uses_its_measured_rate_and_needs_every_other_input() {
    // Option (a): this operator's own machine, configured rather than leased.
    let machine = hash_bytes(b"operator machine rack-1");
    let offer = || Offer {
        worker: machine,
        transport_id: "rack-1".into(),
        ram_headroom_bytes: 16 << 30,
        claimed_macs_per_s: None,
        resident_layers: vec![],
        resident_output: false,
        digest: hash_bytes(b"rack-1 config entry"),
    };
    let mut challenges = ChallengeBook::new(3);
    let mut probes = ProbeBook::new(false);
    let mut ledger = ReservationLedger::new();
    let exclusions = Exclusions::new(3);
    // No measured rate yet: not a candidate.
    let none = book::candidates_from([offer()], &challenges, &probes, &ledger, &exclusions);
    assert!(none.0.is_empty());
    challenges.issue(&machine, 50).unwrap();
    challenges.answer(&machine, Ok(3_000_000_000), 40).unwrap();
    probes.record_probe(&machine, 100, probe(50));
    ledger.set_capacity(machine, 2);
    let (candidates, digests) =
        book::candidates_from([offer()], &challenges, &probes, &ledger, &exclusions);
    assert_eq!(candidates.len(), 1);
    assert_eq!(candidates[0].macs_per_s, 3_000_000_000, "no claim caps it");
    assert_eq!(candidates[0].transport_id, "rack-1");
    assert_eq!(candidates[0].max_concurrency, 2);
    assert_eq!(digests, [hash_bytes(b"rack-1 config entry")]);
}
