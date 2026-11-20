use arc_crypto::{Hash256, hash_bytes};
use serde::{Deserialize, Serialize};

pub const DEFAULT_COMMITTEE_SIZE: usize = 7;

pub const MIN_AGREEMENT: usize = 5;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InferenceValidator {
    pub address: Hash256,
    pub max_tier: u8,
    pub stake: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InferenceCommittee {
    pub members: Vec<Hash256>,
    pub vrf_seed: Hash256,
    pub tier: u8,
    pub min_agreement: usize,
}

#[derive(Debug, Clone)]
pub enum CommitteeResult {
    Consensus {
        output_hash: Hash256,
        agreeing: usize,
        total: usize,
    },
    Disagreement {
        votes: Vec<(Hash256, Hash256)>,
        total: usize,
    },
}

pub fn select_committee(
    vrf_seed: &Hash256,
    eligible: &[InferenceValidator],
    tier: u8,
    k: usize,
) -> InferenceCommittee {
    let mut candidates: Vec<(Hash256, Hash256)> = eligible
        .iter()
        .filter(|v| v.max_tier >= tier)
        .map(|v| {
            let mut input = Vec::with_capacity(64);
            input.extend_from_slice(&vrf_seed.0);
            input.extend_from_slice(&v.address.0);
            let score = hash_bytes(&input);
            (v.address, score)
        })
        .collect();

    candidates.sort_by_key(|a| a.1.0);

    let members: Vec<Hash256> = candidates.iter().take(k).map(|(addr, _)| *addr).collect();

    InferenceCommittee {
        members,
        vrf_seed: *vrf_seed,
        tier,
        min_agreement: MIN_AGREEMENT.min(k),
    }
}

pub fn aggregate_votes(
    committee: &InferenceCommittee,
    votes: &[(Hash256, Hash256)],
) -> CommitteeResult {
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

    let mut vote_counts: std::collections::HashMap<[u8; 32], (Hash256, usize)> =
        std::collections::HashMap::new();

    for (_, output_hash) in &valid_votes {
        let entry = vote_counts
            .entry(output_hash.0)
            .or_insert((*output_hash, 0));
        entry.1 += 1;
    }

    let winner = vote_counts
        .values()
        .filter(|(_, count)| *count >= committee.min_agreement)
        .min_by_key(|(output_hash, count)| (std::cmp::Reverse(*count), output_hash.0));
    if let Some((output_hash, count)) = winner {
        return CommitteeResult::Consensus {
            output_hash: *output_hash,
            agreeing: *count,
            total: committee.members.len(),
        };
    }

    CommitteeResult::Disagreement {
        votes: valid_votes,
        total: committee.members.len(),
    }
}

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
        let mut validators = make_validators(10, 1);
        validators.extend(make_validators(5, 3));

        let seed = hash_bytes(b"tier-filter");
        let committee = select_committee(&seed, &validators, 3, 7);

        assert_eq!(committee.members.len(), 5);
    }

    #[test]
    fn test_aggregate_votes_consensus() {
        let validators = make_validators(7, 2);
        let seed = hash_bytes(b"consensus-test");
        let committee = select_committee(&seed, &validators, 2, 7);

        let output = hash_bytes(b"correct-output");
        let votes: Vec<_> = committee.members.iter().map(|m| (*m, output)).collect();

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
        let p = corruption_probability(0.1, 7, 5);
        assert!(p < 0.001, "P(corrupt) = {p}, expected < 0.1%");

        let p = corruption_probability(0.2, 7, 5);
        assert!(p < 0.01, "P(corrupt) = {p}, expected < 1%");

        let p = corruption_probability(0.5, 7, 5);
        assert!(p > 0.1, "P(corrupt) = {p}, expected > 10%");
    }

    #[test]
    fn tied_agreement_resolves_to_the_same_hash_on_every_aggregation() {
        let validators = make_validators(10, 2);
        let seed = hash_bytes(b"tie-determinism");
        let committee = select_committee(&seed, &validators, 2, 10);
        assert_eq!(committee.members.len(), 10);
        assert_eq!(committee.min_agreement, 5);

        let hash_a = hash_bytes(b"branch-a");
        let hash_b = hash_bytes(b"branch-b");
        let mut votes: Vec<_> = committee.members[..5]
            .iter()
            .map(|m| (*m, hash_a))
            .collect();
        votes.extend(committee.members[5..].iter().map(|m| (*m, hash_b)));

        let expected = if hash_a.0 <= hash_b.0 { hash_a } else { hash_b };

        for attempt in 0..200 {
            match aggregate_votes(&committee, &votes) {
                CommitteeResult::Consensus {
                    output_hash,
                    agreeing,
                    ..
                } => {
                    assert_eq!(agreeing, 5);
                    assert_eq!(
                        output_hash, expected,
                        "aggregation picked a different winner on attempt {attempt}"
                    );
                }
                other => panic!("expected consensus at the threshold, got {other:?}"),
            }
        }
    }

    #[test]
    fn test_different_seeds_different_committees() {
        let validators = make_validators(100, 2);
        let c1 = select_committee(&hash_bytes(b"seed-1"), &validators, 2, 7);
        let c2 = select_committee(&hash_bytes(b"seed-2"), &validators, 2, 7);

        assert_ne!(c1.members, c2.members);
    }
}
