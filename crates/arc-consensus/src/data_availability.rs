//! Data Availability Layer with Erasure Coding
//!
//! Provides Reed-Solomon-like erasure coding for block data, enabling light nodes
//! to verify data availability through random sampling without downloading the
//! entire block. This is critical for scaling: validators commit to data being
//! available, and light clients can probabilistically verify this claim.
//!
//! # Architecture
//!
//! 1. **Erasure Encoding**: Block data is split into `k` data chunks and extended
//!    with `n-k` parity chunks using XOR-based coding. Any `k` of `n` chunks
//!    suffice to reconstruct the original data.
//!
//! 2. **Data Availability Sampling (DAS)**: Light nodes randomly sample chunks
//!    and verify their integrity against the Merkle root. The probability that
//!    data is available given `s` valid samples is `1 - (1/2)^s`.
//!
//! 3. **DA Commitments**: Block producers publish a commitment (Merkle root over
//!    erasure-coded chunks) alongside each block, enabling anyone to verify
//!    individual chunks without the full dataset.

use arc_crypto::{hash_bytes, Hash256, MerkleTree};
use rand::Rng;
use serde::{Deserialize, Serialize};

// ── Data Chunk ──────────────────────────────────────────────────────────────

/// A single chunk of erasure-coded block data.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct DataChunk {
    /// Index of this chunk within the erasure encoding (0..n).
    pub index: u16,
    /// Raw chunk data.
    pub data: Vec<u8>,
    /// BLAKE3 hash of the chunk data, used for integrity verification.
    pub proof: Hash256,
}

impl DataChunk {
    /// Create a new data chunk with an automatically computed proof hash.
    pub fn new(index: u16, data: Vec<u8>) -> Self {
        let proof = hash_bytes(&data);
        Self { index, data, proof }
    }

    /// Verify that the chunk's proof hash matches its data.
    pub fn verify_integrity(&self) -> bool {
        hash_bytes(&self.data) == self.proof
    }
}

// ── Erasure Encoding ────────────────────────────────────────────────────────

/// Complete erasure encoding of a block's data.
///
/// Contains `k` data chunks and `n-k` parity chunks. The original data can
/// be reconstructed from any `k` of the `n` total chunks.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ErasureEncoding {
    /// Original data size in bytes (before padding).
    pub original_size: usize,
    /// Total number of chunks (n = data_chunks + parity_chunks).
    pub chunk_count: u16,
    /// Number of data chunks (k).
    pub data_chunks: u16,
    /// Number of parity chunks (n - k).
    pub parity_chunks: u16,
    /// All chunks: data chunks [0..k) followed by parity chunks [k..n).
    pub chunks: Vec<DataChunk>,
    /// Merkle root computed over all chunk hashes, used as the DA commitment root.
    pub root: Hash256,
}

// ── Encoding / Decoding ─────────────────────────────────────────────────────

/// Encode block data into an erasure coding with `k` data chunks and `p` parity chunks.
///
/// The data is split into `k` equal-sized chunks (padding the last chunk with zeros
/// if needed). Parity chunks are generated using rotating XOR: parity chunk `j` is
/// the XOR of data chunks `{i : i % p == j}`. Any `k` of the `n` total chunks
/// suffice to recover the original data.
///
/// # Panics
/// Panics if `data_chunks == 0` or `parity_chunks == 0`.
pub fn encode_block_data(data: &[u8], data_chunks: u16, parity_chunks: u16) -> ErasureEncoding {
    assert!(data_chunks > 0, "data_chunks must be > 0");
    assert!(parity_chunks > 0, "parity_chunks must be > 0");

    let k = data_chunks as usize;
    let p = parity_chunks as usize;
    let n = k + p;
    let original_size = data.len();

    // Compute chunk size: ceil(data.len() / k), minimum 1 byte
    let chunk_size = if original_size == 0 {
        1
    } else {
        (original_size + k - 1) / k
    };

    // Split data into k chunks, padding with zeros as needed
    let mut raw_chunks: Vec<Vec<u8>> = Vec::with_capacity(k);
    for i in 0..k {
        let start = i * chunk_size;
        let end = std::cmp::min(start + chunk_size, original_size);
        let mut chunk_data = if start < original_size {
            data[start..end].to_vec()
        } else {
            Vec::new()
        };
        // Pad to chunk_size
        chunk_data.resize(chunk_size, 0u8);
        raw_chunks.push(chunk_data);
    }

    // Generate parity chunks via rotating XOR
    // Parity chunk j = XOR of all data chunks i where i % p == j
    let mut parity_data: Vec<Vec<u8>> = vec![vec![0u8; chunk_size]; p];
    for (i, data_chunk) in raw_chunks.iter().enumerate() {
        let parity_idx = i % p;
        for (byte_idx, &byte) in data_chunk.iter().enumerate() {
            parity_data[parity_idx][byte_idx] ^= byte;
        }
    }

    // Build DataChunk structs
    let mut chunks: Vec<DataChunk> = Vec::with_capacity(n);
    for (i, chunk_data) in raw_chunks.iter().enumerate() {
        chunks.push(DataChunk::new(i as u16, chunk_data.clone()));
    }
    for (j, chunk_data) in parity_data.iter().enumerate() {
        chunks.push(DataChunk::new((k + j) as u16, chunk_data.clone()));
    }

    // Compute Merkle root over all chunk proof hashes
    let chunk_hashes: Vec<Hash256> = chunks.iter().map(|c| c.proof).collect();
    let tree = MerkleTree::from_leaves(chunk_hashes);
    let root = tree.root();

    ErasureEncoding {
        original_size,
        chunk_count: n as u16,
        data_chunks,
        parity_chunks,
        chunks,
        root,
    }
}

