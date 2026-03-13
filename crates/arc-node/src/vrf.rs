// Add to lib.rs: pub mod vrf;

//! VRF-based proposer selection for ARC Chain.
//!
//! Implements a Verifiable Random Function (VRF) leader election system
//! inspired by ECVRF-ED25519-SHA512-TAI (RFC 9381), adapted to use the
//! ARC chain's existing Ed25519 + BLAKE3 VRF primitives from `arc-crypto`.
//!
//! # How it works
//!
//! Each slot, every validator computes a VRF output using their secret key
//! and a deterministic input derived from `(slot_number, previous_block_hash)`.
//! The VRF output is a pseudorandom 32-byte value that:
//!
//! 1. **Unpredictable** — nobody knows who the proposer is until they reveal
//!    their VRF proof, because the output depends on the validator's secret key.
//! 2. **Ungameable** — the VRF input includes `prev_block_hash` which is already
//!    committed on-chain; the current proposer cannot manipulate it.
//! 3. **Verifiable** — any node can check the VRF proof to confirm the proposer
//!    was legitimately selected without knowing their secret key.
//! 4. **Stake-weighted** — validators with higher stake have a proportionally
//!    higher probability of being selected as proposer.
//!
//! # Proposer selection
//!
//! Two complementary mechanisms:
//!
//! - **Threshold check** (`is_proposer`): Each validator independently checks
//!   whether their VRF output falls below a stake-weighted threshold. This is
//!   used for the "am I eligible?" fast check before broadcasting.
//!
//! - **Sortition** (`select_proposer`): Given all valid VRF submissions for a
//!   slot, the validator with the lowest stake-weighted VRF hash wins. This
//!   provides a deterministic tiebreaker when multiple validators pass the
//!   threshold.
//!
//! # Integration
//!
//! This module is self-contained and will be wired into `ConsensusManager`
//! in a future change. It uses:
//! - `arc_crypto::vrf::{vrf_prove, vrf_verify}` for the core VRF operations
//! - `arc_crypto::KeyPair` for Ed25519 signing
//! - `arc_crypto::Hash256` as the address / hash type

use arc_crypto::hash::Hash256;
use arc_crypto::signature::{KeyPair, SignatureError};
use arc_crypto::vrf::{vrf_prove, vrf_verify, VrfProof as CryptoVrfProof};
use serde::{Deserialize, Serialize};

// ── Constants ────────────────────────────────────────────────────────────────

/// Domain separation tag for VRF slot inputs.
const VRF_SLOT_DOMAIN: &str = "ARC-vrf-slot-input-v1";

/// Default proposer threshold ratio numerator.
/// With denominator = total_stake, a validator with stake S is eligible when
/// their VRF output (interpreted as a 256-bit integer) < (S * U256::MAX / total_stake).
/// This gives each validator a selection probability proportional to their
/// stake fraction per slot.
///
/// We set an additional scaling factor so that on average ~1 proposer is
/// elected per slot across the entire validator set. Since each validator's
/// probability = stake / total_stake, the sum across all validators = 1.0,
/// meaning on average exactly one proposer per slot.
///
/// If we want multiple proposers possible per slot (for redundancy), scale
/// the threshold up. `EXPECTED_PROPOSERS_PER_SLOT = 1` means exactly one
/// expected proposer per slot on average.
const EXPECTED_PROPOSERS_PER_SLOT: u64 = 1;

// ── Types ────────────────────────────────────────────────────────────────────

/// VRF output — the pseudorandom value derived from the proof.
///
/// This wraps the `arc_crypto::VrfOutput` with a convenience interface
/// for proposer selection arithmetic.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct VrfOutput {
    /// The 32-byte pseudorandom hash output.
    pub value: [u8; 32],
}

impl VrfOutput {
    /// Interpret the VRF output as a big-endian unsigned 256-bit integer
    /// and check whether it falls below a threshold.
    ///
    /// We compare using a ratio to avoid big-integer arithmetic:
    /// `value < (validator_stake / total_stake) * 2^256`
    ///
    /// Equivalent to: `value * total_stake < validator_stake * 2^256`
    /// Since we can't represent 2^256, we compare the first 8 bytes
    /// (64-bit truncation) which gives sufficient granularity for
    /// practical stake distributions.
    fn below_threshold(&self, validator_stake: u64, total_stake: u64) -> bool {
        if total_stake == 0 {
            return false;
        }
        // Use the first 8 bytes of the VRF output as a u64.
        // This gives us uniform randomness in [0, u64::MAX].
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

        // Threshold: (validator_stake * EXPECTED_PROPOSERS_PER_SLOT * u64::MAX) / total_stake
        // To avoid overflow, compute: vrf_value / u64::MAX <= stake / total_stake
        // Equivalent: vrf_value * total_stake <= stake * u64::MAX (in u128)
        // We use <= so a validator with 100% stake is always eligible (boundary-inclusive).
        let lhs = (vrf_value as u128) * (total_stake as u128);
        let rhs =
            (validator_stake as u128) * (EXPECTED_PROPOSERS_PER_SLOT as u128) * (u64::MAX as u128);
        lhs <= rhs
    }

