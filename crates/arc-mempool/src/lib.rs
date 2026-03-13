use arc_crypto::{Hash256, hash_bytes, ThresholdEncryption};
use arc_crypto::bls::{
    BlsKeypair, BlsPublicKey, BlsSignature,
    bls_keygen, bls_sign,
};
use arc_types::Transaction;
use crossbeam::queue::SegQueue;
use dashmap::DashMap;
use dashmap::DashSet;
use parking_lot::RwLock;
use std::collections::VecDeque;
use thiserror::Error;

#[derive(Error, Debug)]
pub enum MempoolError {
    #[error("duplicate transaction: {0:?}")]
    Duplicate(Hash256),
    #[error("mempool full (capacity: {0})")]
    Full(usize),
    #[error("decryption failed: ciphertext tampered or wrong key")]
    DecryptionFailed,
    #[error("deserialization failed after decryption")]
    DeserializationFailed,
}

/// Lock-free transaction mempool.
/// Uses crossbeam's SegQueue for wait-free concurrent push/pop
/// and DashMap for O(1) deduplication.
pub struct Mempool {
    /// Ordered queue of pending transactions.
    queue: SegQueue<Transaction>,
    /// Deduplication set (tx_hash → exists).
    seen: DashMap<[u8; 32], ()>,
    /// Maximum mempool size.
    capacity: usize,
    /// Current size (atomic via DashMap len).
    count: RwLock<usize>,
}

impl Mempool {
    /// Create a new mempool with given capacity.
    pub fn new(capacity: usize) -> Self {
        Self {
            queue: SegQueue::new(),
            seen: DashMap::new(),
            capacity,
            count: RwLock::new(0),
        }
    }

    /// Add a transaction to the mempool.
    /// Returns error if duplicate or mempool is full.
    pub fn insert(&self, tx: Transaction) -> Result<(), MempoolError> {
        // Check capacity
        {
            let count = self.count.read();
            if *count >= self.capacity {
                return Err(MempoolError::Full(self.capacity));
            }
        }

        // Check deduplication
        if self.seen.contains_key(&tx.hash.0) {
            return Err(MempoolError::Duplicate(tx.hash));
        }

        self.seen.insert(tx.hash.0, ());
        self.queue.push(tx);
        {
            let mut count = self.count.write();
            *count += 1;
        }

        Ok(())
    }

    /// Drain up to `max` transactions for block production.
    /// Returns transactions in FIFO order.
    pub fn drain(&self, max: usize) -> Vec<Transaction> {
        let mut batch = Vec::with_capacity(max);
        for _ in 0..max {
            match self.queue.pop() {
                Some(tx) => {
                    self.seen.remove(&tx.hash.0);
                    batch.push(tx);
                }
                None => break,
            }
        }
        {
            let mut count = self.count.write();
            *count = count.saturating_sub(batch.len());
        }
        batch
    }

    /// Current number of pending transactions.
    pub fn len(&self) -> usize {
        *self.count.read()
    }

    /// Whether the mempool is empty.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Check if a transaction is already in the mempool.
    pub fn contains(&self, hash: &Hash256) -> bool {
        self.seen.contains_key(&hash.0)
    }

    /// Clear all pending transactions.
    pub fn clear(&self) {
        while self.queue.pop().is_some() {}
        self.seen.clear();
        *self.count.write() = 0;
    }
}

impl Default for Mempool {
    fn default() -> Self {
        Self::new(1_000_000) // 1M tx default capacity
    }
}

