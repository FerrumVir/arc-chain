//! Light client proofs — allow verification of state without downloading the full chain.
//!
//! Provides:
//! - `StateProof` — prove an account's state against a Merkle state root
//! - `HeaderProof` — verify a chain of block headers by parent-hash linkage
//! - `TxInclusionProof` — prove a transaction was included in a specific block
//! - `LightSnapshot` — compact aggregate view for light client bootstrapping
//!
//! All verification functions are pure (no StateDB required), so light clients
//! can run them with only the proof data and a trusted state root.

use arc_crypto::{Hash256, MerkleProof, MerkleTree, hash_bytes};
use arc_types::{Account, BlockHeader};
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
}
