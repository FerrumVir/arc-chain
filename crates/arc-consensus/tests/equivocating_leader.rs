//! A leader that signs two blocks for one round must never make two honest
//! nodes commit different blocks for it.
//!
//! The commit rule used to consider only the lowest-hash block it held from a
//! round's leader. With an equivocating leader that is not necessarily the
//! block the honest majority built on: a node holding both twins took the
//! lower one - which its own next-round block had referenced, so the weak
//! two-round rule even certified it - and committed it, while every node that
//! only ever saw the other twin committed that one. Now every candidate is
//! checked: exactly one certified candidate is committed, two are fenced.
//!
//! Fencing costs liveness at that round; it never costs safety. (The
//! recovery domain already fenced; this is the legacy quorum mode the local
//! networks run.) Also here: a restarted node never proposes at or below the
//! round it restored, since it may already have signed a block there.

use arc_consensus::{ConsensusEngine, ConsensusError, DagBlock, STAKE_ARC, Validator, ValidatorSet};
use arc_crypto::{Hash256, KeyPair, hash_bytes};

fn committee() -> (ValidatorSet, Vec<KeyPair>) {
    let keys: Vec<KeyPair> = (0..4)
        .map(|i| KeyPair::from_ed25519_secret_bytes(&hash_bytes(format!("eq.{i}").as_bytes()).0))
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

/// The leader of `round`, by the engine's deterministic rule.
fn leader_of(set: &ValidatorSet, round: u64) -> Hash256 {
    let mut addrs: Vec<Hash256> = set.validators.iter().map(|v| v.address).collect();
    addrs.sort_by_key(|a| a.0);
    addrs[round as usize % addrs.len()]
}

/// One round among `engines`: each proposes, every block goes to every
/// engine, all advance. Returns what was proposed.
fn round(engines: &[&ConsensusEngine], ts: &mut u64) -> Vec<DagBlock> {
    let mut produced = Vec::new();
    for e in engines {
        *ts += 1;
        if let Ok(b) = e.propose_block(vec![], *ts) {
            produced.push(b);
        }
    }
    for b in &produced {
        for e in engines {
            let _ = e.receive_block(b);
        }
    }
    for e in engines {
        e.advance_round();
    }
    produced
}

#[test]
fn a_node_holding_both_twins_never_commits_the_one_the_majority_did_not() {
    let (set, keys) = committee();
    let e: Vec<ConsensusEngine> = keys.iter().map(|k| engine(&set, k)).collect();
    let all: Vec<&ConsensusEngine> = e.iter().collect();
    let mut ts = 1_700_000_000_000u64;

    // Run until a round led by someone other than the observer (engine 3).
    let observer = 3usize;
    for _ in 0..4 {
        round(&all, &mut ts);
    }
    let r = e[0].current_round();
    let leader_addr = leader_of(&set, r);
    let leader = keys.iter().position(|k| k.address() == leader_addr).unwrap();
    assert_ne!(leader, observer, "pick a round the observer does not lead");

    // Round r and the rounds after it run normally: everyone, the observer
    // included, builds on the leader's real block.
    let produced = round(&all, &mut ts);
    let real = produced
        .iter()
        .find(|b| b.author == leader_addr)
        .unwrap()
        .clone();
    for _ in 0..8 {
        round(&all, &mut ts);
    }
    // The leader also signed a twin for round r - same author and round,
    // different contents, a LOWER hash - that reaches only the observer, and
    // only now. Nobody ever built on it.
    let twin = (1..10_000u64)
        .map(|bump| {
            let mut t = DagBlock {
                timestamp: real.timestamp + bump,
                hash: Hash256::ZERO,
                signature: vec![],
                ..real.clone()
            };
            t.hash = t.compute_hash();
            t.signature = bincode::serialize(&keys[leader].sign(&t.hash).unwrap()).unwrap();
            t
        })
        .find(|t| t.hash.0 < real.hash.0)
        .expect("a twin with a lower hash");
    e[observer].receive_block(&twin).expect("the twin is inserted as evidence");

    // Every node decides round r now. The observer holds both twins; the
    // lowest-hash one is the twin, which nothing certifies.
    let decided: Vec<Vec<(u64, Hash256)>> = all
        .iter()
        .map(|x| x.try_commit().into_iter().map(|b| (b.round, b.hash)).collect())
        .collect();
    let at_r = |i: usize| decided[i].iter().find(|(round_, _)| *round_ == r).map(|(_, h)| *h);
    for honest in 0..3 {
        assert_eq!(at_r(honest), Some(real.hash), "honest node {honest} commits the real block");
    }
    // Taking only the lowest hash, the observer skipped round r - a chain one
    // block shorter than everyone else's from here on. It must commit the one
    // certified candidate.
    assert_eq!(
        at_r(observer),
        Some(real.hash),
        "the observer did not commit the certified block at round {r}: a fork"
    );
    assert_eq!(decided[observer], decided[0], "the observer's chain differs from node 0's");
}

#[test]
fn a_restarted_node_never_proposes_at_or_below_the_round_it_restored() {
    let (set, keys) = committee();
    let e: Vec<ConsensusEngine> = keys.iter().map(|k| engine(&set, k)).collect();
    let all: Vec<&ConsensusEngine> = e.iter().collect();
    let mut ts = 1_700_000_000_000u64;
    for _ in 0..6 {
        round(&all, &mut ts);
    }
    // Validator 3 restarts: empty DAG, cursors from its own records. It may
    // have signed a block at the restored round that nobody received.
    let restored = e[3].current_round();
    let restarted = engine(&set, &keys[3]);
    restarted.restore_round_from_local_wal(restored, e[3].last_committed_round());
    assert_eq!(restarted.current_round(), restored);
    assert!(matches!(
        restarted.propose_block(vec![], ts + 1),
        Err(ConsensusError::DuplicateBlock)
    ));
}