    /// Compute a stake-weighted score for sortition ranking.
    ///
    /// Lower score = better candidate. The score is computed as:
    /// `vrf_output_u64 / validator_stake`
    ///
    /// This means higher-stake validators need a smaller VRF output
    /// to achieve the same score, giving them proportionally better
    /// odds in the sortition.
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
        // Use u128 to avoid any overflow concerns.
        // Multiply by a large constant to preserve precision when dividing.
        (vrf_value as u128) * 1_000_000_000 / (validator_stake as u128)
    }
}

/// VRF proof — proves the output was correctly computed by the secret key holder.
///
/// Wraps the `arc_crypto::VrfProof` which contains the intermediate gamma value
/// and an Ed25519 signature binding gamma to the VRF input.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct VrfProof {
    /// The underlying cryptographic VRF proof from arc-crypto.
    pub inner: CryptoVrfProof,
}

/// Information about a validator for proposer selection.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ValidatorInfo {
    /// Ed25519 public key (32 bytes).
    pub public_key: [u8; 32],
    /// Amount of ARC tokens staked.
    pub stake: u64,
    /// Validator's on-chain address (BLAKE3 hash of public key).
    pub address: Hash256,
}

/// Address type alias matching arc-types convention.
pub type Address = Hash256;

/// Proposer selection engine based on VRF outputs.
///
/// Holds the current validator set and provides methods for computing,
/// verifying, and evaluating VRF-based proposer eligibility.
pub struct ProposerSelector {
    /// Current validator set with stakes and public keys.
    validators: Vec<ValidatorInfo>,
    /// Sum of all validator stakes (cached for threshold computation).
    total_stake: u64,
}

impl ProposerSelector {
    /// Create a new proposer selector with the given validator set.
    ///
    /// # Arguments
    /// * `validators` — The active validator set for the current epoch.
    ///
    /// # Panics
    /// Panics if `validators` is empty (there must be at least one validator).
    pub fn new(validators: Vec<ValidatorInfo>) -> Self {
        assert!(
            !validators.is_empty(),
            "ProposerSelector requires at least one validator"
        );
        let total_stake = validators.iter().map(|v| v.stake).sum();
        Self {
            validators,
            total_stake,
        }
    }

    /// Get the total stake across all validators.
    pub fn total_stake(&self) -> u64 {
        self.total_stake
    }

    /// Get the number of validators.
    pub fn validator_count(&self) -> usize {
        self.validators.len()
    }

    /// Look up a validator by address.
    pub fn get_validator(&self, address: &Hash256) -> Option<&ValidatorInfo> {
        self.validators.iter().find(|v| v.address == *address)
    }

    /// Compute the VRF input for a given slot.
    ///
    /// The input is a domain-separated BLAKE3 hash of:
    /// `BLAKE3_derive_key("ARC-vrf-slot-input-v1", slot || prev_hash)`
    ///
    /// This ensures:
    /// - Different slots produce different VRF inputs (prevents replay).
    /// - The previous block hash anchors the randomness to committed state.
    /// - Domain separation prevents cross-protocol attacks.
    fn slot_vrf_input(slot: u64, prev_hash: &[u8; 32]) -> Vec<u8> {
        let mut hasher = blake3::Hasher::new_derive_key(VRF_SLOT_DOMAIN);
        hasher.update(&slot.to_le_bytes());
        hasher.update(prev_hash);
        hasher.finalize().as_bytes().to_vec()
    }

    /// Compute the VRF output and proof for a given slot.
    ///
    /// Each validator calls this with their own keypair to produce a
    /// VRF output that determines their proposer eligibility for this slot.
    ///
    /// # Arguments
    /// * `keypair` — The validator's Ed25519 keypair.
    /// * `slot` — The slot number to compute the VRF for.
    /// * `prev_hash` — Hash of the previous block (anchors randomness).
    ///
    /// # Returns
    /// `(VrfOutput, VrfProof)` — The pseudorandom output and its proof.
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

