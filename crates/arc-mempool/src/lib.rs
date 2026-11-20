use arc_crypto::bls::{BlsKeypair, BlsPublicKey, BlsSignature, bls_keygen, bls_sign};
use arc_crypto::{Hash256, ThresholdEncryption, hash_bytes};
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

pub struct Mempool {
    queue: SegQueue<Transaction>,
    seen: DashMap<[u8; 32], ()>,
    capacity: usize,
    count: RwLock<usize>,
}

impl Mempool {
    pub fn new(capacity: usize) -> Self {
        Self {
            queue: SegQueue::new(),
            seen: DashMap::new(),
            capacity,
            count: RwLock::new(0),
        }
    }

    pub fn insert(&self, tx: Transaction) -> Result<(), MempoolError> {
        let mut count = self.count.write();
        if *count >= self.capacity {
            return Err(MempoolError::Full(self.capacity));
        }
        use dashmap::mapref::entry::Entry;
        match self.seen.entry(tx.hash.0) {
            Entry::Occupied(_) => {
                return Err(MempoolError::Duplicate(tx.hash));
            }
            Entry::Vacant(e) => {
                e.insert(());
            }
        }
        *count += 1;
        drop(count);

        self.queue.push(tx);

        Ok(())
    }

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

    pub fn len(&self) -> usize {
        *self.count.read()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn contains(&self, hash: &Hash256) -> bool {
        self.seen.contains_key(&hash.0)
    }

    pub fn clear(&self) {
        while self.queue.pop().is_some() {}
        self.seen.clear();
        *self.count.write() = 0;
    }
}

impl Default for Mempool {
    fn default() -> Self {
        Self::new(1_000_000)
    }
}

#[derive(Clone, Debug)]
pub struct EncryptedTx {
    pub id: Hash256,
    pub encrypted_payload: Vec<u8>,
    pub target_slot: u64,
    pub encrypted_key: Vec<u8>,
    pub submitted_at: std::time::Instant,
    pub sender_commitment: Hash256,
}

#[derive(Clone, Debug)]
pub struct RevealedTx {
    pub encrypted_id: Hash256,
    pub transaction: Transaction,
    pub reveal_round: u64,
}

pub struct BlsCommittee {
    pub keypairs: Vec<BlsKeypair>,
    pub public_keys: Vec<BlsPublicKey>,
}

impl BlsCommittee {
    pub fn new_deterministic(n: usize) -> Self {
        let keypairs: Vec<BlsKeypair> = (0..n)
            .map(|i| bls_keygen(format!("ARC-committee-member-{i}").as_bytes()))
            .collect();
        let public_keys: Vec<BlsPublicKey> = keypairs.iter().map(|kp| kp.public.clone()).collect();
        Self {
            keypairs,
            public_keys,
        }
    }

    pub fn sign_slot(&self, slot: u64) -> Vec<BlsSignature> {
        let msg = ThresholdEncryption::slot_message(slot);
        self.keypairs
            .iter()
            .map(|kp| bls_sign(&kp.secret, &msg))
            .collect()
    }

    pub fn derive_slot_key(&self, slot: u64) -> [u8; 32] {
        let sigs = self.sign_slot(slot);
        ThresholdEncryption::derive_slot_key(slot, &sigs)
    }
}

pub struct EncryptedMempool {
    pending: RwLock<VecDeque<EncryptedTx>>,
    seen: DashSet<[u8; 32]>,
    capacity: usize,
    committee: BlsCommittee,
    current_slot: RwLock<u64>,
}

impl EncryptedMempool {
    pub fn new(capacity: usize) -> Self {
        Self {
            pending: RwLock::new(VecDeque::new()),
            seen: DashSet::new(),
            capacity,
            committee: BlsCommittee::new_deterministic(5),
            current_slot: RwLock::new(0),
        }
    }

    pub fn with_committee(capacity: usize, committee: BlsCommittee) -> Self {
        Self {
            pending: RwLock::new(VecDeque::new()),
            seen: DashSet::new(),
            capacity,
            committee,
            current_slot: RwLock::new(0),
        }
    }

    pub fn committee_pubkeys(&self) -> &[BlsPublicKey] {
        &self.committee.public_keys
    }

    pub fn current_slot(&self) -> u64 {
        *self.current_slot.read()
    }

    pub fn advance_slot(&self) {
        let mut slot = self.current_slot.write();
        *slot += 1;
    }

    pub fn set_slot(&self, slot: u64) {
        *self.current_slot.write() = slot;
    }

    pub fn submit_encrypted(&self, etx: EncryptedTx) -> Result<(), MempoolError> {
        {
            let pending = self.pending.read();
            if pending.len() >= self.capacity {
                return Err(MempoolError::Full(self.capacity));
            }
        }

        if !self.seen.insert(etx.id.0) {
            return Err(MempoolError::Duplicate(etx.id));
        }

        {
            let mut pending = self.pending.write();
            pending.push_back(etx);
        }

        Ok(())
    }

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