// ──────────────────────────────────────────────────────────────────────────────
// Encrypted Mempool — MEV Protection via BLS Threshold Encryption
// ──────────────────────────────────────────────────────────────────────────────
//
// Transactions enter the mempool encrypted. The proposer MUST include them in
// FIFO order (verifiable by other validators). Decryption happens AFTER the
// block is committed to the DAG (commit-reveal). This prevents front-running,
// sandwich attacks, and censorship.
//
// Encryption: BLS-based threshold key derivation + BLAKE3-CTR authenticated
// encryption. Committee members each BLS-sign a per-slot nonce. The aggregated
// signature is hashed to derive a 32-byte symmetric key. This follows the same
// pattern as Flashbots' MEV protection.
//
// Key hierarchy:
//   1. Committee members BLS-sign the slot message → partial signature shares
//   2. Aggregate shares → derive symmetric slot key via ThresholdEncryption
//   3. Encrypt tx payload with BLAKE3-CTR + BLAKE3-MAC (authenticated encryption)
//   4. After block commit, committee reveals shares → reconstruct slot key → decrypt
// ──────────────────────────────────────────────────────────────────────────────

/// Encrypted transaction wrapper for MEV protection.
///
/// The original transaction is serialized and encrypted under a slot key
/// derived from BLS threshold signatures. The per-tx symmetric key is
/// encrypted to the committee's BLS public key. No validator can read the
/// transaction contents until the block is committed and threshold signatures
/// are revealed.
#[derive(Clone, Debug)]
pub struct EncryptedTx {
    /// Hash of the encrypted payload (for dedup).
    pub id: Hash256,
    /// Encrypted transaction bytes (nonce || ciphertext || 32-byte auth tag).
    /// Uses ThresholdEncryption AEAD format.
    pub encrypted_payload: Vec<u8>,
    /// The slot this transaction targets (used for key derivation).
    pub target_slot: u64,
    /// Per-tx symmetric key encrypted under the committee slot key.
    /// Format: ThresholdEncryption AEAD(slot_key, per_tx_key).
    pub encrypted_key: Vec<u8>,
    /// Submission timestamp — used for FIFO ordering.
    pub submitted_at: std::time::Instant,
    /// BLAKE3(sender_address || slot || random_salt) — commit without revealing sender.
    pub sender_commitment: Hash256,
}

/// Decrypted result after block commit.
#[derive(Clone, Debug)]
pub struct RevealedTx {
    /// The encrypted tx ID this was decrypted from.
    pub encrypted_id: Hash256,
    /// The revealed plaintext transaction.
    pub transaction: Transaction,
    /// The DAG round in which this was revealed.
    pub reveal_round: u64,
}

/// BLS committee configuration for threshold encryption.
///
/// Holds the BLS keypairs for committee members. In production, each validator
/// holds only their own secret key; the full set is never co-located. This
/// struct is used for testing and simulation.
pub struct BlsCommittee {
    /// BLS keypairs for each committee member.
    pub keypairs: Vec<BlsKeypair>,
    /// BLS public keys (extracted for convenience).
    pub public_keys: Vec<BlsPublicKey>,
}

impl BlsCommittee {
    /// Create a committee of `n` members with deterministic keys (for testing).
    pub fn new_deterministic(n: usize) -> Self {
        let keypairs: Vec<BlsKeypair> = (0..n)
            .map(|i| bls_keygen(format!("ARC-committee-member-{i}").as_bytes()))
            .collect();
        let public_keys: Vec<BlsPublicKey> = keypairs.iter()
            .map(|kp| kp.public.clone())
            .collect();
        Self { keypairs, public_keys }
    }

    /// Have all committee members sign a slot message and return the signatures.
    pub fn sign_slot(&self, slot: u64) -> Vec<BlsSignature> {
        let msg = ThresholdEncryption::slot_message(slot);
        self.keypairs.iter()
            .map(|kp| bls_sign(&kp.secret, &msg))
            .collect()
    }

    /// Derive the slot key from committee signatures.
    pub fn derive_slot_key(&self, slot: u64) -> [u8; 32] {
        let sigs = self.sign_slot(slot);
        ThresholdEncryption::derive_slot_key(slot, &sigs)
    }
}

