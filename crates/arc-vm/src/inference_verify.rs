//! Inference Result Verification
//! Challenge-based verification system for AI inference results on-chain.

use std::collections::HashMap;

/// A commitment to an inference result, posted by a provider.
#[derive(Debug, Clone)]
pub struct InferenceCommitment {
    pub request_id: [u8; 32],
    pub result_hash: [u8; 32],
    pub provider: [u8; 32],
    pub timestamp: u64,
    pub bond_amount: u64,
}

/// Types of verification challenges that can be issued.
#[derive(Debug, Clone, PartialEq)]
pub enum ChallengeType {
    /// Full re-execution of the inference
    ReExecution,
    /// Spot-check of specific output tokens
    SpotCheck,
    /// Statistical audit of output distribution
    StatisticalAudit,
    /// Consensus-based verification across multiple providers
    ConsensusVerification,
}

/// Current status of a challenge.
#[derive(Debug, Clone, PartialEq)]
pub enum ChallengeStatus {
    Open,
    Responded,
    Resolved,
    Expired,
}

/// A verification challenge against an inference commitment.
#[derive(Debug, Clone)]
pub struct VerificationChallenge {
    pub commitment_id: [u8; 32],
    pub challenger: [u8; 32],
    pub challenge_type: ChallengeType,
    pub bond_amount: u64,
    pub deadline: u64,
    pub status: ChallengeStatus,
    /// Proof data submitted in response (if any)
    pub response_proof: Option<Vec<u8>>,
}

/// Outcome of a resolved challenge.
#[derive(Debug, Clone)]
pub struct ChallengeResolution {
    pub challenge_id: [u8; 32],
    pub winner: [u8; 32],
    pub loser: [u8; 32],
    pub slash_amount: u64,
    pub reason: String,
}

/// Manages inference commitments, challenges, and resolutions.
pub struct VerificationManager {
    commitments: HashMap<[u8; 32], InferenceCommitment>,
    challenges: HashMap<[u8; 32], VerificationChallenge>,
    /// Tracks how many successful/failed challenges per provider for reputation
    provider_successes: HashMap<[u8; 32], u64>,
    provider_failures: HashMap<[u8; 32], u64>,
    next_commitment_nonce: u64,
    next_challenge_nonce: u64,
}

impl VerificationManager {
    /// Create a new verification manager.
    pub fn new() -> Self {
        Self {
            commitments: HashMap::new(),
            challenges: HashMap::new(),
            provider_successes: HashMap::new(),
            provider_failures: HashMap::new(),
            next_commitment_nonce: 1,
            next_challenge_nonce: 1,
        }
    }

    /// Submit an inference commitment from a provider.
    ///
    /// Returns a unique commitment ID derived from the nonce.
    pub fn submit_commitment(&mut self, commitment: InferenceCommitment) -> [u8; 32] {
        let id = self.make_id(self.next_commitment_nonce);
        self.next_commitment_nonce += 1;

        // Initialize provider reputation tracking if new
        self.provider_successes
            .entry(commitment.provider)
            .or_insert(0);
        self.provider_failures
            .entry(commitment.provider)
            .or_insert(0);

        self.commitments.insert(id, commitment);
        id
    }

    /// Create a challenge against an existing commitment.
    ///
    /// The challenger must provide a bond. Returns the challenge ID.
    pub fn create_challenge(
        &mut self,
        commitment_id: [u8; 32],
        challenger: [u8; 32],
        challenge_type: ChallengeType,
        bond: u64,
    ) -> Result<[u8; 32], String> {
        let commitment = self
            .commitments
            .get(&commitment_id)
            .ok_or("Commitment not found")?;

        if challenger == commitment.provider {
            return Err("Provider cannot challenge their own commitment".to_string());
        }

        if bond == 0 {
            return Err("Challenge bond must be greater than zero".to_string());
        }

        let challenge_id = self.make_id(self.next_challenge_nonce);
        self.next_challenge_nonce += 1;

        // Deadline is commitment timestamp + 1 hour (3600 seconds)
        let deadline = commitment.timestamp + 3600;

        let challenge = VerificationChallenge {
            commitment_id,
            challenger,
            challenge_type,
            bond_amount: bond,
            deadline,
            status: ChallengeStatus::Open,
            response_proof: None,
        };

        self.challenges.insert(challenge_id, challenge);
        Ok(challenge_id)
    }

    /// Respond to a challenge by submitting proof data.
    ///
    /// Only works on challenges with Open status.
    pub fn respond_to_challenge(
        &mut self,
        challenge_id: [u8; 32],
        proof: Vec<u8>,
    ) -> Result<(), String> {
        let challenge = self
            .challenges
            .get_mut(&challenge_id)
            .ok_or("Challenge not found")?;

        if challenge.status != ChallengeStatus::Open {
            return Err(format!(
                "Challenge is not open (status: {:?})",
                challenge.status
            ));
        }

        if proof.is_empty() {
            return Err("Proof data must not be empty".to_string());
        }

        challenge.response_proof = Some(proof);
        challenge.status = ChallengeStatus::Responded;
        Ok(())
    }

