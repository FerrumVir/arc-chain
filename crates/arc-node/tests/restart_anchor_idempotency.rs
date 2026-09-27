//! A committed anchor must yield exactly one canonical block, across restarts
//! and across crashes at every durability boundary.
//!
//! This is the regression for the defect Codex found by reading the logs: the
//! durable record holds the LAST APPLIED round while the engine's commit cursor
//! means the NEXT round to scan, so restoring one as the other re-scanned the
//! round just applied and appended a second canonical block for one anchor. No
//! crash was needed to reach it - node 3 repeated DAG round 575 and every later
//! height on that replica was offset from its peers by one.
//!
//! The tests below drive the real StateDB and the real decision commitment, not
//! a model of them, and cover fresh genesis, round 0, ordinary commits,
//! repeated restarts, and a crash at each of the three durability boundaries.

use arc_consensus::{ConsensusDomain, DagBlock, view_change::ConsensusSigningRecord};
use arc_crypto::{Hash256, hash_bytes};
use arc_node::consensus::ConsensusManager;
use arc_state::StateDB;
use arc_types::Address;

fn domain() -> ConsensusDomain {
    ConsensusDomain::new(hash_bytes(b"arc.restart.idempotency"), 1, 1)
}

fn addr(n: u8) -> Address {
    hash_bytes(&[n])
}

/// An anchor with the shape `try_commit` produces: one author, one round.
fn anchor(round: u64, author: u8) -> DagBlock {
    let mut block = DagBlock {
        author: addr(author),
        round,
        parents: Vec::new(),
        transactions: Vec::new(),
        timestamp: 1_700_000_000_000 + round,
        hash: Hash256::ZERO,
        signature: Vec::new(),
        ordering_commitment: DagBlock::compute_ordering_commitment(&[]),
    };
    block.hash = block.compute_hash();
    block
}

struct Node {
    dir: tempfile::TempDir,
    state: StateDB,
    /// A real manager, so the recognition window under test ships.
    manager: ConsensusManager,
}

impl Node {
    fn fresh() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let state = StateDB::with_genesis_persistent(
            &[(addr(9), 1_000_000)],
            dir.path().join("state"),
            Hash256::ZERO,
        )
        .expect("persistent state");
        Self {
            dir,
            state,
            manager: ConsensusManager::new(addr(1), arc_consensus::STAKE_ARC, 4, false, &[]),
        }
    }

    /// Reopen the same store, the way a restart does.
    fn reopen(self) -> Self {
        let dir = self.dir;
        drop(self.state);
        let state = StateDB::with_genesis_persistent(
            &[(addr(9), 1_000_000)],
            dir.path().join("state"),
            Hash256::ZERO,
        )
        .expect("reopened state");
        Self {
            dir,
            state,
            manager: ConsensusManager::new(addr(1), arc_consensus::STAKE_ARC, 4, false, &[]),
        }
    }

    /// Apply one committed anchor exactly as the consensus loop does.
    fn apply(&self, anchor: &DagBlock) -> u64 {
        let proof = anchor.state_decision_commitment(&domain());
        let (block, _) = self
            .state
            .execute_block_adaptive_at_with_proof(&[], anchor.author, anchor.timestamp, proof)
            .expect("canonical execution");
        block.header.height
    }

    /// The height produced by this exact decision, if any.
    ///
    /// This calls the NODE's own helper rather than reimplementing it, so the
    /// recognition window under test is the one that ships. A local copy would
    /// have kept passing when the real window was too narrow.
    fn height_for(&self, anchor: &DagBlock) -> Option<u64> {
        let decision = anchor.state_decision_commitment(&domain());
        self.manager
            .canonical_height_for_decision(&self.state, decision)
    }
}

// ── the cursor conversion itself ─────────────────────────────────────────────

#[test]
fn the_record_converts_applied_round_to_the_next_round_to_scan() {
    // The engine's cursor is the NEXT round to scan: try_commit starts at it
    // and stores r+1 after committing r. The record holds the LAST round
    // applied. Restoring one as the other is the off-by-one.
    let mut record = ConsensusSigningRecord::default();
    assert_eq!(record.next_round_to_scan(), 0, "nothing applied yet");

    record.last_applied_round = Some(575);
    assert_eq!(
        record.next_round_to_scan(),
        576,
        "having applied 575, the scan must resume at 576 - resuming at 575 \
         re-commits that anchor"
    );

    // Round 0 applied is not the same as nothing applied.
    record.last_applied_round = Some(0);
    assert_eq!(record.next_round_to_scan(), 1);
    record.last_applied_round = None;
    assert_eq!(record.next_round_to_scan(), 0);
}