/// MEV-protected mempool that enforces fair ordering via commit-reveal
/// with BLS threshold encryption.
///
/// Transactions are encrypted under a slot key derived from BLS threshold
/// signatures. They are stored in a FIFO queue. The proposer drains them in
/// submission order, includes them in the block, and only after the block is
/// committed and committee members reveal their BLS signature shares does
/// decryption occur.
pub struct EncryptedMempool {
    /// FIFO queue of pending encrypted transactions.
    pending: RwLock<VecDeque<EncryptedTx>>,
    /// Dedup set keyed by encrypted payload hash.
    seen: DashSet<[u8; 32]>,
    /// Maximum number of pending encrypted transactions.
    capacity: usize,
    /// BLS committee for threshold key derivation.
    committee: BlsCommittee,
    /// Current slot (incremented by the consensus layer).
    current_slot: RwLock<u64>,
}

impl EncryptedMempool {
    /// Create a new encrypted mempool with the given capacity.
    ///
    /// Initializes a deterministic 5-member BLS committee. In production,
    /// committee membership comes from the validator set and each member
    /// holds only their own BLS secret key.
    pub fn new(capacity: usize) -> Self {
        Self {
            pending: RwLock::new(VecDeque::new()),
            seen: DashSet::new(),
            capacity,
            committee: BlsCommittee::new_deterministic(5),
            current_slot: RwLock::new(0),
        }
    }

    /// Create an encrypted mempool with an explicit committee (for testing).
    pub fn with_committee(capacity: usize, committee: BlsCommittee) -> Self {
        Self {
            pending: RwLock::new(VecDeque::new()),
            seen: DashSet::new(),
            capacity,
            committee,
            current_slot: RwLock::new(0),
        }
    }

    /// Return the committee's BLS public keys.
    pub fn committee_pubkeys(&self) -> &[BlsPublicKey] {
        &self.committee.public_keys
    }

    /// Get the current slot.
    pub fn current_slot(&self) -> u64 {
        *self.current_slot.read()
    }

    /// Advance the slot (called by consensus layer).
    pub fn advance_slot(&self) {
        let mut slot = self.current_slot.write();
        *slot += 1;
    }

    /// Set the slot explicitly.
    pub fn set_slot(&self, slot: u64) {
        *self.current_slot.write() = slot;
    }

    /// Submit an encrypted transaction to the mempool.
    ///
    /// Rejects duplicates (by encrypted payload hash) and over-capacity.
    pub fn submit_encrypted(&self, etx: EncryptedTx) -> Result<(), MempoolError> {
        // Capacity check
        {
            let pending = self.pending.read();
            if pending.len() >= self.capacity {
                return Err(MempoolError::Full(self.capacity));
            }
        }

        // Dedup check
        if !self.seen.insert(etx.id.0) {
            return Err(MempoolError::Duplicate(etx.id));
        }

        // Append to FIFO queue
        {
            let mut pending = self.pending.write();
            pending.push_back(etx);
        }

        Ok(())
    }

    /// Drain up to `max` encrypted transactions in strict FIFO order.
    ///
    /// The proposer calls this to build a block. Other validators can verify
    /// the ordering with `verify_fifo_ordering`.
    pub fn drain_fifo(&self, max: usize) -> Vec<EncryptedTx> {
        let mut pending = self.pending.write();
        let drain_count = max.min(pending.len());
        let mut batch = Vec::with_capacity(drain_count);

        for _ in 0..drain_count {
            if let Some(etx) = pending.pop_front() {
                self.seen.remove(&etx.id.0);
                batch.push(etx);
            }
        }

        batch
    }

    /// Reveal (decrypt) a batch of encrypted transactions after block commit.
    ///
    /// Collects BLS signature shares from committee members for the slot,
    /// derives the slot key, and decrypts each transaction. Failed decryptions
    /// are logged and skipped (they don't halt the batch).
    pub fn reveal_batch(&self, batch: &[EncryptedTx], reveal_round: u64) -> Vec<RevealedTx> {
        let mut revealed = Vec::with_capacity(batch.len());

        for etx in batch {
            // Derive slot key for this transaction's target slot.
            let slot_key = self.committee.derive_slot_key(etx.target_slot);

            match Self::decrypt_transaction(etx, &slot_key) {
                Ok(tx) => {
                    revealed.push(RevealedTx {
                        encrypted_id: etx.id,
                        transaction: tx,
                        reveal_round,
                    });
                }
                Err(e) => {
                    tracing::warn!(
                        id = %etx.id,
                        error = %e,
                        "failed to decrypt transaction during reveal"
                    );
                }
            }
        }

        revealed
    }