    /// Resolve a responded challenge.
    ///
    /// Mock resolution: if proof length > 32 bytes, provider wins. Otherwise challenger wins.
    /// This simulates a real verification where longer proofs contain more evidence.
    pub fn resolve_challenge(
        &mut self,
        challenge_id: [u8; 32],
    ) -> Result<ChallengeResolution, String> {
        let challenge = self
            .challenges
            .get(&challenge_id)
            .ok_or("Challenge not found")?
            .clone();

        if challenge.status != ChallengeStatus::Responded {
            return Err(format!(
                "Challenge must be in Responded status to resolve (status: {:?})",
                challenge.status
            ));
        }

        let commitment = self
            .commitments
            .get(&challenge.commitment_id)
            .ok_or("Associated commitment not found")?;

        let provider = commitment.provider;
        let challenger = challenge.challenger;

        // Mock: proof > 32 bytes means provider successfully proved correctness
        let proof_len = challenge
            .response_proof
            .as_ref()
            .map(|p| p.len())
            .unwrap_or(0);

        let (winner, loser, slash_amount, reason) = if proof_len > 32 {
            // Provider wins: proof is sufficient
            *self.provider_successes.entry(provider).or_insert(0) += 1;
            (
                provider,
                challenger,
                challenge.bond_amount,
                "Provider proof verified successfully".to_string(),
            )
        } else {
            // Challenger wins: proof is insufficient
            *self.provider_failures.entry(provider).or_insert(0) += 1;
            (
                challenger,
                provider,
                commitment.bond_amount,
                "Provider failed to provide sufficient proof".to_string(),
            )
        };

        // Mark resolved
        self.challenges.get_mut(&challenge_id).unwrap().status = ChallengeStatus::Resolved;

        Ok(ChallengeResolution {
            challenge_id,
            winner,
            loser,
            slash_amount,
            reason,
        })
    }

    /// Expire all challenges whose deadline has passed.
    ///
    /// Returns the number of challenges expired.
    pub fn expire_challenges(&mut self, now: u64) -> usize {
        let mut expired_ids = Vec::new();

        for (id, challenge) in &self.challenges {
            if challenge.status == ChallengeStatus::Open && now >= challenge.deadline {
                expired_ids.push(*id);
            }
        }

        let count = expired_ids.len();
        for id in expired_ids {
            if let Some(challenge) = self.challenges.get_mut(&id) {
                challenge.status = ChallengeStatus::Expired;

                // Challenger wins by default when provider doesn't respond
                if let Some(commitment) = self.commitments.get(&challenge.commitment_id) {
                    *self
                        .provider_failures
                        .entry(commitment.provider)
                        .or_insert(0) += 1;
                }
            }
        }

        count
    }

    /// Get the reputation score of a provider (0.0 to 1.0).
    ///
    /// Calculated as successes / (successes + failures). Returns 1.0 if no
    /// challenges have been resolved yet.
    pub fn get_provider_reputation(&self, provider: [u8; 32]) -> f64 {
        let successes = self.provider_successes.get(&provider).copied().unwrap_or(0);
        let failures = self.provider_failures.get(&provider).copied().unwrap_or(0);

        let total = successes + failures;
        if total == 0 {
            return 1.0;
        }

        successes as f64 / total as f64
    }

    /// Generate a deterministic ID from a nonce.
    fn make_id(&self, nonce: u64) -> [u8; 32] {
        let hash = blake3::hash(&nonce.to_le_bytes());
        *hash.as_bytes()
    }
}

impl Default for VerificationManager {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn provider_addr(seed: u8) -> [u8; 32] {
        let mut a = [0u8; 32];
        a[0] = seed;
        a
    }

    fn make_commitment(provider_seed: u8, bond: u64) -> InferenceCommitment {
        InferenceCommitment {
            request_id: provider_addr(100 + provider_seed),
            result_hash: provider_addr(200 + provider_seed),
            provider: provider_addr(provider_seed),
            timestamp: 1_000_000,
            bond_amount: bond,
        }
    }

    #[test]
    fn test_submit_commitment() {
        let mut mgr = VerificationManager::new();
        let id = mgr.submit_commitment(make_commitment(1, 1000));
        assert!(!id.iter().all(|&b| b == 0));
    }

    #[test]
    fn test_create_challenge() {
        let mut mgr = VerificationManager::new();
        let cid = mgr.submit_commitment(make_commitment(1, 1000));
        let challenger = provider_addr(2);

        let result = mgr.create_challenge(cid, challenger, ChallengeType::ReExecution, 500);
        assert!(result.is_ok());
    }