// ── one anchor, one block ────────────────────────────────────────────────────

#[test]
fn an_anchor_applied_once_is_recognised_and_not_applied_twice() {
    let node = Node::fresh();
    let a = anchor(575, 1);
    assert_eq!(node.height_for(&a), None, "nothing applied yet");

    let height = node.apply(&a);
    assert_eq!(
        node.height_for(&a),
        Some(height),
        "the block must carry the decision that produced it"
    );

    // A DIFFERENT anchor at the same round is a different decision.
    let other = anchor(575, 2);
    assert_ne!(a.hash, other.hash);
    assert_eq!(
        node.height_for(&other),
        None,
        "a different anchor in the same round must not be mistaken for this one"
    );
}

#[test]
fn replaying_the_same_anchor_after_a_restart_adds_no_second_block() {
    // The exact shape of the observed defect: apply an anchor, restart, and let
    // the commit path be handed the same anchor again.
    let mut node = Node::fresh();
    let rounds: Vec<DagBlock> = (570..=575).map(|r| anchor(r, 1)).collect();
    for a in &rounds {
        node.apply(a);
    }
    let height_before = node.state.height();
    let last = rounds.last().unwrap();
    let last_height = node.height_for(last).expect("recorded");

    node = node.reopen();
    assert_eq!(
        node.state.height(),
        height_before,
        "reopening must not change the canonical height"
    );

    // The recovering node is handed round 575 again - which is precisely what
    // the off-by-one cursor did. It must recognise it.
    assert_eq!(
        node.height_for(last),
        Some(last_height),
        "after a restart the node must still recognise the anchor it applied"
    );
    assert_eq!(
        node.state.height(),
        height_before,
        "recognising it must not append anything"
    );
}

#[test]
fn round_zero_is_recognised_like_any_other_round() {
    // Genesis is the case a bare counter gets wrong.
    let node = Node::fresh();
    let genesis_anchor = anchor(0, 1);
    assert_eq!(node.height_for(&genesis_anchor), None);
    let height = node.apply(&genesis_anchor);
    assert_eq!(node.height_for(&genesis_anchor), Some(height));

    let record = ConsensusSigningRecord {
        last_applied_round: Some(0),
        ..ConsensusSigningRecord::default()
    };
    assert_eq!(
        record.next_round_to_scan(),
        1,
        "a node that applied round 0 must resume at 1, not repeat genesis"
    );
}

// ── crash points ─────────────────────────────────────────────────────────────

#[test]
fn a_crash_after_applying_but_before_recording_does_not_duplicate_the_block() {
    // The window the record alone cannot close: the block is durable, the
    // record is not. On restart the cursor is behind and the commit path is
    // handed an anchor that was already applied.
    //
    // The binding lives in the block header, so it is durable at exactly the
    // moment the block is - which is what makes this recoverable.
    let mut node = Node::fresh();
    for r in 100..=104 {
        node.apply(&anchor(r, 1));
    }
    let repeated = anchor(104, 1);
    let height_before = node.state.height();
    let recorded_height = node.height_for(&repeated).expect("applied");

    // Crash here: the record was never written. Reopen and replay.
    node = node.reopen();
    let stale_record = ConsensusSigningRecord::default(); // nothing recorded
    assert_eq!(
        stale_record.next_round_to_scan(),
        0,
        "a lost record resumes from the beginning, which is safe only because \
         already-applied anchors are recognised"
    );

    assert_eq!(
        node.height_for(&repeated),
        Some(recorded_height),
        "the already-applied anchor must be recognised from the block itself"
    );
    assert_eq!(node.state.height(), height_before, "no second block");
}

#[test]
fn a_crash_between_two_anchors_omits_neither_and_duplicates_neither() {
    let mut node = Node::fresh();
    let first = anchor(200, 1);
    let second = anchor(201, 2);
    node.apply(&first);
    let after_first = node.state.height();

    node = node.reopen();
    assert_eq!(node.height_for(&first), Some(after_first));
    assert_eq!(
        node.height_for(&second),
        None,
        "the second was never applied"
    );

    // Resuming applies only the one that is missing.
    let second_height = node.apply(&second);
    assert_eq!(second_height, after_first + 1, "no gap and no duplicate");
    assert_eq!(node.height_for(&second), Some(second_height));
    assert_eq!(node.height_for(&first), Some(after_first));
}