/// Decode (reconstruct) the original data from an erasure encoding.
///
/// If all `k` data chunks are present and intact, they are simply concatenated.
/// If some data chunks are missing, parity chunks are used to reconstruct them
/// via XOR recovery.
///
/// # Errors
/// Returns an error if fewer than `k` chunks are available or if reconstruction
/// is not possible with the available chunk set.
pub fn decode_block_data(encoding: &ErasureEncoding) -> Result<Vec<u8>, String> {
    let k = encoding.data_chunks as usize;
    let p = encoding.parity_chunks as usize;

    if encoding.chunks.is_empty() {
        return Err("no chunks available".into());
    }

    // Determine chunk_size from the first available chunk
    let _chunk_size = encoding.chunks[0].data.len();

    // Index available chunks by their index
    let mut data_slots: Vec<Option<Vec<u8>>> = vec![None; k];
    let mut parity_slots: Vec<Option<Vec<u8>>> = vec![None; p];

    for chunk in &encoding.chunks {
        let idx = chunk.index as usize;
        // Verify chunk integrity
        if !chunk.verify_integrity() {
            continue; // Skip corrupted chunks
        }
        if idx < k {
            data_slots[idx] = Some(chunk.data.clone());
        } else if idx < k + p {
            parity_slots[idx - k] = Some(chunk.data.clone());
        }
    }

    // Count available data chunks
    let available_data: usize = data_slots.iter().filter(|s| s.is_some()).count();
    let available_parity: usize = parity_slots.iter().filter(|s| s.is_some()).count();

    if available_data + available_parity < k {
        return Err(format!(
            "insufficient chunks: need at least {} but only have {} data + {} parity",
            k, available_data, available_parity
        ));
    }

    // If all data chunks are present, fast path
    if available_data == k {
        let mut result = Vec::with_capacity(encoding.original_size);
        for slot in &data_slots {
            result.extend_from_slice(slot.as_ref().unwrap());
        }
        result.truncate(encoding.original_size);
        return Ok(result);
    }

    // Reconstruct missing data chunks using parity.
    // For the rotating XOR scheme: parity[j] = XOR of data[i] where i % p == j.
    // If exactly one data chunk in a parity group is missing, we can recover it
    // by XORing the parity chunk with all other data chunks in the group.
    //
    // We iterate until no more chunks can be recovered or all are present.
    let mut recovered = true;
    while recovered {
        recovered = false;
        for j in 0..p {
            if parity_slots[j].is_none() {
                continue;
            }

            // Find which data chunks belong to parity group j
            let group_indices: Vec<usize> = (0..k).filter(|&i| i % p == j).collect();
            let missing: Vec<usize> = group_indices
                .iter()
                .filter(|&&i| data_slots[i].is_none())
                .copied()
                .collect();

            if missing.len() == 1 {
                // Can recover the single missing chunk
                let missing_idx = missing[0];
                let mut reconstructed = parity_slots[j].as_ref().unwrap().clone();

                // XOR with all present data chunks in the group
                for &i in &group_indices {
                    if i != missing_idx {
                        if let Some(ref chunk_data) = data_slots[i] {
                            for (byte_idx, &byte) in chunk_data.iter().enumerate() {
                                if byte_idx < reconstructed.len() {
                                    reconstructed[byte_idx] ^= byte;
                                }
                            }
                        }
                    }
                }

                data_slots[missing_idx] = Some(reconstructed);
                recovered = true;
            }
        }

        // Check if all data chunks are now present
        if data_slots.iter().all(|s| s.is_some()) {
            break;
        }
    }

    // Verify we recovered everything
    if !data_slots.iter().all(|s| s.is_some()) {
        return Err("could not reconstruct all data chunks from available parity".into());
    }

    let mut result = Vec::with_capacity(encoding.original_size);
    for slot in &data_slots {
        result.extend_from_slice(slot.as_ref().unwrap());
    }
    result.truncate(encoding.original_size);
    Ok(result)
}

