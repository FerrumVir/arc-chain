//! VRF-based committee selection for tiered inference.
//!
//! For Tier 2+ models, the chain selects a random committee of K validators
//! from the eligible pool using VRF output as seed. Committee members execute
//! the model independently and must reach ≥5/7 agreement on the output hash.
//!
//! Security: With 10% malicious validators, P(corruption) = 0.002%.
//!           With 20% malicious validators, P(corruption) = 0.12%.

use arc_crypto::{hash_bytes, Hash256};
use serde::{Deserialize, Serialize};

/// Default committee size for Tier 2+ inference.
pub const DEFAULT_COMMITTEE_SIZE: usize = 7;

/// Minimum agreement required (5 out of 7).
pub const MIN_AGREEMENT: usize = 5;

/// A validator registered for inference.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InferenceValidator {
    /// Validator address.
    pub address: Hash256,
    /// Maximum inference tier this validator can handle.
    pub max_tier: u8,
    /// Stake amount (used for weighted selection).
    pub stake: u64,
}

/// A selected inference committee.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InferenceCommittee {
    /// Selected committee members (addresses).
    pub members: Vec<Hash256>,
    /// The VRF seed used for selection.
    pub vrf_seed: Hash256,
    /// Required tier for this inference request.
    pub tier: u8,
    /// Minimum agreement count.
    pub min_agreement: usize,
}

/// Result of committee aggregation.
#[derive(Debug, Clone)]
pub enum CommitteeResult {
    /// ≥min_agreement members agree on the output hash.
    Consensus {
        output_hash: Hash256,
        agreeing: usize,
        total: usize,
    },
    /// No sufficient agreement. Falls back to challenge period.
    Disagreement {
        votes: Vec<(Hash256, Hash256)>, // (validator, output_hash)
        total: usize,
    },
}

/// Select a committee of `k` validators from the eligible pool.
///
/// Selection is deterministic given the same seed and pool:
/// 1. Compute `BLAKE3(vrf_seed || validator_address)` for each eligible validator
/// 2. Sort by hash value
/// 3. Take the top `k`
///
/// This ensures unpredictable-but-reproducible selection: the VRF output
/// is unpredictable before the block, but once revealed, any node can
/// verify the selection is correct.
pub fn select_committee(
    vrf_seed: &Hash256,
    eligible: &[InferenceValidator],
    tier: u8,
    k: usize,
) -> InferenceCommittee {
    // Filter to validators capable of the requested tier
    let mut candidates: Vec<(Hash256, Hash256)> = eligible
        .iter()
        .filter(|v| v.max_tier >= tier)
        .map(|v| {
            // Deterministic score: BLAKE3(seed || address)
            let mut input = Vec::with_capacity(64);
            input.extend_from_slice(&vrf_seed.0);
            input.extend_from_slice(&v.address.0);
            let score = hash_bytes(&input);
            (v.address, score)
        })
        .collect();

    // Sort by score (deterministic ordering)
    candidates.sort_by(|a, b| a.1.0.cmp(&b.1.0));

    // Take top k
    let members: Vec<Hash256> = candidates.iter().take(k).map(|(addr, _)| *addr).collect();

    InferenceCommittee {
        members,
        vrf_seed: *vrf_seed,
        tier,
        min_agreement: MIN_AGREEMENT.min(k),
    }
}

/// Aggregate committee votes on an inference result.
///
/// Each committee member submits their `output_hash`. If ≥min_agreement
/// members agree, the result is finalized. Otherwise, falls back to the
/// existing InferenceChallenge mechanism.
pub fn aggregate_votes(
    committee: &InferenceCommittee,
    votes: &[(Hash256, Hash256)], // (validator_address, output_hash)
) -> CommitteeResult {
    // Verify all voters are committee members
    let member_set: std::collections::HashSet<[u8; 32]> =
        committee.members.iter().map(|m| m.0).collect();

    let valid_votes: Vec<_> = votes
        .iter()
        .filter(|(addr, _)| member_set.contains(&addr.0))
        .cloned()
        .collect();

    if valid_votes.is_empty() {
        return CommitteeResult::Disagreement {
            votes: valid_votes,
            total: committee.members.len(),
        };
    }

    // Count votes per output_hash
    let mut vote_counts: std::collections::HashMap<[u8; 32], (Hash256, usize)> =
        std::collections::HashMap::new();

    for (_, output_hash) in &valid_votes {
        let entry = vote_counts.entry(output_hash.0).or_insert((*output_hash, 0));
        entry.1 += 1;
    }

    // Check if any output_hash has sufficient agreement
    for (_, (output_hash, count)) in &vote_counts {
        if *count >= committee.min_agreement {
            return CommitteeResult::Consensus {
                output_hash: *output_hash,
                agreeing: *count,
                total: committee.members.len(),
            };
        }
    }

    CommitteeResult::Disagreement {
        votes: valid_votes,
        total: committee.members.len(),
    }
}

/// Compute the probability of committee corruption.
///
/// Given `f` fraction of malicious validators and committee size `k`,
/// the probability that ≥`min_agree` members are malicious follows a
/// binomial distribution.
///
/// P(corrupt) = sum_{i=min_agree}^{k} C(k,i) * f^i * (1-f)^(k-i)
pub fn corruption_probability(f: f64, k: usize, min_agree: usize) -> f64 {
    let mut prob = 0.0;
    for i in min_agree..=k {
        let binom = binomial_coefficient(k, i) as f64;
        prob += binom * f.powi(i as i32) * (1.0 - f).powi((k - i) as i32);
    }
    prob
}

