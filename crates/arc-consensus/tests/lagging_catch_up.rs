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
