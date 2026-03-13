//! Light client proofs — allow verification of state without downloading the full chain.
//!
//! Provides:
//! - `StateProof` — prove an account's state against a Merkle state root
//! - `HeaderProof` — verify a chain of block headers by parent-hash linkage
//! - `TxInclusionProof` — prove a transaction was included in a specific block
//! - `LightSnapshot` — compact aggregate view for light client bootstrapping
//! - `FinalityProof` — prove a block was finalized by a consensus quorum
//! - `LightClientBundle` — complete verification bundle (finality + state + header)
//!
//! All verification functions are pure (no StateDB required), so light clients
//! can run them with only the proof data and a trusted state root.

use arc_crypto::{Hash256, MerkleProof, MerkleTree, hash_bytes};
use arc_types::{Account, Address, BlockHeader};
use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Proof types
// ---------------------------------------------------------------------------

/// Proof that a specific account exists with a given state at a particular block height.
///
/// A light client can verify this against a trusted `state_root` without
/// downloading the full account set.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct StateProof {
    /// Address of the account being proved.
    pub account_address: Hash256,
    /// The full account state (balance, nonce, code_hash, etc.).
    pub account: Account,
    /// Merkle inclusion proof from the account leaf to the state root.
    pub merkle_proof: MerkleProof,
    /// Block height at which this proof was generated.
    pub block_height: u64,
    /// State root that the Merkle proof resolves to.
    pub state_root: Hash256,
    /// Timestamp of the block (unix millis).
    pub timestamp: u64,
}

/// A block header annotated with optional validator signature and explicit
/// parent hash — used by light clients to verify header-chain continuity.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct HeaderProof {
    /// The block header.
    pub header: BlockHeader,
    /// Validator signature over the header (placeholder — not yet enforced).
    pub validator_signature: Option<Vec<u8>>,
    /// Hash of the parent block header (convenience duplicate of `header.parent_hash`).
    pub parent_hash: Hash256,
}

/// Proof that a transaction was included in a specific block's transaction tree.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TxInclusionProof {
    /// Hash of the transaction being proved.
    pub tx_hash: Hash256,
    /// Block height the transaction was included in.
    pub block_height: u64,
    /// Merkle inclusion proof within the block's transaction tree.
    pub merkle_proof: MerkleProof,
    /// Transaction Merkle root stored in the block header.
    pub block_tx_root: Hash256,
}

/// Verify finality by checking that signing stake >= 2/3 of total stake.
/// This is a pure function — light clients can call it with raw values
/// obtained from a FinalityProof (defined in arc-consensus).
pub fn verify_finality_stake(signing_stake: u64, total_stake: u64) -> bool {
    let quorum = (2 * total_stake + 2) / 3;
    signing_stake >= quorum
}

/// Compact aggregate snapshot for light client bootstrapping.
///
/// Gives a light client enough context to know the chain tip without
/// downloading blocks or accounts.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LightSnapshot {
    /// Current chain height.
    pub height: u64,
    /// Merkle root of the current account state.
    pub state_root: Hash256,
    /// Total number of accounts.
    pub account_count: u64,
    /// Sum of all account balances (total supply in circulation).
    pub total_supply: u64,
    /// Hash of the latest committed block.
    pub latest_block_hash: Hash256,
}

// ---------------------------------------------------------------------------
// Pure verification functions (no StateDB dependency)
// ---------------------------------------------------------------------------

/// Verify that a `StateProof`'s Merkle path resolves to the embedded state root.
///
/// This confirms the account leaf is a member of the state tree that produced
/// `proof.state_root`. It does NOT verify that `state_root` itself is trusted —
/// that responsibility falls on the light client (e.g. via header chain).
pub fn verify_state_proof(proof: &StateProof) -> bool {
    // The proof's root must match the declared state root.
    if proof.merkle_proof.root != proof.state_root {
        return false;
    }
    // The leaf in the proof must be the hash of the serialised account.
    let account_bytes = bincode::serialize(&proof.account).expect("serializable account");
    let expected_leaf = hash_bytes(&account_bytes);
    if proof.merkle_proof.leaf != expected_leaf {
        return false;
    }
    MerkleTree::verify_proof(&proof.merkle_proof)
}

