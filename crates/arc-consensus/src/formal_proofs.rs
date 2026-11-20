use super::*;
use arc_crypto::hash_bytes;
use std::collections::{HashMap, HashSet};

fn test_addr(n: u8) -> Address {
    hash_bytes(&[n])
}

fn make_test_validators(stakes: &[(u64, u16)]) -> (ValidatorSet, Vec<Hash256>) {
    let mut validators = Vec::new();
    let mut addresses = Vec::new();
    for (i, &(stake, shard)) in stakes.iter().enumerate() {
        let addr = test_addr(i as u8);
        addresses.push(addr);
        if let Some(v) = Validator::new(addr, stake, shard) {
            validators.push(v);
        }
    }
    (ValidatorSet::new(validators, 1), addresses)
}

fn make_block(
    author: Address,
    round: u64,
    parents: Vec<Hash256>,
    transactions: Vec<Hash256>,
    timestamp: u64,
) -> DagBlock {
    let mut transactions = transactions;
    transactions.sort_by_key(|tx| tx.0);
    let ordering_commitment = DagBlock::compute_ordering_commitment(&transactions);
    let mut block = DagBlock {
        author,
        round,
        parents,
        transactions,
        timestamp,
        hash: Hash256::ZERO,
        signature: Vec::new(),
        ordering_commitment,
    };
    block.hash = block.compute_hash();
    block
}

fn simulate_rounds(
    engine: &ConsensusEngine,
    validators: &[Hash256],
    rounds: u64,
) -> Vec<Vec<DagBlock>> {
    let mut all_rounds: Vec<Vec<DagBlock>> = Vec::new();

    for round in 0..rounds {
        let mut round_blocks = Vec::new();

        let parents = if round == 0 {
            vec![]
        } else {
            engine.blocks_in_round(round - 1)
        };

        for (i, addr) in validators.iter().enumerate() {
            let vs = engine.validator_set();
            if !vs.can_produce_blocks(addr) {
                continue;
            }

            let tx_hash = hash_bytes(&[round as u8, i as u8]);
            let block = make_block(
                *addr,
                round,
                parents.clone(),
                vec![tx_hash],
                round * 1000 + i as u64,
            );
            if round == 0 {
                engine.receive_block(&block).unwrap();
            } else {
                engine.receive_block(&block).unwrap_or_else(|_| {
                    panic!(
                        "Failed to insert block for validator {} in round {}",
                        i, round
                    );
                });
            }
            round_blocks.push(block);
        }

        engine.advance_round();
        all_rounds.push(round_blocks);
    }

    all_rounds
}

fn commits_equivalent(a: &[DagBlock], b: &[DagBlock]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut a_hashes: Vec<Hash256> = a.iter().map(|b| b.hash).collect();
    let mut b_hashes: Vec<Hash256> = b.iter().map(|b| b.hash).collect();
    a_hashes.sort_by(|x, y| x.as_bytes().cmp(y.as_bytes()));
    b_hashes.sort_by(|x, y| x.as_bytes().cmp(y.as_bytes()));
    a_hashes == b_hashes
}

fn build_honest_dag(
    engine: &ConsensusEngine,
    honest_validators: &[Hash256],
    rounds: u64,
) -> Vec<Vec<DagBlock>> {
    simulate_rounds(engine, honest_validators, rounds)
}

#[cfg(test)]
mod formal_tests {
    use super::*;

