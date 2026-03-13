//! Proof compression utilities.
//!
//! Provides lossless compression/decompression for ZK proofs, proof
//! aggregation via Merkle commitments, and batch streaming compression
//! with statistics.

use crate::hash::{Hash256, hash_bytes, hash_pair};
use serde::{Deserialize, Serialize};
use std::time::Instant;

// ---------------------------------------------------------------------------
// Compression types
// ---------------------------------------------------------------------------

/// Compression algorithm used on proof data.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum CompressionType {
    /// No compression (pass-through).
    None,
    /// Run-length encoding.
    RunLength,
    /// Dictionary-based compression (byte-pair encoding).
    Dictionary,
    /// Hybrid: run-length followed by dictionary pass.
    Hybrid,
}

/// A compressed proof blob.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CompressedProof {
    pub original_size: usize,
    pub compressed_data: Vec<u8>,
    pub compression_type: CompressionType,
    pub proof_type: String,
}

/// Statistics from a compression operation.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CompressionStats {
    pub original_bytes: usize,
    pub compressed_bytes: usize,
    pub ratio: f64,
    pub time_us: u64,
}

// ---------------------------------------------------------------------------
// Run-length encoding
// ---------------------------------------------------------------------------

/// Encode bytes using run-length encoding.
///
/// Format: for each run, emit `[count, byte]`.  Counts are stored as a
/// single byte (max run = 255).  Non-repeating bytes get count = 1.
fn rle_encode(data: &[u8]) -> Vec<u8> {
    if data.is_empty() {
        return Vec::new();
    }

    let mut out = Vec::with_capacity(data.len());
    let mut i = 0;

    while i < data.len() {
        let byte = data[i];
        let mut count: u8 = 1;
        while i + (count as usize) < data.len()
            && data[i + count as usize] == byte
            && count < 255
        {
            count += 1;
        }
        out.push(count);
        out.push(byte);
        i += count as usize;
    }
    out
}

/// Decode run-length encoded data.
fn rle_decode(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut i = 0;
    while i + 1 < data.len() {
        let count = data[i] as usize;
        let byte = data[i + 1];
        out.extend(std::iter::repeat(byte).take(count));
        i += 2;
    }
    out
}

// ---------------------------------------------------------------------------
// Dictionary compression (simple byte-pair encoding)
// ---------------------------------------------------------------------------

/// Simple dictionary-based compression.
///
/// Finds the most frequent adjacent byte pair and replaces it with a single
/// unused byte.  Repeats until no improvement or no unused bytes remain.
/// Output format: `[num_rules, (replacement, first, second)*, encoded_data*]`
fn dict_encode(data: &[u8]) -> Vec<u8> {
    if data.is_empty() {
        return vec![0]; // zero rules
    }

    let mut working = data.to_vec();
    let mut rules: Vec<(u8, u8, u8)> = Vec::new(); // (replacement, first, second)

    // Up to 16 replacement rounds to keep things bounded.
    for _ in 0..16 {
        if working.len() < 2 {
            break;
        }

        // Find unused byte value.
        let mut used = [false; 256];
        for &b in &working {
            used[b as usize] = true;
        }
        let replacement = match used.iter().position(|&u| !u) {
            Some(pos) => pos as u8,
            None => break,
        };

        // Find most frequent pair.
        let mut freq = std::collections::HashMap::<(u8, u8), usize>::new();
        for pair in working.windows(2) {
            *freq.entry((pair[0], pair[1])).or_default() += 1;
        }
        let best = freq.into_iter().max_by_key(|&(_, c)| c);
        match best {
            Some(((a, b), count)) if count >= 2 => {
                // Replace all occurrences.
                let mut new = Vec::with_capacity(working.len());
                let mut i = 0;
                while i < working.len() {
                    if i + 1 < working.len() && working[i] == a && working[i + 1] == b {
                        new.push(replacement);
                        i += 2;
                    } else {
                        new.push(working[i]);
                        i += 1;
                    }
                }
                rules.push((replacement, a, b));
                working = new;
            }
            _ => break,
        }
    }

    let mut out = Vec::with_capacity(1 + rules.len() * 3 + working.len());
    out.push(rules.len() as u8);
    for &(r, a, b) in &rules {
        out.push(r);
        out.push(a);
        out.push(b);
    }
    out.extend_from_slice(&working);
    out
}

