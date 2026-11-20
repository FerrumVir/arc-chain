use arc_consensus::pending::{Offered, PendingBlocks, receive_or_hold};
use arc_consensus::{ConsensusEngine, DagBlock, STAKE_ARC, Validator, ValidatorSet};
use arc_crypto::{KeyPair, hash_bytes};
use std::collections::VecDeque;
use std::time::Instant;

const K: u64 = 6;
const R: u64 = 20;

fn committee() -> (ValidatorSet, Vec<KeyPair>) {
    let keys: Vec<KeyPair> = (0..4)
        .map(|i| KeyPair::from_ed25519_secret_bytes(&hash_bytes(format!("lag.{i}").as_bytes()).0))
        .collect();
    let validators = keys
        .iter()
        .enumerate()
        .map(|(i, k)| Validator::new(k.address(), STAKE_ARC, i as u16).expect("valid"))
        .collect();
    (ValidatorSet::new(validators, 1), keys)
}

fn engine(set: &ValidatorSet, key: &KeyPair) -> ConsensusEngine {
    ConsensusEngine::new_with_keypair(set.clone(), key.address(), key.clone())
}

fn run_rounds(engines: &[&ConsensusEngine], rounds: u64, ts: &mut u64) -> Vec<DagBlock> {
    let mut produced = Vec::new();
    for _ in 0..rounds {
        let mut this_round = Vec::new();
        for e in engines {
            *ts += 1;
            if let Ok(b) = e.propose_block(vec![], *ts) {
                this_round.push(b);
            }
        }
        for b in &this_round {
            for e in engines {
                let _ = e.receive_block(b);
            }
        }
        for e in engines {
            e.advance_round();
        }
        produced.extend(this_round);
    }
    produced
}

fn history() -> (ValidatorSet, Vec<KeyPair>, Vec<DagBlock>, Vec<DagBlock>) {
    let (set, keys) = committee();
    let e: Vec<ConsensusEngine> = keys.iter().map(|k| engine(&set, k)).collect();
    let mut ts = 1_700_000_000_000u64;
    let early = run_rounds(&[&e[0], &e[1], &e[2], &e[3]], K + 1, &mut ts);
    let late = run_rounds(&[&e[0], &e[1], &e[2]], R - K, &mut ts);
    assert!(
        e[0].current_round() >= R,
        "the three kept going: {}",
        e[0].current_round()
    );
    (set, keys, early, late)
}

fn restarted(set: &ValidatorSet, keys: &[KeyPair], early: &[DagBlock]) -> ConsensusEngine {
    let lag = engine(set, &keys[3]);
    let mut by_round = early.to_vec();
    by_round.sort_by_key(|b| b.round);
    for b in &by_round {
        lag.receive_block(b).expect("the shared rounds validate");
        lag.advance_round();
    }
    lag
}

#[test]
fn dropping_early_blocks_strands_a_node_that_fell_behind() {
    let (set, keys, early, late) = history();
    let lag = restarted(&set, &keys, &early);
    let start = lag.current_round();

    let mut newest_first = late.clone();
    newest_first.sort_by_key(|b| std::cmp::Reverse(b.round));
    let mut accepted = 0;
    for b in &newest_first {
        if lag.receive_block(b).is_ok() {
            accepted += 1;
            lag.advance_round();
        }
    }
    assert!(
        lag.current_round() <= start + 2,
        "with drop semantics the node should be stranded near round {start}, reached {}",
        lag.current_round()
    );
    assert!(
        accepted < late.len() / 4,
        "dropping should lose almost everything; {accepted} of {} were accepted",
        late.len()
    );
}

