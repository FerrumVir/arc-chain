use arc_crypto::hash::Hash256;
use arc_crypto::signature::{KeyPair, SignatureError};
use arc_crypto::vrf::{VrfProof as CryptoVrfProof, vrf_prove, vrf_verify};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;

const VRF_SLOT_DOMAIN: &str = "ARC-vrf-slot-input-v1";

const EXPECTED_PROPOSERS_PER_SLOT: u64 = 1;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct VrfOutput {
    pub value: [u8; 32],
}

impl VrfOutput {
    fn below_threshold(&self, validator_stake: u64, total_stake: u64) -> bool {
        if total_stake == 0 {
            return false;
        }
        let vrf_value = u64::from_be_bytes([
            self.value[0],
            self.value[1],
            self.value[2],
            self.value[3],
            self.value[4],
            self.value[5],
            self.value[6],
            self.value[7],
        ]);

        let lhs = (vrf_value as u128) * (total_stake as u128);
        let rhs =
            (validator_stake as u128) * (EXPECTED_PROPOSERS_PER_SLOT as u128) * (u64::MAX as u128);
        lhs <= rhs
    }

    fn weighted_score(&self, validator_stake: u64) -> u128 {
        if validator_stake == 0 {
            return u128::MAX;
        }
        let vrf_value = u64::from_be_bytes([
            self.value[0],
            self.value[1],
            self.value[2],
            self.value[3],
            self.value[4],
            self.value[5],
            self.value[6],
            self.value[7],
        ]);
        (vrf_value as u128) * 1_000_000_000 / (validator_stake as u128)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct VrfProof {
    pub inner: CryptoVrfProof,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ValidatorInfo {
    pub public_key: [u8; 32],
    pub stake: u64,
    pub address: Hash256,
}

pub type Address = Hash256;

pub struct ProposerSelector {
    validators: Vec<ValidatorInfo>,
    total_stake: u64,
}

impl ProposerSelector {
    pub fn new(validators: Vec<ValidatorInfo>) -> Self {
        assert!(
            !validators.is_empty(),
            "ProposerSelector requires at least one validator"
        );
        let mut addresses = HashSet::with_capacity(validators.len());
        let mut total_stake = 0u64;
        for validator in &validators {
            assert!(
                addresses.insert(validator.address),
                "duplicate validator address in proposer selector"
            );
            total_stake = total_stake
                .checked_add(validator.stake)
                .expect("total proposer stake exceeds u64::MAX");
        }
        Self {
            validators,
            total_stake,
        }
    }

    pub fn total_stake(&self) -> u64 {
        self.total_stake
    }

    pub fn validator_count(&self) -> usize {
        self.validators.len()
    }

    pub fn get_validator(&self, address: &Hash256) -> Option<&ValidatorInfo> {
        self.validators.iter().find(|v| v.address == *address)
    }

    fn slot_vrf_input(slot: u64, prev_hash: &[u8; 32]) -> Vec<u8> {
        let mut hasher = blake3::Hasher::new_derive_key(VRF_SLOT_DOMAIN);
        hasher.update(&slot.to_le_bytes());
        hasher.update(prev_hash);
        hasher.finalize().as_bytes().to_vec()
    }

    pub fn compute_vrf(
        keypair: &KeyPair,
        slot: u64,
        prev_hash: &[u8; 32],
    ) -> Result<(VrfOutput, VrfProof), SignatureError> {
        let alpha = Self::slot_vrf_input(slot, prev_hash);
        let (crypto_proof, crypto_output) = vrf_prove(keypair, &alpha)?;

        let output = VrfOutput {
            value: *crypto_output.0.as_bytes(),
        };
        let proof = VrfProof {
            inner: crypto_proof,
        };
        Ok((output, proof))
    }

    pub fn verify_vrf(
        validator_address: &Hash256,
        slot: u64,
        prev_hash: &[u8; 32],
        output: &VrfOutput,
        proof: &VrfProof,
    ) -> bool {
        let alpha = Self::slot_vrf_input(slot, prev_hash);

        match vrf_verify(validator_address, &alpha, &proof.inner) {
            Ok(verified_output) => *verified_output.0.as_bytes() == output.value,
            Err(_) => false,
        }
    }

    pub fn is_proposer(&self, validator_stake: u64, vrf_output: &VrfOutput) -> bool {
        vrf_output.below_threshold(validator_stake, self.total_stake)
    }

    pub fn select_proposer(
        &self,
        candidates: &[(Address, VrfOutput, VrfProof)],
    ) -> Option<Address> {
        if candidates.is_empty() {
            return None;
        }

        let mut best_address: Option<Address> = None;
        let mut best_score: u128 = u128::MAX;

        for (address, vrf_output, _proof) in candidates {
            let stake = match self.get_validator(address) {
                Some(v) => v.stake,
                None => continue,
            };

            let score = vrf_output.weighted_score(stake);
            if score < best_score {
                best_score = score;
                best_address = Some(*address);
            }
        }

        best_address
    }

    pub fn select_proposer_verified(
        &self,
        slot: u64,
        prev_hash: &[u8; 32],
        candidates: &[(Address, VrfOutput, VrfProof)],
    ) -> Option<Address> {
        let valid_candidates: Vec<(Address, VrfOutput, VrfProof)> = candidates
            .iter()
            .filter(|(addr, output, proof)| Self::verify_vrf(addr, slot, prev_hash, output, proof))
            .cloned()
            .collect();

        self.select_proposer(&valid_candidates)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use arc_crypto::hash::hash_bytes;
    use arc_crypto::signature::KeyPair;

    fn make_validator(keypair: &KeyPair, stake: u64) -> ValidatorInfo {
        let address = keypair.address();
        let pk_bytes = keypair.public_key_bytes();
        let mut public_key = [0u8; 32];
        public_key.copy_from_slice(&pk_bytes[..32]);
        ValidatorInfo {
            public_key,
            stake,
            address,
        }
    }

    fn test_keypair(index: u8) -> KeyPair {
        let seed = blake3::derive_key("ARC-test-vrf-keypair-v1", &[index]);
        let signing_key = ed25519_dalek::SigningKey::from_bytes(&seed);
        KeyPair::Ed25519(signing_key)
    }

    #[test]
    fn compute_and_verify_vrf() {
        let kp = test_keypair(0);
        let validator = make_validator(&kp, 10_000_000);
        let selector = ProposerSelector::new(vec![validator.clone()]);

        let slot = 42;
        let prev_hash = hash_bytes(b"previous-block").0;

        let (output, proof) =
            ProposerSelector::compute_vrf(&kp, slot, &prev_hash).expect("compute ok");

        assert!(
            ProposerSelector::verify_vrf(&validator.address, slot, &prev_hash, &output, &proof),
            "valid proof must verify"
        );

        assert_eq!(selector.total_stake(), 10_000_000);
    }

    #[test]
    fn vrf_deterministic() {
        let kp = test_keypair(1);
        let prev_hash = hash_bytes(b"anchor").0;

        let (output1, _) = ProposerSelector::compute_vrf(&kp, 100, &prev_hash).expect("1");
        let (output2, _) = ProposerSelector::compute_vrf(&kp, 100, &prev_hash).expect("2");

        assert_eq!(
            output1, output2,
            "same key + same slot + same prev_hash must produce identical output"
        );
    }

    #[test]
    fn vrf_different_slots_produce_different_outputs() {
        let kp = test_keypair(2);
        let prev_hash = hash_bytes(b"anchor").0;

        let (output_a, _) = ProposerSelector::compute_vrf(&kp, 1, &prev_hash).expect("a");
        let (output_b, _) = ProposerSelector::compute_vrf(&kp, 2, &prev_hash).expect("b");

        assert_ne!(
            output_a.value, output_b.value,
            "different slots must produce different VRF outputs"
        );
    }

    #[test]
    fn vrf_different_prev_hashes_produce_different_outputs() {
        let kp = test_keypair(3);
        let hash_a = hash_bytes(b"block-A").0;
        let hash_b = hash_bytes(b"block-B").0;

        let (output_a, _) = ProposerSelector::compute_vrf(&kp, 1, &hash_a).expect("a");
        let (output_b, _) = ProposerSelector::compute_vrf(&kp, 1, &hash_b).expect("b");

        assert_ne!(
            output_a.value, output_b.value,
            "different prev_hashes must produce different VRF outputs"
        );
    }

    #[test]
    fn vrf_different_keys_produce_different_outputs() {
        let kp_a = test_keypair(4);
        let kp_b = test_keypair(5);
        let prev_hash = hash_bytes(b"anchor").0;

        let (output_a, _) = ProposerSelector::compute_vrf(&kp_a, 1, &prev_hash).expect("a");
        let (output_b, _) = ProposerSelector::compute_vrf(&kp_b, 1, &prev_hash).expect("b");

        assert_ne!(
            output_a.value, output_b.value,
            "different keys must produce different VRF outputs"
        );
    }

    #[test]
    fn verify_fails_with_wrong_address() {
        let kp = test_keypair(6);
        let wrong_address = hash_bytes(b"wrong-validator");
        let prev_hash = hash_bytes(b"anchor").0;

        let (output, proof) = ProposerSelector::compute_vrf(&kp, 1, &prev_hash).expect("ok");

        assert!(
            !ProposerSelector::verify_vrf(&wrong_address, 1, &prev_hash, &output, &proof),
            "must fail with wrong address"
        );
    }

    #[test]
    fn verify_fails_with_wrong_slot() {
        let kp = test_keypair(7);
        let validator = make_validator(&kp, 5_000_000);
        let prev_hash = hash_bytes(b"anchor").0;

        let (output, proof) = ProposerSelector::compute_vrf(&kp, 1, &prev_hash).expect("ok");

        assert!(
            !ProposerSelector::verify_vrf(&validator.address, 999, &prev_hash, &output, &proof),
            "must fail with wrong slot"
        );
    }

    #[test]
    fn verify_fails_with_wrong_prev_hash() {
        let kp = test_keypair(8);
        let validator = make_validator(&kp, 5_000_000);
        let prev_hash = hash_bytes(b"anchor").0;
        let wrong_hash = hash_bytes(b"wrong").0;

        let (output, proof) = ProposerSelector::compute_vrf(&kp, 1, &prev_hash).expect("ok");

        assert!(
            !ProposerSelector::verify_vrf(&validator.address, 1, &wrong_hash, &output, &proof),
            "must fail with wrong prev_hash"
        );
    }

    #[test]
    fn verify_fails_with_tampered_output() {
        let kp = test_keypair(9);
        let validator = make_validator(&kp, 5_000_000);
        let prev_hash = hash_bytes(b"anchor").0;

        let (mut output, proof) = ProposerSelector::compute_vrf(&kp, 1, &prev_hash).expect("ok");

        output.value[0] ^= 0xff;

        assert!(
            !ProposerSelector::verify_vrf(&validator.address, 1, &prev_hash, &output, &proof),
            "must fail with tampered output"
        );
    }

    #[test]
    fn sole_validator_always_proposer() {
        let kp = test_keypair(10);
        let validator = make_validator(&kp, 10_000_000);
        let selector = ProposerSelector::new(vec![validator]);

        let prev_hash = hash_bytes(b"anchor").0;

        let mut eligible_count = 0;
        for slot in 0..100 {
            let (output, _) = ProposerSelector::compute_vrf(&kp, slot, &prev_hash).expect("ok");
            if selector.is_proposer(10_000_000, &output) {
                eligible_count += 1;
            }
        }

        assert_eq!(
            eligible_count, 100,
            "sole validator must be eligible for every slot"
        );
    }

    #[test]
    fn zero_stake_never_proposer() {
        let kp = test_keypair(11);
        let validator = make_validator(&kp, 10_000_000);
        let selector = ProposerSelector::new(vec![validator]);

        let prev_hash = hash_bytes(b"anchor").0;
        let (output, _) = ProposerSelector::compute_vrf(&kp, 1, &prev_hash).expect("ok");

        assert!(
            !selector.is_proposer(0, &output),
            "zero-stake validator must never be eligible"
        );
    }

    #[test]
    fn stake_weighted_selection_probability() {
        let kp_high = test_keypair(12);
        let kp_low = test_keypair(13);

        let v_high = make_validator(&kp_high, 90_000_000);
        let v_low = make_validator(&kp_low, 10_000_000);

        let selector = ProposerSelector::new(vec![v_high, v_low]);
        let prev_hash = hash_bytes(b"distribution-test").0;

        let mut high_eligible = 0u64;
        let mut low_eligible = 0u64;
        let num_slots = 10_000;

        for slot in 0..num_slots {
            let (output_h, _) =
                ProposerSelector::compute_vrf(&kp_high, slot, &prev_hash).expect("h");
            let (output_l, _) =
                ProposerSelector::compute_vrf(&kp_low, slot, &prev_hash).expect("l");

            if selector.is_proposer(90_000_000, &output_h) {
                high_eligible += 1;
            }
            if selector.is_proposer(10_000_000, &output_l) {
                low_eligible += 1;
            }
        }

        assert!(
            high_eligible > low_eligible,
            "higher stake must be eligible more often: high={}, low={}",
            high_eligible,
            low_eligible
        );

        if low_eligible > 0 {
            let ratio = high_eligible as f64 / low_eligible as f64;
            assert!(
                ratio > 3.0 && ratio < 27.0,
                "stake ratio ~9:1 should produce eligibility ratio ~9:1, got {:.2}",
                ratio
            );
        }
    }

    #[test]
    fn select_proposer_from_single_candidate() {
        let kp = test_keypair(14);
        let validator = make_validator(&kp, 10_000_000);
        let selector = ProposerSelector::new(vec![validator.clone()]);

        let prev_hash = hash_bytes(b"anchor").0;
        let (output, proof) = ProposerSelector::compute_vrf(&kp, 1, &prev_hash).expect("ok");

        let candidates = vec![(validator.address, output, proof)];
        let winner = selector.select_proposer(&candidates);

        assert_eq!(winner, Some(validator.address), "single candidate must win");
    }

    #[test]
    fn select_proposer_empty_candidates_returns_none() {
        let kp = test_keypair(15);
        let validator = make_validator(&kp, 10_000_000);
        let selector = ProposerSelector::new(vec![validator]);

        let winner = selector.select_proposer(&[]);
        assert_eq!(winner, None, "empty candidates must return None");
    }

    #[test]
    fn select_proposer_deterministic() {
        let kp_a = test_keypair(16);
        let kp_b = test_keypair(17);
        let v_a = make_validator(&kp_a, 10_000_000);
        let v_b = make_validator(&kp_b, 10_000_000);
        let selector = ProposerSelector::new(vec![v_a.clone(), v_b.clone()]);

        let prev_hash = hash_bytes(b"anchor").0;
        let slot = 42;

        let (out_a, proof_a) = ProposerSelector::compute_vrf(&kp_a, slot, &prev_hash).expect("a");
        let (out_b, proof_b) = ProposerSelector::compute_vrf(&kp_b, slot, &prev_hash).expect("b");

        let candidates = vec![
            (v_a.address, out_a.clone(), proof_a.clone()),
            (v_b.address, out_b.clone(), proof_b.clone()),
        ];

        let winner1 = selector.select_proposer(&candidates);
        let winner2 = selector.select_proposer(&candidates);

        assert_eq!(winner1, winner2, "select_proposer must be deterministic");
    }

    #[test]
    fn select_proposer_verified_rejects_invalid_proofs() {
        let kp_valid = test_keypair(18);
        let kp_invalid = test_keypair(19);
        let v_valid = make_validator(&kp_valid, 10_000_000);
        let v_invalid = make_validator(&kp_invalid, 10_000_000);
        let selector = ProposerSelector::new(vec![v_valid.clone(), v_invalid.clone()]);

        let prev_hash = hash_bytes(b"anchor").0;
        let slot = 1;

        let (out_valid, proof_valid) =
            ProposerSelector::compute_vrf(&kp_valid, slot, &prev_hash).expect("ok");

        let (out_fake, proof_fake) =
            ProposerSelector::compute_vrf(&kp_invalid, slot, &prev_hash).expect("ok");

        let candidates = vec![
            (v_valid.address, out_valid, proof_valid),
            (v_valid.address, out_fake, proof_fake),
        ];

        let winner = selector.select_proposer_verified(slot, &prev_hash, &candidates);

        assert_eq!(winner, Some(v_valid.address));
    }

    #[test]
    fn below_threshold_zero_total_stake() {
        let output = VrfOutput { value: [0u8; 32] };
        assert!(
            !output.below_threshold(100, 0),
            "zero total_stake must always return false"
        );
    }

    #[test]
    fn below_threshold_full_stake() {
        let output = VrfOutput { value: [0xff; 32] };
        assert!(
            output.below_threshold(1_000_000, 1_000_000),
            "100% stake should always pass threshold"
        );
    }

    #[test]
    fn below_threshold_minimum_value_passes() {
        let output = VrfOutput { value: [0u8; 32] };
        assert!(
            output.below_threshold(1, 1_000_000),
            "VRF value of 0 must always pass threshold"
        );
    }

    #[test]
    fn weighted_score_higher_stake_lower_score() {
        let output = VrfOutput {
            value: {
                let mut v = [0u8; 32];
                v[0] = 0x80;
                v
            },
        };

        let score_high = output.weighted_score(100_000_000);
        let score_low = output.weighted_score(10_000_000);

        assert!(
            score_high < score_low,
            "higher stake must produce lower (better) score"
        );
    }

    #[test]
    fn weighted_score_zero_stake_returns_max() {
        let output = VrfOutput { value: [0x42; 32] };
        assert_eq!(
            output.weighted_score(0),
            u128::MAX,
            "zero stake must return MAX score"
        );
    }

    #[test]
    #[should_panic(expected = "at least one validator")]
    fn selector_panics_on_empty_validators() {
        ProposerSelector::new(vec![]);
    }

    #[test]
    fn selector_total_stake_computed_correctly() {
        let kp1 = test_keypair(20);
        let kp2 = test_keypair(21);
        let v1 = make_validator(&kp1, 5_000_000);
        let v2 = make_validator(&kp2, 15_000_000);
        let selector = ProposerSelector::new(vec![v1, v2]);
        assert_eq!(selector.total_stake(), 20_000_000);
        assert_eq!(selector.validator_count(), 2);
    }

    #[test]
    #[should_panic(expected = "duplicate validator address in proposer selector")]
    fn selector_rejects_duplicate_validator_identities() {
        let key = test_keypair(22);
        let validator = make_validator(&key, 5_000_000);
        ProposerSelector::new(vec![validator.clone(), validator]);
    }

    #[test]
    #[should_panic(expected = "total proposer stake exceeds u64::MAX")]
    fn selector_rejects_total_stake_overflow() {
        let first = make_validator(&test_keypair(23), u64::MAX);
        let second = make_validator(&test_keypair(24), 1);
        ProposerSelector::new(vec![first, second]);
    }

    #[test]
    fn end_to_end_proposer_election() {
        let kp1 = test_keypair(30);
        let kp2 = test_keypair(31);
        let kp3 = test_keypair(32);

        let v1 = make_validator(&kp1, 50_000_000);
        let v2 = make_validator(&kp2, 5_000_000);
        let v3 = make_validator(&kp3, 5_000_000);

        let selector = ProposerSelector::new(vec![v1.clone(), v2.clone(), v3.clone()]);
        let prev_hash = hash_bytes(b"genesis").0;

        let mut wins = [0u64; 3];
        let keypairs = [&kp1, &kp2, &kp3];
        let validators = [&v1, &v2, &v3];
        let num_slots = 1_000;

        for slot in 0..num_slots {
            let mut candidates = Vec::new();

            for (i, kp) in keypairs.iter().enumerate() {
                let (output, proof) =
                    ProposerSelector::compute_vrf(kp, slot, &prev_hash).expect("compute");

                assert!(
                    ProposerSelector::verify_vrf(
                        &validators[i].address,
                        slot,
                        &prev_hash,
                        &output,
                        &proof
                    ),
                    "proof must verify for validator {}",
                    i
                );

                candidates.push((validators[i].address, output, proof));
            }

            if let Some(winner) = selector.select_proposer(&candidates) {
                for (i, v) in validators.iter().enumerate() {
                    if winner == v.address {
                        wins[i] += 1;
                        break;
                    }
                }
            }
        }

        assert!(
            wins[0] > wins[1] && wins[0] > wins[2],
            "highest-stake validator should win most: wins={:?}",
            wins
        );

        let total: u64 = wins.iter().sum();
        assert_eq!(total, num_slots, "every slot must have exactly one winner");
    }

    #[test]
    fn vrf_output_serialization_roundtrip() {
        let kp = test_keypair(40);
        let prev_hash = hash_bytes(b"serde-test").0;
        let (output, _) = ProposerSelector::compute_vrf(&kp, 1, &prev_hash).expect("ok");

        let json = serde_json::to_string(&output).expect("serialize");
        let recovered: VrfOutput = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(output, recovered);
    }

    #[test]
    fn vrf_proof_serialization_roundtrip() {
        let kp = test_keypair(41);
        let prev_hash = hash_bytes(b"serde-test").0;
        let (output, proof) = ProposerSelector::compute_vrf(&kp, 1, &prev_hash).expect("ok");

        let json = serde_json::to_string(&proof).expect("serialize");
        let recovered: VrfProof = serde_json::from_str(&json).expect("deserialize");

        let address = kp.address();
        assert!(ProposerSelector::verify_vrf(
            &address, 1, &prev_hash, &output, &recovered
        ));
    }
}