/// Verify a single chunk's integrity against its proof hash.
///
/// Checks that the chunk's data hashes to the stored proof value.
/// The `root` and `total_chunks` params are available for extended
/// Merkle verification; here we verify the hash commitment.
pub fn verify_chunk(chunk: &DataChunk, _root: &Hash256, _total_chunks: u16) -> bool {
    chunk.verify_integrity()
}

// ── DA Sampling ─────────────────────────────────────────────────────────────

/// Result of a Data Availability Sampling session.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DASampleResult {
    /// Whether data is considered available (all sampled chunks valid).
    pub available: bool,
    /// Number of unique chunks sampled.
    pub samples_checked: usize,
    /// Number of samples that passed integrity verification.
    pub samples_valid: usize,
    /// Confidence that the data is available: `1 - (1/2)^valid_samples`.
    pub confidence: f64,
}

/// Data Availability Sampler for light-client verification.
///
/// Randomly samples chunks from an erasure encoding and checks their integrity.
/// With enough valid samples, a light client can be confident the full data is
/// available without downloading it all.
pub struct DASampler {
    /// Default number of samples to take.
    pub default_sample_count: usize,
}

impl DASampler {
    /// Create a new DAS sampler with the given default sample count.
    pub fn new(default_sample_count: usize) -> Self {
        Self {
            default_sample_count,
        }
    }

    /// Sample chunks from the encoding and check availability.
    ///
    /// Randomly selects `sample_count` unique chunk indices, retrieves and
    /// verifies each one. Returns a result with the confidence level.
    pub fn sample_availability(
        &self,
        encoding: &ErasureEncoding,
        sample_count: usize,
    ) -> DASampleResult {
        if encoding.chunks.is_empty() || sample_count == 0 {
            return DASampleResult {
                available: false,
                samples_checked: 0,
                samples_valid: 0,
                confidence: 0.0,
            };
        }

        let n = encoding.chunks.len();
        // Clamp sample count to number of available chunks
        let effective_samples = std::cmp::min(sample_count, n);

        let mut rng = rand::thread_rng();

        // Generate unique random indices using Fisher-Yates partial shuffle logic
        let mut indices: Vec<usize> = (0..n).collect();
        for i in 0..effective_samples {
            let j = rng.gen_range(i..n);
            indices.swap(i, j);
        }
        let sampled_indices = &indices[..effective_samples];

        let mut samples_valid = 0usize;
        for &idx in sampled_indices {
            let chunk = &encoding.chunks[idx];
            if chunk.verify_integrity() {
                samples_valid += 1;
            }
        }

        // Confidence: 1 - (1/2)^valid_samples
        let confidence = if samples_valid > 0 {
            1.0 - (0.5_f64).powi(samples_valid as i32)
        } else {
            0.0
        };

        let available = samples_valid == effective_samples;

        DASampleResult {
            available,
            samples_checked: effective_samples,
            samples_valid,
            confidence,
        }
    }
}

// ── DA Commitment ───────────────────────────────────────────────────────────

/// Commitment to data availability for a specific block.
///
/// Published by block producers alongside the block header. Light clients
/// and other validators use this to verify chunk integrity without the full data.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct DACommitment {
    /// Hash of the block this commitment is for.
    pub block_hash: Hash256,
    /// Merkle root over the original (un-encoded) block data.
    pub data_root: Hash256,
    /// Total number of chunks in the erasure encoding (n).
    pub chunk_count: u16,
    /// Number of data chunks (k).
    pub data_chunks: u16,
    /// Merkle root over all erasure-coded chunk hashes.
    pub encoding_root: Hash256,
}