/// Decode dictionary-compressed data.
fn dict_decode(data: &[u8]) -> Vec<u8> {
    if data.is_empty() {
        return Vec::new();
    }

    let num_rules = data[0] as usize;
    if data.len() < 1 + num_rules * 3 {
        return Vec::new();
    }

    // Read rules in order.
    let mut rules = Vec::with_capacity(num_rules);
    for i in 0..num_rules {
        let base = 1 + i * 3;
        let replacement = data[base];
        let a = data[base + 1];
        let b = data[base + 2];
        rules.push((replacement, a, b));
    }

    let mut working = data[1 + num_rules * 3..].to_vec();

    // Apply rules in reverse order.
    for &(replacement, a, b) in rules.iter().rev() {
        let mut expanded = Vec::with_capacity(working.len());
        for &byte in &working {
            if byte == replacement {
                expanded.push(a);
                expanded.push(b);
            } else {
                expanded.push(byte);
            }
        }
        working = expanded;
    }

    working
}

// ---------------------------------------------------------------------------
// Public compress / decompress API
// ---------------------------------------------------------------------------

/// Compress proof bytes using a simple but effective scheme.
///
/// Tries all compression strategies and picks the smallest result.
pub fn compress_proof(proof_bytes: &[u8]) -> CompressedProof {
    let original_size = proof_bytes.len();

    // Try each strategy.
    let none_data = proof_bytes.to_vec();
    let rle_data = rle_encode(proof_bytes);
    let dict_data = dict_encode(proof_bytes);

    // Hybrid: RLE then dictionary.
    let hybrid_data = dict_encode(&rle_encode(proof_bytes));

    // Pick smallest.
    let candidates = [
        (CompressionType::None, none_data),
        (CompressionType::RunLength, rle_data),
        (CompressionType::Dictionary, dict_data),
        (CompressionType::Hybrid, hybrid_data),
    ];

    let (best_type, best_data) = candidates
        .into_iter()
        .min_by_key(|(_, d)| d.len())
        .unwrap();

    CompressedProof {
        original_size,
        compressed_data: best_data,
        compression_type: best_type,
        proof_type: String::new(),
    }
}

/// Decompress a previously compressed proof.
pub fn decompress_proof(compressed: &CompressedProof) -> Vec<u8> {
    match compressed.compression_type {
        CompressionType::None => compressed.compressed_data.clone(),
        CompressionType::RunLength => rle_decode(&compressed.compressed_data),
        CompressionType::Dictionary => dict_decode(&compressed.compressed_data),
        CompressionType::Hybrid => {
            // Reverse: dict_decode then rle_decode.
            let after_dict = dict_decode(&compressed.compressed_data);
            rle_decode(&after_dict)
        }
    }
}

// ---------------------------------------------------------------------------
// Proof aggregation
// ---------------------------------------------------------------------------

/// An aggregated proof combining multiple sub-proofs under a Merkle root.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AggregatedProof {
    pub sub_proofs: Vec<Vec<u8>>,
    pub merkle_root: [u8; 32],
    pub count: usize,
    pub total_original_size: usize,
    pub compressed_size: usize,
}

/// Aggregates multiple proofs into a single aggregated proof structure.
pub struct ProofAggregator {
    proofs: Vec<Vec<u8>>,
    public_inputs: Vec<Vec<u64>>,
}

impl ProofAggregator {
    pub fn new() -> Self {
        Self {
            proofs: Vec::new(),
            public_inputs: Vec::new(),
        }
    }

    /// Add a proof with its public inputs.
    pub fn add_proof(&mut self, proof: Vec<u8>, public_inputs: Vec<u64>) {
        self.proofs.push(proof);
        self.public_inputs.push(public_inputs);
    }

    /// Number of proofs collected so far.
    pub fn count(&self) -> usize {
        self.proofs.len()
    }

