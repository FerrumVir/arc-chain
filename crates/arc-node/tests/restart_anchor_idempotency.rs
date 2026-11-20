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

    fn apply(&self, anchor: &DagBlock) -> u64 {
        let proof = anchor.state_decision_commitment(&domain());
        let (block, _) = self
            .state
            .execute_block_adaptive_at_with_proof(&[], anchor.author, anchor.timestamp, proof)
            .expect("canonical execution");
        block.header.height
    }

    fn height_for(&self, anchor: &DagBlock) -> Option<u64> {
        let decision = anchor.state_decision_commitment(&domain());
        self.manager
            .canonical_height_for_decision(&self.state, decision)
    }
}

#[test]
fn the_record_converts_applied_round_to_the_next_round_to_scan() {
    let mut record = ConsensusSigningRecord::default();
    assert_eq!(record.next_round_to_scan(), 0, "nothing applied yet");

    record.last_applied_round = Some(575);
    assert_eq!(
        record.next_round_to_scan(),
        576,
        "having applied 575, the scan must resume at 576 - resuming at 575 \
         re-commits that anchor"
    );

    record.last_applied_round = Some(0);
    assert_eq!(record.next_round_to_scan(), 1);
    record.last_applied_round = None;
    assert_eq!(record.next_round_to_scan(), 0);
}

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

#[test]
fn a_crash_after_applying_but_before_recording_does_not_duplicate_the_block() {
    let mut node = Node::fresh();
    for r in 100..=104 {
        node.apply(&anchor(r, 1));
    }
    let repeated = anchor(104, 1);
    let height_before = node.state.height();
    let recorded_height = node.height_for(&repeated).expect("applied");

    node = node.reopen();
    let stale_record = ConsensusSigningRecord::default();
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

    let second_height = node.apply(&second);
    assert_eq!(second_height, after_first + 1, "no gap and no duplicate");
    assert_eq!(node.height_for(&second), Some(second_height));
    assert_eq!(node.height_for(&first), Some(after_first));
}

#[test]
fn repeated_restarts_keep_the_canonical_chain_identical() {
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
        for r in 300..=305 {
            assert!(node.height_for(&anchor(r, 1)).is_some());
        }
    }
}

#[test]
fn balances_are_unchanged_by_replaying_an_applied_anchor() {
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

#[test]
fn an_anchor_is_recognised_across_the_whole_retention_window() {
    let node = Node::fresh();

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

    for (a, height) in rounds.iter().zip(&heights) {
        assert_eq!(node.height_for(a), Some(*height));
    }
}

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
