use crate::hash::{Hash256, hash_bytes};
use rayon::prelude::*;
use serde::{Deserialize, Serialize};

/// A BLAKE3-based transaction commitment.
/// This is the fast-path commitment used for throughput — each transaction
/// is hashed into a 256-bit commitment that can be independently verified.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TransactionCommitment {
    /// The BLAKE3 hash of the serialized transaction data.
    pub hash: Hash256,
    /// Domain separation tag so different transaction types never collide.
    pub domain: u8,
}

/// Domain tags for transaction types.
pub mod domains {
    pub const TRANSFER: u8 = 0x01;
    pub const SETTLE: u8 = 0x02;
    pub const SWAP: u8 = 0x03;
    pub const ESCROW: u8 = 0x04;
    pub const STAKE: u8 = 0x05;
    pub const WASM_CALL: u8 = 0x06;
    pub const MULTISIG: u8 = 0x07;
}

/// Commit a single transaction. Domain-separated BLAKE3 hash.
#[inline]
pub fn commit_transaction(domain: u8, data: &[u8]) -> TransactionCommitment {
    let mut hasher = blake3::Hasher::new_derive_key("ARC-chain-tx-v1");
    hasher.update(&[domain]);
    hasher.update(data);
    TransactionCommitment {
        hash: Hash256(*hasher.finalize().as_bytes()),
        domain,
    }
}

/// Batch-commit many transactions in parallel using Rayon.
/// This is the core throughput function — splits work across all CPU cores.
pub fn batch_commit_parallel(transactions: &[(u8, &[u8])]) -> Vec<TransactionCommitment> {
    transactions
        .par_iter()
        .map(|(domain, data)| commit_transaction(*domain, data))
        .collect()
}

/// Commit raw bytes without domain separation (for benchmarking raw BLAKE3 speed).
#[inline]
pub fn commit_raw(data: &[u8]) -> Hash256 {
    hash_bytes(data)
}

/// Batch-commit raw bytes in parallel (benchmarking raw throughput).
pub fn batch_commit_raw_parallel(items: &[&[u8]]) -> Vec<Hash256> {
    items.par_iter().map(|data| hash_bytes(data)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_commit_deterministic() {
        let a = commit_transaction(domains::TRANSFER, b"tx-data-1");
        let b = commit_transaction(domains::TRANSFER, b"tx-data-1");
        assert_eq!(a.hash, b.hash);
    }

    #[test]
    fn test_domain_separation() {
        let transfer = commit_transaction(domains::TRANSFER, b"same-data");
        let swap = commit_transaction(domains::SWAP, b"same-data");
        assert_ne!(transfer.hash, swap.hash);
    }

    #[test]
    fn test_batch_commit() {
        let txns: Vec<(u8, &[u8])> = vec![
            (domains::TRANSFER, b"tx1" as &[u8]),
            (domains::SETTLE, b"tx2"),
            (domains::SWAP, b"tx3"),
        ];
        let commits = batch_commit_parallel(&txns);
        assert_eq!(commits.len(), 3);
        // Each should match individual commit
        assert_eq!(commits[0].hash, commit_transaction(domains::TRANSFER, b"tx1").hash);
        assert_eq!(commits[1].hash, commit_transaction(domains::SETTLE, b"tx2").hash);
    }

    #[test]
    fn test_batch_1m() {
        let data: Vec<[u8; 128]> = (0..1_000_000u32)
            .map(|i| {
                let mut buf = [0u8; 128];
                buf[..4].copy_from_slice(&i.to_le_bytes());
                buf
            })
            .collect();
        let txns: Vec<(u8, &[u8])> = data.iter().map(|d| (domains::TRANSFER, d.as_slice())).collect();
        let commits = batch_commit_parallel(&txns);
        assert_eq!(commits.len(), 1_000_000);
    }
}
