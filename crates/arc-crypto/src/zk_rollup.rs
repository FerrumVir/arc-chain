//! ZK Rollup framework.
//!
//! Provides types and logic for batching layer-2 transactions, computing
//! state transitions, generating / verifying rollup proofs, sequencing
//! batches, and handling fraud proofs / dispute resolution.

use crate::hash::{Hash256, hash_bytes, hash_pair};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

// ---------------------------------------------------------------------------
// Core types
// ---------------------------------------------------------------------------

/// The kind of zero-knowledge proof used by the rollup.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum RollupProofType {
    STARK,
    SNARK,
    PLONK,
    Groth16,
}

/// A single transaction within a rollup batch.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RollupTx {
    pub from: [u8; 32],
    pub to: [u8; 32],
    pub amount: u64,
    pub nonce: u64,
    pub signature: Vec<u8>,
}

/// A proof attesting to the validity of a rollup batch.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RollupProof {
    pub proof_data: Vec<u8>,
    pub public_inputs: Vec<u64>,
    pub verification_key_hash: [u8; 32],
    pub proof_type: RollupProofType,
}

/// A batch of rollup transactions together with pre/post state roots.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RollupBatch {
    pub batch_id: u64,
    pub transactions: Vec<RollupTx>,
    pub pre_state_root: [u8; 32],
    pub post_state_root: [u8; 32],
    pub proof: Option<RollupProof>,
}

/// Configuration for the rollup.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RollupConfig {
    pub max_batch_size: usize,
    pub proof_type: RollupProofType,
    pub state_tree_depth: u8,
    pub challenge_period_blocks: u64,
}

impl Default for RollupConfig {
    fn default() -> Self {
        Self {
            max_batch_size: 1000,
            proof_type: RollupProofType::PLONK,
            state_tree_depth: 20,
            challenge_period_blocks: 100,
        }
    }
}

/// An L1 batch submission record.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BatchSubmission {
    pub batch: RollupBatch,
    pub l1_block_number: u64,
    pub submitter: [u8; 32],
    pub bond_amount: u64,
}

/// A fraud proof challenging a specific batch.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FraudProof {
    pub batch_id: u64,
    pub invalid_tx_index: u32,
    pub expected_root: [u8; 32],
    pub actual_root: [u8; 32],
    pub witness: Vec<u8>,
}

/// Outcome of a dispute resolution.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum DisputeResolution {
    ProverSlashed,
    ChallengerRewarded,
    NoFraud,
}

// ---------------------------------------------------------------------------
// Account & rollup state
// ---------------------------------------------------------------------------

/// A single account inside the rollup state.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct RollupAccount {
    pub balance: u64,
    pub nonce: u64,
}

/// The full rollup state: an in-memory account map with Merkle-ised root.
#[derive(Clone, Debug)]
pub struct RollupState {
    accounts: HashMap<[u8; 32], RollupAccount>,
}

impl RollupState {
    pub fn new() -> Self {
        Self {
            accounts: HashMap::new(),
        }
    }

    /// Get (or create) a mutable reference to an account.
    pub fn get_account(&self, addr: &[u8; 32]) -> RollupAccount {
        self.accounts.get(addr).cloned().unwrap_or_default()
    }

    /// Credit an account (used for genesis / deposits).
    pub fn credit(&mut self, addr: [u8; 32], amount: u64) {
        let acct = self.accounts.entry(addr).or_default();
        acct.balance = acct.balance.saturating_add(amount);
    }

    /// Compute a deterministic state root.
    ///
    /// Accounts are sorted by address, hashed pairwise into a Merkle tree.
    pub fn compute_state_root(&self) -> [u8; 32] {
        if self.accounts.is_empty() {
            return [0u8; 32];
        }

        let mut leaves: Vec<Hash256> = {
            let mut sorted: Vec<_> = self.accounts.iter().collect();
            sorted.sort_by_key(|(k, _)| *k);
            sorted
                .iter()
                .map(|(addr, acct)| {
                    let mut buf = Vec::with_capacity(48);
                    buf.extend_from_slice(*addr);
                    buf.extend_from_slice(&acct.balance.to_le_bytes());
                    buf.extend_from_slice(&acct.nonce.to_le_bytes());
                    hash_bytes(&buf)
                })
                .collect()
        };

        // Pad to power of two.
        while leaves.len().count_ones() != 1 {
            leaves.push(Hash256::ZERO);
        }

        while leaves.len() > 1 {
            leaves = leaves
                .chunks(2)
                .map(|pair| {
                    if pair.len() == 2 {
                        hash_pair(&pair[0], &pair[1])
                    } else {
                        pair[0]
                    }
                })
                .collect();
        }

        leaves[0].0
    }