    #[test]
    fn test_create_challenge_nonexistent_commitment() {
        let mut mgr = VerificationManager::new();
        let fake_id = provider_addr(99);
        let result = mgr.create_challenge(fake_id, provider_addr(2), ChallengeType::SpotCheck, 100);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("Commitment not found"));
    }

    #[test]
    fn test_self_challenge_rejected() {
        let mut mgr = VerificationManager::new();
        let provider = provider_addr(1);
        let cid = mgr.submit_commitment(make_commitment(1, 1000));

        let result = mgr.create_challenge(cid, provider, ChallengeType::ReExecution, 500);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("cannot challenge their own"));
    }

    #[test]
    fn test_zero_bond_rejected() {
        let mut mgr = VerificationManager::new();
        let cid = mgr.submit_commitment(make_commitment(1, 1000));
        let result = mgr.create_challenge(cid, provider_addr(2), ChallengeType::SpotCheck, 0);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("greater than zero"));
    }

    #[test]
    fn test_respond_to_challenge() {
        let mut mgr = VerificationManager::new();
        let cid = mgr.submit_commitment(make_commitment(1, 1000));
        let challenge_id = mgr
            .create_challenge(cid, provider_addr(2), ChallengeType::ReExecution, 500)
            .unwrap();

        let result = mgr.respond_to_challenge(challenge_id, vec![0xAA; 64]);
        assert!(result.is_ok());

        // Cannot respond again
        let result2 = mgr.respond_to_challenge(challenge_id, vec![0xBB; 32]);
        assert!(result2.is_err());
    }

    #[test]
    fn test_resolve_challenge_provider_wins() {
        let mut mgr = VerificationManager::new();
        let cid = mgr.submit_commitment(make_commitment(1, 1000));
        let challenge_id = mgr
            .create_challenge(cid, provider_addr(2), ChallengeType::ReExecution, 500)
            .unwrap();

        // Proof > 32 bytes: provider wins
        mgr.respond_to_challenge(challenge_id, vec![0xAA; 64]).unwrap();
        let resolution = mgr.resolve_challenge(challenge_id).unwrap();

        assert_eq!(resolution.winner, provider_addr(1));
        assert_eq!(resolution.loser, provider_addr(2));
        assert_eq!(resolution.slash_amount, 500); // challenger's bond
        assert!(resolution.reason.contains("verified successfully"));
    }

    #[test]
    fn test_resolve_challenge_challenger_wins() {
        let mut mgr = VerificationManager::new();
        let cid = mgr.submit_commitment(make_commitment(1, 1000));
        let challenge_id = mgr
            .create_challenge(cid, provider_addr(2), ChallengeType::SpotCheck, 500)
            .unwrap();

        // Proof <= 32 bytes: challenger wins
        mgr.respond_to_challenge(challenge_id, vec![0xBB; 16]).unwrap();
        let resolution = mgr.resolve_challenge(challenge_id).unwrap();

        assert_eq!(resolution.winner, provider_addr(2));
        assert_eq!(resolution.loser, provider_addr(1));
        assert_eq!(resolution.slash_amount, 1000); // provider's bond
    }

    #[test]
    fn test_expire_challenges() {
        let mut mgr = VerificationManager::new();
        let cid = mgr.submit_commitment(make_commitment(1, 1000));
        let _ch_id = mgr
            .create_challenge(cid, provider_addr(2), ChallengeType::StatisticalAudit, 500)
            .unwrap();

        // Before deadline
        let expired = mgr.expire_challenges(1_001_000);
        assert_eq!(expired, 0);

        // After deadline (timestamp 1_000_000 + 3600 = 1_003_600)
        let expired = mgr.expire_challenges(1_003_601);
        assert_eq!(expired, 1);

        // Provider gets a failure mark
        assert!(mgr.get_provider_reputation(provider_addr(1)) < 1.0);
    }

    #[test]
    fn test_provider_reputation() {
        let mut mgr = VerificationManager::new();
        let provider = provider_addr(1);

        // New provider has perfect reputation
        assert_eq!(mgr.get_provider_reputation(provider), 1.0);

        // Submit commitment and win a challenge
        let cid = mgr.submit_commitment(make_commitment(1, 1000));
        let ch = mgr
            .create_challenge(cid, provider_addr(2), ChallengeType::ReExecution, 500)
            .unwrap();
        mgr.respond_to_challenge(ch, vec![0xAA; 64]).unwrap();
        mgr.resolve_challenge(ch).unwrap();

        assert_eq!(mgr.get_provider_reputation(provider), 1.0); // 1 success, 0 failures

        // Lose a challenge
        let cid2 = mgr.submit_commitment(make_commitment(1, 1000));
        let ch2 = mgr
            .create_challenge(cid2, provider_addr(3), ChallengeType::SpotCheck, 500)
            .unwrap();
        mgr.respond_to_challenge(ch2, vec![0xBB; 8]).unwrap();
        mgr.resolve_challenge(ch2).unwrap();

        assert_eq!(mgr.get_provider_reputation(provider), 0.5); // 1 success, 1 failure
    }
}