#[test]
fn holding_early_blocks_lets_a_node_that_fell_behind_reach_the_tip() {
    let (set, keys, early, late) = history();
    let lag = restarted(&set, &keys, &early);
    let mut pending = PendingBlocks::new();
    let now = Instant::now();

    let mut queue: VecDeque<DagBlock> = {
        let mut v = late.clone();
        v.sort_by_key(|b| std::cmp::Reverse(b.round));
        v.into()
    };
    let mut accepted = 0usize;
    while let Some(block) = queue.pop_front() {
        match receive_or_hold(&lag, &mut pending, block, vec![], now) {
            Offered::Accepted(block, _) => {
                accepted += 1;
                lag.advance_round();
                for (early, _) in pending.release_on(&block.hash) {
                    queue.push_back(early);
                }
                for (early, _) in pending.release_up_to_round(lag.current_round()) {
                    queue.push_back(early);
                }
            }
            Offered::Held => {}
            Offered::NotHeld(error) | Offered::Rejected(error) => {
                panic!("an authentic block from the committee was refused: {error}")
            }
        }
    }

    assert_eq!(
        accepted,
        late.len(),
        "every block the other three produced must eventually be accepted"
    );
    assert!(pending.is_empty(), "{} blocks still held", pending.len());
    assert!(
        lag.current_round() >= R,
        "the restarted node reached round {}, the others reached {R}",
        lag.current_round()
    );
}

#[test]
fn a_block_with_a_wrong_round_parent_is_never_held() {
    let (set, keys, early, _late) = history();
    let lag = restarted(&set, &keys, &early);
    let mut pending = PendingBlocks::new();

    let round0 = early
        .iter()
        .find(|b| b.round == 0)
        .expect("round 0 exists")
        .hash;
    let proposer = engine(&set, &keys[1]);
    let mut forged = DagBlock {
        author: keys[1].address(),
        round: lag.current_round() + 1,
        parents: vec![round0],
        transactions: vec![],
        timestamp: 42,
        hash: arc_crypto::Hash256::ZERO,
        signature: vec![],
        ordering_commitment: DagBlock::compute_ordering_commitment(&[]),
    };
    forged.hash = forged.compute_hash();
    forged.signature = bincode::serialize(&keys[1].sign(&forged.hash).unwrap()).unwrap();
    drop(proposer);

    match receive_or_hold(&lag, &mut pending, forged, vec![], Instant::now()) {
        Offered::Rejected(_) => {}
        other => panic!("a wrong-round parent must be rejected, not {other:?}"),
    }
    assert!(pending.is_empty());
}

const RETAINED: u64 = 100;

fn run_committing(
    engines: &[&ConsensusEngine],
    rounds: u64,
    ts: &mut u64,
    committed: &mut [Vec<(u64, arc_crypto::Hash256)>],
) {
    for _ in 0..rounds {
        let mut this_round = Vec::new();
        for e in engines {
            *ts += 1;
            if let Ok(b) = e.propose_block(vec![], *ts) {
                this_round.push(b);
            }
        }
        for b in &this_round {
            for e in engines {
                let _ = e.receive_block(b);
            }
        }
        for (i, e) in engines.iter().enumerate() {
            e.advance_round();
            committed[i].extend(e.try_commit().into_iter().map(|b| (b.round, b.hash)));
        }
    }
}

fn served_from(peer: &ConsensusEngine, from: u64) -> Vec<DagBlock> {
    (from..=peer.current_round())
        .flat_map(|round| peer.blocks_in_round(round))
        .filter_map(|hash| peer.get_block(&hash))
        .collect()
}

