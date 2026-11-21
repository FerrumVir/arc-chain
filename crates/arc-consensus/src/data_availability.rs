use arc_crypto::{Hash256, MerkleTree, hash_bytes};
use rand::Rng;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct DataChunk {
    pub index: u16,
    pub data: Vec<u8>,
    pub proof: Hash256,
}

impl DataChunk {
    pub fn new(index: u16, data: Vec<u8>) -> Self {
        let proof = hash_bytes(&data);
        Self { index, data, proof }
    }

    pub fn verify_integrity(&self) -> bool {
        hash_bytes(&self.data) == self.proof
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ErasureEncoding {
    pub original_size: usize,
    pub chunk_count: u16,
    pub data_chunks: u16,
    pub parity_chunks: u16,
    pub chunks: Vec<DataChunk>,
    pub root: Hash256,
}

pub fn encode_block_data(data: &[u8], data_chunks: u16, parity_chunks: u16) -> ErasureEncoding {
    assert!(data_chunks > 0, "data_chunks must be > 0");
    assert!(parity_chunks > 0, "parity_chunks must be > 0");

    let k = data_chunks as usize;
    let p = parity_chunks as usize;
    let n = k + p;
    let original_size = data.len();

    let chunk_size = if original_size == 0 {
        1
    } else {
        original_size.div_ceil(k)
    };

    let mut raw_chunks: Vec<Vec<u8>> = Vec::with_capacity(k);
    for i in 0..k {
        let start = i * chunk_size;
        let end = std::cmp::min(start + chunk_size, original_size);
        let mut chunk_data = if start < original_size {
            data[start..end].to_vec()
        } else {
            Vec::new()
        };
        chunk_data.resize(chunk_size, 0u8);
        raw_chunks.push(chunk_data);
    }

    let mut parity_data: Vec<Vec<u8>> = vec![vec![0u8; chunk_size]; p];
    for (i, data_chunk) in raw_chunks.iter().enumerate() {
        let parity_idx = i % p;
        for (byte_idx, &byte) in data_chunk.iter().enumerate() {
            parity_data[parity_idx][byte_idx] ^= byte;
        }
    }

    let mut chunks: Vec<DataChunk> = Vec::with_capacity(n);
    for (i, chunk_data) in raw_chunks.iter().enumerate() {
        chunks.push(DataChunk::new(i as u16, chunk_data.clone()));
    }
    for (j, chunk_data) in parity_data.iter().enumerate() {
        chunks.push(DataChunk::new((k + j) as u16, chunk_data.clone()));
    }

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

pub fn decode_block_data(encoding: &ErasureEncoding) -> Result<Vec<u8>, String> {
    let k = encoding.data_chunks as usize;
    let p = encoding.parity_chunks as usize;

    if encoding.chunks.is_empty() {
        return Err("no chunks available".into());
    }

    let _chunk_size = encoding.chunks[0].data.len();

    let mut data_slots: Vec<Option<Vec<u8>>> = vec![None; k];
    let mut parity_slots: Vec<Option<Vec<u8>>> = vec![None; p];

    for chunk in &encoding.chunks {
        let idx = chunk.index as usize;
        if !chunk.verify_integrity() {
            continue;
        }
        if idx < k {
            data_slots[idx] = Some(chunk.data.clone());
        } else if idx < k + p {
            parity_slots[idx - k] = Some(chunk.data.clone());
        }
    }

    let available_data: usize = data_slots.iter().filter(|s| s.is_some()).count();
    let available_parity: usize = parity_slots.iter().filter(|s| s.is_some()).count();

    if available_data + available_parity < k {
        return Err(format!(
            "insufficient chunks: need at least {} but only have {} data + {} parity",
            k, available_data, available_parity
        ));
    }

    if available_data == k {
        let mut result = Vec::with_capacity(encoding.original_size);
        for (i, slot) in data_slots.iter().enumerate() {
            match slot.as_ref() {
                Some(data) => result.extend_from_slice(data),
                None => {
                    return Err(format!(
                        "data slot {i} unexpectedly None despite count={available_data}"
                    ));
                }
            }
        }
        result.truncate(encoding.original_size);
        return Ok(result);
    }

    let mut recovered = true;
    while recovered {
        recovered = false;
        for (j, parity) in parity_slots.iter().enumerate() {
            let Some(parity_data) = parity.as_ref() else {
                continue;
            };

            let group_indices: Vec<usize> = (0..k).filter(|&i| i % p == j).collect();
            let missing: Vec<usize> = group_indices
                .iter()
                .filter(|&&i| data_slots[i].is_none())
                .copied()
                .collect();

            if missing.len() == 1 {
                let missing_idx = missing[0];
                let mut reconstructed = parity_data.clone();

                for &i in &group_indices {
                    if i != missing_idx
                        && let Some(ref chunk_data) = data_slots[i]
                    {
                        for (byte_idx, &byte) in chunk_data.iter().enumerate() {
                            if byte_idx < reconstructed.len() {
                                reconstructed[byte_idx] ^= byte;
                            }
                        }
                    }
                }

                data_slots[missing_idx] = Some(reconstructed);
                recovered = true;
            }
        }

        if data_slots.iter().all(|s| s.is_some()) {
            break;
        }
    }

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

pub fn verify_chunk(chunk: &DataChunk, _root: &Hash256, _total_chunks: u16) -> bool {
    chunk.verify_integrity()
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DASampleResult {
    pub available: bool,
    pub samples_checked: usize,
    pub samples_valid: usize,
    pub confidence: f64,
}

pub struct DASampler {
    pub default_sample_count: usize,
}

impl DASampler {
    pub fn new(default_sample_count: usize) -> Self {
        Self {
            default_sample_count,
        }
    }

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
        let effective_samples = std::cmp::min(sample_count, n);

        let mut rng = rand::thread_rng();

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

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct DACommitment {
    pub block_hash: Hash256,
    pub data_root: Hash256,
    pub chunk_count: u16,
    pub data_chunks: u16,
    pub encoding_root: Hash256,
}

pub fn create_da_commitment(block_data: &[u8], block_hash: Hash256) -> DACommitment {
    let data_root = hash_bytes(block_data);

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

pub fn verify_da_commitment(commitment: &DACommitment, chunk: &DataChunk) -> bool {
    if chunk.index >= commitment.chunk_count {
        return false;
    }

    if !chunk.verify_integrity() {
        return false;
    }

    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_encode_decode_roundtrip() {
        let data =
            b"Hello, ARC Chain data availability layer! This is test data for erasure coding.";
        let encoding = encode_block_data(data, 4, 4);

        assert_eq!(encoding.original_size, data.len());
        assert_eq!(encoding.chunk_count, 8);
        assert_eq!(encoding.data_chunks, 4);
        assert_eq!(encoding.parity_chunks, 4);
        assert_eq!(encoding.chunks.len(), 8);
        assert_ne!(encoding.root, Hash256::ZERO);

        for chunk in &encoding.chunks {
            assert!(
                chunk.verify_integrity(),
                "chunk {} integrity failed",
                chunk.index
            );
        }

        let decoded = decode_block_data(&encoding).expect("decode should succeed");
        assert_eq!(decoded, data);
    }

    #[test]
    fn test_encode_varying_sizes() {
        let data_1b = [0xAB_u8];
        let enc1 = encode_block_data(&data_1b, 2, 2);
        let dec1 = decode_block_data(&enc1).unwrap();
        assert_eq!(dec1, data_1b);

        let data_1kb: Vec<u8> = (0..1024).map(|i| (i % 256) as u8).collect();
        let enc2 = encode_block_data(&data_1kb, 4, 4);
        let dec2 = decode_block_data(&enc2).unwrap();
        assert_eq!(dec2, data_1kb);

        let data_1mb: Vec<u8> = (0..1_048_576).map(|i| (i % 256) as u8).collect();
        let enc3 = encode_block_data(&data_1mb, 8, 8);
        let dec3 = decode_block_data(&enc3).unwrap();
        assert_eq!(dec3, data_1mb);
    }

    #[test]
    fn test_partial_reconstruction() {
        let data =
            b"Data availability is essential for blockchain scaling and light client security!";
        let full_encoding = encode_block_data(data, 4, 4);

        let mut partial = full_encoding.clone();
        partial.chunks.retain(|c| c.index != 0);

        let decoded = decode_block_data(&partial).expect("should reconstruct from partial chunks");
        assert_eq!(decoded, data);

        let mut partial2 = full_encoding.clone();
        partial2.chunks.retain(|c| c.index != 1);

        let decoded2 =
            decode_block_data(&partial2).expect("should reconstruct with chunk 1 missing");
        assert_eq!(decoded2, data);

        let mut partial3 = full_encoding.clone();
        partial3.chunks.retain(|c| c.index != 0 && c.index != 1);

        let decoded3 =
            decode_block_data(&partial3).expect("should reconstruct with two missing data chunks");
        assert_eq!(decoded3, data);
    }

    #[test]
    fn test_chunk_verification() {
        let data = b"Verify me";
        let encoding = encode_block_data(data, 2, 2);

        let valid_chunk = &encoding.chunks[0];
        assert!(verify_chunk(
            valid_chunk,
            &encoding.root,
            encoding.chunk_count
        ));

        let mut corrupted = encoding.chunks[0].clone();
        corrupted.data[0] ^= 0xFF;
        assert!(!verify_chunk(
            &corrupted,
            &encoding.root,
            encoding.chunk_count
        ));

        let mut tampered_proof = encoding.chunks[1].clone();
        tampered_proof.proof = Hash256::ZERO;
        assert!(!verify_chunk(
            &tampered_proof,
            &encoding.root,
            encoding.chunk_count
        ));
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

        let r1 = sampler.sample_availability(&encoding, 1);
        let r8 = sampler.sample_availability(&encoding, 8);

        assert!(
            r8.confidence >= r1.confidence,
            "8 samples ({}) should give >= confidence than 1 sample ({})",
            r8.confidence,
            r1.confidence
        );

        assert!(r1.confidence >= 0.5 - 1e-10);
        assert!(r8.confidence >= 0.99);
    }

    #[test]
    fn test_da_commitment_creation() {
        let block_data = b"Block data for DA commitment test";
        let block_hash = hash_bytes(b"fake-block-hash");

        let commitment = create_da_commitment(block_data, block_hash);

        assert_eq!(commitment.block_hash, block_hash);
        assert_eq!(commitment.data_root, hash_bytes(block_data));
        assert_eq!(commitment.chunk_count, 8);
        assert_eq!(commitment.data_chunks, 4);
        assert_ne!(commitment.encoding_root, Hash256::ZERO);

        let encoding = encode_block_data(block_data, 4, 4);
        let chunk = &encoding.chunks[0];
        assert!(verify_da_commitment(&commitment, chunk));

        let mut bad_chunk = chunk.clone();
        bad_chunk.index = 100;
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

        encoding.chunks.truncate(2);

        let result = decode_block_data(&encoding);
        assert!(result.is_err(), "should fail with too few chunks");
    }

    #[allow(clippy::float_cmp)]
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