    /// Verify a VRF proof from another validator.
    ///
    /// Checks that the proof is valid for the given public key and slot input,
    /// and that the claimed output matches the proof.
    ///
    /// # Arguments
    /// * `validator_address` — The ARC address of the validator who produced the proof.
    /// * `slot` — The slot number the proof claims to be for.
    /// * `prev_hash` — The previous block hash used as VRF input.
    /// * `output` — The claimed VRF output to verify.
    /// * `proof` — The VRF proof to verify.
    ///
    /// # Returns
    /// `true` if the proof is valid and the output matches, `false` otherwise.
    pub fn verify_vrf(
        validator_address: &Hash256,
        slot: u64,
        prev_hash: &[u8; 32],
        output: &VrfOutput,
        proof: &VrfProof,
    ) -> bool {
        let alpha = Self::slot_vrf_input(slot, prev_hash);

        // Verify the cryptographic proof and get the expected output.
        match vrf_verify(validator_address, &alpha, &proof.inner) {
            Ok(verified_output) => {
                // The verified output must match the claimed output.
                *verified_output.0.as_bytes() == output.value
            }
            Err(_) => false,
        }
    }

    /// Check if a validator is eligible to propose for this slot.
    ///
    /// Uses a stake-weighted threshold: the probability of being selected
    /// is proportional to `validator_stake / total_stake`. On average,
    /// `EXPECTED_PROPOSERS_PER_SLOT` validators will pass this check
    /// across the entire validator set.
    ///
    /// # Arguments
    /// * `validator_stake` — The stake of the validator to check.
    /// * `vrf_output` — The validator's VRF output for this slot.
    ///
    /// # Returns
    /// `true` if the validator's VRF output falls below the stake-weighted
    /// threshold, making them eligible to propose.
    pub fn is_proposer(&self, validator_stake: u64, vrf_output: &VrfOutput) -> bool {
        vrf_output.below_threshold(validator_stake, self.total_stake)
    }

    /// Select the winning proposer from a set of valid VRF candidates.
    ///
    /// All candidates must have already been verified (valid proofs).
    /// The validator with the lowest stake-weighted VRF score wins.
    /// This provides a deterministic, ungameable tiebreaker when multiple
    /// validators pass the threshold check.
    ///
    /// # Arguments
    /// * `candidates` — Tuples of (address, vrf_output, vrf_proof) for each
    ///   candidate who passed the threshold check and has a valid proof.
    ///
    /// # Returns
    /// The address of the winning proposer, or `None` if no valid candidates.
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
            // Look up the validator's stake for weighted scoring.
            let stake = match self.get_validator(address) {
                Some(v) => v.stake,
                None => continue, // Skip unknown validators.
            };