    #[test]
    fn test_safety_no_conflicting_commits() {
        let stakes = vec![
            (STAKE_ARC, 0),
            (STAKE_ARC, 0),
            (STAKE_ARC, 0),
            (STAKE_ARC, 0),
        ];
        let (vs, addrs) = make_test_validators(&stakes);
        let honest = &addrs[0..3];
        let byzantine = addrs[3];

        let engines: Vec<ConsensusEngine> = honest
            .iter()
            .map(|addr| ConsensusEngine::new(vs.clone(), *addr))
            .collect();

        let mut r0_blocks = Vec::new();
        for (i, addr) in addrs.iter().enumerate() {
            let block = make_block(
                *addr,
                0,
                vec![],
                vec![hash_bytes(&[0, i as u8])],
                100 + i as u64,
            );
            r0_blocks.push(block);
        }

        let _byz_conflicting = make_block(
            byzantine,
            0,
            vec![],
            vec![hash_bytes(b"byzantine_conflict")],
            199,
        );

        for engine in &engines {
            for block in &r0_blocks {
                let _ = engine.receive_block(block);
            }
        }

        for engine in &engines {
            assert!(engine.advance_round());
        }

        let r0_hashes: Vec<Hash256> = r0_blocks.iter().map(|b| b.hash).collect();
        let quorum_parents = r0_hashes[0..3].to_vec();

        let mut r1_blocks = Vec::new();
        for (i, addr) in honest.iter().enumerate() {
            let block = make_block(
                *addr,
                1,
                quorum_parents.clone(),
                vec![hash_bytes(&[1, i as u8])],
                200 + i as u64,
            );
            r1_blocks.push(block);
        }

        for engine in &engines {
            for block in &r1_blocks {
                let _ = engine.receive_block(block);
            }
            assert!(engine.advance_round());
        }

        let r1_hashes: Vec<Hash256> = r1_blocks.iter().map(|b| b.hash).collect();
        let mut r2_blocks = Vec::new();
        for (i, addr) in honest.iter().enumerate() {
            let block = make_block(
                *addr,
                2,
                r1_hashes.clone(),
                vec![hash_bytes(&[2, i as u8])],
                300 + i as u64,
            );
            r2_blocks.push(block);
        }

        for engine in &engines {
            for block in &r2_blocks {
                let _ = engine.receive_block(block);
            }
        }

        let mut committed_per_engine: Vec<Vec<DagBlock>> = Vec::new();
        for engine in &engines {
            let committed = engine.try_commit();
            committed_per_engine.push(committed);
        }

        for i in 0..committed_per_engine.len() {
            for j in (i + 1)..committed_per_engine.len() {
                let ci = &committed_per_engine[i];
                let cj = &committed_per_engine[j];

                let rounds_i: HashMap<u64, Vec<Hash256>> = {
                    let mut m = HashMap::new();
                    for b in ci {
                        m.entry(b.round).or_insert_with(Vec::new).push(b.hash);
                    }
                    m
                };
                let rounds_j: HashMap<u64, Vec<Hash256>> = {
                    let mut m = HashMap::new();
                    for b in cj {
                        m.entry(b.round).or_insert_with(Vec::new).push(b.hash);
                    }
                    m
                };

                for (round, hashes_i) in &rounds_i {
                    if let Some(hashes_j) = rounds_j.get(round) {
                        let set_i: HashSet<_> = hashes_i.iter().collect();
                        let set_j: HashSet<_> = hashes_j.iter().collect();
                        assert_eq!(
                            set_i, set_j,
                            "SAFETY VIOLATION: engines {} and {} committed different blocks in round {}",
                            i, j, round
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn test_safety_under_one_third_byzantine() {
        let stakes = vec![
            (STAKE_ARC, 0),
            (STAKE_ARC, 1),
            (STAKE_ARC, 0),
            (STAKE_ARC, 1),
        ];
        let (vs, addrs) = make_test_validators(&stakes);
        let honest = &addrs[0..3];
        let byzantine = addrs[3];

        let engine_0 = ConsensusEngine::new(vs.clone(), honest[0]);
        let engine_1 = ConsensusEngine::new(vs.clone(), honest[1]);
        let engine_2 = ConsensusEngine::new(vs.clone(), honest[2]);

        let h0 = make_block(honest[0], 0, vec![], vec![hash_bytes(b"h0")], 100);
        let h1 = make_block(honest[1], 0, vec![], vec![hash_bytes(b"h1")], 101);
        let h2 = make_block(honest[2], 0, vec![], vec![hash_bytes(b"h2")], 102);

        let byz_block_a = make_block(byzantine, 0, vec![], vec![hash_bytes(b"byz_a")], 103);
        let byz_block_b = make_block(byzantine, 0, vec![], vec![hash_bytes(b"byz_b")], 104);

        let honest_blocks = [&h0, &h1, &h2];
        let all_engines = [&engine_0, &engine_1, &engine_2];

        for engine in &all_engines {
            for block in &honest_blocks {
                engine.receive_block(block).unwrap();
            }
        }
        engine_0.receive_block(&byz_block_a).unwrap();
        engine_1.receive_block(&byz_block_a).unwrap();
        engine_2.receive_block(&byz_block_b).unwrap();

        for engine in &all_engines {
            assert!(engine.advance_round());
        }

        let honest_parents = vec![h0.hash, h1.hash, h2.hash];
        let r1_0 = make_block(honest[0], 1, honest_parents.clone(), vec![], 200);
        let r1_1 = make_block(honest[1], 1, honest_parents.clone(), vec![], 201);
        let r1_2 = make_block(honest[2], 1, honest_parents.clone(), vec![], 202);

        let r1_blocks = [&r1_0, &r1_1, &r1_2];
        for engine in &all_engines {
            for block in &r1_blocks {
                engine.receive_block(block).unwrap();
            }
            assert!(engine.advance_round());
        }

        let r1_parents = vec![r1_0.hash, r1_1.hash, r1_2.hash];
        let r2_0 = make_block(honest[0], 2, r1_parents.clone(), vec![], 300);
        let r2_1 = make_block(honest[1], 2, r1_parents.clone(), vec![], 301);
        let r2_2 = make_block(honest[2], 2, r1_parents.clone(), vec![], 302);

        let r2_blocks = [&r2_0, &r2_1, &r2_2];
        for engine in &all_engines {
            for block in &r2_blocks {
                engine.receive_block(block).unwrap();
            }
        }

        let c0 = engine_0.try_commit();
        let c1 = engine_1.try_commit();
        let c2 = engine_2.try_commit();

        assert!(
            commits_equivalent(&c0, &c1),
            "SAFETY VIOLATION: engine 0 and 1 disagree on commits"
        );
        assert!(
            commits_equivalent(&c0, &c2),
            "SAFETY VIOLATION: engine 0 and 2 disagree on commits"
        );
    }

    #[test]
    fn test_safety_total_ordering() {
        let stakes = vec![
            (STAKE_ARC, 0),
            (STAKE_ARC, 1),
            (STAKE_ARC, 0),
            (STAKE_ARC, 1),
        ];
        let (vs, addrs) = make_test_validators(&stakes);

        let engine_a = ConsensusEngine::new(vs.clone(), addrs[0]);
        let engine_b = ConsensusEngine::new(vs.clone(), addrs[1]);

        let mut r0 = Vec::new();
        for (i, addr) in addrs.iter().enumerate() {
            let block = make_block(
                *addr,
                0,
                vec![],
                vec![hash_bytes(&[0, i as u8])],
                100 + i as u64,
            );
            r0.push(block);
        }

        for engine in [&engine_a, &engine_b] {
            for block in &r0 {
                engine.receive_block(block).unwrap();
            }
            engine.advance_round();
        }

        let r0_parents: Vec<Hash256> = r0[0..3].iter().map(|b| b.hash).collect();
        let mut r1 = Vec::new();
        for (i, addr) in addrs.iter().enumerate() {
            let block = make_block(*addr, 1, r0_parents.clone(), vec![], 200 + i as u64);
            r1.push(block);
        }

        for engine in [&engine_a, &engine_b] {
            for block in &r1 {
                engine.receive_block(block).unwrap();
            }
            engine.advance_round();
        }

        let r1_parents: Vec<Hash256> = r1[0..3].iter().map(|b| b.hash).collect();
        let mut r2 = Vec::new();
        for (i, addr) in addrs.iter().enumerate() {
            let block = make_block(*addr, 2, r1_parents.clone(), vec![], 300 + i as u64);
            r2.push(block);
        }

        for engine in [&engine_a, &engine_b] {
            for block in &r2 {
                engine.receive_block(block).unwrap();
            }
        }

        let ca = engine_a.try_commit();
        let cb = engine_b.try_commit();

        assert!(
            commits_equivalent(&ca, &cb),
            "TOTAL ORDER VIOLATION: two engines with identical DAG state produced different commit orders.\n\
             Engine A committed {} blocks: {:?}\n\
             Engine B committed {} blocks: {:?}",
            ca.len(),
            ca.iter().map(|b| b.hash).collect::<Vec<_>>(),
            cb.len(),
            cb.iter().map(|b| b.hash).collect::<Vec<_>>(),
        );

        for window in ca.windows(2) {
            assert!(
                window[0].round <= window[1].round,
                "Committed blocks must be in non-decreasing round order"
            );
        }
    }

    #[test]
    fn test_liveness_progress_with_honest_majority() {
        let stakes = vec![
            (STAKE_ARC, 0),
            (STAKE_ARC, 1),
            (STAKE_ARC, 0),
            (STAKE_ARC, 1),
        ];
        let (vs, addrs) = make_test_validators(&stakes);
        let honest = addrs[0..3].to_vec();

        let engine = ConsensusEngine::new(vs, honest[0]);

        let _rounds = build_honest_dag(&engine, &honest, 5);

        let committed = engine.try_commit();
        assert!(
            !committed.is_empty(),
            "LIVENESS VIOLATION: system stalled despite 3/4 honest validators (75% > 2/3)"
        );

        for block in &committed {
            assert!(
                honest.contains(&block.author),
                "Committed block from unexpected author"
            );
        }
    }

    #[test]
    fn test_liveness_stall_with_too_many_faults() {
        let stakes = vec![
            (STAKE_ARC, 0),
            (STAKE_ARC, 1),
            (STAKE_ARC, 0),
            (STAKE_ARC, 1),
        ];
        let (vs, addrs) = make_test_validators(&stakes);
        let honest = &addrs[0..2];

        let engine = ConsensusEngine::new(vs, honest[0]);

        let b0 = make_block(honest[0], 0, vec![], vec![hash_bytes(b"h0")], 100);
        let b1 = make_block(honest[1], 0, vec![], vec![hash_bytes(b"h1")], 101);
        engine.receive_block(&b0).unwrap();
        engine.receive_block(&b1).unwrap();

        let advanced = engine.advance_round();
        assert!(
            !advanced,
            "Should NOT advance round with only 50% stake (below 2/3 threshold)"
        );

        let committed = engine.try_commit();
        assert!(
            committed.is_empty(),
            "Should NOT commit with only 50% stake active"
        );
    }

    #[test]
    fn test_liveness_recovery_after_fault() {
        let stakes = vec![
            (STAKE_ARC, 0),
            (STAKE_ARC, 1),
            (STAKE_ARC, 0),
            (STAKE_ARC, 1),
        ];
        let (vs, addrs) = make_test_validators(&stakes);

        let active = addrs[0..3].to_vec();
        let engine = ConsensusEngine::new(vs.clone(), addrs[0]);

        build_honest_dag(&engine, &active, 3);

        let committed_before = engine.try_commit();
        assert!(
            !committed_before.is_empty(),
            "Should commit with 3/4 validators active"
        );

        let committed_count_before = committed_before.len();

        let all_validators = addrs.clone();

        let current = engine.current_round();
        assert!(
            current >= 3,
            "Should be at round 3 or later, got {}",
            current
        );

        for round_offset in 0..15 {
            let round = current + round_offset;
            let parents = engine.blocks_in_round(round.saturating_sub(1));

            if parents.is_empty() && round > 0 {
                continue;
            }

            for (i, addr) in all_validators.iter().enumerate() {
                let block = make_block(
                    *addr,
                    round,
                    if round == 0 { vec![] } else { parents.clone() },
                    vec![hash_bytes(&[round as u8, i as u8, 0xFF])],
                    round * 1000 + i as u64 + 5000,
                );
                let _ = engine.receive_block(&block);
            }
            engine.advance_round();
        }

        let _committed_after = engine.try_commit();
        let total_committed = engine.committed_blocks().len();

        assert!(
            total_committed > committed_count_before,
            "LIVENESS VIOLATION: system did not resume committing after validator recovery. \
             Before: {} committed, After: {} total committed",
            committed_count_before,
            total_committed
        );
    }

    #[test]
    fn test_bft_equivocation_detected() {
        let stakes = vec![
            (STAKE_ARC, 0),
            (STAKE_ARC, 1),
            (STAKE_ARC, 0),
            (STAKE_ARC, 1),
        ];
        let (vs, addrs) = make_test_validators(&stakes);
        let byzantine = addrs[3];

        let engine = ConsensusEngine::new(vs.clone(), addrs[0]);

        let byz_block_1 = make_block(
            byzantine,
            0,
            vec![],
            vec![hash_bytes(b"equivocation_1")],
            100,
        );
        let byz_block_2 = make_block(
            byzantine,
            0,
            vec![],
            vec![hash_bytes(b"equivocation_2")],
            101,
        );

        engine.receive_block(&byz_block_1).unwrap();

        let result = engine.receive_block(&byz_block_2);

        let round_0_blocks = engine.blocks_in_round(0);
        let mut authors_in_round: HashMap<Address, Vec<Hash256>> = HashMap::new();
        for hash in &round_0_blocks {
            if let Some(block) = engine.get_block(hash) {
                authors_in_round
                    .entry(block.author)
                    .or_default()
                    .push(block.hash);
            }
        }

        let byz_blocks = authors_in_round.get(&byzantine).unwrap();
        assert!(
            byz_blocks.len() >= 2 || result.is_err(),
            "Equivocation should be detectable: Byzantine validator produced {} blocks in round 0",
            byz_blocks.len()
        );

        let h0 = make_block(addrs[0], 0, vec![], vec![], 200);
        let h1 = make_block(addrs[1], 0, vec![], vec![], 201);
        let h2 = make_block(addrs[2], 0, vec![], vec![], 202);
        engine.receive_block(&h0).unwrap();
        engine.receive_block(&h1).unwrap();
        engine.receive_block(&h2).unwrap();

        let blocks_in_r0 = engine.blocks_in_round(0);
        let mut unique_authors = HashSet::new();
        let mut effective_stake = 0u64;
        let validator_set = engine.validator_set();
        for hash in &blocks_in_r0 {
            if let Some(block) = engine.get_block(hash)
                && unique_authors.insert(block.author)
                && let Some(v) = validator_set.get_validator(&block.author)
            {
                effective_stake += v.stake;
            }
        }

        let slashed_stake = STAKE_ARC - (STAKE_ARC * SLASH_RATE_ARC / 100);
        assert_eq!(
            effective_stake,
            3 * STAKE_ARC + slashed_stake,
            "Equivocating validator should only count once and have reduced stake from slashing"
        );
    }

    #[test]
    fn test_bft_withholding_attack() {
        let stakes = vec![
            (STAKE_ARC, 0),
            (STAKE_ARC, 1),
            (STAKE_ARC, 0),
            (STAKE_ARC, 1),
        ];
        let (vs, addrs) = make_test_validators(&stakes);
        let honest = &addrs[0..3];

        let engine_without_byz = ConsensusEngine::new(vs.clone(), honest[0]);
        let engine_with_byz = ConsensusEngine::new(vs.clone(), honest[1]);

        let h0 = make_block(honest[0], 0, vec![], vec![hash_bytes(b"h0")], 100);
        let h1 = make_block(honest[1], 0, vec![], vec![hash_bytes(b"h1")], 101);
        let h2 = make_block(honest[2], 0, vec![], vec![hash_bytes(b"h2")], 102);
        let byz = make_block(addrs[3], 0, vec![], vec![hash_bytes(b"byz")], 103);

        for block in [&h0, &h1, &h2] {
            engine_without_byz.receive_block(block).unwrap();
        }
        for block in [&h0, &h1, &h2, &byz] {
            engine_with_byz.receive_block(block).unwrap();
        }

        assert!(engine_without_byz.advance_round());
        assert!(engine_with_byz.advance_round());

        let honest_r0_parents = vec![h0.hash, h1.hash, h2.hash];

        let r1_blocks: Vec<DagBlock> = honest
            .iter()
            .enumerate()
            .map(|(i, addr)| {
                make_block(*addr, 1, honest_r0_parents.clone(), vec![], 200 + i as u64)
            })
            .collect();

        for engine in [&engine_without_byz, &engine_with_byz] {
            for block in &r1_blocks {
                engine.receive_block(block).unwrap();
            }
            engine.advance_round();
        }

        let r1_parents: Vec<Hash256> = r1_blocks.iter().map(|b| b.hash).collect();
        let r2_blocks: Vec<DagBlock> = honest
            .iter()
            .enumerate()
            .map(|(i, addr)| make_block(*addr, 2, r1_parents.clone(), vec![], 300 + i as u64))
            .collect();

        for engine in [&engine_without_byz, &engine_with_byz] {
            for block in &r2_blocks {
                engine.receive_block(block).unwrap();
            }
        }

        let c_without = engine_without_byz.try_commit();
        let c_with = engine_with_byz.try_commit();

        assert!(
            !c_without.is_empty(),
            "LIVENESS VIOLATION: engine without Byzantine blocks should still commit"
        );
        assert!(
            !c_with.is_empty(),
            "LIVENESS VIOLATION: engine with Byzantine blocks should commit"
        );

        let honest_commits_without: Vec<Hash256> = c_without
            .iter()
            .filter(|b| honest.contains(&b.author))
            .map(|b| b.hash)
            .collect();
        let honest_commits_with: Vec<Hash256> = c_with
            .iter()
            .filter(|b| honest.contains(&b.author))
            .map(|b| b.hash)
            .collect();

        assert_eq!(
            honest_commits_without, honest_commits_with,
            "Withholding attack should not cause honest validators to commit differently"
        );
    }

    #[test]
    fn test_bft_one_third_threshold() {
        for n in [4, 6, 7, 10, 13] {
            let f = (n - 1) / 3;

            {
                let honest_count = n - f;
                let stakes: Vec<(u64, u16)> = (0..n).map(|i| (STAKE_ARC, i as u16 % 4)).collect();
                let (vs, addrs) = make_test_validators(&stakes);
                let honest: Vec<Hash256> = addrs[0..honest_count].to_vec();

                let engine = ConsensusEngine::new(vs.clone(), honest[0]);

                let honest_stake: u64 = honest_count as u64 * STAKE_ARC;
                let required_quorum = vs.quorum;

                if honest_stake >= required_quorum {
                    build_honest_dag(&engine, &honest, 3);
                    let committed = engine.try_commit();
                    assert!(
                        !committed.is_empty(),
                        "N={}, f={}: System should make progress with {} honest validators (stake {} >= quorum {})",
                        n,
                        f,
                        honest_count,
                        honest_stake,
                        required_quorum
                    );
                }
            }

            {
                let byz_count = f + 1;
                let honest_count = n - byz_count;
                let stakes: Vec<(u64, u16)> = (0..n).map(|i| (STAKE_ARC, i as u16 % 4)).collect();
                let (vs, addrs) = make_test_validators(&stakes);
                let honest: Vec<Hash256> = addrs[0..honest_count].to_vec();

                let honest_stake = honest_count as u64 * STAKE_ARC;
                let required_quorum = vs.quorum;

                if honest_stake < required_quorum {
                    let engine = ConsensusEngine::new(vs.clone(), honest[0]);

                    for (i, addr) in honest.iter().enumerate() {
                        let block = make_block(*addr, 0, vec![], vec![], 100 + i as u64);
                        engine.receive_block(&block).unwrap();
                    }

                    let advanced = engine.advance_round();
                    assert!(
                        !advanced,
                        "N={}, f+1={}: System should NOT advance with only {} honest validators (stake {} < quorum {})",
                        n, byz_count, honest_count, honest_stake, required_quorum
                    );
                }
            }
        }
    }

    #[test]
    fn test_deterministic_commit_order() {
        let stakes = vec![
            (STAKE_ARC, 0),
            (STAKE_ARC, 1),
            (STAKE_ARC, 0),
            (STAKE_ARC, 1),
        ];
        let (vs, addrs) = make_test_validators(&stakes);

        let engine_forward = ConsensusEngine::new(vs.clone(), addrs[0]);
        let engine_reverse = ConsensusEngine::new(vs.clone(), addrs[0]);

        let mut r0 = Vec::new();
        for (i, addr) in addrs.iter().enumerate() {
            r0.push(make_block(
                *addr,
                0,
                vec![],
                vec![hash_bytes(&[0, i as u8])],
                100 + i as u64,
            ));
        }

        for block in &r0 {
            engine_forward.receive_block(block).unwrap();
        }
        for block in r0.iter().rev() {
            engine_reverse.receive_block(block).unwrap();
        }

        engine_forward.advance_round();
        engine_reverse.advance_round();

        let r0_parents: Vec<Hash256> = r0[0..3].iter().map(|b| b.hash).collect();
        let mut r1 = Vec::new();
        for (i, addr) in addrs.iter().enumerate() {
            r1.push(make_block(
                *addr,
                1,
                r0_parents.clone(),
                vec![],
                200 + i as u64,
            ));
        }

        for block in &r1 {
            engine_forward.receive_block(block).unwrap();
        }
        for block in r1.iter().rev() {
            engine_reverse.receive_block(block).unwrap();
        }

        engine_forward.advance_round();
        engine_reverse.advance_round();

        let r1_parents: Vec<Hash256> = r1[0..3].iter().map(|b| b.hash).collect();
        let mut r2 = Vec::new();
        for (i, addr) in addrs.iter().enumerate() {
            r2.push(make_block(
                *addr,
                2,
                r1_parents.clone(),
                vec![],
                300 + i as u64,
            ));
        }

        for block in &r2 {
            engine_forward.receive_block(block).unwrap();
        }
        for block in r2.iter().rev() {
            engine_reverse.receive_block(block).unwrap();
        }

        let cf = engine_forward.try_commit();
        let cr = engine_reverse.try_commit();

        assert!(
            commits_equivalent(&cf, &cr),
            "DETERMINISM VIOLATION: different insertion order produced different commit order.\n\
             Forward: {:?}\nReverse: {:?}",
            cf.iter().map(|b| b.hash).collect::<Vec<_>>(),
            cr.iter().map(|b| b.hash).collect::<Vec<_>>(),
        );
    }

    #[test]
    fn test_commit_rule_two_round_latency() {
        let stakes = vec![
            (STAKE_ARC, 0),
            (STAKE_ARC, 1),
            (STAKE_ARC, 0),
            (STAKE_ARC, 1),
        ];
        let (vs, addrs) = make_test_validators(&stakes);
        let engine = ConsensusEngine::new(vs, addrs[0]);

        let mut r0 = Vec::new();
        for (i, addr) in addrs.iter().enumerate() {
            let block = make_block(
                *addr,
                0,
                vec![],
                vec![hash_bytes(&[0, i as u8])],
                100 + i as u64,
            );
            engine.receive_block(&block).unwrap();
            r0.push(block);
        }
        engine.advance_round();

        assert!(
            engine.try_commit().is_empty(),
            "No block should be committed with only round 0 data"
        );

        let r0_parents: Vec<Hash256> = r0[0..3].iter().map(|b| b.hash).collect();
        let mut r1 = Vec::new();
        for (i, addr) in addrs.iter().enumerate() {
            let block = make_block(*addr, 1, r0_parents.clone(), vec![], 200 + i as u64);
            engine.receive_block(&block).unwrap();
            r1.push(block);
        }
        engine.advance_round();

        assert!(
            engine.try_commit().is_empty(),
            "No block should be committed with only rounds 0-1 data"
        );

        let r1_parents: Vec<Hash256> = r1[0..3].iter().map(|b| b.hash).collect();
        for (i, addr) in addrs.iter().enumerate() {
            let block = make_block(*addr, 2, r1_parents.clone(), vec![], 300 + i as u64);
            engine.receive_block(&block).unwrap();
        }

        let committed = engine.try_commit();
        assert!(
            !committed.is_empty(),
            "Blocks from round 0 should be committed after round 2 data is available"
        );

        for block in &committed {
            assert_eq!(
                block.round, 0,
                "Only round 0 blocks should be committed at this point, got round {}",
                block.round
            );
        }
    }

    #[test]
    fn test_commit_requires_quorum() {
        let stakes = vec![
            (STAKE_ARC, 0),
            (STAKE_ARC, 1),
            (STAKE_ARC, 0),
            (STAKE_ARC, 1),
        ];
        let (vs, addrs) = make_test_validators(&stakes);
        let engine = ConsensusEngine::new(vs.clone(), addrs[0]);

        let mut r0 = Vec::new();
        for (i, addr) in addrs.iter().enumerate() {
            let block = make_block(*addr, 0, vec![], vec![], 100 + i as u64);
            engine.receive_block(&block).unwrap();
            r0.push(block);
        }
        engine.advance_round();

        let r0_parents: Vec<Hash256> = r0[0..3].iter().map(|b| b.hash).collect();
        let block_c = make_block(addrs[1], 1, r0_parents.clone(), vec![], 200);
        engine.receive_block(&block_c).unwrap();

        let c2 = make_block(addrs[2], 1, r0_parents.clone(), vec![], 201);
        let c3 = make_block(addrs[3], 1, r0_parents.clone(), vec![], 202);
        engine.receive_block(&c2).unwrap();
        engine.receive_block(&c3).unwrap();
        engine.advance_round();

        let d0 = make_block(
            addrs[0],
            2,
            vec![block_c.hash, c2.hash, c3.hash],
            vec![],
            300,
        );
        engine.receive_block(&d0).unwrap();

        let committed = engine.try_commit();
        let r0_0_committed = committed.iter().any(|b| b.hash == r0[0].hash);
        assert!(
            !r0_0_committed,
            "Block should NOT be committed with only 1 validator (5M) referencing certifier in R+2 (quorum requires ~13.3M)"
        );

        let d2 = make_block(
            addrs[2],
            2,
            vec![block_c.hash, c2.hash, c3.hash],
            vec![],
            301,
        );
        let d3 = make_block(
            addrs[3],
            2,
            vec![block_c.hash, c2.hash, c3.hash],
            vec![],
            302,
        );
        engine.receive_block(&d2).unwrap();
        engine.receive_block(&d3).unwrap();

        let committed = engine.try_commit();
        let r0_0_committed = committed.iter().any(|b| b.hash == r0[0].hash);
        assert!(
            r0_0_committed,
            "Block SHOULD be committed now with 3 validators (15M >= ~13.3M quorum) in R+2"
        );
    }

    #[test]
    fn test_stake_weighted_quorum() {
        let stakes = vec![(STAKE_CORE, 0), (STAKE_ARC, 1), (STAKE_ARC, 0)];
        let (vs, addrs) = make_test_validators(&stakes);

        assert_eq!(vs.total_stake, STAKE_CORE + 2 * STAKE_ARC);
        assert_eq!(vs.quorum, strict_supermajority_threshold(60_000_000));

        assert!(
            vs.has_quorum(&[addrs[0]]),
            "Core validator (50M) alone should reach quorum ({})",
            vs.quorum
        );

        assert!(
            !vs.has_quorum(&[addrs[1], addrs[2]]),
            "Two Arc validators (10M) should NOT reach quorum ({})",
            vs.quorum
        );

        let engine = ConsensusEngine::new(vs.clone(), addrs[0]);
        let b0 = make_block(addrs[0], 0, vec![], vec![hash_bytes(b"core")], 100);
        engine.receive_block(&b0).unwrap();

        assert!(
            engine.advance_round(),
            "Core validator (50M) alone should be able to advance round"
        );

        let engine2 = ConsensusEngine::new(vs.clone(), addrs[1]);
        let a1 = make_block(addrs[1], 0, vec![], vec![hash_bytes(b"arc1")], 200);
        let a2 = make_block(addrs[2], 0, vec![], vec![hash_bytes(b"arc2")], 201);
        engine2.receive_block(&a1).unwrap();
        engine2.receive_block(&a2).unwrap();

        assert!(
            !engine2.advance_round(),
            "Two Arc validators (10M) should NOT be able to advance round"
        );
    }

    #[test]
    fn test_minimum_quorum_stake() {
        for &total_validators in &[4, 6, 7, 10, 13] {
            let stakes: Vec<(u64, u16)> = (0..total_validators)
                .map(|i| (STAKE_ARC, i as u16 % 4))
                .collect();
            let (vs, addrs) = make_test_validators(&stakes);

            let total_stake = total_validators as u64 * STAKE_ARC;
            let expected_quorum = strict_supermajority_threshold(total_stake);
            assert_eq!(
                vs.quorum, expected_quorum,
                "N={}: quorum mismatch",
                total_validators
            );

            let f = vs.fault_tolerance_stake();
            assert_eq!(
                f,
                total_stake - expected_quorum,
                "N={}: fault tolerance mismatch",
                total_validators
            );

            assert!(
                vs.quorum > total_stake * 2 / 3,
                "N={}: quorum must be strictly greater than 2/3 of total stake",
                total_validators
            );
            assert!(
                vs.quorum <= total_stake,
                "N={}: quorum cannot exceed total stake",
                total_validators
            );

            let min_validators_for_quorum = expected_quorum.div_ceil(STAKE_ARC);
            let max_validators_below_quorum = min_validators_for_quorum - 1;

            let quorum_addrs: Vec<Hash256> = addrs[..min_validators_for_quorum as usize].to_vec();
            assert!(
                vs.has_quorum(&quorum_addrs),
                "N={}: {} validators should meet quorum",
                total_validators,
                min_validators_for_quorum
            );

            if max_validators_below_quorum > 0 {
                let below_quorum_addrs: Vec<Hash256> =
                    addrs[..max_validators_below_quorum as usize].to_vec();
                assert!(
                    !vs.has_quorum(&below_quorum_addrs),
                    "N={}: {} validators should NOT meet quorum",
                    total_validators,
                    max_validators_below_quorum
                );
            }

            let engine = ConsensusEngine::new(vs.clone(), addrs[0]);

            let mut r0 = Vec::new();
            for (i, addr) in addrs.iter().enumerate() {
                let block = make_block(*addr, 0, vec![], vec![], 100 + i as u64);
                engine.receive_block(&block).unwrap();
                r0.push(block);
            }
            engine.advance_round();

            let r0_parents: Vec<Hash256> = r0[..min_validators_for_quorum as usize]
                .iter()
                .map(|b| b.hash)
                .collect();
            let mut r1 = Vec::new();
            for (i, addr) in addrs.iter().enumerate() {
                let block = make_block(*addr, 1, r0_parents.clone(), vec![], 200 + i as u64);
                engine.receive_block(&block).unwrap();
                r1.push(block);
            }
            engine.advance_round();

            let r1_parents: Vec<Hash256> = r1[..min_validators_for_quorum as usize]
                .iter()
                .map(|b| b.hash)
                .collect();

            for (i, addr) in addrs
                .iter()
                .enumerate()
                .take(max_validators_below_quorum as usize)
            {
                let block = make_block(*addr, 2, r1_parents.clone(), vec![], 300 + i as u64);
                engine.receive_block(&block).unwrap();
            }

            let _committed_below = engine.try_commit();

            let idx = min_validators_for_quorum as usize - 1;
            if idx < addrs.len() && idx >= max_validators_below_quorum as usize {
                let block = make_block(addrs[idx], 2, r1_parents.clone(), vec![], 300 + idx as u64);
                let _ = engine.receive_block(&block);
            }

            let _committed_at_quorum = engine.try_commit();
            let total_committed = engine.committed_blocks().len();
            assert!(
                total_committed > 0,
                "N={}: Should have committed blocks once quorum is reached in R+2",
                total_validators
            );
        }
    }

    #[test]
    fn test_try_commit_idempotent() {
        let stakes = vec![
            (STAKE_ARC, 0),
            (STAKE_ARC, 1),
            (STAKE_ARC, 0),
            (STAKE_ARC, 1),
        ];
        let (vs, addrs) = make_test_validators(&stakes);
        let engine = ConsensusEngine::new(vs, addrs[0]);

        build_honest_dag(&engine, &addrs, 4);

        let first_commit = engine.try_commit();
        let first_count = first_commit.len();

        let second_commit = engine.try_commit();
        assert!(
            second_commit.is_empty(),
            "Second try_commit should return empty (all blocks already committed). Got {} blocks.",
            second_commit.len()
        );

        let total = engine.committed_blocks().len();
        assert_eq!(
            total, first_count,
            "Total committed should not change on repeated try_commit"
        );
    }

    #[test]
    fn test_safety_empty_round_zero() {
        let stakes = vec![(STAKE_ARC, 0), (STAKE_ARC, 1), (STAKE_ARC, 0)];
        let (vs, addrs) = make_test_validators(&stakes);
        let engine = ConsensusEngine::new(vs, addrs[0]);

        let committed = engine.try_commit();
        assert!(committed.is_empty(), "No blocks -> no commits");

        let advanced = engine.advance_round();
        assert!(!advanced, "No blocks -> cannot advance");
    }

    #[test]
    fn test_concurrent_proposals_deterministic() {
        let n = 7;
        let stakes: Vec<(u64, u16)> = (0..n).map(|i| (STAKE_ARC, i as u16 % 4)).collect();
        let (vs, addrs) = make_test_validators(&stakes);

        let engines: Vec<ConsensusEngine> = (0..3)
            .map(|i| ConsensusEngine::new(vs.clone(), addrs[i]))
            .collect();

        let mut all_blocks: Vec<Vec<DagBlock>> = Vec::new();

        for round in 0..4u64 {
            let parents = if round == 0 {
                vec![]
            } else {
                let prev = &all_blocks[(round - 1) as usize];
                let min_quorum_count = strict_supermajority_threshold(n as u64) as usize;
                let parent_count = min_quorum_count.min(prev.len());
                prev[..parent_count].iter().map(|b| b.hash).collect()
            };

            let mut round_blocks = Vec::new();
            for (i, addr) in addrs.iter().enumerate() {
                let block = make_block(
                    *addr,
                    round,
                    parents.clone(),
                    vec![hash_bytes(&[round as u8, i as u8])],
                    round * 1000 + i as u64,
                );
                round_blocks.push(block);
            }

            for engine in &engines {
                for block in &round_blocks {
                    let _ = engine.receive_block(block);
                }
                engine.advance_round();
            }

            all_blocks.push(round_blocks);
        }

        let commits: Vec<Vec<DagBlock>> = engines.iter().map(|e| e.try_commit()).collect();

        for i in 1..commits.len() {
            assert!(
                commits_equivalent(&commits[0], &commits[i]),
                "Engine 0 and {} produced different commit orders with identical DAGs",
                i
            );
        }
    }

    #[test]
    fn test_byzantine_cannot_force_conflicting_commits() {
        let stakes = vec![
            (STAKE_ARC, 0),
            (STAKE_ARC, 1),
            (STAKE_ARC, 0),
            (STAKE_ARC, 1),
        ];
        let (vs, addrs) = make_test_validators(&stakes);
        let honest = &addrs[0..3];
        let byzantine = addrs[3];

        let engine_a = ConsensusEngine::new(vs.clone(), honest[0]);
        let engine_b = ConsensusEngine::new(vs.clone(), honest[1]);

        let h_blocks: Vec<DagBlock> = honest
            .iter()
            .enumerate()
            .map(|(i, addr)| {
                make_block(
                    *addr,
                    0,
                    vec![],
                    vec![hash_bytes(&[0, i as u8])],
                    100 + i as u64,
                )
            })
            .collect();

        let byz_for_a = make_block(byzantine, 0, vec![], vec![hash_bytes(b"fork_a")], 150);
        let byz_for_b = make_block(byzantine, 0, vec![], vec![hash_bytes(b"fork_b")], 151);

        for block in &h_blocks {
            engine_a.receive_block(block).unwrap();
        }
        engine_a.receive_block(&byz_for_a).unwrap();

        for block in &h_blocks {
            engine_b.receive_block(block).unwrap();
        }
        engine_b.receive_block(&byz_for_b).unwrap();

        for engine in [&engine_a, &engine_b] {
            engine.advance_round();
        }

        let honest_parents: Vec<Hash256> = h_blocks.iter().map(|b| b.hash).collect();

        let r1: Vec<DagBlock> = honest
            .iter()
            .enumerate()
            .map(|(i, addr)| make_block(*addr, 1, honest_parents.clone(), vec![], 200 + i as u64))
            .collect();

        for engine in [&engine_a, &engine_b] {
            for block in &r1 {
                engine.receive_block(block).unwrap();
            }
            engine.advance_round();
        }

        let r1_parents: Vec<Hash256> = r1.iter().map(|b| b.hash).collect();
        let r2: Vec<DagBlock> = honest
            .iter()
            .enumerate()
            .map(|(i, addr)| make_block(*addr, 2, r1_parents.clone(), vec![], 300 + i as u64))
            .collect();

        for engine in [&engine_a, &engine_b] {
            for block in &r2 {
                engine.receive_block(block).unwrap();
            }
        }

        let ca = engine_a.try_commit();
        let cb = engine_b.try_commit();

        let honest_ca: Vec<Hash256> = ca
            .iter()
            .filter(|b| honest.contains(&b.author))
            .map(|b| b.hash)
            .collect();
        let honest_cb: Vec<Hash256> = cb
            .iter()
            .filter(|b| honest.contains(&b.author))
            .map(|b| b.hash)
            .collect();

        assert_eq!(
            honest_ca, honest_cb,
            "SAFETY VIOLATION: Byzantine equivocation caused honest validators to commit different honest blocks"
        );
    }
}