    /// Aggregate all collected proofs.
    pub fn aggregate(&self) -> AggregatedProof {
        let total_original_size: usize = self.proofs.iter().map(|p| p.len()).sum();

        // Compute Merkle root over proof hashes.
        let merkle_root = self.compute_merkle_root();

        // Compress each sub-proof.
        let compressed_proofs: Vec<Vec<u8>> = self
            .proofs
            .iter()
            .map(|p| compress_proof(p).compressed_data)
            .collect();

        let compressed_size: usize = compressed_proofs.iter().map(|p| p.len()).sum();

        AggregatedProof {
            sub_proofs: compressed_proofs,
            merkle_root,
            count: self.proofs.len(),
            total_original_size,
            compressed_size,
        }
    }

    /// Verify an aggregated proof by recomputing the Merkle root.
    pub fn verify_aggregated(&self, proof: &AggregatedProof) -> bool {
        if proof.count != self.proofs.len() {
            return false;
        }

        let expected_root = self.compute_merkle_root();
        expected_root == proof.merkle_root
    }

    /// Compute Merkle root of all proof hashes.
    fn compute_merkle_root(&self) -> [u8; 32] {
        if self.proofs.is_empty() {
            return [0u8; 32];
        }

        let mut leaves: Vec<Hash256> = self
            .proofs
            .iter()
            .map(|p| hash_bytes(p))
            .collect();

        // Pad to even length.
        if leaves.len() % 2 != 0 {
            leaves.push(*leaves.last().unwrap());
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

            if leaves.len() > 1 && leaves.len() % 2 != 0 {
                leaves.push(*leaves.last().unwrap());
            }
        }

        leaves[0].0
    }
}

impl Default for ProofAggregator {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Batch compressor (streaming)
// ---------------------------------------------------------------------------

/// Streaming batch compressor that accumulates proofs and compresses them
/// incrementally, tracking statistics.
pub struct BatchCompressor {
    compressed: Vec<CompressedProof>,
    stats: CompressionStats,
}

impl BatchCompressor {
    pub fn new() -> Self {
        Self {
            compressed: Vec::new(),
            stats: CompressionStats {
                original_bytes: 0,
                compressed_bytes: 0,
                ratio: 1.0,
                time_us: 0,
            },
        }
    }

    /// Add and compress a proof, updating running statistics.
    pub fn add(&mut self, proof_bytes: &[u8]) {
        let start = Instant::now();
        let cp = compress_proof(proof_bytes);
        let elapsed = start.elapsed().as_micros() as u64;

        self.stats.original_bytes += cp.original_size;
        self.stats.compressed_bytes += cp.compressed_data.len();
        self.stats.time_us += elapsed;
        self.stats.ratio = if self.stats.original_bytes > 0 {
            self.stats.compressed_bytes as f64 / self.stats.original_bytes as f64
        } else {
            1.0
        };

        self.compressed.push(cp);
    }

    /// Number of proofs compressed so far.
    pub fn count(&self) -> usize {
        self.compressed.len()
    }

    /// Current cumulative statistics.
    pub fn stats(&self) -> &CompressionStats {
        &self.stats
    }

    /// Consume the compressor and return all compressed proofs.
    pub fn finish(self) -> Vec<CompressedProof> {
        self.compressed
    }
}

impl Default for BatchCompressor {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_compress_decompress_roundtrip() {
        let data = b"hello world this is a test proof with some repeated bytes aaaaaaaa";
        let compressed = compress_proof(data);
        let decompressed = decompress_proof(&compressed);
        assert_eq!(decompressed, data);
    }

    #[test]
    fn test_compress_empty() {
        let compressed = compress_proof(&[]);
        let decompressed = decompress_proof(&compressed);
        assert_eq!(decompressed, Vec::<u8>::new());
        assert_eq!(compressed.original_size, 0);
    }

    #[test]
    fn test_compress_single_byte() {
        let data = [0x42];
        let compressed = compress_proof(&data);
        let decompressed = decompress_proof(&compressed);
        assert_eq!(decompressed, data);
    }

    #[test]
    fn test_rle_highly_repetitive() {
        // 1000 zeroes should compress well with RLE.
        let data = vec![0u8; 1000];
        let compressed = compress_proof(&data);
        let decompressed = decompress_proof(&compressed);
        assert_eq!(decompressed, data);
        // Should be much smaller.
        assert!(
            compressed.compressed_data.len() < data.len(),
            "RLE should compress repetitive data"
        );
    }

