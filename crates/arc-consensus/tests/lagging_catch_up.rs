//! A node that falls behind must be able to catch up from live gossip.
//!
//! The soak diagnostics measured what happened when it could not: a node two
//! rounds behind found every live block either more than one round ahead or
//! missing parents - one node accepted 2 of 83 - so it advanced only through
//! history requests throttled to one per two seconds. At N=4 the tip needs
//! three blocks, so two such nodes held the whole network to that pace, before
//! any fault and again after a restart.
//!
//! This is the smallest deterministic reproduction. Validator 3 runs rounds
//! 0..=K with everyone, then stops; the other three carry on to round R. A
//! restarted validator 3, holding rounds 0..=K, then receives the later blocks
//! in the worst order - newest first - with NO history transfer and NO
//! re-gossip. (The consensus simulation re-sends every node's recent blocks on
//! every tick, which the real transport never does; that redundancy is why the
//! simulation could not see this.)
//!
//! Dropping early blocks strands it. Holding them - through the same
//! `receive_block` validation, released only when their parents are present -
//! brings it to the tip.

use arc_consensus::pending::{Offered, PendingBlocks, receive_or_hold};
use arc_consensus::{ConsensusEngine, DagBlock, STAKE_ARC, Validator, ValidatorSet};
use arc_crypto::{KeyPair, hash_bytes};
use std::collections::VecDeque;
use std::time::Instant;

const K: u64 = 6; // rounds everyone runs together
const R: u64 = 20; // the round the other three reach

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

/// Quorum-only advancement - the rule the soak network runs. (Installing a
/// consensus domain would switch to the recovery domain's all-validator
/// participation rule, where a departed member must be excused by certificate
/// before a round can close; that is covered by view_change_simulation.)
fn engine(set: &ValidatorSet, key: &KeyPair) -> ConsensusEngine {
    ConsensusEngine::new_with_keypair(set.clone(), key.address(), key.clone())
}

/// Run `rounds` with the given participants, delivering everything to them.
/// Returns every block produced, in production order.
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

/// Blocks of rounds 0..=K, and of rounds K+1..=R produced without validator 3.
fn history() -> (ValidatorSet, Vec<KeyPair>, Vec<DagBlock>, Vec<DagBlock>) {
    let (set, keys) = committee();
    let e: Vec<ConsensusEngine> = keys.iter().map(|k| engine(&set, k)).collect();
    let mut ts = 1_700_000_000_000u64;
    let early = run_rounds(&[&e[0], &e[1], &e[2], &e[3]], K + 1, &mut ts);
    let late = run_rounds(&[&e[0], &e[1], &e[2]], R - K, &mut ts);
    assert!(e[0].current_round() >= R, "the three kept going: {}", e[0].current_round());
    (set, keys, early, late)
}

/// A restarted validator 3 that holds rounds 0..=K.
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
    // Holding must not become a place to park malformed blocks: a parent that
    // IS present but from the wrong round makes the block invalid, not early.
    let (set, keys, early, _late) = history();
    let lag = restarted(&set, &keys, &early);
    let mut pending = PendingBlocks::new();

    // Build a block for round K+2 whose parent is a real round-0 block.
    let round0 = early.iter().find(|b| b.round == 0).expect("round 0 exists").hash;
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

// ─── Restart past the retention window ─────────────────────────────────────
//
// A restarted node's DAG is empty. It used to rebuild it from round 0, the
// only round whose blocks validate without parents. Once the chain ran past
// the retention window, peers had pruned round 0 and served their oldest
// retained rounds instead - all below the node's own commit cursor - and it
// refused every batch as adding nothing. A 20-minute native-workload run hit
// exactly that on its first fault: killed at round 4291 with 4096 retained,
// 38 refused batches, never caught up.

/// The retention floor (`PRUNE_DEPTH`): the smallest window an engine keeps.
const RETAINED: u64 = 100;

/// Run rounds as a live node does - proposing, receiving, advancing and
/// committing, which also prunes. Each engine's committed anchors are
/// appended to the matching entry of `committed`.
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

/// Everything a peer still holds from `from` to its tip, as it would serve it.
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

    // All four run together well past the retention window.
    run_committing(&[&e[0], &e[1], &e[2], &e[3]], 3 * RETAINED, &mut ts, &mut committed);
    // Validator 3 is killed. Its durable records keep its round and cursor.
    let crashed_round = e[3].current_round();
    let cursor = e[3].last_committed_round();
    assert!(cursor > RETAINED, "the cursor must be past the window for this to test anything");
    // The other three carry on, for less than their retention window.
    run_committing(&[&e[0], &e[1], &e[2]], RETAINED / 2, &mut ts, &mut committed[..3]);
    assert!(e[0].blocks_in_round(0).is_empty(), "peers pruned round 0");
    assert!(
        !e[0].blocks_in_round(cursor).is_empty(),
        "peers still hold the restarted node's cursor round"
    );

    // The restarted process: an empty DAG, cursors from its own records.
    let restarted = engine(&set, &keys[3]);
    restarted.restore_round_from_local_wal(crashed_round, cursor);

    // What it used to ask for - "from round 0" - is served from the peers'
    // oldest retained round, whose parents nobody has any more.
    let oldest = served_from(&e[0], 0);
    assert!(oldest.iter().all(|b| b.round > 0));
    assert!(
        restarted.import_history(&oldest, u64::MAX).is_err(),
        "history from the oldest retained round cannot validate on an empty DAG"
    );
    // From its own cursor, but without the base round, the first round
    // cannot validate either.
    assert!(restarted.import_history(&served_from(&e[0], cursor), u64::MAX).is_err());
    assert!(restarted.dag_is_empty(), "a refused import leaves nothing behind");

    // Only at its own cursor, only on an empty DAG.
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

    // Rejoin. From here the restarted node must commit exactly what its peers
    // commit, and never anything below its cursor again.
    let mut rejoined = vec![Vec::new(); 4];
    run_committing(&[&e[0], &e[1], &e[2], &restarted], 30, &mut ts, &mut rejoined);
    let mine = &rejoined[3];
    assert!(mine.len() >= 5, "the restarted node committed {} anchors", mine.len());
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
    assert_eq!(restarted.restart_base_round(), None, "the base round closed itself");
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
    run_committing(&[&e[0], &e[1], &e[2], &e[3]], 2 * RETAINED, &mut ts, &mut committed);
    let cursor = e[3].last_committed_round();
    let restarted = engine(&set, &keys[3]);
    restarted.restore_round_from_local_wal(e[3].current_round(), cursor);
    restarted.set_restart_base_round(cursor).unwrap();

    let real = e[0]
        .get_block(&e[0].blocks_in_round(cursor)[0])
        .expect("a block at the base round");
    // The excuse is for missing parents during a history import, never for
    // live gossip - even for an authentic block.
    assert!(restarted.receive_block(&real).is_err());

    // Parentless, zero-parent, duplicated-parent and unsigned variants of a
    // real block: none is imported, whatever else the batch carries.
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
        assert!(restarted.get_block(&v.hash).is_none(), "{:?} was imported", v.parents);
    }
    assert!(restarted.get_block(&real.hash).is_some(), "the authentic base block is admitted");
}