            let score = vrf_output.weighted_score(stake);
            if score < best_score {
                best_score = score;
                best_address = Some(*address);
            }
        }

        best_address
    }

    /// Select the proposer for a slot, verifying all proofs in the process.
    ///
    /// This is the full pipeline: verify each candidate's VRF proof,
    /// filter to eligible proposers, and select the winner.
    ///
    /// # Arguments
    /// * `slot` — The slot number.
    /// * `prev_hash` — The previous block hash.
    /// * `candidates` — Tuples of (address, vrf_output, vrf_proof).
    ///
    /// # Returns
    /// The address of the winning proposer, or `None` if no valid candidates.
    pub fn select_proposer_verified(
        &self,
        slot: u64,
        prev_hash: &[u8; 32],
        candidates: &[(Address, VrfOutput, VrfProof)],
    ) -> Option<Address> {
        // Filter to only candidates with valid proofs.
        let valid_candidates: Vec<(Address, VrfOutput, VrfProof)> = candidates
            .iter()
            .filter(|(addr, output, proof)| Self::verify_vrf(addr, slot, prev_hash, output, proof))
            .cloned()
            .collect();

        self.select_proposer(&valid_candidates)
    }
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use arc_crypto::hash::hash_bytes;
    use arc_crypto::signature::KeyPair;

    /// Helper: create a ValidatorInfo from a keypair and stake.
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

    /// Helper: create a deterministic keypair from a seed index.
    fn test_keypair(index: u8) -> KeyPair {
        let seed = blake3::derive_key("ARC-test-vrf-keypair-v1", &[index]);
        let signing_key = ed25519_dalek::SigningKey::from_bytes(&seed);
        KeyPair::Ed25519(signing_key)
    }

    // ── Basic VRF computation and verification ──

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

        // Selector should exist (sanity check).
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

    // ── Verification failures ──

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

        // Tamper with the output value.
        output.value[0] ^= 0xff;

        assert!(
            !ProposerSelector::verify_vrf(&validator.address, 1, &prev_hash, &output, &proof),
            "must fail with tampered output"
        );
    }

    // ── Threshold (is_proposer) tests ──

    #[test]
    fn sole_validator_always_proposer() {
        // A single validator with 100% of stake should always be the proposer.
        let kp = test_keypair(10);
        let validator = make_validator(&kp, 10_000_000);
        let selector = ProposerSelector::new(vec![validator]);

        let prev_hash = hash_bytes(b"anchor").0;

        // Test across many slots — a sole validator should always be eligible.
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
        // Create validators with different stakes and verify that higher
        // stake correlates with higher selection probability.
        let kp_high = test_keypair(12);
        let kp_low = test_keypair(13);

        let v_high = make_validator(&kp_high, 90_000_000); // 90% of stake
        let v_low = make_validator(&kp_low, 10_000_000); // 10% of stake

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

        // With 90% stake, high should be eligible ~9x more often than low.
        // Allow generous bounds for statistical variance.
        assert!(
            high_eligible > low_eligible,
            "higher stake must be eligible more often: high={}, low={}",
            high_eligible,
            low_eligible
        );

        // The ratio should be roughly 9:1. Allow 3:1 to 27:1 for variance.
        if low_eligible > 0 {
            let ratio = high_eligible as f64 / low_eligible as f64;
            assert!(
                ratio > 3.0 && ratio < 27.0,
                "stake ratio ~9:1 should produce eligibility ratio ~9:1, got {:.2}",
                ratio
            );
        }
    }

    // ── Proposer selection (sortition) tests ──

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

        // Create an invalid proof: use kp_invalid's proof but claim it's from v_valid.
        let (out_fake, proof_fake) =
            ProposerSelector::compute_vrf(&kp_invalid, slot, &prev_hash).expect("ok");

        // Candidate 0: valid proof for v_valid
        // Candidate 1: invalid — proof from kp_invalid but address of v_valid
        let candidates = vec![
            (v_valid.address, out_valid, proof_valid),
            (v_valid.address, out_fake, proof_fake), // address mismatch
        ];

        let winner = selector.select_proposer_verified(slot, &prev_hash, &candidates);

        // Only the first candidate has a valid proof.
        assert_eq!(winner, Some(v_valid.address));
    }

    // ── VrfOutput arithmetic tests ──

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
        // When validator has 100% of stake, any VRF value should pass.
        let output = VrfOutput {
            value: [0xff; 32], // Maximum possible value
        };
        // validator_stake == total_stake → always eligible
        assert!(
            output.below_threshold(1_000_000, 1_000_000),
            "100% stake should always pass threshold"
        );
    }

    #[test]
    fn below_threshold_minimum_value_passes() {
        let output = VrfOutput { value: [0u8; 32] };
        // Zero VRF value should always pass any non-zero threshold.
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
                v[0] = 0x80; // Mid-range value
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

    // ── ProposerSelector constructor tests ──

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

    // ── Full end-to-end test ──

    #[test]
    fn end_to_end_proposer_election() {
        // Simulate a full proposer election across 3 validators.
        let kp1 = test_keypair(30);
        let kp2 = test_keypair(31);
        let kp3 = test_keypair(32);

        let v1 = make_validator(&kp1, 50_000_000); // Core tier
        let v2 = make_validator(&kp2, 5_000_000); // Arc tier
        let v3 = make_validator(&kp3, 5_000_000); // Arc tier

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

                // Verify the proof.
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

        // v1 has 50M / 60M = ~83% of stake, should win most often.
        // v2 and v3 each have 5M / 60M = ~8.3%.
        assert!(
            wins[0] > wins[1] && wins[0] > wins[2],
            "highest-stake validator should win most: wins={:?}",
            wins
        );

        // Total wins should equal total slots (always one winner from 3 candidates).
        let total: u64 = wins.iter().sum();
        assert_eq!(total, num_slots, "every slot must have exactly one winner");
    }

    // ── Serialization roundtrip ──

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

        // Verify the deserialized proof still works.
        let address = kp.address();
        assert!(ProposerSelector::verify_vrf(
            &address, 1, &prev_hash, &output, &recovered
        ));
    }
}