#[test]
fn a_restarted_node_rejoins_from_its_own_commit_cursor_after_peers_pruned_round_zero() {
    let (set, keys) = committee();
    let e: Vec<ConsensusEngine> = keys.iter().map(|k| engine(&set, k)).collect();
    for x in &e {
        x.set_retained_rounds(RETAINED);
    }
    let mut ts = 1_700_000_000_000u64;
    let mut committed = vec![Vec::new(); 4];

    run_committing(
        &[&e[0], &e[1], &e[2], &e[3]],
        3 * RETAINED,
        &mut ts,
        &mut committed,
    );
    let crashed_round = e[3].current_round();
    let cursor = e[3].last_committed_round();
    assert!(
        cursor > RETAINED,
        "the cursor must be past the window for this to test anything"
    );
    run_committing(
        &[&e[0], &e[1], &e[2]],
        RETAINED / 2,
        &mut ts,
        &mut committed[..3],
    );
    assert!(e[0].blocks_in_round(0).is_empty(), "peers pruned round 0");
    assert!(
        !e[0].blocks_in_round(cursor).is_empty(),
        "peers still hold the restarted node's cursor round"
    );

    let restarted = engine(&set, &keys[3]);
    restarted.restore_round_from_local_wal(crashed_round, cursor);

    let oldest = served_from(&e[0], 0);
    assert!(oldest.iter().all(|b| b.round > 0));
    assert!(
        restarted.import_history(&oldest, u64::MAX).is_err(),
        "history from the oldest retained round cannot validate on an empty DAG"
    );
    assert!(
        restarted
            .import_history(&served_from(&e[0], cursor), u64::MAX)
            .is_err()
    );
    assert!(
        restarted.dag_is_empty(),
        "a refused import leaves nothing behind"
    );

    assert!(restarted.set_restart_base_round(cursor + 1).is_err());
    restarted
        .set_restart_base_round(cursor)
        .expect("an empty DAG at its own commit cursor");
    let imported = restarted
        .import_history(&served_from(&e[0], cursor), u64::MAX)
        .expect("history from the cursor imports with the base round open");
    assert!(imported >= e[0].current_round().saturating_sub(1));
    assert!(
        restarted.set_restart_base_round(cursor).is_err(),
        "a base round can be opened only on an empty DAG"
    );

    let mut rejoined = vec![Vec::new(); 4];
    run_committing(
        &[&e[0], &e[1], &e[2], &restarted],
        30,
        &mut ts,
        &mut rejoined,
    );
    let mine = &rejoined[3];
    assert!(
        mine.len() >= 5,
        "the restarted node committed {} anchors",
        mine.len()
    );
    assert!(
        mine.iter().all(|(round, _)| *round >= cursor),
        "a round below the durable cursor was committed again: {mine:?}"
    );
    let peers: std::collections::HashMap<u64, arc_crypto::Hash256> = committed[0]
        .iter()
        .chain(rejoined[0].iter())
        .copied()
        .collect();
    for (round, hash) in mine {
        assert_eq!(
            peers.get(round),
            Some(hash),
            "the restarted node committed a different anchor at round {round}"
        );
    }
    assert_eq!(
        restarted.restart_base_round(),
        None,
        "the base round closed itself"
    );
}

#[test]
fn a_restart_base_round_admits_only_well_formed_signed_blocks() {
    let (set, keys) = committee();
    let e: Vec<ConsensusEngine> = keys.iter().map(|k| engine(&set, k)).collect();
    for x in &e {
        x.set_retained_rounds(RETAINED);
    }
    let mut ts = 1_700_000_000_000u64;
    let mut committed = vec![Vec::new(); 4];
    run_committing(
        &[&e[0], &e[1], &e[2], &e[3]],
        2 * RETAINED,
        &mut ts,
        &mut committed,
    );
    let cursor = e[3].last_committed_round();
    let restarted = engine(&set, &keys[3]);
    restarted.restore_round_from_local_wal(e[3].current_round(), cursor);
    restarted.set_restart_base_round(cursor).unwrap();

    let real = e[0]
        .get_block(&e[0].blocks_in_round(cursor)[0])
        .expect("a block at the base round");
    assert!(restarted.receive_block(&real).is_err());

    let author = keys.iter().find(|k| k.address() == real.author).unwrap();
    let mut variants = Vec::new();
    for parents in [
        vec![],
        vec![arc_crypto::Hash256::ZERO],
        vec![real.parents[0], real.parents[0]],
    ] {
        let mut forged = DagBlock {
            parents,
            hash: arc_crypto::Hash256::ZERO,
            signature: vec![],
            ..real.clone()
        };
        forged.hash = forged.compute_hash();
        forged.signature = bincode::serialize(&author.sign(&forged.hash).unwrap()).unwrap();
        variants.push(forged);
    }
    let mut unsigned = real.clone();
    unsigned.timestamp += 1;
    unsigned.hash = unsigned.compute_hash();
    unsigned.signature = vec![];
    variants.push(unsigned);

    let mut batch = variants.clone();
    batch.extend(served_from(&e[0], cursor));
    restarted
        .import_history(&batch, u64::MAX)
        .expect("the real history imports");
    for v in &variants {
        assert!(
            restarted.get_block(&v.hash).is_none(),
            "{:?} was imported",
            v.parents
        );
    }
    assert!(
        restarted.get_block(&real.hash).is_some(),
        "the authentic base block is admitted"
    );
}