/// Verify a state proof AND check that the account's balance matches `expected_balance`.
pub fn verify_account_balance(proof: &StateProof, expected_balance: u64) -> bool {
    if !verify_state_proof(proof) {
        return false;
    }
    proof.account.balance == expected_balance
}

/// Verify a state proof — returns `true` if the Merkle proof is valid, which
/// inherently proves the account exists in the state tree.
pub fn verify_account_exists(proof: &StateProof) -> bool {
    verify_state_proof(proof)
}

/// Verify that a sequence of `HeaderProof`s form a valid parent-hash chain.
///
/// Each header (except the first) must reference the previous header's computed
/// block hash as its `parent_hash`.
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
        // Also verify the child header's own parent_hash field is consistent.
        if child.header.parent_hash != parent_block_hash {
            return false;
        }
    }
    true
}

/// Verify that a transaction was included in a block by checking its Merkle
/// inclusion proof against the declared `block_tx_root`.
pub fn verify_tx_inclusion(proof: &TxInclusionProof) -> bool {
    if proof.merkle_proof.root != proof.block_tx_root {
        return false;
    }
    if proof.merkle_proof.leaf != proof.tx_hash {
        return false;
    }
    MerkleTree::verify_proof(&proof.merkle_proof)
}

// ---------------------------------------------------------------------------
// Finality proof types
// ---------------------------------------------------------------------------

/// A single validator's attestation to a finalized block.
///
/// Each attestation records the validator's address, their stake weight,
/// and a serialized signature over the block hash.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ValidatorAttestation {
    /// Address (public key hash) of the attesting validator.
    pub validator_address: Address,
    /// Stake weight of this validator at the time of attestation.
    pub stake: u64,
    /// Serialized signature bytes over the block hash.
    pub signature_bytes: Vec<u8>,
}

/// Proof that a block was finalized by a consensus quorum.
///
/// Contains the block metadata plus a set of validator attestations whose
/// aggregate stake must meet the supermajority threshold (>= 2/3 + 1 of
/// total stake) for the proof to be considered valid.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FinalityProof {
    /// Hash of the finalized block.
    pub block_hash: Hash256,
    /// Consensus round in which finality was reached.
    pub round: u64,
    /// Height of the finalized block.
    pub block_height: u64,
    /// State root committed by this block.
    pub state_root: Hash256,
    /// Individual validator attestations forming the quorum.
    pub quorum_signatures: Vec<ValidatorAttestation>,
    /// Total stake across all validators in the active set.
    pub total_stake: u64,
    /// Sum of stake from validators that signed (cached from attestations).
    pub signing_stake: u64,
}

/// Complete light client verification bundle.
///
/// Combines a finality proof (consensus quorum), a state proof (Merkle
/// inclusion of an account), and a header proof (block header chain) into
/// a single verifiable unit.  A light client can call `verify_complete()`
/// to check all three layers plus cross-layer consistency.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LightClientBundle {
    /// Proof that the block was finalized by consensus.
    pub finality_proof: FinalityProof,
    /// Merkle proof of an account's state at the finalized block.
    pub state_proof: StateProof,
    /// Block header proof for chain continuity.
    pub header_proof: HeaderProof,
}

// ---------------------------------------------------------------------------
// FinalityProof implementation
// ---------------------------------------------------------------------------