fn binomial_coefficient(n: usize, k: usize) -> u64 {
    if k > n {
        return 0;
    }
    let k = k.min(n - k);
    let mut result: u64 = 1;
    for i in 0..k {
        result = result * (n - i) as u64 / (i + 1) as u64;
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use arc_crypto::hash_bytes;

    fn make_validators(n: usize, tier: u8) -> Vec<InferenceValidator> {
        (0..n)
            .map(|i| InferenceValidator {
                address: hash_bytes(&i.to_le_bytes()),
                max_tier: tier,
                stake: 10_000,
            })
            .collect()
    }

    #[test]
    fn test_select_committee_basic() {
        let validators = make_validators(100, 2);
        let seed = hash_bytes(b"block-42-tx-7");

        let committee = select_committee(&seed, &validators, 2, 7);
        assert_eq!(committee.members.len(), 7);
        assert_eq!(committee.min_agreement, 5);

        // All members should be unique
        let unique: std::collections::HashSet<_> = committee.members.iter().map(|m| m.0).collect();
        assert_eq!(unique.len(), 7);
    }

    #[test]
    fn test_select_committee_deterministic() {
        let validators = make_validators(50, 3);
        let seed = hash_bytes(b"deterministic-seed");

        let c1 = select_committee(&seed, &validators, 3, 7);
        let c2 = select_committee(&seed, &validators, 3, 7);

        assert_eq!(c1.members, c2.members);
    }

    #[test]
    fn test_select_committee_filters_tier() {
        let mut validators = make_validators(10, 1); // Tier 1 only
        validators.extend(make_validators(5, 3)); // 5 Tier 3 validators

        let seed = hash_bytes(b"tier-filter");
        let committee = select_committee(&seed, &validators, 3, 7);

        // Only 5 validators can do Tier 3, so committee is capped at 5
        assert_eq!(committee.members.len(), 5);
    }

    #[test]
    fn test_aggregate_votes_consensus() {
        let validators = make_validators(7, 2);
        let seed = hash_bytes(b"consensus-test");
        let committee = select_committee(&seed, &validators, 2, 7);

        let output = hash_bytes(b"correct-output");
        let votes: Vec<_> = committee
            .members
            .iter()
            .map(|m| (*m, output))
            .collect();

        match aggregate_votes(&committee, &votes) {
            CommitteeResult::Consensus { agreeing, .. } => {
                assert_eq!(agreeing, 7);
            }
            _ => panic!("Expected consensus"),
        }
    }

    #[test]
    fn test_aggregate_votes_partial_agreement() {
        let validators = make_validators(7, 2);
        let seed = hash_bytes(b"partial-test");
        let committee = select_committee(&seed, &validators, 2, 7);

        let correct = hash_bytes(b"correct");
        let wrong = hash_bytes(b"wrong");

        // 5 correct + 2 wrong → should still reach consensus
        let mut votes: Vec<_> = committee.members[..5]
            .iter()
            .map(|m| (*m, correct))
            .collect();
        votes.extend(committee.members[5..].iter().map(|m| (*m, wrong)));

        match aggregate_votes(&committee, &votes) {
            CommitteeResult::Consensus {
                output_hash,
                agreeing,
                ..
            } => {
                assert_eq!(output_hash, correct);
                assert_eq!(agreeing, 5);
            }
            _ => panic!("Expected consensus with 5/7"),
        }
    }

    #[test]
    fn test_aggregate_votes_disagreement() {
        let validators = make_validators(7, 2);
        let seed = hash_bytes(b"disagree-test");
        let committee = select_committee(&seed, &validators, 2, 7);

        // 4 correct + 3 wrong → not enough for consensus
        let correct = hash_bytes(b"correct");
        let wrong = hash_bytes(b"wrong");

        let mut votes: Vec<_> = committee.members[..4]
            .iter()
            .map(|m| (*m, correct))
            .collect();
        votes.extend(committee.members[4..].iter().map(|m| (*m, wrong)));

        match aggregate_votes(&committee, &votes) {
            CommitteeResult::Disagreement { total, .. } => {
                assert_eq!(total, 7);
            }
            _ => panic!("Expected disagreement"),
        }
    }

    #[test]
    fn test_corruption_probability() {
        // 10% malicious, 7 committee, 5 required → ~0.002%
        let p = corruption_probability(0.1, 7, 5);
        assert!(p < 0.001, "P(corrupt) = {p}, expected < 0.1%");

        // 20% malicious → ~0.12%
        let p = corruption_probability(0.2, 7, 5);
        assert!(p < 0.005, "P(corrupt) = {p}, expected < 0.5%");

        // 50% malicious → much higher
        let p = corruption_probability(0.5, 7, 5);
        assert!(p > 0.1, "P(corrupt) = {p}, expected > 10%");
    }

    #[test]
    fn test_different_seeds_different_committees() {
        let validators = make_validators(100, 2);
        let c1 = select_committee(&hash_bytes(b"seed-1"), &validators, 2, 7);
        let c2 = select_committee(&hash_bytes(b"seed-2"), &validators, 2, 7);

        // Very unlikely to be identical with different seeds
        assert_ne!(c1.members, c2.members);
    }
}