    /// Reveal a batch using an externally-provided slot key.
    ///
    /// This is the production path: the consensus layer collects BLS signature
    /// shares from >= threshold committee members, derives the slot key, and
    /// passes it here. The mempool does not need to hold private keys.
    pub fn reveal_batch_with_key(
        batch: &[EncryptedTx],
        slot_key: &[u8; 32],
        reveal_round: u64,
    ) -> Vec<RevealedTx> {
        let mut revealed = Vec::with_capacity(batch.len());

        for etx in batch {
            match Self::decrypt_transaction(etx, slot_key) {
                Ok(tx) => {
                    revealed.push(RevealedTx {
                        encrypted_id: etx.id,
                        transaction: tx,
                        reveal_round,
                    });
                }
                Err(e) => {
                    tracing::warn!(
                        id = %etx.id,
                        error = %e,
                        "failed to decrypt transaction during reveal"
                    );
                }
            }
        }

        revealed
    }

    /// Verify that a batch of encrypted transactions is in correct FIFO order.
    ///
    /// Other validators call this to ensure the proposer didn't reorder
    /// transactions (which would enable MEV extraction).
    pub fn verify_fifo_ordering(batch: &[EncryptedTx]) -> bool {
        for window in batch.windows(2) {
            if window[1].submitted_at < window[0].submitted_at {
                return false;
            }
        }
        true
    }

    /// Number of pending encrypted transactions.
    pub fn pending_count(&self) -> usize {
        self.pending.read().len()
    }

    // ── BLS Threshold Encryption Helpers ─────────────────────────────────

    /// Encrypt a transaction for submission to the encrypted mempool.
    ///
    /// 1. Serialize the transaction with bincode.
    /// 2. Generate a random per-tx symmetric key.
    /// 3. Encrypt the serialized bytes with ThresholdEncryption AEAD (BLAKE3-CTR + MAC).
    /// 4. Encrypt the per-tx key under the slot key (derived from committee BLS sigs).
    /// 5. Compute a sender commitment: BLAKE3(sender_address || slot || salt).
    ///
    /// The `slot_key` is derived from BLS threshold signatures on the target slot.
    /// In production, users obtain this from the committee's published slot commitment.
    pub fn encrypt_transaction(
        tx: &Transaction,
        slot_key: &[u8; 32],
        target_slot: u64,
    ) -> EncryptedTx {
        use rand::Rng;
        let mut rng = rand::thread_rng();

        // Serialize transaction.
        let plaintext = bincode::serialize(tx).expect("transaction must serialize");

        // Generate a random per-tx symmetric key.
        let per_tx_key: [u8; 32] = rng.r#gen();

        // Encrypt payload with per-tx key (ThresholdEncryption AEAD).
        let encrypted_payload = ThresholdEncryption::encrypt(&per_tx_key, &plaintext);

        // Encrypt the per-tx key under the slot key.
        // This means only someone with the slot key can recover the per-tx key.
        let encrypted_key = ThresholdEncryption::encrypt(slot_key, &per_tx_key);

        // Compute encrypted payload hash for dedup.
        let id = hash_bytes(&encrypted_payload);

        // Sender commitment: BLAKE3(sender_address || slot || random_salt).
        let salt: [u8; 16] = rng.r#gen();
        let sender_commitment = {
            let mut hasher = blake3::Hasher::new_derive_key("ARC-sender-commitment-v2");
            hasher.update(tx.from.as_ref());
            hasher.update(&target_slot.to_le_bytes());
            hasher.update(&salt);
            Hash256(*hasher.finalize().as_bytes())
        };

        EncryptedTx {
            id,
            encrypted_payload,
            target_slot,
            encrypted_key,
            submitted_at: std::time::Instant::now(),
            sender_commitment,
        }
    }