impl FinalityProof {
    /// Create a new finality proof from validator attestations.
    ///
    /// `signing_stake` is computed automatically from the attestation set.
    pub fn new(
        block_hash: Hash256,
        round: u64,
        block_height: u64,
        state_root: Hash256,
        attestations: Vec<ValidatorAttestation>,
        total_stake: u64,
    ) -> Self {
        let signing_stake = attestations.iter().map(|a| a.stake).sum();
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

    /// Compute the supermajority threshold for a given total stake.
    ///
    /// The threshold is `ceil(2 * total_stake / 3) + 1`, which ensures
    /// strictly more than two-thirds of stake have attested.
    pub fn supermajority_threshold(total_stake: u64) -> u64 {
        // ceil(2 * total / 3) = (2 * total + 2) / 3  (integer arithmetic)
        let two_thirds_ceil = (2 * total_stake + 2) / 3;
        two_thirds_ceil + 1
    }

    /// Verify that the finality proof has sufficient stake (>= supermajority).
    ///
    /// Returns `true` if `signing_stake >= supermajority_threshold(total_stake)`.
    pub fn verify_quorum(&self) -> bool {
        if self.total_stake == 0 {
            return false;
        }
        self.signing_stake >= Self::supermajority_threshold(self.total_stake)
    }

    /// Get the fraction of total stake that signed, as a percentage (0.0–100.0).
    pub fn stake_percentage(&self) -> f64 {
        if self.total_stake == 0 {
            return 0.0;
        }
        (self.signing_stake as f64 / self.total_stake as f64) * 100.0
    }

    /// Verify that the proof is internally consistent.
    ///
    /// Checks:
    /// 1. At least one attestation exists.
    /// 2. The `signing_stake` field matches the sum of attestation stakes.
    /// 3. The `signing_stake` does not exceed `total_stake`.
    /// 4. The quorum threshold is met.
    pub fn is_valid(&self) -> bool {
        if self.quorum_signatures.is_empty() {
            return false;
        }
        let computed_stake: u64 = self.quorum_signatures.iter().map(|a| a.stake).sum();
        if computed_stake != self.signing_stake {
            return false;
        }
        if self.signing_stake > self.total_stake {
            return false;
        }
        self.verify_quorum()
    }
}

// ---------------------------------------------------------------------------
// LightClientBundle implementation
// ---------------------------------------------------------------------------

impl LightClientBundle {
    /// Create a complete verification bundle.
    pub fn new(finality: FinalityProof, state: StateProof, header: HeaderProof) -> Self {
        Self {
            finality_proof: finality,
            state_proof: state,
            header_proof: header,
        }
    }