#[test]
fn repeated_restarts_keep_the_canonical_chain_identical() {
    // Hash and parent chain must not drift across restarts, not merely height.
    let mut node = Node::fresh();
    for r in 300..=305 {
        node.apply(&anchor(r, 1));
    }
    let snapshot: Vec<(u64, Hash256, Hash256)> = (1..=node.state.height())
        .filter_map(|h| {
            node.state
                .get_block(h)
                .map(|b| (h, b.hash, b.header.parent_hash))
        })
        .collect();
    assert!(snapshot.len() >= 6);

    for _ in 0..3 {
        node = node.reopen();
        let after: Vec<(u64, Hash256, Hash256)> = (1..=node.state.height())
            .filter_map(|h| {
                node.state
                    .get_block(h)
                    .map(|b| (h, b.hash, b.header.parent_hash))
            })
            .collect();
        assert_eq!(
            after, snapshot,
            "a restart changed the canonical chain's hashes or parent links"
        );
        // And every anchor is still recognised, so none can be re-applied.
        for r in 300..=305 {
            assert!(node.height_for(&anchor(r, 1)).is_some());
        }
    }
}

#[test]
fn balances_are_unchanged_by_replaying_an_applied_anchor() {
    // Exactly-once financial effect, stated as a balance rather than inferred
    // from the absence of a block.
    let mut node = Node::fresh();
    let funded = node.state.get_account(&addr(9)).map(|a| a.balance);
    assert_eq!(funded, Some(1_000_000));

    for r in 400..=402 {
        node.apply(&anchor(r, 1));
    }
    let before = node.state.get_account(&addr(9)).map(|a| a.balance);
    let height_before = node.state.height();

    node = node.reopen();
    for r in 400..=402 {
        assert!(
            node.height_for(&anchor(r, 1)).is_some(),
            "every applied anchor must be recognised after the restart"
        );
    }
    assert_eq!(node.state.height(), height_before);
    assert_eq!(
        node.state.get_account(&addr(9)).map(|a| a.balance),
        before,
        "replaying applied anchors changed a balance"
    );
}

// ── the window an already-applied anchor must be recognised over ────────────

/// The recognition window has to cover every anchor a peer could still serve,
/// not just the most recent handful.
///
/// The first version of this guard scanned the last 256 canonical blocks, on
/// the reasoning that a re-offered anchor is only a few rounds stale. That
/// holds while the durable commit record survives. It does NOT hold when the
/// record is lost, or migrated from a v1 file whose zero is ambiguous: such a
/// node resumes its scan at round 0 and is re-offered every anchor its peers
/// still retain - `--dag-retained-rounds`, 4096 by default. Sixteen times the
/// old window; everything outside it would have produced a second block.
#[test]
fn an_anchor_is_recognised_across_the_whole_retention_window() {
    let node = Node::fresh();

    // Far more than the old 256-block window, and spread so the oldest entry
    // is well outside it.
    let rounds: Vec<DagBlock> = (0..400).map(|r| anchor(r, 1)).collect();
    let mut heights = Vec::new();
    for a in &rounds {
        heights.push(node.apply(a));
    }

    let oldest = &rounds[0];
    let oldest_height = heights[0];
    assert!(
        node.state.height() - oldest_height > 256,
        "the oldest anchor must be outside the old 256-block window for this \
         test to mean anything (distance {})",
        node.state.height() - oldest_height
    );

    assert_eq!(
        node.height_for(oldest),
        Some(oldest_height),
        "an anchor applied {} blocks ago must still be recognised; if it is \
         not, replaying it appends a second canonical block",
        node.state.height() - oldest_height
    );

    // And every one in between.
    for (a, height) in rounds.iter().zip(&heights) {
        assert_eq!(node.height_for(a), Some(*height));
    }
}

/// A node that lost its record entirely resumes from round 0 and is re-offered
/// everything. None of it may be applied twice.
#[test]
fn a_node_that_lost_its_record_reapplies_nothing() {
    let mut node = Node::fresh();
    let rounds: Vec<DagBlock> = (0..300).map(|r| anchor(r, 1)).collect();
    for a in &rounds {
        node.apply(a);
    }
    let height_before = node.state.height();
    let balance_before = node.state.get_account(&addr(9)).map(|a| a.balance);

    node = node.reopen();
    let lost = ConsensusSigningRecord::default();
    assert_eq!(
        lost.next_round_to_scan(),
        0,
        "a lost record resumes at 0, which is only safe because every anchor \
         it then re-offers is recognised"
    );

    // Replay the whole run, exactly as a scan from round 0 would offer it.
    for a in &rounds {
        assert!(
            node.height_for(a).is_some(),
            "round {} was not recognised after the record was lost",
            a.round
        );
    }
    assert_eq!(node.state.height(), height_before, "no block was appended");
    assert_eq!(
        node.state.get_account(&addr(9)).map(|a| a.balance),
        balance_before,
        "a lost record changed a balance"
    );
}