    #[test]
    fn test_rle_encode_decode() {
        let data = vec![1, 1, 1, 2, 2, 3, 3, 3, 3];
        let encoded = rle_encode(&data);
        let decoded = rle_decode(&encoded);
        assert_eq!(decoded, data);
    }

    #[test]
    fn test_dict_encode_decode() {
        let data = b"abcabcabcdef".to_vec();
        let encoded = dict_encode(&data);
        let decoded = dict_decode(&encoded);
        assert_eq!(decoded, data);
    }

    #[test]
    fn test_aggregator_basic() {
        let mut agg = ProofAggregator::new();
        agg.add_proof(vec![1, 2, 3], vec![100]);
        agg.add_proof(vec![4, 5, 6], vec![200]);
        assert_eq!(agg.count(), 2);

        let aggregated = agg.aggregate();
        assert_eq!(aggregated.count, 2);
        assert_ne!(aggregated.merkle_root, [0u8; 32]);
    }

    #[test]
    fn test_aggregator_verify() {
        let mut agg = ProofAggregator::new();
        agg.add_proof(vec![10, 20, 30], vec![1]);
        agg.add_proof(vec![40, 50, 60], vec![2]);
        agg.add_proof(vec![70, 80, 90], vec![3]);

        let aggregated = agg.aggregate();
        assert!(agg.verify_aggregated(&aggregated));
    }

    #[test]
    fn test_aggregator_verify_tampered() {
        let mut agg = ProofAggregator::new();
        agg.add_proof(vec![1, 2], vec![1]);
        agg.add_proof(vec![3, 4], vec![2]);

        let mut aggregated = agg.aggregate();
        aggregated.merkle_root = [0xFF; 32]; // tamper
        assert!(!agg.verify_aggregated(&aggregated));
    }

    #[test]
    fn test_batch_compressor() {
        let mut bc = BatchCompressor::new();
        bc.add(&vec![0u8; 500]);
        bc.add(&vec![0xAB; 300]);
        bc.add(b"proof data with varied bytes 123456789");
        assert_eq!(bc.count(), 3);

        let stats = bc.stats();
        assert!(stats.original_bytes > 0);
        assert!(stats.compressed_bytes > 0);
        assert!(stats.ratio > 0.0);
        assert!(stats.ratio <= 1.5); // sanity — should not be wildly worse
    }

    #[test]
    fn test_batch_compressor_roundtrip() {
        let mut bc = BatchCompressor::new();
        let originals: Vec<Vec<u8>> = vec![
            vec![0u8; 100],
            (0..=255).collect(),
            b"a]b]c]d]e]f]g]h]i]j]k]l]m]".to_vec(),
        ];

        for o in &originals {
            bc.add(o);
        }

        let compressed_proofs = bc.finish();
        for (cp, original) in compressed_proofs.iter().zip(originals.iter()) {
            let decompressed = decompress_proof(cp);
            assert_eq!(&decompressed, original);
        }
    }

    #[test]
    fn test_compression_stats_ratio() {
        let data = vec![0u8; 2000];
        let compressed = compress_proof(&data);
        let ratio = compressed.compressed_data.len() as f64 / data.len() as f64;
        // Repetitive data should compress to well under 10%.
        assert!(
            ratio < 0.1,
            "2000 zeros should compress to <10%, got {ratio:.4}"
        );
    }

    #[test]
    fn test_aggregator_empty() {
        let agg = ProofAggregator::new();
        let aggregated = agg.aggregate();
        assert_eq!(aggregated.count, 0);
        assert_eq!(aggregated.merkle_root, [0u8; 32]);
    }

    #[test]
    fn test_aggregator_single_proof() {
        let mut agg = ProofAggregator::new();
        agg.add_proof(vec![0xDE, 0xAD], vec![42]);
        let aggregated = agg.aggregate();
        assert_eq!(aggregated.count, 1);
        assert_ne!(aggregated.merkle_root, [0u8; 32]);
        assert!(agg.verify_aggregated(&aggregated));
    }
}