    /// Verify the entire bundle: finality + state + header chain consistency.
    ///
    /// Checks:
    /// 1. The finality proof has a valid quorum.
    /// 2. The state proof Merkle path is valid.
    /// 3. The finality proof's state root matches the state proof's state root.
    /// 4. The header proof's block state root matches the finality proof's state root.
    /// 5. The finality proof's block height matches the state proof's block height.
    pub fn verify_complete(&self) -> bool {
        // 1. Verify finality quorum.
        if !self.finality_proof.is_valid() {
            return false;
        }

        // 2. Verify the state proof's Merkle path.
        if !verify_state_proof(&self.state_proof) {
            return false;
        }

        // 3. Cross-check: finality state root == state proof state root.
        if self.finality_proof.state_root != self.state_proof.state_root {
            return false;
        }

        // 4. Cross-check: header state root == finality state root.
        if self.header_proof.header.state_root != self.finality_proof.state_root {
            return false;
        }

        // 5. Cross-check: block heights are consistent.
        if self.finality_proof.block_height != self.state_proof.block_height {
            return false;
        }

        true
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::StateDB;
    use arc_crypto::hash_bytes;
    use arc_types::Transaction;

    fn addr(n: u8) -> Hash256 {
        hash_bytes(&[n])
    }

    // -- StateProof ----------------------------------------------------------

    #[test]
    fn test_state_proof_valid() {
        let state = StateDB::with_genesis(&[(addr(1), 1_000_000), (addr(2), 500)]);
        // Execute a block so we have height > 0 with a committed block.
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
        // Proof itself is valid…
        assert!(verify_state_proof(&proof));
        // …but balance check fails for the wrong value.
        assert!(!verify_account_balance(&proof, 42));
    }

    #[test]
    fn test_state_proof_nonexistent() {
        let state = StateDB::with_genesis(&[(addr(1), 1_000_000)]);
        let tx = Transaction::new_transfer(addr(1), addr(2), 100, 0);
        state.execute_block(&[tx], addr(99)).unwrap();

        // addr(99) was never funded (it's only the block producer, not an account).
        // Actually addr(99) may or may not exist. Use addr(50) which is definitely absent.
        let result = state.generate_state_proof(&addr(50));
        assert!(result.is_err());
    }

    // -- HeaderProof ---------------------------------------------------------

    #[test]
    fn test_header_chain_valid() {
        let state = StateDB::with_genesis(&[(addr(1), u64::MAX)]);
        // Produce 3 blocks.
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

        // Corrupt the middle header's parent hash.
        proofs[1].parent_hash = Hash256::ZERO;
        proofs[1].header.parent_hash = Hash256::ZERO;
        assert!(!verify_header_chain(&proofs));
    }

    // -- TxInclusionProof ----------------------------------------------------

    #[test]
    fn test_tx_inclusion_proof() {
        let state = StateDB::with_genesis(&[(addr(1), 1_000_000)]);
        let tx = Transaction::new_transfer(addr(1), addr(2), 100, 0);
        let tx_hash = tx.hash;
        state.execute_block(&[tx], addr(99)).unwrap();

        let proof = state.generate_tx_inclusion_proof(&tx_hash).unwrap();
        assert!(verify_tx_inclusion(&proof));
    }

    // -- LightSnapshot -------------------------------------------------------

    #[test]
    fn test_light_snapshot() {
        let state = StateDB::with_genesis(&[(addr(1), 1_000_000), (addr(2), 500_000)]);
        let tx = Transaction::new_transfer(addr(1), addr(2), 100, 0);
        state.execute_block(&[tx], addr(99)).unwrap();

        let snap = state.generate_light_snapshot();
        assert_eq!(snap.height, 1);
        assert_eq!(snap.account_count, 2);
        // Total supply is conserved: 1_000_000 + 500_000 = 1_500_000
        assert_eq!(snap.total_supply, 1_500_000);
        assert_ne!(snap.state_root, Hash256::ZERO);
        assert_ne!(snap.latest_block_hash, Hash256::ZERO);
    }

    // -- State root changes --------------------------------------------------

    #[test]
    fn test_state_root_changes() {
        let state = StateDB::with_genesis(&[(addr(1), 1_000_000), (addr(2), 0)]);
        let root_before = state.get_state_root();

        let tx = Transaction::new_transfer(addr(1), addr(2), 500, 0);
        state.execute_block(&[tx], addr(99)).unwrap();

        let root_after = state.get_state_root();
        assert_ne!(root_before, root_after, "state root must change after executing transactions");
    }

    // -- FinalityProof -------------------------------------------------------

    /// Helper: build a set of validator attestations with the given stakes.
    fn make_attestations(stakes: &[u64]) -> Vec<ValidatorAttestation> {
        stakes
            .iter()
            .enumerate()
            .map(|(i, &stake)| ValidatorAttestation {
                validator_address: addr(100 + i as u8),
                stake,
                signature_bytes: vec![i as u8; 64], // placeholder signature
            })
            .collect()
    }

    #[test]
    fn test_finality_proof_quorum_sufficient() {
        // 70% of stake signed — should be valid (threshold is ~68 for total=100).
        let attestations = make_attestations(&[30, 20, 20]);
        let proof = FinalityProof::new(
            hash_bytes(b"block"),
            1,
            10,
            hash_bytes(b"state"),
            attestations,
            100,
        );
        assert!(proof.verify_quorum(), "70% stake should satisfy quorum");
        assert!(proof.is_valid());
    }

    #[test]
    fn test_finality_proof_quorum_insufficient() {
        // 60% of stake signed — should be invalid (threshold is 68 for total=100).
        let attestations = make_attestations(&[30, 30]);
        let proof = FinalityProof::new(
            hash_bytes(b"block"),
            1,
            10,
            hash_bytes(b"state"),
            attestations,
            100,
        );
        assert!(!proof.verify_quorum(), "60% stake should NOT satisfy quorum");
        assert!(!proof.is_valid());
    }

    #[test]
    fn test_finality_proof_exact_threshold() {
        // For total_stake=100: threshold = ceil(200/3) + 1 = 67 + 1 = 68.
        // Exactly 68 should pass.
        let attestations = make_attestations(&[34, 34]);
        let proof = FinalityProof::new(
            hash_bytes(b"block"),
            1,
            10,
            hash_bytes(b"state"),
            attestations,
            100,
        );
        assert_eq!(proof.signing_stake, 68);
        assert!(
            proof.verify_quorum(),
            "exactly 2/3+1 threshold should satisfy quorum"
        );
        assert!(proof.is_valid());
    }

    #[test]
    fn test_finality_proof_stake_percentage() {
        let attestations = make_attestations(&[25, 25, 25]);
        let proof = FinalityProof::new(
            hash_bytes(b"block"),
            1,
            10,
            hash_bytes(b"state"),
            attestations,
            100,
        );
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
        assert!(!proof.is_valid(), "empty attestations should be invalid");
        assert!(!proof.verify_quorum(), "0 signing stake should fail quorum");
    }

    #[test]
    fn test_supermajority_threshold_calculation() {
        // For total_stake = 100:
        //   ceil(200 / 3) + 1 = 67 + 1 = 68
        assert_eq!(FinalityProof::supermajority_threshold(100), 68);

        // For total_stake = 3:
        //   ceil(6 / 3) + 1 = 2 + 1 = 3
        assert_eq!(FinalityProof::supermajority_threshold(3), 3);

        // For total_stake = 10:
        //   ceil(20 / 3) + 1 = 7 + 1 = 8
        assert_eq!(FinalityProof::supermajority_threshold(10), 8);

        // For total_stake = 1:
        //   ceil(2 / 3) + 1 = 1 + 1 = 2
        assert_eq!(FinalityProof::supermajority_threshold(1), 2);
    }

    // -- LightClientBundle ---------------------------------------------------

    #[test]
    fn test_light_client_bundle_consistent() {
        // Build a real state + header proof from StateDB, then wrap in a bundle
        // with a matching finality proof.
        let state = StateDB::with_genesis(&[(addr(1), 1_000_000), (addr(2), 500)]);
        let tx = Transaction::new_transfer(addr(1), addr(2), 100, 0);
        state.execute_block(&[tx], addr(99)).unwrap();

        let state_proof = state.generate_state_proof(&addr(1)).unwrap();
        let header_proof = state.generate_header_proof(1).unwrap();
        let block_hash = arc_types::Block::compute_hash(&header_proof.header);

        // Build a finality proof that matches the state root from the proofs.
        let attestations = make_attestations(&[40, 40]);
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
            bundle.verify_complete(),
            "bundle with consistent hashes should verify"
        );
    }

    #[test]
    fn test_light_client_bundle_mismatched() {
        // Build valid proofs but give the finality proof a different state root.
        let state = StateDB::with_genesis(&[(addr(1), 1_000_000), (addr(2), 500)]);
        let tx = Transaction::new_transfer(addr(1), addr(2), 100, 0);
        state.execute_block(&[tx], addr(99)).unwrap();

        let state_proof = state.generate_state_proof(&addr(1)).unwrap();
        let header_proof = state.generate_header_proof(1).unwrap();
        let block_hash = arc_types::Block::compute_hash(&header_proof.header);

        // Use a WRONG state root in the finality proof.
        let wrong_state_root = hash_bytes(b"wrong-state-root");
        let attestations = make_attestations(&[40, 40]);
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
            !bundle.verify_complete(),
            "bundle with mismatched state roots should fail"
        );
    }
}