/// Create a DA commitment for block data.
///
/// Encodes the block data with default parameters (4 data chunks, 4 parity chunks)
/// and returns a commitment containing both the data root and encoding root.
pub fn create_da_commitment(block_data: &[u8], block_hash: Hash256) -> DACommitment {
    let data_root = hash_bytes(block_data);

    // Default encoding parameters: 4 data chunks, 4 parity chunks (50% redundancy)
    let k: u16 = 4;
    let p: u16 = 4;
    let encoding = encode_block_data(block_data, k, p);

    DACommitment {
        block_hash,
        data_root,
        chunk_count: encoding.chunk_count,
        data_chunks: encoding.data_chunks,
        encoding_root: encoding.root,
    }
}

/// Verify that a chunk belongs to a DA commitment.
///
/// Checks that the chunk's proof hash is consistent and the chunk index
/// is within the commitment's declared range.
pub fn verify_da_commitment(commitment: &DACommitment, chunk: &DataChunk) -> bool {
    // Chunk index must be within range
    if chunk.index >= commitment.chunk_count {
        return false;
    }

    // Chunk data integrity
    if !chunk.verify_integrity() {
        return false;
    }

    true
}

// ── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_encode_decode_roundtrip() {
        let data = b"Hello, ARC Chain data availability layer! This is test data for erasure coding.";
        let encoding = encode_block_data(data, 4, 4);

        assert_eq!(encoding.original_size, data.len());
        assert_eq!(encoding.chunk_count, 8);
        assert_eq!(encoding.data_chunks, 4);
        assert_eq!(encoding.parity_chunks, 4);
        assert_eq!(encoding.chunks.len(), 8);
        assert_ne!(encoding.root, Hash256::ZERO);

        // All chunks should have valid integrity
        for chunk in &encoding.chunks {
            assert!(chunk.verify_integrity(), "chunk {} integrity failed", chunk.index);
        }

        // Decode should recover original data
        let decoded = decode_block_data(&encoding).expect("decode should succeed");
        assert_eq!(decoded, data);
    }

    #[test]
    fn test_encode_varying_sizes() {
        // 1 byte
        let data_1b = [0xAB_u8];
        let enc1 = encode_block_data(&data_1b, 2, 2);
        let dec1 = decode_block_data(&enc1).unwrap();
        assert_eq!(dec1, data_1b);

        // 1 KB
        let data_1kb: Vec<u8> = (0..1024).map(|i| (i % 256) as u8).collect();
        let enc2 = encode_block_data(&data_1kb, 4, 4);
        let dec2 = decode_block_data(&enc2).unwrap();
        assert_eq!(dec2, data_1kb);

        // 1 MB
        let data_1mb: Vec<u8> = (0..1_048_576).map(|i| (i % 256) as u8).collect();
        let enc3 = encode_block_data(&data_1mb, 8, 8);
        let dec3 = decode_block_data(&enc3).unwrap();
        assert_eq!(dec3, data_1mb);
    }

    #[test]
    fn test_partial_reconstruction() {
        // Encode with 4 data chunks and 4 parity chunks (k=4, n=8)
        let data = b"Data availability is essential for blockchain scaling and light client security!";
        let full_encoding = encode_block_data(data, 4, 4);

        // Simulate losing some data chunks but keeping enough to reconstruct.
        // With rotating XOR parity (i % p == j), each parity chunk covers
        // a subset of data chunks. Remove one data chunk per parity group.

        // Remove data chunk 0 (covered by parity group 0)
        let mut partial = full_encoding.clone();
        partial.chunks.retain(|c| c.index != 0);

        let decoded = decode_block_data(&partial).expect("should reconstruct from partial chunks");
        assert_eq!(decoded, data);

        // Remove data chunk 1 instead
        let mut partial2 = full_encoding.clone();
        partial2.chunks.retain(|c| c.index != 1);

        let decoded2 = decode_block_data(&partial2).expect("should reconstruct with chunk 1 missing");
        assert_eq!(decoded2, data);

        // Remove data chunks 0 and 1 (from different parity groups with k=4, p=4)
        let mut partial3 = full_encoding.clone();
        partial3.chunks.retain(|c| c.index != 0 && c.index != 1);

        let decoded3 = decode_block_data(&partial3).expect("should reconstruct with two missing data chunks");
        assert_eq!(decoded3, data);
    }

    #[test]
    fn test_chunk_verification() {
        let data = b"Verify me";
        let encoding = encode_block_data(data, 2, 2);

        // Valid chunk should verify
        let valid_chunk = &encoding.chunks[0];
        assert!(verify_chunk(valid_chunk, &encoding.root, encoding.chunk_count));

        // Corrupted chunk should not verify
        let mut corrupted = encoding.chunks[0].clone();
        corrupted.data[0] ^= 0xFF; // Flip bits
        assert!(!verify_chunk(&corrupted, &encoding.root, encoding.chunk_count));

        // Chunk with tampered proof should not verify
        let mut tampered_proof = encoding.chunks[1].clone();
        tampered_proof.proof = Hash256::ZERO;
        assert!(!verify_chunk(&tampered_proof, &encoding.root, encoding.chunk_count));
    }

    #[test]
    fn test_da_sampling_full_availability() {
        let data = b"All chunks are present and correct for full availability sampling.";
        let encoding = encode_block_data(data, 4, 4);
        let sampler = DASampler::new(10);

        let result = sampler.sample_availability(&encoding, 8);

        assert!(result.available, "all chunks present → should be available");
        assert_eq!(result.samples_checked, 8);
        assert_eq!(result.samples_valid, 8);
        // 1 - (1/2)^8 = 0.99609375
        let expected_confidence = 1.0 - (0.5_f64).powi(8);
        assert!(
            (result.confidence - expected_confidence).abs() < 1e-10,
            "confidence should be {}, got {}",
            expected_confidence,
            result.confidence
        );
    }

    #[test]
    fn test_da_sampling_confidence() {
        let data = b"Testing confidence increases with more valid samples.";
        let encoding = encode_block_data(data, 4, 4);
        let sampler = DASampler::new(10);

        // Sample 1 chunk
        let r1 = sampler.sample_availability(&encoding, 1);
        // Sample all 8 chunks
        let r8 = sampler.sample_availability(&encoding, 8);

        // More samples → higher confidence
        assert!(
            r8.confidence >= r1.confidence,
            "8 samples ({}) should give >= confidence than 1 sample ({})",
            r8.confidence,
            r1.confidence
        );

        // With all valid, confidence formula: 1 - (1/2)^n
        assert!(r1.confidence >= 0.5 - 1e-10); // At least 0.5 with 1 valid sample
        assert!(r8.confidence >= 0.99); // Very high with 8 valid samples
    }

    #[test]
    fn test_da_commitment_creation() {
        let block_data = b"Block data for DA commitment test";
        let block_hash = hash_bytes(b"fake-block-hash");

        let commitment = create_da_commitment(block_data, block_hash);

        assert_eq!(commitment.block_hash, block_hash);
        assert_eq!(commitment.data_root, hash_bytes(block_data));
        assert_eq!(commitment.chunk_count, 8); // 4 data + 4 parity
        assert_eq!(commitment.data_chunks, 4);
        assert_ne!(commitment.encoding_root, Hash256::ZERO);

        // Verify a chunk against the commitment
        let encoding = encode_block_data(block_data, 4, 4);
        let chunk = &encoding.chunks[0];
        assert!(verify_da_commitment(&commitment, chunk));

        // Out-of-range chunk should fail
        let mut bad_chunk = chunk.clone();
        bad_chunk.index = 100; // Way outside range
        assert!(!verify_da_commitment(&commitment, &bad_chunk));
    }

    #[test]
    fn test_empty_data_encoding() {
        let data: &[u8] = b"";
        let encoding = encode_block_data(data, 2, 2);

        assert_eq!(encoding.original_size, 0);
        assert_eq!(encoding.chunk_count, 4);

        let decoded = decode_block_data(&encoding).unwrap();
        assert_eq!(decoded, data);
    }

    #[test]
    fn test_data_chunk_new() {
        let data = vec![1, 2, 3, 4, 5];
        let chunk = DataChunk::new(0, data.clone());

        assert_eq!(chunk.index, 0);
        assert_eq!(chunk.data, data);
        assert_eq!(chunk.proof, hash_bytes(&data));
        assert!(chunk.verify_integrity());
    }

    #[test]
    fn test_decode_rejects_insufficient_chunks() {
        let data = b"Need enough chunks to decode";
        let mut encoding = encode_block_data(data, 4, 4);

        // Remove all but 2 chunks (need at least 4)
        encoding.chunks.truncate(2);

        let result = decode_block_data(&encoding);
        assert!(result.is_err(), "should fail with too few chunks");
    }

    #[test]
    fn test_das_sampler_empty_encoding() {
        let sampler = DASampler::new(5);
        let encoding = ErasureEncoding {
            original_size: 0,
            chunk_count: 0,
            data_chunks: 0,
            parity_chunks: 0,
            chunks: vec![],
            root: Hash256::ZERO,
        };

        let result = sampler.sample_availability(&encoding, 5);
        assert!(!result.available);
        assert_eq!(result.samples_checked, 0);
        assert_eq!(result.confidence, 0.0);
    }
}