    pub fn reveal_batch(&self, batch: &[EncryptedTx], reveal_round: u64) -> Vec<RevealedTx> {
        let mut revealed = Vec::with_capacity(batch.len());

        for etx in batch {
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

    pub fn verify_fifo_ordering(batch: &[EncryptedTx]) -> bool {
        for window in batch.windows(2) {
            if window[1].submitted_at < window[0].submitted_at {
                return false;
            }
        }
        true
    }

    pub fn pending_count(&self) -> usize {
        self.pending.read().len()
    }

    pub fn encrypt_transaction(
        tx: &Transaction,
        slot_key: &[u8; 32],
        target_slot: u64,
    ) -> EncryptedTx {
        use rand::Rng;
        let mut rng = rand::thread_rng();

        let plaintext = bincode::serialize(tx).expect("transaction must serialize");

        let per_tx_key: [u8; 32] = rng.r#gen();

        let encrypted_payload = ThresholdEncryption::encrypt(&per_tx_key, &plaintext);

        let encrypted_key = ThresholdEncryption::encrypt(slot_key, &per_tx_key);

        let id = hash_bytes(&encrypted_payload);

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

    pub fn decrypt_transaction(
        etx: &EncryptedTx,
        slot_key: &[u8; 32],
    ) -> Result<Transaction, MempoolError> {
        let per_tx_key_bytes = ThresholdEncryption::decrypt(slot_key, &etx.encrypted_key)
            .ok_or(MempoolError::DecryptionFailed)?;

        if per_tx_key_bytes.len() != 32 {
            return Err(MempoolError::DecryptionFailed);
        }
        let mut per_tx_key = [0u8; 32];
        per_tx_key.copy_from_slice(&per_tx_key_bytes);

        let plaintext = ThresholdEncryption::decrypt(&per_tx_key, &etx.encrypted_payload)
            .ok_or(MempoolError::DecryptionFailed)?;

        bincode::deserialize::<Transaction>(&plaintext)
            .map_err(|_| MempoolError::DeserializationFailed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use arc_crypto::bls::bls_verify;
    use arc_crypto::{THRESHOLD_TAG_LEN, hash_bytes};

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
        pool.insert(Transaction::new_transfer(addr(1), addr(2), 1, 0))
            .unwrap();
        pool.insert(Transaction::new_transfer(addr(1), addr(2), 2, 1))
            .unwrap();
        let result = pool.insert(Transaction::new_transfer(addr(1), addr(2), 3, 2));
        assert!(result.is_err());
    }

    #[test]
    fn test_fifo_order() {
        let pool = Mempool::new(100);
        for i in 0..10u64 {
            pool.insert(Transaction::new_transfer(addr(1), addr(2), i, i))
                .unwrap();
        }
        let batch = pool.drain(10);
        assert_eq!(batch.len(), 10);
        assert_eq!(batch[0].nonce, 0);
        assert_eq!(batch[9].nonce, 9);
    }

    #[test]
    fn test_encrypted_submit_and_drain_fifo() {
        let pool = EncryptedMempool::new(100);
        let slot = pool.current_slot();
        let slot_key = pool.committee.derive_slot_key(slot);

        let mut expected_ids = Vec::new();
        for i in 0..5u64 {
            let tx = Transaction::new_transfer(addr(1), addr(2), i * 100, i);
            let etx = EncryptedMempool::encrypt_transaction(&tx, &slot_key, slot);
            expected_ids.push(etx.id);
            pool.submit_encrypted(etx).unwrap();
        }

        assert_eq!(pool.pending_count(), 5);

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

        let plaintext_bytes = bincode::serialize(&tx).unwrap();
        let ct_body_start = 12;
        let ct_body_end = etx
            .encrypted_payload
            .len()
            .saturating_sub(THRESHOLD_TAG_LEN);
        if ct_body_end > ct_body_start && ct_body_end - ct_body_start >= plaintext_bytes.len() {
            assert_ne!(
                &etx.encrypted_payload[ct_body_start..ct_body_start + plaintext_bytes.len()],
                plaintext_bytes.as_slice(),
                "payload must be encrypted, not cleartext"
            );
        }

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

        let mut batch = Vec::new();
        for i in 0..5u64 {
            let tx = Transaction::new_transfer(addr(1), addr(2), i, i);
            let etx = EncryptedMempool::encrypt_transaction(&tx, &slot_key, 0);
            batch.push(etx);
            std::thread::sleep(std::time::Duration::from_millis(1));
        }

        assert!(EncryptedMempool::verify_fifo_ordering(&batch));

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

        let mut originals = Vec::new();
        for i in 0..10u64 {
            let tx = Transaction::new_transfer(addr((i as u8) + 1), addr(20), i * 50, i);
            originals.push(tx);
        }

        let encrypted: Vec<EncryptedTx> = originals
            .iter()
            .map(|tx| EncryptedMempool::encrypt_transaction(tx, &slot_key, slot))
            .collect();

        let revealed = pool.reveal_batch(&encrypted, 42);
        assert_eq!(revealed.len(), 10, "all 10 should decrypt successfully");

        for (i, r) in revealed.iter().enumerate() {
            assert_eq!(
                r.transaction.hash, originals[i].hash,
                "hash mismatch at {i}"
            );
            assert_eq!(
                r.transaction.nonce, originals[i].nonce,
                "nonce mismatch at {i}"
            );
            assert_eq!(r.reveal_round, 42);
            assert_eq!(r.encrypted_id, encrypted[i].id);
        }
    }

    #[test]
    fn test_capacity_limit() {
        let pool = EncryptedMempool::new(3);
        let slot_key = pool.committee.derive_slot_key(0);

        for i in 0..3u64 {
            let tx = Transaction::new_transfer(addr(1), addr(2), i, i);
            let etx = EncryptedMempool::encrypt_transaction(&tx, &slot_key, 0);
            pool.submit_encrypted(etx).unwrap();
        }

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

        let raw_sender_hash = hash_bytes(sender.as_ref());
        assert_ne!(
            etx.sender_commitment, raw_sender_hash,
            "commitment must not be a simple hash of the sender address"
        );

        let sender_bytes = sender.as_ref();
        assert_ne!(
            &etx.sender_commitment.0[..],
            sender_bytes,
            "commitment must not expose raw sender bytes"
        );

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

        let payload_body_start = 12;
        let payload_body_end = etx
            .encrypted_payload
            .len()
            .saturating_sub(THRESHOLD_TAG_LEN);
        if payload_body_end > payload_body_start + 1 {
            etx.encrypted_payload[payload_body_start] ^= 0xFF;
        }

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

        let result = EncryptedMempool::decrypt_transaction(&etx, &slot_key_1);
        assert!(
            matches!(result, Err(MempoolError::DecryptionFailed)),
            "wrong slot key must fail decryption, got: {result:?}"
        );

        let decrypted = EncryptedMempool::decrypt_transaction(&etx, &slot_key_0).unwrap();
        assert_eq!(decrypted.hash, tx.hash);
    }

    #[test]
    fn test_reveal_batch_with_external_key() {
        let committee = BlsCommittee::new_deterministic(7);
        let slot = 42u64;
        let slot_key = committee.derive_slot_key(slot);

        let originals: Vec<Transaction> = (0..5u64)
            .map(|i| Transaction::new_transfer(addr(1), addr(2), i * 100, i))
            .collect();
        let encrypted: Vec<EncryptedTx> = originals
            .iter()
            .map(|tx| EncryptedMempool::encrypt_transaction(tx, &slot_key, slot))
            .collect();

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

        for i in 0..3 {
            assert_eq!(c1.public_keys[i], c2.public_keys[i]);
        }

        let key1 = c1.derive_slot_key(10);
        let key2 = c2.derive_slot_key(10);
        assert_eq!(key1, key2);

        let key3 = c1.derive_slot_key(11);
        assert_ne!(key1, key3);
    }

    #[test]
    fn test_bls_signatures_are_real() {
        let committee = BlsCommittee::new_deterministic(3);
        let slot = 7u64;
        let msg = ThresholdEncryption::slot_message(slot);
        let sigs = committee.sign_slot(slot);

        for (kp, sig) in committee.keypairs.iter().zip(sigs.iter()) {
            assert!(
                bls_verify(&kp.public, &msg, sig),
                "BLS signature must verify - this is real blst crypto"
            );
        }
    }

    #[test]
    fn test_slot_advance_and_multi_slot_encryption() {
        let pool = EncryptedMempool::new(100);

        let slot0_key = pool.committee.derive_slot_key(0);
        let tx0 = Transaction::new_transfer(addr(1), addr(2), 100, 0);
        let etx0 = EncryptedMempool::encrypt_transaction(&tx0, &slot0_key, 0);

        pool.advance_slot();
        assert_eq!(pool.current_slot(), 1);

        let slot1_key = pool.committee.derive_slot_key(1);
        let tx1 = Transaction::new_transfer(addr(1), addr(2), 200, 1);
        let etx1 = EncryptedMempool::encrypt_transaction(&tx1, &slot1_key, 1);

        assert!(EncryptedMempool::decrypt_transaction(&etx0, &slot0_key).is_ok());
        assert!(EncryptedMempool::decrypt_transaction(&etx0, &slot1_key).is_err());
        assert!(EncryptedMempool::decrypt_transaction(&etx1, &slot1_key).is_ok());
        assert!(EncryptedMempool::decrypt_transaction(&etx1, &slot0_key).is_err());
    }
}
