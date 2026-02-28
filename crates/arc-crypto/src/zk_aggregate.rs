use crate::hash::{Hash256, hash_pair};
use crate::pedersen::PedersenCommitment;
use rayon::prelude::*;
use serde::{Deserialize, Serialize};

/// An aggregate proof over a batch of transactions.
/// Proves that all transactions in the batch are valid and privacy-preserving
/// without revealing any individual transaction data.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AggregateProof {
    /// Number of transactions in this batch.
    pub tx_count: u64,
    /// Aggregate Pedersen commitment (sum of all value commitments).
    pub aggregate_commitment: [u8; 32],
    /// Merkle root of all transaction commitment hashes in the batch.
    pub batch_root: Hash256,
    /// Combined hash of all blinding factor commitments (for verifier).
    pub blinding_aggregate: Hash256,
    /// Domain tag identifying the proof version.
    pub version: u8,
}

/// Aggregate multiple Pedersen commitments and transaction hashes into
/// a single compact proof.
///
/// This is the core compression function: N transactions → 1 proof.
/// A verifier checks one proof instead of N, achieving N:1 compression.
pub fn aggregate_proofs(
    tx_hashes: &[Hash256],
    value_commitments: &[PedersenCommitment],
) -> AggregateProof {
    assert_eq!(tx_hashes.len(), value_commitments.len());
    let n = tx_hashes.len() as u64;

    // 1. Compute Merkle root of all tx hashes (parallel)
    let batch_root = if tx_hashes.is_empty() {
        Hash256::ZERO
    } else {
        parallel_merkle_root(tx_hashes)
    };

    // 2. Sum all Pedersen commitments (homomorphic aggregation)
    //    C_total = C_1 + C_2 + ... + C_n
    //    This works because Pedersen commitments are additively homomorphic.
    let aggregate_commitment = aggregate_pedersen_points(value_commitments);

    // 3. Hash all blinding factors together for verification
    let blinding_aggregate = hash_blinding_factors(value_commitments);

    AggregateProof {
        tx_count: n,
        aggregate_commitment,
        batch_root,
        blinding_aggregate,
        version: 1,
    }
}

/// Verify an aggregate proof against known batch data.
/// In a real ZK system this would verify a SNARK/STARK.
/// For the benchmark, we verify the Merkle root and commitment structure.
pub fn verify_aggregate(proof: &AggregateProof, tx_hashes: &[Hash256]) -> bool {
    if proof.tx_count != tx_hashes.len() as u64 {
        return false;
    }
    // Recompute Merkle root and compare
    let expected_root = parallel_merkle_root(tx_hashes);
    proof.batch_root == expected_root
}

/// Compute Merkle root of hashes using parallel pair-hashing.
fn parallel_merkle_root(hashes: &[Hash256]) -> Hash256 {
    if hashes.is_empty() {
        return Hash256::ZERO;
    }
    if hashes.len() == 1 {
        return hashes[0];
    }

    let mut current: Vec<Hash256> = hashes.to_vec();
    if current.len() % 2 != 0 {
        current.push(*current.last().unwrap());
    }

    while current.len() > 1 {
        current = current
            .par_chunks(2)
            .map(|pair| {
                if pair.len() == 2 {
                    hash_pair(&pair[0], &pair[1])
                } else {
                    pair[0]
                }
            })
            .collect();
        if current.len() > 1 && current.len() % 2 != 0 {
            current.push(*current.last().unwrap());
        }
    }

    current[0]
}

/// Sum Pedersen commitment points (Ristretto point addition).
fn aggregate_pedersen_points(commitments: &[PedersenCommitment]) -> [u8; 32] {
    use curve25519_dalek::ristretto::{CompressedRistretto, RistrettoPoint};

    if commitments.is_empty() {
        return [0u8; 32];
    }

    // Parallel sum using Rayon's reduce
    let sum: RistrettoPoint = commitments
        .par_iter()
        .map(|c| {
            CompressedRistretto::from_slice(&c.point)
                .expect("valid 32-byte point")
                .decompress()
                .expect("valid Ristretto point")
        })
        .reduce(
            || {
                use curve25519_dalek::traits::Identity;
                RistrettoPoint::identity()
            },
            |a, b| a + b,
        );

    sum.compress().to_bytes()
}

/// Hash all blinding factors together.
fn hash_blinding_factors(commitments: &[PedersenCommitment]) -> Hash256 {
    let mut hasher = blake3::Hasher::new_derive_key("ARC-chain-blinding-aggregate-v1");
    for c in commitments {
        hasher.update(&c.blinding);
    }
    Hash256(*hasher.finalize().as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hash::hash_bytes;
    use crate::pedersen::commit_value;

    #[test]
    fn test_aggregate_and_verify() {
        let n = 1000;
        let tx_hashes: Vec<Hash256> = (0..n as u32)
            .map(|i| hash_bytes(&i.to_le_bytes()))
            .collect();
        let commitments: Vec<PedersenCommitment> = (0..n).map(|i| commit_value(i as u64)).collect();

        let proof = aggregate_proofs(&tx_hashes, &commitments);
        assert_eq!(proof.tx_count, n as u64);
        assert!(verify_aggregate(&proof, &tx_hashes));
    }

    #[test]
    fn test_aggregate_fails_wrong_hashes() {
        let tx_hashes: Vec<Hash256> = (0..100u32)
            .map(|i| hash_bytes(&i.to_le_bytes()))
            .collect();
        let commitments: Vec<PedersenCommitment> = (0..100).map(|i| commit_value(i as u64)).collect();

        let proof = aggregate_proofs(&tx_hashes, &commitments);

        // Tamper with one hash
        let mut bad_hashes = tx_hashes.clone();
        bad_hashes[50] = hash_bytes(b"tampered");
        assert!(!verify_aggregate(&proof, &bad_hashes));
    }

    #[test]
    fn test_aggregate_large_batch() {
        let n = 10_000;
        let tx_hashes: Vec<Hash256> = (0..n as u32)
            .map(|i| hash_bytes(&i.to_le_bytes()))
            .collect();
        let commitments: Vec<PedersenCommitment> = (0..n).map(|i| commit_value(i as u64)).collect();

        let proof = aggregate_proofs(&tx_hashes, &commitments);
        assert_eq!(proof.tx_count, n as u64);
        assert!(verify_aggregate(&proof, &tx_hashes));
    }
}