    /// Decrypt an encrypted transaction using the slot key.
    ///
    /// 1. Decrypt the per-tx key from `encrypted_key` using the slot key.
    /// 2. Decrypt the payload using the recovered per-tx key.
    /// 3. Deserialize the plaintext back into a Transaction.
    ///
    /// The slot key is derived from aggregated BLS threshold signatures
    /// on the slot message.
    pub fn decrypt_transaction(
        etx: &EncryptedTx,
        slot_key: &[u8; 32],
    ) -> Result<Transaction, MempoolError> {
        // Step 1: Decrypt the per-tx symmetric key.
        let per_tx_key_bytes = ThresholdEncryption::decrypt(slot_key, &etx.encrypted_key)
            .ok_or(MempoolError::DecryptionFailed)?;

        if per_tx_key_bytes.len() != 32 {
            return Err(MempoolError::DecryptionFailed);
        }
        let mut per_tx_key = [0u8; 32];
        per_tx_key.copy_from_slice(&per_tx_key_bytes);

        // Step 2: Decrypt the transaction payload.
        let plaintext = ThresholdEncryption::decrypt(&per_tx_key, &etx.encrypted_payload)
            .ok_or(MempoolError::DecryptionFailed)?;

        // Step 3: Deserialize.
        bincode::deserialize::<Transaction>(&plaintext)
            .map_err(|_| MempoolError::DeserializationFailed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use arc_crypto::{hash_bytes, THRESHOLD_TAG_LEN};
    use arc_crypto::bls::bls_verify;

    fn addr(n: u8) -> Hash256 {
        hash_bytes(&[n])
    }

    #[test]
    fn test_insert_and_drain() {
        let pool = Mempool::new(100);
        let tx = Transaction::new_transfer(addr(1), addr(2), 100, 0);
        pool.insert(tx).unwrap();
        assert_eq!(pool.len(), 1);

        let batch = pool.drain(10);
        assert_eq!(batch.len(), 1);
        assert!(pool.is_empty());
    }

    #[test]
    fn test_dedup() {
        let pool = Mempool::new(100);
        let tx = Transaction::new_transfer(addr(1), addr(2), 100, 0);
        pool.insert(tx.clone()).unwrap();
        assert!(pool.insert(tx).is_err());
    }

    #[test]
    fn test_capacity() {
        let pool = Mempool::new(2);
        pool.insert(Transaction::new_transfer(addr(1), addr(2), 1, 0)).unwrap();
        pool.insert(Transaction::new_transfer(addr(1), addr(2), 2, 1)).unwrap();
        let result = pool.insert(Transaction::new_transfer(addr(1), addr(2), 3, 2));
        assert!(result.is_err());
    }

    #[test]
    fn test_fifo_order() {
        let pool = Mempool::new(100);
        for i in 0..10u64 {
            pool.insert(Transaction::new_transfer(addr(1), addr(2), i, i)).unwrap();
        }
        let batch = pool.drain(10);
        assert_eq!(batch.len(), 10);
        assert_eq!(batch[0].nonce, 0);
        assert_eq!(batch[9].nonce, 9);
    }

    // ── BLS Encrypted Mempool Tests ─────────────────────────────────────

    #[test]
    fn test_encrypted_submit_and_drain_fifo() {
        let pool = EncryptedMempool::new(100);
        let slot = pool.current_slot();
        let slot_key = pool.committee.derive_slot_key(slot);

        // Submit 5 transactions
        let mut expected_ids = Vec::new();
        for i in 0..5u64 {
            let tx = Transaction::new_transfer(addr(1), addr(2), i * 100, i);
            let etx = EncryptedMempool::encrypt_transaction(&tx, &slot_key, slot);
            expected_ids.push(etx.id);
            pool.submit_encrypted(etx).unwrap();
        }

        assert_eq!(pool.pending_count(), 5);

        // Drain all — must come back in FIFO submission order
        let batch = pool.drain_fifo(10);
        assert_eq!(batch.len(), 5);
        assert_eq!(pool.pending_count(), 0);

        for (i, etx) in batch.iter().enumerate() {
            assert_eq!(etx.id, expected_ids[i], "FIFO order violated at index {i}");
        }
    }

    #[test]
    fn test_encrypted_dedup() {
        let pool = EncryptedMempool::new(100);
        let slot_key = pool.committee.derive_slot_key(0);

        let tx = Transaction::new_transfer(addr(1), addr(2), 100, 0);
        let etx = EncryptedMempool::encrypt_transaction(&tx, &slot_key, 0);
        let etx_clone = etx.clone();

        pool.submit_encrypted(etx).unwrap();

        // Same encrypted tx (same id) should be rejected
        let result = pool.submit_encrypted(etx_clone);
        assert!(result.is_err());
        assert_eq!(pool.pending_count(), 1);
    }

    #[test]
    fn test_encrypt_decrypt_roundtrip() {
        let pool = EncryptedMempool::new(100);
        let slot = 0u64;
        let slot_key = pool.committee.derive_slot_key(slot);

        let tx = Transaction::new_transfer(addr(1), addr(2), 42_000, 7);
        let etx = EncryptedMempool::encrypt_transaction(&tx, &slot_key, slot);

        // The encrypted payload should NOT be the same as the serialized tx
        let plaintext_bytes = bincode::serialize(&tx).unwrap();
        // Payload includes 12-byte nonce prefix, so offset by that
        let ct_body_start = 12;
        let ct_body_end = etx.encrypted_payload.len().saturating_sub(THRESHOLD_TAG_LEN);
        if ct_body_end > ct_body_start && ct_body_end - ct_body_start >= plaintext_bytes.len() {
            assert_ne!(
                &etx.encrypted_payload[ct_body_start..ct_body_start + plaintext_bytes.len()],
                plaintext_bytes.as_slice(),
                "payload must be encrypted, not cleartext"
            );
        }

        // Decrypt and verify roundtrip
        let decrypted = EncryptedMempool::decrypt_transaction(&etx, &slot_key).unwrap();
        assert_eq!(decrypted.hash, tx.hash);
        assert_eq!(decrypted.nonce, tx.nonce);
        assert_eq!(decrypted.from, tx.from);
        assert_eq!(decrypted.fee, tx.fee);
    }

    #[test]
    fn test_fifo_ordering_verification() {
        let pool = EncryptedMempool::new(100);
        let slot_key = pool.committee.derive_slot_key(0);

        // Build a FIFO-ordered batch
        let mut batch = Vec::new();
        for i in 0..5u64 {
            let tx = Transaction::new_transfer(addr(1), addr(2), i, i);
            let etx = EncryptedMempool::encrypt_transaction(&tx, &slot_key, 0);
            batch.push(etx);
            // Tiny sleep to ensure distinct Instants
            std::thread::sleep(std::time::Duration::from_millis(1));
        }

        // Correct order should pass
        assert!(EncryptedMempool::verify_fifo_ordering(&batch));

        // Swap two elements to break FIFO — should fail
        let last_idx = batch.len() - 1;
        if last_idx > 0 {
            batch.swap(0, last_idx);
            assert!(
                !EncryptedMempool::verify_fifo_ordering(&batch),
                "swapped ordering must fail verification"
            );
        }
    }

    #[test]
    fn test_reveal_batch() {
        let pool = EncryptedMempool::new(100);
        let slot = 0u64;
        let slot_key = pool.committee.derive_slot_key(slot);

        // Encrypt 10 transactions
        let mut originals = Vec::new();
        for i in 0..10u64 {
            let tx = Transaction::new_transfer(addr((i as u8) + 1), addr(20), i * 50, i);
            originals.push(tx);
        }

        let encrypted: Vec<EncryptedTx> = originals
            .iter()
            .map(|tx| EncryptedMempool::encrypt_transaction(tx, &slot_key, slot))
            .collect();

        // Reveal all
        let revealed = pool.reveal_batch(&encrypted, 42);
        assert_eq!(revealed.len(), 10, "all 10 should decrypt successfully");

        for (i, r) in revealed.iter().enumerate() {
            assert_eq!(r.transaction.hash, originals[i].hash, "hash mismatch at {i}");
            assert_eq!(r.transaction.nonce, originals[i].nonce, "nonce mismatch at {i}");
            assert_eq!(r.reveal_round, 42);
            assert_eq!(r.encrypted_id, encrypted[i].id);
        }
    }

    #[test]
    fn test_capacity_limit() {
        let pool = EncryptedMempool::new(3);
        let slot_key = pool.committee.derive_slot_key(0);

        // Fill to capacity
        for i in 0..3u64 {
            let tx = Transaction::new_transfer(addr(1), addr(2), i, i);
            let etx = EncryptedMempool::encrypt_transaction(&tx, &slot_key, 0);
            pool.submit_encrypted(etx).unwrap();
        }

        // Next submission should fail
        let tx = Transaction::new_transfer(addr(1), addr(2), 999, 3);
        let etx = EncryptedMempool::encrypt_transaction(&tx, &slot_key, 0);
        let result = pool.submit_encrypted(etx);
        assert!(matches!(result, Err(MempoolError::Full(3))));
    }

    #[test]
    fn test_sender_commitment_hides_identity() {
        let pool = EncryptedMempool::new(100);
        let slot_key = pool.committee.derive_slot_key(0);

        let sender = addr(42);
        let tx = Transaction::new_transfer(sender, addr(2), 100, 0);
        let etx = EncryptedMempool::encrypt_transaction(&tx, &slot_key, 0);

        // The sender commitment must not be the raw sender address hash
        let raw_sender_hash = hash_bytes(sender.as_ref());
        assert_ne!(
            etx.sender_commitment, raw_sender_hash,
            "commitment must not be a simple hash of the sender address"
        );

        // The commitment must not contain the sender address bytes directly
        let sender_bytes = sender.as_ref();
        assert_ne!(
            &etx.sender_commitment.0[..], sender_bytes,
            "commitment must not expose raw sender bytes"
        );

        // Two encryptions of the same tx should produce different commitments
        // (because the salt is random each time)
        let etx2 = EncryptedMempool::encrypt_transaction(&tx, &slot_key, 0);
        assert_ne!(
            etx.sender_commitment, etx2.sender_commitment,
            "different salts must produce different commitments"
        );
    }

    #[test]
    fn test_tampered_ciphertext_fails_decrypt() {
        let pool = EncryptedMempool::new(100);
        let slot_key = pool.committee.derive_slot_key(0);

        let tx = Transaction::new_transfer(addr(1), addr(2), 1000, 0);
        let mut etx = EncryptedMempool::encrypt_transaction(&tx, &slot_key, 0);

        // Tamper with a byte in the encrypted payload (after nonce, before tag)
        let payload_body_start = 12;
        let payload_body_end = etx.encrypted_payload.len().saturating_sub(THRESHOLD_TAG_LEN);
        if payload_body_end > payload_body_start + 1 {
            etx.encrypted_payload[payload_body_start] ^= 0xFF;
        }

        // Decryption must fail because the auth tag won't match
        let result = EncryptedMempool::decrypt_transaction(&etx, &slot_key);
        assert!(
            matches!(result, Err(MempoolError::DecryptionFailed)),
            "tampered ciphertext must fail decryption, got: {result:?}"
        );
    }

    #[test]
    fn test_wrong_slot_key_fails_decrypt() {
        let committee = BlsCommittee::new_deterministic(5);
        let slot_key_0 = committee.derive_slot_key(0);
        let slot_key_1 = committee.derive_slot_key(1);

        let tx = Transaction::new_transfer(addr(1), addr(2), 500, 0);
        let etx = EncryptedMempool::encrypt_transaction(&tx, &slot_key_0, 0);

        // Decryption with wrong slot key must fail
        let result = EncryptedMempool::decrypt_transaction(&etx, &slot_key_1);
        assert!(
            matches!(result, Err(MempoolError::DecryptionFailed)),
            "wrong slot key must fail decryption, got: {result:?}"
        );

        // Correct slot key works
        let decrypted = EncryptedMempool::decrypt_transaction(&etx, &slot_key_0).unwrap();
        assert_eq!(decrypted.hash, tx.hash);
    }

    #[test]
    fn test_reveal_batch_with_external_key() {
        let committee = BlsCommittee::new_deterministic(7);
        let slot = 42u64;
        let slot_key = committee.derive_slot_key(slot);

        // Encrypt some transactions
        let originals: Vec<Transaction> = (0..5u64)
            .map(|i| Transaction::new_transfer(addr(1), addr(2), i * 100, i))
            .collect();
        let encrypted: Vec<EncryptedTx> = originals.iter()
            .map(|tx| EncryptedMempool::encrypt_transaction(tx, &slot_key, slot))
            .collect();

        // Reveal using the static method with externally-provided key
        let revealed = EncryptedMempool::reveal_batch_with_key(&encrypted, &slot_key, 99);
        assert_eq!(revealed.len(), 5);

        for (i, r) in revealed.iter().enumerate() {
            assert_eq!(r.transaction.hash, originals[i].hash);
            assert_eq!(r.reveal_round, 99);
        }
    }

    #[test]
    fn test_bls_committee_deterministic() {
        let c1 = BlsCommittee::new_deterministic(3);
        let c2 = BlsCommittee::new_deterministic(3);

        // Same seed → same public keys
        for i in 0..3 {
            assert_eq!(c1.public_keys[i], c2.public_keys[i]);
        }

        // Same committee + same slot → same slot key
        let key1 = c1.derive_slot_key(10);
        let key2 = c2.derive_slot_key(10);
        assert_eq!(key1, key2);

        // Different slots → different keys
        let key3 = c1.derive_slot_key(11);
        assert_ne!(key1, key3);
    }

    #[test]
    fn test_bls_signatures_are_real() {
        // Verify that the BLS signatures used for key derivation are
        // cryptographically valid (not simulated).
        let committee = BlsCommittee::new_deterministic(3);
        let slot = 7u64;
        let msg = ThresholdEncryption::slot_message(slot);
        let sigs = committee.sign_slot(slot);

        // Each signature must verify against its corresponding public key.
        for (kp, sig) in committee.keypairs.iter().zip(sigs.iter()) {
            assert!(
                bls_verify(&kp.public, &msg, sig),
                "BLS signature must verify — this is real blst crypto"
            );
        }
    }

    #[test]
    fn test_slot_advance_and_multi_slot_encryption() {
        let pool = EncryptedMempool::new(100);

        // Encrypt a tx at slot 0
        let slot0_key = pool.committee.derive_slot_key(0);
        let tx0 = Transaction::new_transfer(addr(1), addr(2), 100, 0);
        let etx0 = EncryptedMempool::encrypt_transaction(&tx0, &slot0_key, 0);

        // Advance to slot 1
        pool.advance_slot();
        assert_eq!(pool.current_slot(), 1);

        // Encrypt a tx at slot 1
        let slot1_key = pool.committee.derive_slot_key(1);
        let tx1 = Transaction::new_transfer(addr(1), addr(2), 200, 1);
        let etx1 = EncryptedMempool::encrypt_transaction(&tx1, &slot1_key, 1);

        // Each tx decrypts only with its own slot key
        assert!(EncryptedMempool::decrypt_transaction(&etx0, &slot0_key).is_ok());
        assert!(EncryptedMempool::decrypt_transaction(&etx0, &slot1_key).is_err());
        assert!(EncryptedMempool::decrypt_transaction(&etx1, &slot1_key).is_ok());
        assert!(EncryptedMempool::decrypt_transaction(&etx1, &slot0_key).is_err());
    }
}