#[test]
fn history_served_from_below_the_base_round_still_reaches_it() {
    let (set, keys) = committee();
    let e: Vec<ConsensusEngine> = keys.iter().map(|k| engine(&set, k)).collect();
    for x in &e {
        x.set_retained_rounds(RETAINED);
    }
    let mut ts = 1_700_000_000_000u64;
    let mut committed = vec![Vec::new(); 4];
    run_committing(
        &[&e[0], &e[1], &e[2], &e[3]],
        2 * RETAINED,
        &mut ts,
        &mut committed,
    );
    let crashed_round = e[3].current_round();
    let cursor = e[3].last_committed_round();
    run_committing(&[&e[0], &e[1], &e[2]], 10, &mut ts, &mut committed[..3]);

    let restarted = engine(&set, &keys[3]);
    restarted.restore_round_from_local_wal(crashed_round, cursor);
    restarted.set_restart_base_round(cursor).unwrap();
    let from_below = served_from(&e[0], cursor - 3);
    assert!(
        from_below.iter().any(|b| b.round < cursor),
        "the batch starts below the base"
    );
    restarted
        .import_history(&from_below, u64::MAX)
        .expect("rounds below the base are skipped, and the base onwards imports");
    assert!(
        restarted.blocks_in_round(cursor - 1).is_empty(),
        "nothing below the base was taken"
    );
    assert!(!restarted.blocks_in_round(cursor).is_empty());
    assert!(restarted.current_round() >= e[0].current_round().saturating_sub(1));
}

#[test]
fn a_base_round_block_nothing_in_the_next_round_supports_is_not_imported() {
    let (set, keys) = committee();
    let e: Vec<ConsensusEngine> = keys.iter().map(|k| engine(&set, k)).collect();
    for x in &e {
        x.set_retained_rounds(RETAINED);
    }
    let mut ts = 1_700_000_000_000u64;
    let mut committed = vec![Vec::new(); 4];
    run_committing(
        &[&e[0], &e[1], &e[2], &e[3]],
        2 * RETAINED,
        &mut ts,
        &mut committed,
    );
    let crashed_round = e[3].current_round();
    let cursor = e[3].last_committed_round();

    let restarted = engine(&set, &keys[3]);
    restarted.restore_round_from_local_wal(crashed_round, cursor);
    restarted.set_restart_base_round(cursor).unwrap();

    let real = e[0]
        .get_block(
            &e[0]
                .blocks_in_round(cursor)
                .into_iter()
                .find(|h| e[0].get_block(h).unwrap().author == keys[1].address())
                .unwrap(),
        )
        .unwrap();
    let mut fabricated = DagBlock {
        parents: vec![
            hash_bytes(b"invented-1"),
            hash_bytes(b"invented-2"),
            hash_bytes(b"invented-3"),
        ],
        timestamp: real.timestamp + 1,
        hash: arc_crypto::Hash256::ZERO,
        signature: vec![],
        ..real.clone()
    };
    fabricated.hash = fabricated.compute_hash();
    fabricated.signature = bincode::serialize(&keys[1].sign(&fabricated.hash).unwrap()).unwrap();

    assert!(restarted.receive_block(&fabricated).is_err());
    let mut batch = vec![fabricated.clone()];
    batch.extend(served_from(&e[0], cursor));
    restarted
        .import_history(&batch, u64::MAX)
        .expect("the supported history imports");
    assert!(
        restarted.get_block(&fabricated.hash).is_none(),
        "the fabricated block was imported"
    );
    assert!(restarted.get_block(&real.hash).is_some());
    let fresh = engine(&set, &keys[3]);
    fresh.restore_round_from_local_wal(crashed_round, cursor);
    fresh.set_restart_base_round(cursor).unwrap();
    assert!(fresh.import_history(&[fabricated], u64::MAX).is_err());
    assert!(fresh.dag_is_empty());
}

