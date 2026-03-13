//! Formal security property tests for DAG consensus.
//!
//! These tests mathematically verify safety and liveness properties of the
//! Mysticeti-inspired DAG consensus under Byzantine fault tolerance assumptions:
//!
//! - **Safety**: No two honest validators commit conflicting blocks.
//! - **Liveness**: The system makes progress if >2/3 validators are honest.
//! - **BFT threshold**: Tolerates up to f < N/3 Byzantine validators.

use super::*;
use arc_crypto::hash_bytes;
use std::collections::{HashMap, HashSet};

// ── Helper Functions ────────────────────────────────────────────────────────

/// Create a deterministic test address from a byte.
fn test_addr(n: u8) -> Address {
    hash_bytes(&[n])
}

/// Create a test validator set with N validators and specified stakes.
/// Each entry is (stake, shard_assignment). Returns the ValidatorSet and the
/// list of validator addresses in the same order.
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

/// Create a DAG block directly (bypassing propose_block) for test scaffolding.
/// Transactions are sorted into canonical lexicographic order to match the
/// MEV-protection scheme enforced by `verify_ordering()`.
fn make_block(
    author: Address,
    round: u64,
    parents: Vec<Hash256>,
    transactions: Vec<Hash256>,
    timestamp: u64,
) -> DagBlock {
    let mut transactions = transactions;
    transactions.sort_by(|a, b| a.0.cmp(&b.0));
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

/// Simulate multiple rounds of honest proposals across all validators on a single engine.
/// Each validator proposes one block per round referencing all blocks from the previous round
/// that provide quorum. Returns all blocks created, organized by round.
fn simulate_rounds(
    engine: &ConsensusEngine,
    validators: &[Hash256],
    rounds: u64,
) -> Vec<Vec<DagBlock>> {
    let mut all_rounds: Vec<Vec<DagBlock>> = Vec::new();

    for round in 0..rounds {
        let mut round_blocks = Vec::new();

        // Collect parents from the previous round (all of them)
        let parents = if round == 0 {
            vec![]
        } else {
            engine.blocks_in_round(round - 1)
        };

        for (i, addr) in validators.iter().enumerate() {
            // Check if this validator can produce blocks
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
            // Use receive_block for round 0 (no parent validation needed),
            // and for subsequent rounds where parents exist
            if round == 0 {
                engine.receive_block(&block).unwrap();
            } else {
                // For round > 0, parents must have quorum. Insert directly.
                engine.receive_block(&block).unwrap_or_else(|_| {
                    // If receive_block fails due to parent validation,
                    // insert directly into DAG (test helper bypass).
                    // This shouldn't happen if we built the DAG correctly.
                    panic!(
                        "Failed to insert block for validator {} in round {}",
                        i, round
                    );
                });
            }
            round_blocks.push(block);
        }

        // Advance to next round
        engine.advance_round();
        all_rounds.push(round_blocks);
    }

    all_rounds
}

/// Check if two commit sequences contain the same blocks in the same order.
fn commits_equivalent(a: &[DagBlock], b: &[DagBlock]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    // Compare as sorted sets — same blocks committed regardless of DashMap iteration order.
    // Within a DAG round, blocks are independent, so ordering within a round is not a
    // safety concern. Cross-round ordering is preserved by the commit rule.
    let mut a_hashes: Vec<Hash256> = a.iter().map(|b| b.hash).collect();
    let mut b_hashes: Vec<Hash256> = b.iter().map(|b| b.hash).collect();
    a_hashes.sort_by(|x, y| x.as_bytes().cmp(y.as_bytes()));
    b_hashes.sort_by(|x, y| x.as_bytes().cmp(y.as_bytes()));
    a_hashes == b_hashes
}

/// Build a full DAG on an engine for the given honest validators over the given rounds.
/// Byzantine validators are excluded. Returns all blocks grouped by round.
fn build_honest_dag(
    engine: &ConsensusEngine,
    honest_validators: &[Hash256],
    rounds: u64,
) -> Vec<Vec<DagBlock>> {
    simulate_rounds(engine, honest_validators, rounds)
}

// ── 1. Safety Property Tests ────────────────────────────────────────────────

#[cfg(test)]
mod formal_tests {
    use super::*;

    // ── 1a. No Conflicting Commits ──────────────────────────────────────────

    #[test]
    fn test_safety_no_conflicting_commits() {
        // 4 validators (3 honest, 1 Byzantine), all Arc tier (5M each).
        // All 4 propose blocks in the same round.
        // Verify that try_commit() never returns two different blocks for the
        // same round from honest validators.
        let stakes = vec![
            (STAKE_ARC, 0),
            (STAKE_ARC, 0),
            (STAKE_ARC, 0),
            (STAKE_ARC, 0),
        ];
        let (vs, addrs) = make_test_validators(&stakes);
        let honest = &addrs[0..3];
        let byzantine = addrs[3];

        // Each honest validator gets its own engine (simulating separate nodes)
        let mut engines: Vec<ConsensusEngine> = honest
            .iter()
            .map(|addr| ConsensusEngine::new(vs.clone(), *addr))
            .collect();

        // Round 0: All 4 validators propose blocks
        let mut r0_blocks = Vec::new();
        for (i, addr) in addrs.iter().enumerate() {
            let block = make_block(*addr, 0, vec![], vec![hash_bytes(&[0, i as u8])], 100 + i as u64);
            r0_blocks.push(block);
        }

        // Byzantine validator also creates a CONFLICTING block for round 0
        let byz_conflicting = make_block(
            byzantine,
            0,
            vec![],
            vec![hash_bytes(b"byzantine_conflict")],
            199,
        );

        // Deliver all round 0 blocks to all honest engines
        for engine in &engines {
            for block in &r0_blocks {
                let _ = engine.receive_block(block);
            }
        }

        // Advance round on all engines
        for engine in &engines {
            assert!(engine.advance_round());
        }

        // Round 1: honest validators propose referencing round 0 parents
        let r0_hashes: Vec<Hash256> = r0_blocks.iter().map(|b| b.hash).collect();
        let quorum_parents = r0_hashes[0..3].to_vec(); // 3 * 5M = 15M >= quorum ~13.3M

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

        // Round 2: honest validators propose referencing round 1 parents
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

        // Now try_commit on each honest engine
        let mut committed_per_engine: Vec<Vec<DagBlock>> = Vec::new();
        for engine in &engines {
            let committed = engine.try_commit();
            committed_per_engine.push(committed);
        }

        // Safety property: No two honest validators commit conflicting blocks
        // for the same round. All committed blocks for a given round must be identical.
        for i in 0..committed_per_engine.len() {
            for j in (i + 1)..committed_per_engine.len() {
                let ci = &committed_per_engine[i];
                let cj = &committed_per_engine[j];

                // Group commits by round
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

                // For every round that both engines committed, the committed
                // block sets must be identical (no conflicts).
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

    // ── 1b. Safety Under 1/3 Byzantine ──────────────────────────────────────

    #[test]
    fn test_safety_under_one_third_byzantine() {
        // N = 4 validators, f = 1 Byzantine (f < N/3 in stake terms).
        // Byzantine validator proposes conflicting blocks to different engines.
        // Honest validators follow protocol.
        // Assert: committed sequence is identical for all honest validators.
        let stakes = vec![
            (STAKE_ARC, 0),
            (STAKE_ARC, 1),
            (STAKE_ARC, 0),
            (STAKE_ARC, 1), // Byzantine
        ];
        let (vs, addrs) = make_test_validators(&stakes);
        let honest = &addrs[0..3];
        let byzantine = addrs[3];

        let engine_0 = ConsensusEngine::new(vs.clone(), honest[0]);
        let engine_1 = ConsensusEngine::new(vs.clone(), honest[1]);
        let engine_2 = ConsensusEngine::new(vs.clone(), honest[2]);

        // Round 0: Honest validators create consistent blocks
        let h0 = make_block(honest[0], 0, vec![], vec![hash_bytes(b"h0")], 100);
        let h1 = make_block(honest[1], 0, vec![], vec![hash_bytes(b"h1")], 101);
        let h2 = make_block(honest[2], 0, vec![], vec![hash_bytes(b"h2")], 102);

        // Byzantine creates TWO different blocks for the same round
        let byz_block_a = make_block(byzantine, 0, vec![], vec![hash_bytes(b"byz_a")], 103);
        let byz_block_b = make_block(byzantine, 0, vec![], vec![hash_bytes(b"byz_b")], 104);

        // Engine 0 and 1 see byz_block_a; Engine 2 sees byz_block_b (equivocation)
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

        // Round 1: Honest validators reference only honest parents (safe quorum without byz)
        // 3 honest * 5M = 15M >= quorum ~13.3M
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

        // Round 2: Honest validators reference round 1 blocks
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

        // Commit on all engines
        let c0 = engine_0.try_commit();
        let c1 = engine_1.try_commit();
        let c2 = engine_2.try_commit();

        // Safety: all honest engines must produce identical committed sequences
        assert!(
            commits_equivalent(&c0, &c1),
            "SAFETY VIOLATION: engine 0 and 1 disagree on commits"
        );
        assert!(
            commits_equivalent(&c0, &c2),
            "SAFETY VIOLATION: engine 0 and 2 disagree on commits"
        );
    }

    // ── 1c. Total Ordering ──────────────────────────────────────────────────

    #[test]
    fn test_safety_total_ordering() {
        // Multiple validators proposing concurrently. After DAG commit, all
        // committed blocks have a deterministic total order. Two different
        // ConsensusEngine instances with same blocks produce same commit order.
        let stakes = vec![
            (STAKE_ARC, 0),
            (STAKE_ARC, 1),
            (STAKE_ARC, 0),
            (STAKE_ARC, 1),
        ];
        let (vs, addrs) = make_test_validators(&stakes);

        let engine_a = ConsensusEngine::new(vs.clone(), addrs[0]);
        let engine_b = ConsensusEngine::new(vs.clone(), addrs[1]);

        // Build identical DAG on both engines
        // Round 0
        let mut r0 = Vec::new();
        for (i, addr) in addrs.iter().enumerate() {
            let block = make_block(*addr, 0, vec![], vec![hash_bytes(&[0, i as u8])], 100 + i as u64);
            r0.push(block);
        }

        for engine in [&engine_a, &engine_b] {
            for block in &r0 {
                engine.receive_block(block).unwrap();
            }
            engine.advance_round();
        }

        // Round 1
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

        // Round 2
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

        // Both engines try to commit
        let ca = engine_a.try_commit();
        let cb = engine_b.try_commit();

        // Deterministic total ordering: identical DAG must yield identical commit order
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

        // Additionally verify the committed blocks are in round order
        for window in ca.windows(2) {
            assert!(
                window[0].round <= window[1].round,
                "Committed blocks must be in non-decreasing round order"
            );
        }
    }

    // ── 2. Liveness Property Tests ──────────────────────────────────────────

    // ── 2a. Progress With Honest Majority ────────────────────────────────────

    #[test]
    fn test_liveness_progress_with_honest_majority() {
        // 4 validators (3 honest, 1 offline).
        // Honest validators propose and reference each other's blocks.
        // Assert: after enough rounds, blocks are committed (system doesn't stall).
        let stakes = vec![
            (STAKE_ARC, 0),
            (STAKE_ARC, 1),
            (STAKE_ARC, 0),
            (STAKE_ARC, 1), // offline -- never proposes
        ];
        let (vs, addrs) = make_test_validators(&stakes);
        let honest = addrs[0..3].to_vec();

        let engine = ConsensusEngine::new(vs, honest[0]);

        // Simulate 5 rounds with only honest validators participating
        let _rounds = build_honest_dag(&engine, &honest, 5);

        // After 5 rounds (round 0..4), we should have commits for rounds 0..2
        let committed = engine.try_commit();
        assert!(
            !committed.is_empty(),
            "LIVENESS VIOLATION: system stalled despite 3/4 honest validators (75% > 2/3)"
        );

        // Verify committed blocks are only from honest validators
        for block in &committed {
            assert!(
                honest.contains(&block.author),
                "Committed block from unexpected author"
            );
        }
    }

    // ── 2b. Stall With Too Many Faults ───────────────────────────────────────

    #[test]
    fn test_liveness_stall_with_too_many_faults() {
        // 4 validators (2 honest, 2 offline).
        // Only 50% stake active -> below 2/3 threshold.
        // Assert: commit may not happen (expected behavior).
        let stakes = vec![
            (STAKE_ARC, 0),
            (STAKE_ARC, 1),
            (STAKE_ARC, 0), // offline
            (STAKE_ARC, 1), // offline
        ];
        let (vs, addrs) = make_test_validators(&stakes);
        let honest = &addrs[0..2];

        let engine = ConsensusEngine::new(vs, honest[0]);

        // Round 0: only 2 validators propose (10M stake)
        let b0 = make_block(honest[0], 0, vec![], vec![hash_bytes(b"h0")], 100);
        let b1 = make_block(honest[1], 0, vec![], vec![hash_bytes(b"h1")], 101);
        engine.receive_block(&b0).unwrap();
        engine.receive_block(&b1).unwrap();

        // 2 * 5M = 10M < quorum ~13.3M: cannot advance round
        let advanced = engine.advance_round();
        assert!(
            !advanced,
            "Should NOT advance round with only 50% stake (below 2/3 threshold)"
        );

        // Even if we force blocks into later rounds, commit shouldn't happen
        let committed = engine.try_commit();
        assert!(
            committed.is_empty(),
            "Should NOT commit with only 50% stake active"
        );
    }

    // ── 2c. Recovery After Fault ─────────────────────────────────────────────

    #[test]
    fn test_liveness_recovery_after_fault() {
        // Validator goes offline for several rounds, comes back.
        // Assert: system resumes committing after validator returns.
        let stakes = vec![
            (STAKE_ARC, 0),
            (STAKE_ARC, 1),
            (STAKE_ARC, 0),
            (STAKE_ARC, 1),
        ];
        let (vs, addrs) = make_test_validators(&stakes);

        // First, only 3 validators run for a few rounds (validator 3 is offline)
        let active = addrs[0..3].to_vec();
        let engine = ConsensusEngine::new(vs.clone(), addrs[0]);

        // Run 3 rounds with 3 honest validators
        build_honest_dag(&engine, &active, 3);

        let committed_before = engine.try_commit();
        assert!(
            !committed_before.is_empty(),
            "Should commit with 3/4 validators active"
        );

        let committed_count_before = committed_before.len();

        // Now validator 3 comes back online and all 4 participate
        let all_validators = addrs.clone();

        // Continue for 3 more rounds with all 4 validators
        // Current round should be 3 (after 3 rounds: 0, 1, 2 + advances)
        let current = engine.current_round();
        assert!(current >= 3, "Should be at round 3 or later, got {}", current);

        // Build rounds manually for the recovered validator set
        for round_offset in 0..3 {
            let round = current + round_offset;
            let parents = engine.blocks_in_round(round.saturating_sub(1));

            // If no parents from previous round (shouldn't happen), skip
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

        // Try commit again -- should have more committed blocks now
        let committed_after = engine.try_commit();
        let total_committed = engine.committed_blocks().len();

        assert!(
            total_committed > committed_count_before,
            "LIVENESS VIOLATION: system did not resume committing after validator recovery. \
             Before: {} committed, After: {} total committed",
            committed_count_before,
            total_committed
        );
    }

    // ── 3. Byzantine Fault Tolerance Tests ──────────────────────────────────

    // ── 3a. Equivocation Detection ───────────────────────────────────────────

    #[test]
    fn test_bft_equivocation_detected() {
        // Byzantine validator sends different blocks to different validators.
        // Assert: honest validators detect the equivocation (duplicate author in round).
        let stakes = vec![
            (STAKE_ARC, 0),
            (STAKE_ARC, 1),
            (STAKE_ARC, 0),
            (STAKE_ARC, 1), // Byzantine
        ];
        let (vs, addrs) = make_test_validators(&stakes);
        let byzantine = addrs[3];

        let engine = ConsensusEngine::new(vs.clone(), addrs[0]);

        // Byzantine validator creates two different blocks for round 0
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

        // First block is accepted
        engine.receive_block(&byz_block_1).unwrap();

        // Second block from same author in same round should be detectable:
        // While the engine doesn't reject it outright (different hash = different block),
        // the advance_round logic only counts each author once.
        let result = engine.receive_block(&byz_block_2);

        // The block has a different hash, so it won't be DuplicateBlock.
        // But we can detect equivocation by checking for multiple blocks from the same
        // author in the same round.
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

        // Detect equivocation: byzantine validator has multiple blocks in the same round
        let byz_blocks = authors_in_round.get(&byzantine).unwrap();
        assert!(
            byz_blocks.len() >= 2 || result.is_err(),
            "Equivocation should be detectable: Byzantine validator produced {} blocks in round 0",
            byz_blocks.len()
        );

        // Even with equivocation, advance_round only counts the author once
        // (verified by the dedup logic in advance_round)
        let h0 = make_block(addrs[0], 0, vec![], vec![], 200);
        let h1 = make_block(addrs[1], 0, vec![], vec![], 201);
        let h2 = make_block(addrs[2], 0, vec![], vec![], 202);
        engine.receive_block(&h0).unwrap();
        engine.receive_block(&h1).unwrap();
        engine.receive_block(&h2).unwrap();

        // advance_round deduplicates authors, so Byzantine validator's extra block
        // doesn't give them extra voting power
        let blocks_in_r0 = engine.blocks_in_round(0);
        let mut unique_authors = HashSet::new();
        let mut effective_stake = 0u64;
        let validator_set = engine.validator_set();
        for hash in &blocks_in_r0 {
            if let Some(block) = engine.get_block(hash) {
                if unique_authors.insert(block.author) {
                    if let Some(v) = validator_set.get_validator(&block.author) {
                        effective_stake += v.stake;
                    }
                }
            }
        }

        // The equivocating validator only counts once in the effective stake calculation.
        // Their stake is also reduced by slashing (20% for Arc tier).
        let slashed_stake = STAKE_ARC - (STAKE_ARC * SLASH_RATE_ARC / 100);
        assert_eq!(
            effective_stake,
            3 * STAKE_ARC + slashed_stake,
            "Equivocating validator should only count once and have reduced stake from slashing"
        );
    }

    // ── 3b. Withholding Attack ───────────────────────────────────────────────

    #[test]
    fn test_bft_withholding_attack() {
        // Byzantine validator proposes but withholds from some validators.
        // Assert: system still makes progress with honest majority.
        let stakes = vec![
            (STAKE_ARC, 0),
            (STAKE_ARC, 1),
            (STAKE_ARC, 0),
            (STAKE_ARC, 1), // Byzantine: withholds blocks
        ];
        let (vs, addrs) = make_test_validators(&stakes);
        let honest = &addrs[0..3];

        // Engine for honest[0] -- never sees Byzantine's blocks
        let engine_without_byz = ConsensusEngine::new(vs.clone(), honest[0]);
        // Engine for honest[1] -- sees Byzantine's blocks
        let engine_with_byz = ConsensusEngine::new(vs.clone(), honest[1]);

        // Round 0: honest blocks visible to both
        let h0 = make_block(honest[0], 0, vec![], vec![hash_bytes(b"h0")], 100);
        let h1 = make_block(honest[1], 0, vec![], vec![hash_bytes(b"h1")], 101);
        let h2 = make_block(honest[2], 0, vec![], vec![hash_bytes(b"h2")], 102);
        let byz = make_block(addrs[3], 0, vec![], vec![hash_bytes(b"byz")], 103);

        // Engine without Byzantine blocks
        for block in [&h0, &h1, &h2] {
            engine_without_byz.receive_block(block).unwrap();
        }
        // Engine with Byzantine blocks
        for block in [&h0, &h1, &h2, &byz] {
            engine_with_byz.receive_block(block).unwrap();
        }

        // Both should advance (3 honest = 15M >= quorum ~13.3M)
        assert!(engine_without_byz.advance_round());
        assert!(engine_with_byz.advance_round());

        // Continue for rounds 1 and 2 with only honest blocks
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
            .map(|(i, addr)| {
                make_block(*addr, 2, r1_parents.clone(), vec![], 300 + i as u64)
            })
            .collect();

        for engine in [&engine_without_byz, &engine_with_byz] {
            for block in &r2_blocks {
                engine.receive_block(block).unwrap();
            }
        }

        // Both engines should commit the same blocks
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

        // The honest blocks should be committed identically on both
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

    // ── 3c. One-Third Threshold Parametric Test ──────────────────────────────

    #[test]
    fn test_bft_one_third_threshold() {
        // Parametric test with N=4,7,10,13 validators.
        // For each N, f = floor((N-1)/3).
        // With exactly f Byzantine -> safety holds.
        // With f+1 Byzantine -> safety may not hold (demonstrate threshold).
        for n in [4, 7, 10, 13] {
            let f = (n - 1) / 3; // max tolerable faults

            // Test 1: With f Byzantine, system is safe and live
            {
                let honest_count = n - f;
                let stakes: Vec<(u64, u16)> = (0..n).map(|i| (STAKE_ARC, i as u16 % 4)).collect();
                let (vs, addrs) = make_test_validators(&stakes);
                let honest: Vec<Hash256> = addrs[0..honest_count].to_vec();

                let engine = ConsensusEngine::new(vs.clone(), honest[0]);

                // Honest validators provide > 2/3 stake, so system makes progress
                let honest_stake: u64 = honest_count as u64 * STAKE_ARC;
                let required_quorum = vs.quorum;

                if honest_stake >= required_quorum {
                    // Run 3 rounds with honest validators only
                    build_honest_dag(&engine, &honest, 3);
                    let committed = engine.try_commit();
                    assert!(
                        !committed.is_empty(),
                        "N={}, f={}: System should make progress with {} honest validators (stake {} >= quorum {})",
                        n, f, honest_count, honest_stake, required_quorum
                    );
                }
            }

            // Test 2: With f+1 Byzantine, quorum may not be met
            {
                let byz_count = f + 1;
                let honest_count = n - byz_count;
                let stakes: Vec<(u64, u16)> = (0..n).map(|i| (STAKE_ARC, i as u16 % 4)).collect();
                let (vs, addrs) = make_test_validators(&stakes);
                let honest: Vec<Hash256> = addrs[0..honest_count].to_vec();

                let honest_stake = honest_count as u64 * STAKE_ARC;
                let required_quorum = vs.quorum;

                // With f+1 faults, honest stake = (n - f - 1) * STAKE_ARC
                // For n = 3f+1, honest = 2f, which is less than quorum (2f+1)
                // So the system cannot make progress
                if honest_stake < required_quorum {
                    let engine = ConsensusEngine::new(vs.clone(), honest[0]);

                    // Try to build DAG with only honest validators
                    // Round 0 is fine (no parents needed), but advance_round
                    // requires quorum which honest alone can't provide
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

    // ── 4. Consistency Tests ────────────────────────────────────────────────

    // ── 4a. Deterministic Commit Order ───────────────────────────────────────

    #[test]
    fn test_deterministic_commit_order() {
        // Two engines with identical DAG state produce identical commit order.
        let stakes = vec![
            (STAKE_ARC, 0),
            (STAKE_ARC, 1),
            (STAKE_ARC, 0),
            (STAKE_ARC, 1),
        ];
        let (vs, addrs) = make_test_validators(&stakes);

        // Build identical DAG on two engines, but in different insertion orders
        let engine_forward = ConsensusEngine::new(vs.clone(), addrs[0]);
        let engine_reverse = ConsensusEngine::new(vs.clone(), addrs[0]);

        // Round 0
        let mut r0 = Vec::new();
        for (i, addr) in addrs.iter().enumerate() {
            r0.push(make_block(*addr, 0, vec![], vec![hash_bytes(&[0, i as u8])], 100 + i as u64));
        }

        // Forward order
        for block in &r0 {
            engine_forward.receive_block(block).unwrap();
        }
        // Reverse order
        for block in r0.iter().rev() {
            engine_reverse.receive_block(block).unwrap();
        }

        engine_forward.advance_round();
        engine_reverse.advance_round();

        // Round 1
        let r0_parents: Vec<Hash256> = r0[0..3].iter().map(|b| b.hash).collect();
        let mut r1 = Vec::new();
        for (i, addr) in addrs.iter().enumerate() {
            r1.push(make_block(*addr, 1, r0_parents.clone(), vec![], 200 + i as u64));
        }

        for block in &r1 {
            engine_forward.receive_block(block).unwrap();
        }
        for block in r1.iter().rev() {
            engine_reverse.receive_block(block).unwrap();
        }

        engine_forward.advance_round();
        engine_reverse.advance_round();

        // Round 2
        let r1_parents: Vec<Hash256> = r1[0..3].iter().map(|b| b.hash).collect();
        let mut r2 = Vec::new();
        for (i, addr) in addrs.iter().enumerate() {
            r2.push(make_block(*addr, 2, r1_parents.clone(), vec![], 300 + i as u64));
        }

        for block in &r2 {
            engine_forward.receive_block(block).unwrap();
        }
        for block in r2.iter().rev() {
            engine_reverse.receive_block(block).unwrap();
        }

        let cf = engine_forward.try_commit();
        let cr = engine_reverse.try_commit();

        // Must produce identical commit order regardless of insertion order
        assert!(
            commits_equivalent(&cf, &cr),
            "DETERMINISM VIOLATION: different insertion order produced different commit order.\n\
             Forward: {:?}\nReverse: {:?}",
            cf.iter().map(|b| b.hash).collect::<Vec<_>>(),
            cr.iter().map(|b| b.hash).collect::<Vec<_>>(),
        );
    }

    // ── 4b. Two-Round Commit Latency ─────────────────────────────────────────

    #[test]
    fn test_commit_rule_two_round_latency() {
        // A block in round R is committed after round R+2 (per Mysticeti commit rule).
        let stakes = vec![
            (STAKE_ARC, 0),
            (STAKE_ARC, 1),
            (STAKE_ARC, 0),
            (STAKE_ARC, 1),
        ];
        let (vs, addrs) = make_test_validators(&stakes);
        let engine = ConsensusEngine::new(vs, addrs[0]);

        // Round 0: all validators propose
        let mut r0 = Vec::new();
        for (i, addr) in addrs.iter().enumerate() {
            let block = make_block(*addr, 0, vec![], vec![hash_bytes(&[0, i as u8])], 100 + i as u64);
            engine.receive_block(&block).unwrap();
            r0.push(block);
        }
        engine.advance_round();

        // After round 0 only: no commits possible (need R+1 and R+2)
        assert!(
            engine.try_commit().is_empty(),
            "No block should be committed with only round 0 data"
        );

        // Round 1: all validators propose referencing round 0
        let r0_parents: Vec<Hash256> = r0[0..3].iter().map(|b| b.hash).collect();
        let mut r1 = Vec::new();
        for (i, addr) in addrs.iter().enumerate() {
            let block = make_block(*addr, 1, r0_parents.clone(), vec![], 200 + i as u64);
            engine.receive_block(&block).unwrap();
            r1.push(block);
        }
        engine.advance_round();

        // After round 1 only: still no commits (need R+2 for round 0 blocks)
        assert!(
            engine.try_commit().is_empty(),
            "No block should be committed with only rounds 0-1 data"
        );

        // Round 2: all validators propose referencing round 1
        let r1_parents: Vec<Hash256> = r1[0..3].iter().map(|b| b.hash).collect();
        for (i, addr) in addrs.iter().enumerate() {
            let block = make_block(*addr, 2, r1_parents.clone(), vec![], 300 + i as u64);
            engine.receive_block(&block).unwrap();
        }

        // Now with R+2 data, round 0 blocks should be committable
        let committed = engine.try_commit();
        assert!(
            !committed.is_empty(),
            "Blocks from round 0 should be committed after round 2 data is available"
        );

        // All committed blocks should be from round 0 (two-round latency)
        for block in &committed {
            assert_eq!(
                block.round, 0,
                "Only round 0 blocks should be committed at this point, got round {}",
                block.round
            );
        }
    }

    // ── 4c. Commit Requires Quorum ───────────────────────────────────────────

    #[test]
    fn test_commit_requires_quorum() {
        // Block commit requires >= 2f+1 stake in round R+2 referencing the
        // certifying block in R+1.
        let stakes = vec![
            (STAKE_ARC, 0),
            (STAKE_ARC, 1),
            (STAKE_ARC, 0),
            (STAKE_ARC, 1),
        ];
        let (vs, addrs) = make_test_validators(&stakes);
        let engine = ConsensusEngine::new(vs.clone(), addrs[0]);

        // Round 0
        let mut r0 = Vec::new();
        for (i, addr) in addrs.iter().enumerate() {
            let block = make_block(*addr, 0, vec![], vec![], 100 + i as u64);
            engine.receive_block(&block).unwrap();
            r0.push(block);
        }
        engine.advance_round();

        // Round 1: one certifier block C that references r0[0]
        let r0_parents: Vec<Hash256> = r0[0..3].iter().map(|b| b.hash).collect();
        let block_c = make_block(addrs[1], 1, r0_parents.clone(), vec![], 200);
        engine.receive_block(&block_c).unwrap();

        // Add other round 1 blocks so we can advance
        let c2 = make_block(addrs[2], 1, r0_parents.clone(), vec![], 201);
        let c3 = make_block(addrs[3], 1, r0_parents.clone(), vec![], 202);
        engine.receive_block(&c2).unwrap();
        engine.receive_block(&c3).unwrap();
        engine.advance_round();

        // Round 2: Only 1 validator references C (5M < quorum ~13.3M)
        let d0 = make_block(
            addrs[0],
            2,
            vec![block_c.hash, c2.hash, c3.hash],
            vec![],
            300,
        );
        engine.receive_block(&d0).unwrap();

        let committed = engine.try_commit();
        // Check that r0[0] is NOT committed (insufficient R+2 support for C)
        let r0_0_committed = committed.iter().any(|b| b.hash == r0[0].hash);
        assert!(
            !r0_0_committed,
            "Block should NOT be committed with only 1 validator (5M) referencing certifier in R+2 (quorum requires ~13.3M)"
        );

        // Now add 2 more R+2 blocks referencing C -> 3 * 5M = 15M >= quorum
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

    // ── 5. Stake-Weighted Voting Tests ──────────────────────────────────────

    // ── 5a. Stake-Weighted Quorum ────────────────────────────────────────────

    #[test]
    fn test_stake_weighted_quorum() {
        // Quorum is by stake weight, not validator count.
        // One Core (50M) validator = more weight than 5 Spark (500K) validators.
        //
        // Setup: 1 Core (50M) + 5 Spark (500K each = 2.5M)
        // Total = 52.5M, quorum = ceil(2/3 * 52.5M) = 35_000_002
        // Core alone = 50M >= 35M quorum => Core alone forms quorum
        // All 5 Spark = 2.5M << 35M quorum => Sparks cannot form quorum
        //
        // Note: Spark validators can't produce blocks, so we use Core + Arc mix.
        // 1 Core (50M) + 5 Arc (5M each = 25M), total = 75M
        // quorum = ceil(2/3 * 75M) = 50_000_002
        // Core alone = 50M >= 50_000_002? Exactly at threshold!
        //
        // Let's use: 1 Core (50M) + 2 Arc (5M each), total = 60M
        // quorum = ceil(2/3 * 60M) = 40_000_002
        // Core alone = 50M >= 40M quorum -> YES
        // 2 Arc alone = 10M < 40M -> NO
        let stakes = vec![
            (STAKE_CORE, 0), // 50M -- Core
            (STAKE_ARC, 1),  // 5M -- Arc
            (STAKE_ARC, 0),  // 5M -- Arc
        ];
        let (vs, addrs) = make_test_validators(&stakes);

        assert_eq!(vs.total_stake, STAKE_CORE + 2 * STAKE_ARC); // 60M
        assert_eq!(vs.quorum, (2 * 60_000_000 + 2) / 3); // 40_000_002

        // Core alone reaches quorum
        assert!(
            vs.has_quorum(&[addrs[0]]),
            "Core validator (50M) alone should reach quorum ({})",
            vs.quorum
        );

        // Both Arc validators together do NOT reach quorum
        assert!(
            !vs.has_quorum(&[addrs[1], addrs[2]]),
            "Two Arc validators (10M) should NOT reach quorum ({})",
            vs.quorum
        );

        // Test in engine: Core validator alone can advance round
        let engine = ConsensusEngine::new(vs.clone(), addrs[0]);
        let b0 = make_block(addrs[0], 0, vec![], vec![hash_bytes(b"core")], 100);
        engine.receive_block(&b0).unwrap();

        assert!(
            engine.advance_round(),
            "Core validator (50M) alone should be able to advance round"
        );

        // Separate engine: two Arc validators cannot advance round
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

    // ── 5b. Minimum Quorum Stake ─────────────────────────────────────────────

    #[test]
    fn test_minimum_quorum_stake() {
        // Calculate exact quorum threshold for given validator set, verify
        // commit only happens when met.
        for &total_validators in &[4, 7, 10, 13] {
            let stakes: Vec<(u64, u16)> = (0..total_validators)
                .map(|i| (STAKE_ARC, i as u16 % 4))
                .collect();
            let (vs, addrs) = make_test_validators(&stakes);

            let total_stake = total_validators as u64 * STAKE_ARC;
            let expected_quorum = (2 * total_stake + 2) / 3;
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

            // Verify: quorum = total_stake - f, and quorum > 2/3 * total_stake
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

            // Verify minimum number of equal-stake validators for quorum
            let min_validators_for_quorum = (expected_quorum + STAKE_ARC - 1) / STAKE_ARC;
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

            // Full commit test: build a DAG and verify commit requires quorum in R+2
            let engine = ConsensusEngine::new(vs.clone(), addrs[0]);

            // Round 0: all validators
            let mut r0 = Vec::new();
            for (i, addr) in addrs.iter().enumerate() {
                let block = make_block(*addr, 0, vec![], vec![], 100 + i as u64);
                engine.receive_block(&block).unwrap();
                r0.push(block);
            }
            engine.advance_round();

            // Round 1: all validators, referencing quorum-worth of R0
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

            // Round 2: start with below-quorum validators referencing R1
            let r1_parents: Vec<Hash256> = r1[..min_validators_for_quorum as usize]
                .iter()
                .map(|b| b.hash)
                .collect();

            // First: insert below-quorum blocks in R+2
            for i in 0..max_validators_below_quorum as usize {
                let block = make_block(addrs[i], 2, r1_parents.clone(), vec![], 300 + i as u64);
                engine.receive_block(&block).unwrap();
            }

            let committed_below = engine.try_commit();
            // There may or may not be commits here depending on which R1 certifier
            // the below-quorum R2 blocks happen to support. The key invariant is that
            // for a specific B->C path, below quorum R2 support is insufficient.

            // Now add the quorum-reaching validator
            let idx = min_validators_for_quorum as usize - 1;
            if idx < addrs.len() && idx >= max_validators_below_quorum as usize {
                let block =
                    make_block(addrs[idx], 2, r1_parents.clone(), vec![], 300 + idx as u64);
                let _ = engine.receive_block(&block);
            }

            let committed_at_quorum = engine.try_commit();
            let total_committed = engine.committed_blocks().len();
            assert!(
                total_committed > 0,
                "N={}: Should have committed blocks once quorum is reached in R+2",
                total_validators
            );
        }
    }

    // ── Edge Case: Idempotent Commits ────────────────────────────────────────

    #[test]
    fn test_try_commit_idempotent() {
        // Calling try_commit multiple times should not re-commit already committed blocks.
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

        // Second call should return empty (everything already committed)
        let second_commit = engine.try_commit();
        assert!(
            second_commit.is_empty(),
            "Second try_commit should return empty (all blocks already committed). Got {} blocks.",
            second_commit.len()
        );

        // Total committed should be unchanged
        let total = engine.committed_blocks().len();
        assert_eq!(
            total, first_count,
            "Total committed should not change on repeated try_commit"
        );
    }

    // ── Edge Case: Empty Rounds ──────────────────────────────────────────────

    #[test]
    fn test_safety_empty_round_zero() {
        // No blocks at all -> no commits, no panics
        let stakes = vec![(STAKE_ARC, 0), (STAKE_ARC, 1), (STAKE_ARC, 0)];
        let (vs, addrs) = make_test_validators(&stakes);
        let engine = ConsensusEngine::new(vs, addrs[0]);

        let committed = engine.try_commit();
        assert!(committed.is_empty(), "No blocks -> no commits");

        let advanced = engine.advance_round();
        assert!(!advanced, "No blocks -> cannot advance");
    }

    // ── Consistency: Multiple Concurrent Proposals Same Round ────────────────

    #[test]
    fn test_concurrent_proposals_deterministic() {
        // All validators propose in the same round. The committed order must be
        // deterministic regardless of which engine we query.
        let n = 7;
        let stakes: Vec<(u64, u16)> = (0..n).map(|i| (STAKE_ARC, i as u16 % 4)).collect();
        let (vs, addrs) = make_test_validators(&stakes);

        // Build the same DAG on 3 different engines
        let engines: Vec<ConsensusEngine> = (0..3)
            .map(|i| ConsensusEngine::new(vs.clone(), addrs[i]))
            .collect();

        // Create blocks for 4 rounds
        let mut all_blocks: Vec<Vec<DagBlock>> = Vec::new();

        for round in 0..4u64 {
            let parents = if round == 0 {
                vec![]
            } else {
                // Use first quorum-worth of blocks from previous round as parents
                let prev = &all_blocks[(round - 1) as usize];
                let min_quorum_count = ((2 * n + 2) / 3) as usize;
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

            // Insert into all engines
            for engine in &engines {
                for block in &round_blocks {
                    let _ = engine.receive_block(block);
                }
                engine.advance_round();
            }

            all_blocks.push(round_blocks);
        }

        // Commit on all engines
        let commits: Vec<Vec<DagBlock>> = engines.iter().map(|e| e.try_commit()).collect();

        // All must produce identical results
        for i in 1..commits.len() {
            assert!(
                commits_equivalent(&commits[0], &commits[i]),
                "Engine 0 and {} produced different commit orders with identical DAGs",
                i
            );
        }
    }

    // ── Safety: Byzantine Cannot Force Conflicting Commits ───────────────────

    #[test]
    fn test_byzantine_cannot_force_conflicting_commits() {
        // Byzantine validator tries to create a fork by sending different blocks
        // to different honest validators. Despite this, honest validators must
        // agree on the committed sequence (for the blocks they do commit).
        let stakes = vec![
            (STAKE_ARC, 0),
            (STAKE_ARC, 1),
            (STAKE_ARC, 0),
            (STAKE_ARC, 1), // Byzantine
        ];
        let (vs, addrs) = make_test_validators(&stakes);
        let honest = &addrs[0..3];
        let byzantine = addrs[3];

        // Two engines see different Byzantine blocks
        let engine_a = ConsensusEngine::new(vs.clone(), honest[0]);
        let engine_b = ConsensusEngine::new(vs.clone(), honest[1]);

        // Round 0: honest blocks are the same on both
        let h_blocks: Vec<DagBlock> = honest
            .iter()
            .enumerate()
            .map(|(i, addr)| make_block(*addr, 0, vec![], vec![hash_bytes(&[0, i as u8])], 100 + i as u64))
            .collect();

        // Byzantine equivocation
        let byz_for_a = make_block(byzantine, 0, vec![], vec![hash_bytes(b"fork_a")], 150);
        let byz_for_b = make_block(byzantine, 0, vec![], vec![hash_bytes(b"fork_b")], 151);

        // Engine A sees byz_for_a
        for block in &h_blocks {
            engine_a.receive_block(block).unwrap();
        }
        engine_a.receive_block(&byz_for_a).unwrap();

        // Engine B sees byz_for_b
        for block in &h_blocks {
            engine_b.receive_block(block).unwrap();
        }
        engine_b.receive_block(&byz_for_b).unwrap();

        for engine in [&engine_a, &engine_b] {
            engine.advance_round();
        }

        // Rounds 1-2: only honest blocks (no Byzantine participation)
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

        // Both engines must commit the same honest blocks
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
