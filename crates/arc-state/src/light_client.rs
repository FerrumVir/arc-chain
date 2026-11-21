use arc_crypto::{Hash256, MerkleProof, MerkleTree, Signature as CryptoSignature, hash_bytes};
use arc_types::{Account, Address, BlockHeader, strict_supermajority_threshold};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct StateProof {
    pub account_address: Hash256,
    pub account: Account,
    pub merkle_proof: MerkleProof,
    pub block_height: u64,
    pub state_root: Hash256,
    pub timestamp: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct HeaderProof {
    pub header: BlockHeader,
    pub validator_signature: Option<Vec<u8>>,
    pub parent_hash: Hash256,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TxInclusionProof {
    pub tx_hash: Hash256,
    pub block_height: u64,
    pub merkle_proof: MerkleProof,
    pub block_tx_root: Hash256,
}

pub fn verify_finality_stake(signing_stake: u64, total_stake: u64) -> bool {
    total_stake > 0
        && signing_stake <= total_stake
        && signing_stake >= strict_supermajority_threshold(total_stake)
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LightSnapshot {
    pub height: u64,
    pub state_root: Hash256,
    pub account_count: u64,
    pub total_supply: u64,
    pub latest_block_hash: Hash256,
}

pub fn verify_state_proof(proof: &StateProof) -> bool {
    if proof.merkle_proof.root != proof.state_root {
        return false;
    }
    let Ok(account_bytes) = bincode::serialize(&proof.account) else {
        return false;
    };
    let expected_leaf = hash_bytes(&account_bytes);
    if proof.merkle_proof.leaf != expected_leaf {
        return false;
    }
    MerkleTree::verify_proof(&proof.merkle_proof)
}

pub fn verify_account_balance(proof: &StateProof, expected_balance: u64) -> bool {
    if !verify_state_proof(proof) {
        return false;
    }
    proof.account.balance == expected_balance
}

pub fn verify_account_exists(proof: &StateProof) -> bool {
    verify_state_proof(proof)
}

pub fn verify_header_chain(headers: &[HeaderProof]) -> bool {
    if headers.is_empty() {
        return true;
    }
    for window in headers.windows(2) {
        let parent = &window[0];
        let child = &window[1];
        let parent_block_hash = arc_types::Block::compute_hash(&parent.header);
        if child.parent_hash != parent_block_hash {
            return false;
        }
        if child.header.parent_hash != parent_block_hash {
            return false;
        }
    }
    true
}

pub fn verify_tx_inclusion(proof: &TxInclusionProof) -> bool {
    if proof.merkle_proof.root != proof.block_tx_root {
        return false;
    }
    if proof.merkle_proof.leaf != proof.tx_hash {
        return false;
    }
    MerkleTree::verify_proof(&proof.merkle_proof)
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ValidatorAttestation {
    pub validator_address: Address,
    pub stake: u64,
    pub signature_bytes: Vec<u8>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct TrustedValidator {
    pub validator_address: Address,
    pub stake: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FinalityProof {
    pub block_hash: Hash256,
    pub round: u64,
    pub block_height: u64,
    pub state_root: Hash256,
    pub quorum_signatures: Vec<ValidatorAttestation>,
    pub total_stake: u64,
    pub signing_stake: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LightClientBundle {
    pub finality_proof: FinalityProof,
    pub state_proof: StateProof,
    pub header_proof: HeaderProof,
}

fn checked_attestation_stake(attestations: &[ValidatorAttestation]) -> Option<u64> {
    let mut seen = HashSet::with_capacity(attestations.len());
    let mut total = 0u64;
    for attestation in attestations {
        if !seen.insert(attestation.validator_address) {
            return None;
        }
        total = total.checked_add(attestation.stake)?;
    }
    Some(total)
}

impl FinalityProof {
    pub fn signing_commitment(
        block_hash: &Hash256,
        round: u64,
        block_height: u64,
        state_root: &Hash256,
        total_stake: u64,
    ) -> Hash256 {
        let mut hasher = blake3::Hasher::new_derive_key("ARC-light-client-finality-v1");
        hasher.update(block_hash.as_ref());
        hasher.update(&round.to_le_bytes());
        hasher.update(&block_height.to_le_bytes());
        hasher.update(state_root.as_ref());
        hasher.update(&total_stake.to_le_bytes());
        Hash256(*hasher.finalize().as_bytes())
    }

    pub fn signing_hash(&self) -> Hash256 {
        Self::signing_commitment(
            &self.block_hash,
            self.round,
            self.block_height,
            &self.state_root,
            self.total_stake,
        )
    }

    pub fn new(
        block_hash: Hash256,
        round: u64,
        block_height: u64,
        state_root: Hash256,
        attestations: Vec<ValidatorAttestation>,
        total_stake: u64,
    ) -> Self {
        let signing_stake = checked_attestation_stake(&attestations)
            .expect("finality attestations contain duplicate identities or stake overflow");
        Self {
            block_hash,
            round,
            block_height,
            state_root,
            quorum_signatures: attestations,
            total_stake,
            signing_stake,
        }
    }

    pub fn supermajority_threshold(total_stake: u64) -> u64 {
        strict_supermajority_threshold(total_stake)
    }

    pub fn has_structural_quorum(&self) -> bool {
        verify_finality_stake(self.signing_stake, self.total_stake)
    }

    pub fn stake_percentage(&self) -> f64 {
        if self.total_stake == 0 {
            return 0.0;
        }
        (self.signing_stake as f64 / self.total_stake as f64) * 100.0
    }

    pub fn is_structurally_valid(&self) -> bool {
        if self.quorum_signatures.is_empty() {
            return false;
        }
        let Some(computed_stake) = checked_attestation_stake(&self.quorum_signatures) else {
            return false;
        };
        if computed_stake != self.signing_stake {
            return false;
        }
        self.has_structural_quorum()
    }

    pub fn verify_against_trusted_set(&self, trusted_validators: &[TrustedValidator]) -> bool {
        if !self.is_structurally_valid() || trusted_validators.is_empty() {
            return false;
        }

        let mut trusted = HashMap::with_capacity(trusted_validators.len());
        let mut trusted_total = 0u64;
        for validator in trusted_validators {
            if trusted
                .insert(validator.validator_address, validator.stake)
                .is_some()
            {
                return false;
            }
            let Some(total) = trusted_total.checked_add(validator.stake) else {
                return false;
            };
            trusted_total = total;
        }
        if trusted_total != self.total_stake {
            return false;
        }

        let signing_hash = self.signing_hash();
        for attestation in &self.quorum_signatures {
            if trusted.get(&attestation.validator_address) != Some(&attestation.stake) {
                return false;
            }
            if attestation.signature_bytes.len() > 8 * 1024 {
                return false;
            }
            let Ok(signature) =
                bincode::deserialize::<CryptoSignature>(&attestation.signature_bytes)
            else {
                return false;
            };
            if signature
                .verify(&signing_hash, &attestation.validator_address)
                .is_err()
            {
                return false;
            }
        }

        true
    }
}

impl LightClientBundle {
    pub fn new(finality: FinalityProof, state: StateProof, header: HeaderProof) -> Self {
        Self {
            finality_proof: finality,
            state_proof: state,
            header_proof: header,
        }
    }

    pub fn verify_complete(&self, trusted_validators: &[TrustedValidator]) -> bool {
        if !self
            .finality_proof
            .verify_against_trusted_set(trusted_validators)
        {
            return false;
        }

        if !verify_state_proof(&self.state_proof) {
            return false;
        }

        if self.finality_proof.state_root != self.state_proof.state_root {
            return false;
        }

        if self.header_proof.header.state_root != self.finality_proof.state_root {
            return false;
        }

        let header_hash = arc_types::Block::compute_hash(&self.header_proof.header);
        if self.finality_proof.block_hash != header_hash
            || self.header_proof.parent_hash != self.header_proof.header.parent_hash
            || self.finality_proof.block_height != self.state_proof.block_height
            || self.finality_proof.block_height != self.header_proof.header.height
            || self.state_proof.timestamp != self.header_proof.header.timestamp
        {
            return false;
        }

        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::StateDB;
    use arc_crypto::{KeyPair, hash_bytes};
    use arc_types::Transaction;

    fn addr(n: u8) -> Hash256 {
        hash_bytes(&[n])
    }

    #[test]
    fn test_state_proof_valid() {
        let state = StateDB::with_genesis(&[(addr(1), 1_000_000), (addr(2), 500)]);
        let tx = Transaction::new_transfer(addr(1), addr(2), 100, 0);
        state.execute_block(&[tx], addr(99)).unwrap();

        let proof = state.generate_state_proof(&addr(1)).unwrap();
        assert!(verify_state_proof(&proof));
        assert!(verify_account_exists(&proof));
        assert!(verify_account_balance(&proof, 999_900));
    }

    #[test]
    fn test_state_proof_wrong_balance() {
        let state = StateDB::with_genesis(&[(addr(1), 1_000_000)]);
        let tx = Transaction::new_transfer(addr(1), addr(2), 100, 0);
        state.execute_block(&[tx], addr(99)).unwrap();

        let proof = state.generate_state_proof(&addr(1)).unwrap();
        assert!(verify_state_proof(&proof));
        assert!(!verify_account_balance(&proof, 42));
    }

    #[test]
    fn test_state_proof_nonexistent() {
        let state = StateDB::with_genesis(&[(addr(1), 1_000_000)]);
        let tx = Transaction::new_transfer(addr(1), addr(2), 100, 0);
        state.execute_block(&[tx], addr(99)).unwrap();

        let result = state.generate_state_proof(&addr(50));
        assert!(result.is_err());
    }

    #[test]
    fn test_header_chain_valid() {
        let state = StateDB::with_genesis(&[(addr(1), u64::MAX)]);
        for i in 0..3u64 {
            let tx = Transaction::new_transfer(addr(1), addr(2), 1, i);
            state.execute_block(&[tx], addr(99)).unwrap();
        }

        let proofs: Vec<HeaderProof> = (1..=3)
            .map(|h| state.generate_header_proof(h).unwrap())
            .collect();
        assert!(verify_header_chain(&proofs));
    }

    #[test]
    fn test_header_chain_broken() {
        let state = StateDB::with_genesis(&[(addr(1), u64::MAX)]);
        for i in 0..3u64 {
            let tx = Transaction::new_transfer(addr(1), addr(2), 1, i);
            state.execute_block(&[tx], addr(99)).unwrap();
        }

        let mut proofs: Vec<HeaderProof> = (1..=3)
            .map(|h| state.generate_header_proof(h).unwrap())
            .collect();

        proofs[1].parent_hash = Hash256::ZERO;
        proofs[1].header.parent_hash = Hash256::ZERO;
        assert!(!verify_header_chain(&proofs));
    }

    #[test]
    fn test_tx_inclusion_proof() {
        let state = StateDB::with_genesis(&[(addr(1), 1_000_000)]);
        let tx = Transaction::new_transfer(addr(1), addr(2), 100, 0);
        let tx_hash = tx.hash;
        state.execute_block(&[tx], addr(99)).unwrap();

        let proof = state.generate_tx_inclusion_proof(&tx_hash).unwrap();
        assert!(verify_tx_inclusion(&proof));
    }

    #[test]
    fn test_light_snapshot() {
        let state = StateDB::with_genesis(&[(addr(1), 1_000_000), (addr(2), 500_000)]);
        let tx = Transaction::new_transfer(addr(1), addr(2), 100, 0);
        state.execute_block(&[tx], addr(99)).unwrap();

        let snap = state.generate_light_snapshot();
        assert_eq!(snap.height, 1);
        assert_eq!(snap.account_count, 2);
        assert_eq!(snap.total_supply, 1_500_000);
        assert_ne!(snap.state_root, Hash256::ZERO);
        assert_ne!(snap.latest_block_hash, Hash256::ZERO);
    }

    #[test]
    fn test_state_root_changes() {
        let state = StateDB::with_genesis(&[(addr(1), 1_000_000), (addr(2), 0)]);
        let root_before = state.get_state_root();

        let tx = Transaction::new_transfer(addr(1), addr(2), 500, 0);
        state.execute_block(&[tx], addr(99)).unwrap();

        let root_after = state.get_state_root();
        assert_ne!(
            root_before, root_after,
            "state root must change after executing transactions"
        );
    }

    struct TestCommittee {
        keys: Vec<KeyPair>,
        trusted: Vec<TrustedValidator>,
    }

    fn make_committee(stakes: &[u64]) -> TestCommittee {
        let keys: Vec<KeyPair> = stakes.iter().map(|_| KeyPair::generate_ed25519()).collect();
        let trusted = keys
            .iter()
            .zip(stakes)
            .map(|(key, &stake)| TrustedValidator {
                validator_address: key.address(),
                stake,
            })
            .collect();
        TestCommittee { keys, trusted }
    }

    fn make_attestations(
        block_hash: &Hash256,
        round: u64,
        block_height: u64,
        state_root: &Hash256,
        total_stake: u64,
        committee: &TestCommittee,
        signer_indices: &[usize],
    ) -> Vec<ValidatorAttestation> {
        let signing_hash = FinalityProof::signing_commitment(
            block_hash,
            round,
            block_height,
            state_root,
            total_stake,
        );
        signer_indices
            .iter()
            .map(|&index| {
                let key = &committee.keys[index];
                let signature = key.sign(&signing_hash).unwrap();
                ValidatorAttestation {
                    validator_address: key.address(),
                    stake: committee.trusted[index].stake,
                    signature_bytes: bincode::serialize(&signature).unwrap(),
                }
            })
            .collect()
    }

    #[test]
    fn test_finality_proof_quorum_sufficient() {
        let block_hash = hash_bytes(b"block");
        let state_root = hash_bytes(b"state");
        let committee = make_committee(&[30, 20, 20, 30]);
        let attestations =
            make_attestations(&block_hash, 1, 10, &state_root, 100, &committee, &[0, 1, 2]);
        let proof = FinalityProof::new(block_hash, 1, 10, state_root, attestations, 100);
        assert!(proof.has_structural_quorum());
        assert!(proof.is_structurally_valid());
        assert!(proof.verify_against_trusted_set(&committee.trusted));
    }

    #[test]
    fn test_finality_proof_quorum_insufficient() {
        let block_hash = hash_bytes(b"block");
        let state_root = hash_bytes(b"state");
        let committee = make_committee(&[30, 30, 40]);
        let attestations =
            make_attestations(&block_hash, 1, 10, &state_root, 100, &committee, &[0, 1]);
        let proof = FinalityProof::new(block_hash, 1, 10, state_root, attestations, 100);
        assert!(!proof.has_structural_quorum());
        assert!(!proof.is_structurally_valid());
        assert!(!proof.verify_against_trusted_set(&committee.trusted));
    }

    #[test]
    fn test_finality_proof_exact_threshold() {
        let block_hash = hash_bytes(b"block");
        let state_root = hash_bytes(b"state");
        let committee = make_committee(&[34, 33, 33]);
        let attestations =
            make_attestations(&block_hash, 1, 10, &state_root, 100, &committee, &[0, 1]);
        let proof = FinalityProof::new(block_hash, 1, 10, state_root, attestations, 100);
        assert_eq!(proof.signing_stake, 67);
        assert!(proof.has_structural_quorum());
        assert!(proof.verify_against_trusted_set(&committee.trusted));
    }

    #[test]
    fn test_finality_proof_stake_percentage() {
        let block_hash = hash_bytes(b"block");
        let state_root = hash_bytes(b"state");
        let committee = make_committee(&[25, 25, 25, 25]);
        let attestations =
            make_attestations(&block_hash, 1, 10, &state_root, 100, &committee, &[0, 1, 2]);
        let proof = FinalityProof::new(block_hash, 1, 10, state_root, attestations, 100);
        let pct = proof.stake_percentage();
        assert!(
            (pct - 75.0).abs() < f64::EPSILON,
            "stake percentage should be 75.0, got {pct}"
        );
    }

    #[test]
    fn test_finality_proof_empty_attestations() {
        let proof = FinalityProof::new(
            hash_bytes(b"block"),
            1,
            10,
            hash_bytes(b"state"),
            vec![],
            100,
        );
        assert!(!proof.is_structurally_valid());
        assert!(!proof.has_structural_quorum());
    }

    #[test]
    fn test_supermajority_threshold_calculation() {
        assert_eq!(FinalityProof::supermajority_threshold(0), 1);
        assert_eq!(FinalityProof::supermajority_threshold(1), 1);
        assert_eq!(FinalityProof::supermajority_threshold(3), 3);
        assert_eq!(FinalityProof::supermajority_threshold(10), 7);
        assert_eq!(FinalityProof::supermajority_threshold(100), 67);
        assert_eq!(
            FinalityProof::supermajority_threshold(u64::MAX),
            12_297_829_382_473_034_411
        );
    }

    #[test]
    fn test_six_equal_validators_require_five_attestations() {
        let block_hash = hash_bytes(b"six-validator-block");
        let state_root = hash_bytes(b"state");
        let committee = make_committee(&[5, 5, 5, 5, 5, 5]);

        let four = FinalityProof::new(
            block_hash,
            1,
            10,
            state_root,
            make_attestations(
                &block_hash,
                1,
                10,
                &state_root,
                30,
                &committee,
                &[0, 1, 2, 3],
            ),
            30,
        );
        assert!(!four.has_structural_quorum());
        assert!(!four.verify_against_trusted_set(&committee.trusted));

        let five = FinalityProof::new(
            block_hash,
            1,
            10,
            state_root,
            make_attestations(
                &block_hash,
                1,
                10,
                &state_root,
                30,
                &committee,
                &[0, 1, 2, 3, 4],
            ),
            30,
        );
        assert!(five.verify_against_trusted_set(&committee.trusted));
    }

    #[test]
    fn test_finality_rejects_duplicate_forged_stake_and_invalid_signature() {
        let block_hash = hash_bytes(b"adversarial-block");
        let state_root = hash_bytes(b"state");
        let committee = make_committee(&[5, 5, 5, 5, 5, 5]);
        let mut proof = FinalityProof::new(
            block_hash,
            1,
            10,
            state_root,
            make_attestations(
                &block_hash,
                1,
                10,
                &state_root,
                30,
                &committee,
                &[0, 1, 2, 3, 4],
            ),
            30,
        );

        let mut duplicate = proof.clone();
        duplicate
            .quorum_signatures
            .push(duplicate.quorum_signatures[0].clone());
        duplicate.signing_stake += 5;
        assert!(!duplicate.is_structurally_valid());
        assert!(!duplicate.verify_against_trusted_set(&committee.trusted));

        let mut forged_stake = proof.clone();
        forged_stake.quorum_signatures[0].stake = 10;
        forged_stake.signing_stake = 30;
        assert!(forged_stake.is_structurally_valid());
        assert!(!forged_stake.verify_against_trusted_set(&committee.trusted));

        let metadata_mutations: [fn(&mut FinalityProof); 3] = [
            |candidate: &mut FinalityProof| candidate.round += 1,
            |candidate: &mut FinalityProof| candidate.block_height += 1,
            |candidate: &mut FinalityProof| candidate.state_root = hash_bytes(b"forged-root"),
        ];
        for mutate in metadata_mutations {
            let mut altered_metadata = proof.clone();
            mutate(&mut altered_metadata);
            assert!(altered_metadata.is_structurally_valid());
            assert!(
                !altered_metadata.verify_against_trusted_set(&committee.trusted),
                "signatures must bind round, height, and state root"
            );
        }

        let last = proof.quorum_signatures[0].signature_bytes.len() - 1;
        proof.quorum_signatures[0].signature_bytes[last] ^= 1;
        assert!(!proof.verify_against_trusted_set(&committee.trusted));
    }

    #[test]
    #[should_panic(expected = "duplicate identities or stake overflow")]
    fn test_finality_constructor_rejects_attestation_stake_overflow() {
        let block_hash = hash_bytes(b"overflow-block");
        let state_root = hash_bytes(b"state");
        let committee = make_committee(&[u64::MAX, 1]);
        FinalityProof::new(
            block_hash,
            1,
            10,
            state_root,
            make_attestations(
                &block_hash,
                1,
                10,
                &state_root,
                u64::MAX,
                &committee,
                &[0, 1],
            ),
            u64::MAX,
        );
    }

    #[test]
    fn test_light_client_bundle_consistent() {
        let state = StateDB::with_genesis(&[(addr(1), 1_000_000), (addr(2), 500)]);
        let tx = Transaction::new_transfer(addr(1), addr(2), 100, 0);
        state.execute_block(&[tx], addr(99)).unwrap();

        let state_proof = state.generate_state_proof(&addr(1)).unwrap();
        let header_proof = state.generate_header_proof(1).unwrap();
        let block_hash = arc_types::Block::compute_hash(&header_proof.header);

        let committee = make_committee(&[40, 40, 20]);
        let attestations = make_attestations(
            &block_hash,
            1,
            state_proof.block_height,
            &state_proof.state_root,
            100,
            &committee,
            &[0, 1],
        );
        let finality = FinalityProof::new(
            block_hash,
            1,
            state_proof.block_height,
            state_proof.state_root,
            attestations,
            100,
        );

        let bundle = LightClientBundle::new(finality, state_proof, header_proof);
        assert!(
            bundle.verify_complete(&committee.trusted),
            "bundle with consistent hashes should verify"
        );
    }

    #[test]
    fn test_light_client_bundle_mismatched() {
        let state = StateDB::with_genesis(&[(addr(1), 1_000_000), (addr(2), 500)]);
        let tx = Transaction::new_transfer(addr(1), addr(2), 100, 0);
        state.execute_block(&[tx], addr(99)).unwrap();

        let state_proof = state.generate_state_proof(&addr(1)).unwrap();
        let header_proof = state.generate_header_proof(1).unwrap();
        let block_hash = arc_types::Block::compute_hash(&header_proof.header);

        let wrong_state_root = hash_bytes(b"wrong-state-root");
        let committee = make_committee(&[40, 40, 20]);
        let attestations = make_attestations(
            &block_hash,
            1,
            state_proof.block_height,
            &wrong_state_root,
            100,
            &committee,
            &[0, 1],
        );
        let finality = FinalityProof::new(
            block_hash,
            1,
            state_proof.block_height,
            wrong_state_root,
            attestations,
            100,
        );

        let bundle = LightClientBundle::new(finality, state_proof, header_proof);
        assert!(
            !bundle.verify_complete(&committee.trusted),
            "bundle with mismatched state roots should fail"
        );
    }
}