#[test]
fn rebasing_to_a_checkpoint_clears_the_dag_and_resumes_after_its_anchor() {
    let (set, keys) = committee();
    let e: Vec<ConsensusEngine> = keys.iter().map(|k| engine(&set, k)).collect();
    let mut ts = 1_700_000_000_000u64;
    let mut committed = vec![Vec::new(); 4];
    run_committing(&[&e[0], &e[1], &e[2], &e[3]], 20, &mut ts, &mut committed);
    let reached = e[3].current_round();
    assert!(!e[3].dag_is_empty());

    let cursor = e[3].rebase_to(500).expect("rebase");
    assert_eq!(cursor, 501);
    assert!(e[3].dag_is_empty(), "nothing below the checkpoint is kept");
    assert_eq!(e[3].last_committed_round(), 501);
    assert!(e[3].current_round() >= 501);
    assert_eq!(
        e[3].restart_base_round(),
        Some(501),
        "the DAG is rebuilt from the resume round"
    );
    assert!(reached < 501);

    let behind = engine(&set, &keys[3]);
    behind.restore_round_from_local_wal(reached, reached - 2);
    assert!(behind.check_rebase(3).is_err());
    assert!(behind.rebase_to(3).is_err());
    assert_eq!(behind.last_committed_round(), reached - 2, "nothing moved");
    assert!(behind.check_rebase(reached).is_ok());

    let fresh = engine(&set, &keys[3]);
    fresh.restore_round_from_local_wal(reached, 2);
    fresh
        .rebase_to(reached - 3)
        .expect("forward in the commit cursor, below the reached round");
    assert!(matches!(
        fresh.propose_block(vec![], ts + 1),
        Err(arc_consensus::ConsensusError::DuplicateBlock)
    ));
}

#[test]
fn a_live_engine_never_re_signs_its_round_after_a_rebase() {
    let (set, keys) = committee();
    let e: Vec<ConsensusEngine> = keys.iter().map(|k| engine(&set, k)).collect();
    let mut ts = 1_700_000_000_000u64;
    let mut committed = vec![Vec::new(); 4];
    run_committing(&[&e[0], &e[1], &e[2], &e[3]], 12, &mut ts, &mut committed);
    ts += 1;
    e[3].propose_block(vec![], ts)
        .expect("signs its block at the reached round");
    let reached = e[3].current_round();
    let committed_round = e[3].last_committed_round();
    assert!(
        committed_round + 1 < reached,
        "a resume round between the commit cursor and the reached round exists"
    );
    let cursor = e[3].rebase_to(committed_round).expect("a forward rebase");
    assert!(cursor < reached);
    assert_eq!(e[3].current_round(), reached);
    ts += 1;
    assert!(matches!(
        e[3].propose_block(vec![], ts),
        Err(arc_consensus::ConsensusError::DuplicateBlock)
    ));
}