#[test]
fn history_served_from_below_the_base_round_still_reaches_it() {
    // Peers may answer "from the cursor" with a batch that starts a few rounds
    // lower. Those rounds are below the restarting node's cursor - decided
    // already - and their parents are gone; they must be skipped, not allowed
    // to stop the import before it reaches the base.
    let (set, keys) = committee();
    let e: Vec<ConsensusEngine> = keys.iter().map(|k| engine(&set, k)).collect();
    for x in &e {
        x.set_retained_rounds(RETAINED);
    }
    let mut ts = 1_700_000_000_000u64;
    let mut committed = vec![Vec::new(); 4];
    run_committing(&[&e[0], &e[1], &e[2], &e[3]], 2 * RETAINED, &mut ts, &mut committed);
    let crashed_round = e[3].current_round();
    let cursor = e[3].last_committed_round();
    run_committing(&[&e[0], &e[1], &e[2]], 10, &mut ts, &mut committed[..3]);

    let restarted = engine(&set, &keys[3]);
    restarted.restore_round_from_local_wal(crashed_round, cursor);
    restarted.set_restart_base_round(cursor).unwrap();
    let from_below = served_from(&e[0], cursor - 3);
    assert!(from_below.iter().any(|b| b.round < cursor), "the batch starts below the base");
    restarted
        .import_history(&from_below, u64::MAX)
        .expect("rounds below the base are skipped, and the base onwards imports");
    assert!(restarted.blocks_in_round(cursor - 1).is_empty(), "nothing below the base was taken");
    assert!(!restarted.blocks_in_round(cursor).is_empty());
    assert!(restarted.current_round() >= e[0].current_round().saturating_sub(1));
}

#[test]
fn a_base_round_block_nothing_in_the_next_round_supports_is_not_imported() {
    // A byzantine peer's fabricated base block - committee-signed, garbage
    // parents - must not become part of a restarting node's DAG, and live
    // gossip never gets the missing-parents excuse at all.
    let (set, keys) = committee();
    let e: Vec<ConsensusEngine> = keys.iter().map(|k| engine(&set, k)).collect();
    for x in &e {
        x.set_retained_rounds(RETAINED);
    }
    let mut ts = 1_700_000_000_000u64;
    let mut committed = vec![Vec::new(); 4];
    run_committing(&[&e[0], &e[1], &e[2], &e[3]], 2 * RETAINED, &mut ts, &mut committed);
    let crashed_round = e[3].current_round();
    let cursor = e[3].last_committed_round();

    let restarted = engine(&set, &keys[3]);
    restarted.restore_round_from_local_wal(crashed_round, cursor);
    restarted.set_restart_base_round(cursor).unwrap();

    // Validator 1 signs a second block at the base round with invented parents.
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
        parents: vec![hash_bytes(b"invented-1"), hash_bytes(b"invented-2"), hash_bytes(b"invented-3")],
        timestamp: real.timestamp + 1,
        hash: arc_crypto::Hash256::ZERO,
        signature: vec![],
        ..real.clone()
    };
    fabricated.hash = fabricated.compute_hash();
    fabricated.signature = bincode::serialize(&keys[1].sign(&fabricated.hash).unwrap()).unwrap();

    // Live gossip: no excuse, so its missing parents refuse it.
    assert!(restarted.receive_block(&fabricated).is_err());
    // A batch carrying it next to the real history: only the base blocks the
    // next round names are admitted.
    let mut batch = vec![fabricated.clone()];
    batch.extend(served_from(&e[0], cursor));
    restarted
        .import_history(&batch, u64::MAX)
        .expect("the supported history imports");
    assert!(restarted.get_block(&fabricated.hash).is_none(), "the fabricated block was imported");
    assert!(restarted.get_block(&real.hash).is_some());
    // A batch whose base round nothing supports is refused outright.
    let fresh = engine(&set, &keys[3]);
    fresh.restore_round_from_local_wal(crashed_round, cursor);
    fresh.set_restart_base_round(cursor).unwrap();
    assert!(fresh.import_history(&[fabricated], u64::MAX).is_err());
    assert!(fresh.dag_is_empty());
}