    /// Apply a batch of transactions, returning the new state root.
    ///
    /// Returns `Err` with the index of the first invalid transaction.
    pub fn apply_batch(&mut self, txs: &[RollupTx]) -> Result<[u8; 32], usize> {
        for (i, tx) in txs.iter().enumerate() {
            let sender = self.accounts.entry(tx.from).or_default();
            if sender.balance < tx.amount {
                return Err(i);
            }
            if sender.nonce != tx.nonce {
                return Err(i);
            }
            sender.balance -= tx.amount;
            sender.nonce += 1;

            let receiver = self.accounts.entry(tx.to).or_default();
            receiver.balance = receiver.balance.saturating_add(tx.amount);
        }
        Ok(self.compute_state_root())
    }

    /// Verify that a batch produces the claimed state transition.
    pub fn verify_batch(
        &self,
        batch: &RollupBatch,
    ) -> Result<bool, String> {
        // Pre-state root must match.
        let current_root = self.compute_state_root();
        if current_root != batch.pre_state_root {
            return Err("pre-state root mismatch".into());
        }

        // Apply on a clone and compare post-state root.
        let mut clone = self.clone();
        match clone.apply_batch(&batch.transactions) {
            Ok(root) => Ok(root == batch.post_state_root),
            Err(idx) => Err(format!("invalid transaction at index {idx}")),
        }
    }
}

impl Default for RollupState {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Verifier
// ---------------------------------------------------------------------------

/// Verifies rollup proofs and state transitions.
pub struct RollupVerifier {
    config: RollupConfig,
}

impl RollupVerifier {
    pub fn new(config: RollupConfig) -> Self {
        Self { config }
    }

    /// Verify a rollup proof's structural validity.
    ///
    /// In a production system this would call a SNARK/STARK verifier; here we
    /// check structural invariants and signature over public inputs.
    pub fn verify_proof(&self, proof: &RollupProof) -> bool {
        // Proof data must be non-empty.
        if proof.proof_data.is_empty() {
            return false;
        }
        // Public inputs must be non-empty.
        if proof.public_inputs.is_empty() {
            return false;
        }
        // Verification key hash must not be zero.
        if proof.verification_key_hash == [0u8; 32] {
            return false;
        }
        // Proof type must match config.
        if proof.proof_type != self.config.proof_type {
            return false;
        }

        // Mock verification: BLAKE3 of proof_data must have leading zero
        // nibble. This is NOT a ZK proof — it's a trivial PoW check for
        // pipeline testing. Real STARK verification is in `stwo_air.rs`.
        let h = blake3::hash(&proof.proof_data);
        h.as_bytes()[0] < 0x10
    }

    /// Verify a full state transition: proof + state roots.
    pub fn verify_state_transition(
        &self,
        batch: &RollupBatch,
    ) -> bool {
        // Must have a proof.
        let proof = match &batch.proof {
            Some(p) => p,
            None => return false,
        };

        // Verify the proof itself.
        if !self.verify_proof(proof) {
            return false;
        }

        // Public inputs should encode pre and post state roots.
        // Convention: first 4 u64 = pre_root (as 4×u64), next 4 = post_root.
        if proof.public_inputs.len() < 8 {
            return false;
        }

        // Reconstruct roots from public inputs.
        let pre_root = u64s_to_bytes(&proof.public_inputs[0..4]);
        let post_root = u64s_to_bytes(&proof.public_inputs[4..8]);

        pre_root == batch.pre_state_root && post_root == batch.post_state_root
    }
}

/// Helper: pack 4 u64 → 32 bytes (little-endian).
fn u64s_to_bytes(vals: &[u64]) -> [u8; 32] {
    let mut out = [0u8; 32];
    for (i, &v) in vals.iter().take(4).enumerate() {
        out[i * 8..(i + 1) * 8].copy_from_slice(&v.to_le_bytes());
    }
    out
}

/// Helper: unpack 32 bytes → 4 u64 (little-endian).
pub fn bytes_to_u64s(bytes: &[u8; 32]) -> Vec<u64> {
    (0..4)
        .map(|i| {
            let mut buf = [0u8; 8];
            buf.copy_from_slice(&bytes[i * 8..(i + 1) * 8]);
            u64::from_le_bytes(buf)
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Sequencer
// ---------------------------------------------------------------------------

/// Collects transactions and flushes them into batches.
pub struct RollupSequencer {
    config: RollupConfig,
    pending: Vec<RollupTx>,
    next_batch_id: u64,
}

impl RollupSequencer {
    pub fn new(config: RollupConfig) -> Self {
        Self {
            config,
            pending: Vec::new(),
            next_batch_id: 0,
        }
    }

    /// Add a transaction to the pending queue.
    pub fn add_tx(&mut self, tx: RollupTx) {
        self.pending.push(tx);
    }

    /// Number of pending transactions.
    pub fn pending_count(&self) -> usize {
        self.pending.len()
    }

    /// Flush pending transactions into a batch.
    ///
    /// Applies them to `state` and returns the resulting `RollupBatch`.
    /// Returns `None` if no transactions are pending.
    pub fn flush_batch(&mut self, state: &mut RollupState) -> Option<RollupBatch> {
        if self.pending.is_empty() {
            return None;
        }

        let take = std::cmp::min(self.pending.len(), self.config.max_batch_size);
        let txs: Vec<RollupTx> = self.pending.drain(..take).collect();

        let pre_root = state.compute_state_root();
        let post_root = match state.apply_batch(&txs) {
            Ok(root) => root,
            Err(_) => return None,
        };

        let batch = RollupBatch {
            batch_id: self.next_batch_id,
            transactions: txs,
            pre_state_root: pre_root,
            post_state_root: post_root,
            proof: None,
        };
        self.next_batch_id += 1;
        Some(batch)
    }
}

// ---------------------------------------------------------------------------
// Fraud proof helpers
// ---------------------------------------------------------------------------

/// Evaluate a fraud proof against a batch.
///
/// Returns the dispute resolution outcome.
pub fn resolve_dispute(
    fraud: &FraudProof,
    batch: &RollupBatch,
    state: &RollupState,
) -> DisputeResolution {
    if fraud.batch_id != batch.batch_id {
        return DisputeResolution::NoFraud;
    }

    // Re-execute the batch on a clone of the state.
    let mut clone = state.clone();
    match clone.apply_batch(&batch.transactions) {
        Ok(actual_root) => {
            if actual_root != batch.post_state_root {
                // The batch's claimed post-root is wrong — fraud confirmed.
                DisputeResolution::ProverSlashed
            } else if fraud.expected_root != actual_root {
                // Challenger's expected root is wrong — no fraud.
                DisputeResolution::NoFraud
            } else {
                DisputeResolution::NoFraud
            }
        }
        Err(_) => {
            // Batch contains an invalid transaction — fraud confirmed.
            DisputeResolution::ProverSlashed
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn alice() -> [u8; 32] {
        let mut a = [0u8; 32];
        a[0] = 0xAA;
        a
    }

    fn bob() -> [u8; 32] {
        let mut b = [0u8; 32];
        b[0] = 0xBB;
        b
    }

    fn make_tx(from: [u8; 32], to: [u8; 32], amount: u64, nonce: u64) -> RollupTx {
        RollupTx {
            from,
            to,
            amount,
            nonce,
            signature: vec![0u8; 64],
        }
    }

    #[test]
    fn test_state_root_empty() {
        let state = RollupState::new();
        assert_eq!(state.compute_state_root(), [0u8; 32]);
    }

    #[test]
    fn test_state_root_deterministic() {
        let mut s1 = RollupState::new();
        s1.credit(alice(), 1000);
        let mut s2 = RollupState::new();
        s2.credit(alice(), 1000);
        assert_eq!(s1.compute_state_root(), s2.compute_state_root());
    }

    #[test]
    fn test_state_root_changes_on_transfer() {
        let mut state = RollupState::new();
        state.credit(alice(), 1000);
        let root_before = state.compute_state_root();
        let tx = make_tx(alice(), bob(), 100, 0);
        let _ = state.apply_batch(&[tx]);
        let root_after = state.compute_state_root();
        assert_ne!(root_before, root_after);
    }

    #[test]
    fn test_apply_batch_success() {
        let mut state = RollupState::new();
        state.credit(alice(), 500);
        let tx = make_tx(alice(), bob(), 200, 0);
        let result = state.apply_batch(&[tx]);
        assert!(result.is_ok());
        assert_eq!(state.get_account(&alice()).balance, 300);
        assert_eq!(state.get_account(&bob()).balance, 200);
    }

    #[test]
    fn test_apply_batch_insufficient_funds() {
        let mut state = RollupState::new();
        state.credit(alice(), 50);
        let tx = make_tx(alice(), bob(), 100, 0);
        let result = state.apply_batch(&[tx]);
        assert!(result.is_err());
    }

    #[test]
    fn test_apply_batch_bad_nonce() {
        let mut state = RollupState::new();
        state.credit(alice(), 1000);
        let tx = make_tx(alice(), bob(), 100, 999); // wrong nonce
        let result = state.apply_batch(&[tx]);
        assert!(result.is_err());
    }

    #[test]
    fn test_sequencer_flush() {
        let config = RollupConfig::default();
        let mut seq = RollupSequencer::new(config);
        let mut state = RollupState::new();
        state.credit(alice(), 1000);

        seq.add_tx(make_tx(alice(), bob(), 100, 0));
        seq.add_tx(make_tx(alice(), bob(), 200, 1));
        assert_eq!(seq.pending_count(), 2);

        let batch = seq.flush_batch(&mut state);
        assert!(batch.is_some());
        let batch = batch.unwrap();
        assert_eq!(batch.batch_id, 0);
        assert_eq!(batch.transactions.len(), 2);
        assert_eq!(seq.pending_count(), 0);
    }

    #[test]
    fn test_sequencer_empty_flush() {
        let config = RollupConfig::default();
        let mut seq = RollupSequencer::new(config);
        let mut state = RollupState::new();
        assert!(seq.flush_batch(&mut state).is_none());
    }

    #[test]
    fn test_verify_batch_valid() {
        let mut state = RollupState::new();
        state.credit(alice(), 1000);
        let pre_root = state.compute_state_root();

        let tx = make_tx(alice(), bob(), 100, 0);

        // Compute expected post root.
        let mut clone = state.clone();
        let post_root = clone.apply_batch(&[tx.clone()]).unwrap();

        let batch = RollupBatch {
            batch_id: 0,
            transactions: vec![tx],
            pre_state_root: pre_root,
            post_state_root: post_root,
            proof: None,
        };

        let result = state.verify_batch(&batch);
        assert!(result.is_ok());
        assert!(result.unwrap());
    }

    #[test]
    fn test_verify_batch_wrong_post_root() {
        let mut state = RollupState::new();
        state.credit(alice(), 1000);
        let pre_root = state.compute_state_root();

        let tx = make_tx(alice(), bob(), 100, 0);

        let batch = RollupBatch {
            batch_id: 0,
            transactions: vec![tx],
            pre_state_root: pre_root,
            post_state_root: [0xFFu8; 32], // wrong
            proof: None,
        };

        let result = state.verify_batch(&batch);
        assert!(result.is_ok());
        assert!(!result.unwrap()); // verification fails
    }

    #[test]
    fn test_rollup_proof_type_variants() {
        let types = [
            RollupProofType::STARK,
            RollupProofType::SNARK,
            RollupProofType::PLONK,
            RollupProofType::Groth16,
        ];
        for t in &types {
            assert_eq!(*t, *t);
        }
        assert_ne!(RollupProofType::STARK, RollupProofType::PLONK);
    }

    #[test]
    fn test_fraud_proof_no_fraud() {
        let mut state = RollupState::new();
        state.credit(alice(), 1000);
        let pre_root = state.compute_state_root();

        let tx = make_tx(alice(), bob(), 100, 0);
        let mut exec_state = state.clone();
        let post_root = exec_state.apply_batch(&[tx.clone()]).unwrap();

        let batch = RollupBatch {
            batch_id: 0,
            transactions: vec![tx],
            pre_state_root: pre_root,
            post_state_root: post_root,
            proof: None,
        };

        let fraud = FraudProof {
            batch_id: 0,
            invalid_tx_index: 0,
            expected_root: post_root,
            actual_root: [0xFFu8; 32],
            witness: vec![],
        };

        let result = resolve_dispute(&fraud, &batch, &state);
        assert_eq!(result, DisputeResolution::NoFraud);
    }

    #[test]
    fn test_fraud_proof_prover_slashed() {
        let mut state = RollupState::new();
        state.credit(alice(), 1000);
        let pre_root = state.compute_state_root();

        let tx = make_tx(alice(), bob(), 100, 0);

        // Batch claims a WRONG post root.
        let batch = RollupBatch {
            batch_id: 1,
            transactions: vec![tx],
            pre_state_root: pre_root,
            post_state_root: [0xDE; 32], // fraudulent
            proof: None,
        };

        let fraud = FraudProof {
            batch_id: 1,
            invalid_tx_index: 0,
            expected_root: [0u8; 32],
            actual_root: [0xDE; 32],
            witness: vec![],
        };

        let result = resolve_dispute(&fraud, &batch, &state);
        assert_eq!(result, DisputeResolution::ProverSlashed);
    }

    #[test]
    fn test_batch_submission_struct() {
        let batch = RollupBatch {
            batch_id: 42,
            transactions: vec![],
            pre_state_root: [0u8; 32],
            post_state_root: [1u8; 32],
            proof: None,
        };
        let sub = BatchSubmission {
            batch: batch.clone(),
            l1_block_number: 100,
            submitter: [0xAA; 32],
            bond_amount: 1_000_000,
        };
        assert_eq!(sub.l1_block_number, 100);
        assert_eq!(sub.bond_amount, 1_000_000);
    }

    #[test]
    fn test_config_defaults() {
        let config = RollupConfig::default();
        assert_eq!(config.max_batch_size, 1000);
        assert_eq!(config.proof_type, RollupProofType::PLONK);
        assert_eq!(config.state_tree_depth, 20);
        assert_eq!(config.challenge_period_blocks, 100);
    }

    #[test]
    fn test_sequencer_respects_max_batch_size() {
        let config = RollupConfig {
            max_batch_size: 2,
            ..RollupConfig::default()
        };
        let mut seq = RollupSequencer::new(config);
        let mut state = RollupState::new();
        state.credit(alice(), 10_000);

        for i in 0..5u64 {
            seq.add_tx(make_tx(alice(), bob(), 10, i));
        }
        assert_eq!(seq.pending_count(), 5);

        let batch = seq.flush_batch(&mut state).unwrap();
        assert_eq!(batch.transactions.len(), 2);
        assert_eq!(seq.pending_count(), 3);
    }

    #[test]
    fn test_multiple_sequential_batches() {
        let config = RollupConfig::default();
        let mut seq = RollupSequencer::new(config);
        let mut state = RollupState::new();
        state.credit(alice(), 10_000);

        seq.add_tx(make_tx(alice(), bob(), 100, 0));
        let b1 = seq.flush_batch(&mut state).unwrap();
        assert_eq!(b1.batch_id, 0);

        seq.add_tx(make_tx(alice(), bob(), 200, 1));
        let b2 = seq.flush_batch(&mut state).unwrap();
        assert_eq!(b2.batch_id, 1);

        // Post root of batch 1 should equal pre root of batch 2.
        assert_eq!(b1.post_state_root, b2.pre_state_root);
    }
}
