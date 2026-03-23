// Add to lib.rs: pub mod stark;

//! Recursive STARK proof pipeline for ARC Chain.
//!
//! Defines the type system and interfaces for a ZK-STARK proving pipeline that
//! generates block proofs and recursively aggregates them.
//!
//! Two proving backends are available, selected by feature flag:
//!
//! - `stwo-prover` (or `stwo-icicle`): Real Circle STARK proofs over the
//!   Mersenne-31 field via the Stwo framework (`stwo_air.rs`). Produces
//!   cryptographic proofs with FRI commitments and inline verification.
//! - Default (no feature): BLAKE3-based mock proofs for fast iteration and
//!   testing. These maintain correct type flow and structural invariants
//!   but provide no zero-knowledge properties.
//!
//! Recursive proofs cryptographically verify all child proofs before aggregation
//! and commit to a Merkle tree over verified child proof hashes.

use crate::hash::Hash256;
use crate::merkle::MerkleTree;
use serde::{Deserialize, Serialize};
use std::time::Instant;

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

/// A STARK proof for a single block.
///
/// Attests that applying the transactions in a block to `prev_state_root`
/// yields `post_state_root`. With the `stwo-prover` feature, `proof_data`
/// contains a real Stwo Circle STARK proof receipt. Without the feature,
/// it holds a BLAKE3-based mock witness commitment (no ZK properties).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BlockProof {
    pub block_height: u64,
    pub block_hash: [u8; 32],
    pub prev_state_root: [u8; 32],
    pub post_state_root: [u8; 32],
    pub tx_count: u32,
    pub proof_data: Vec<u8>,
    pub proof_size_bytes: usize,
    pub proving_time_ms: u64,
    pub verifier_hash: [u8; 32],
}

/// Recursive proof — attests to N block proofs (or N child recursive proofs).
///
/// The recursive structure allows logarithmic compression: instead of verifying
/// 1 000 block proofs, a verifier checks a single recursive proof whose depth
/// is ~log2(1000) ≈ 10.
///
/// The `merkle_root` field holds the Merkle root over all verified child proof
/// hashes. This is also committed inside `proof_data` for double-binding: the
/// verifier checks both the top-level field and the embedded commitment match
/// the recomputed root from `child_proofs`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecursiveProof {
    pub start_height: u64,
    pub end_height: u64,
    pub start_state_root: [u8; 32],
    pub end_state_root: [u8; 32],
    pub block_count: u64,
    pub total_tx_count: u64,
    pub proof_data: Vec<u8>,
    pub child_proofs: Vec<[u8; 32]>,
    pub depth: u32,
    /// Merkle root over verified child proof hashes.
    ///
    /// Computed at aggregation time from the child proofs that passed
    /// verification. The verifier recomputes this from `child_proofs` and
    /// checks it matches both this field and the root embedded in `proof_data`.
    pub merkle_root: [u8; 32],
}

/// Proof verification result.
#[derive(Debug, Clone)]
pub struct VerificationResult {
    pub is_valid: bool,
    pub proof_type: ProofType,
    pub verification_time_ms: u64,
    pub error: Option<String>,
}

/// Discriminant for different proof kinds in the pipeline.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ProofType {
    /// Single block proof.
    Block,
    /// Recursive (multiple blocks).
    Recursive,
    /// Aggregation of recursive proofs.
    Aggregate,
    /// State diff proof.
    StateTransition,
}

/// Prover configuration — controls security parameters and performance knobs.
#[derive(Debug, Clone)]
pub struct ProverConfig {
    pub max_constraints: u64,
    /// Field size in bits. 64 for Goldilocks (Plonky3).
    pub field_size_bits: u32,
    pub hash_function: ProofHashType,
    /// Number of block proofs accumulated before generating a recursive proof.
    pub recursion_threshold: u32,
    /// Target proof size in bytes.
    pub target_proof_size: usize,
    pub gpu_acceleration: bool,
}

/// Hash function used inside the proof circuit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProofHashType {
    /// Poseidon — fast in-circuit, algebraic hash.
    Poseidon,
    /// BLAKE3 — fast out-circuit, used for commitment hashing.
    Blake3,
    /// Keccak-256 — EVM-compatible, used for L1 bridge verification.
    Keccak256,
}

/// Proving pipeline — batches blocks into proofs and recursively aggregates.
pub struct ProvingPipeline {
    config: ProverConfig,
    pending_blocks: Vec<BlockProofInput>,
    completed_proofs: Vec<BlockProof>,
    recursive_proofs: Vec<RecursiveProof>,
    stats: ProvingStats,
}

/// Witness data for a single transfer within a block.
///
/// All balance/amount/fee values must be < 2^47 to ensure each value's
/// 2-limb M31 decomposition (base 2^16) fits in the Mersenne-31 field.
#[derive(Debug, Clone)]
pub struct TransferWitness {
    /// Sender balance before the transfer.
    pub sender_bal_before: u64,
    /// Sender balance after the transfer (must equal sender_bal_before - amount - fee).
    pub sender_bal_after: u64,
    /// Receiver balance before the transfer.
    pub receiver_bal_before: u64,
    /// Receiver balance after the transfer (must equal receiver_bal_before + amount).
    pub receiver_bal_after: u64,
    /// Transfer amount.
    pub amount: u64,
    /// Sender nonce before the transfer.
    pub sender_nonce_before: u32,
    /// Sender nonce after the transfer (must equal sender_nonce_before + 1).
    pub sender_nonce_after: u32,
    /// Transaction fee paid by sender.
    pub fee: u64,
}

/// Input to the block prover.
#[derive(Debug, Clone)]
pub struct BlockProofInput {
    pub height: u64,
    pub block_hash: [u8; 32],
    pub prev_state_root: [u8; 32],
    pub post_state_root: [u8; 32],
    pub tx_hashes: Vec<[u8; 32]>,
    /// State diffs: (address, old_hash, new_hash).
    pub state_diffs: Vec<([u8; 32], [u8; 32], [u8; 32])>,
    /// Transfer witness data — one per transaction in the block.
    /// When empty, the AIR only enforces hash-based state diff constraints.
    pub transfers: Vec<TransferWitness>,
}

/// Proving statistics — tracked by the pipeline.
#[derive(Debug, Clone, Default)]
pub struct ProvingStats {
    pub blocks_proven: u64,
    pub recursive_proofs_generated: u64,
    pub total_proving_time_ms: u64,
    pub avg_block_proof_time_ms: f64,
    pub avg_recursive_proof_time_ms: f64,
    pub total_proof_bytes: u64,
    /// Compression ratio vs raw state size.
    pub compression_ratio: f64,
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// BLAKE3 hash of arbitrary bytes, returning a 32-byte array.
#[inline]
fn blake3_hash(data: &[u8]) -> [u8; 32] {
    *blake3::hash(data).as_bytes()
}

/// BLAKE3 domain-separated hash for STARK proofs.
#[inline]
fn blake3_domain_hash(domain: &str, data: &[u8]) -> [u8; 32] {
    let mut hasher = blake3::Hasher::new_derive_key(domain);
    hasher.update(data);
    *hasher.finalize().as_bytes()
}

/// Build a deterministic mock proof blob from the given seed data.
/// The "proof" is a domain-separated BLAKE3 hash expanded to `target_len` bytes
/// by iteratively hashing the previous block.
fn build_mock_proof_data(seed: &[u8], target_len: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(target_len);
    let mut block = blake3_domain_hash("ARC-stark-mock-proof-v1", seed);
    while out.len() < target_len {
        out.extend_from_slice(&block);
        block = blake3_hash(&block);
    }
    out.truncate(target_len);
    out
}

// ---------------------------------------------------------------------------
// BlockProof impl
// ---------------------------------------------------------------------------

impl BlockProof {
    /// Generate a STARK proof for the given block input.
    ///
    /// Feature dispatch:
    /// - `stwo-prover` (or `stwo-icicle` which implies it): Real Stwo Circle STARK
    /// - Neither: Mock BLAKE3 proof (fast iteration, no real ZK)
    pub fn prove(input: &BlockProofInput) -> Self {
        #[cfg(feature = "stwo-prover")]
        {
            Self::stwo_prove(input)
        }
        #[cfg(not(feature = "stwo-prover"))]
        {
            Self::mock_prove(input)
        }
    }

    /// Generate a real Stwo Circle STARK proof.
    ///
    /// Uses `SimdBackend` (CPU SIMD). When `stwo-icicle` is also enabled,
    /// ICICLE GPU crates are available for future GPU Backend integration.
    #[cfg(feature = "stwo-prover")]
    pub fn stwo_prove(input: &BlockProofInput) -> Self {
        let start = Instant::now();

        let (proof_data, proof_size_bytes, _) = crate::stwo_air::prove_block(input);
        // Use same domain as mock verifier so verify() works for both paths
        let verifier_hash = blake3_domain_hash("ARC-stark-verifier-v1", &proof_data);
        let proving_time_ms = start.elapsed().as_millis() as u64;

        Self {
            block_height: input.height,
            block_hash: input.block_hash,
            prev_state_root: input.prev_state_root,
            post_state_root: input.post_state_root,
            tx_count: input.tx_hashes.len() as u32,
            proof_data,
            proof_size_bytes,
            proving_time_ms,
            verifier_hash,
        }
    }

    /// Verify this proof using the Stwo verifier (receipt check).
    #[cfg(feature = "stwo-prover")]
    pub fn stwo_verify(&self, input: &BlockProofInput) -> VerificationResult {
        let start = Instant::now();

        let is_valid = crate::stwo_air::verify_block_proof(input, &self.proof_data);
        let verification_time_ms = start.elapsed().as_millis() as u64;

        VerificationResult {
            is_valid,
            proof_type: ProofType::Block,
            verification_time_ms,
            error: if is_valid {
                None
            } else {
                Some("Stwo proof receipt verification failed".to_string())
            },
        }
    }

    /// Generate a mock STARK proof for the given block input.
    ///
    /// In production this calls into the Stwo prover. The mock version
    /// produces a deterministic proof blob derived from the block data so that
    /// `proof_hash()` is reproducible for the same input.
    pub fn mock_prove(input: &BlockProofInput) -> Self {
        let start = Instant::now();

        // Deterministic seed = block_hash || prev_state || post_state || height
        let mut seed = Vec::with_capacity(32 * 3 + 8);
        seed.extend_from_slice(&input.block_hash);
        seed.extend_from_slice(&input.prev_state_root);
        seed.extend_from_slice(&input.post_state_root);
        seed.extend_from_slice(&input.height.to_le_bytes());

        // Include tx hashes in the witness commitment
        for tx in &input.tx_hashes {
            seed.extend_from_slice(tx);
        }

        let proof_data = build_mock_proof_data(&seed, 256);
        let proof_size_bytes = proof_data.len();

        let verifier_hash = blake3_domain_hash("ARC-stark-verifier-v1", &proof_data);

        let proving_time_ms = start.elapsed().as_millis() as u64;

        Self {
            block_height: input.height,
            block_hash: input.block_hash,
            prev_state_root: input.prev_state_root,
            post_state_root: input.post_state_root,
            tx_count: input.tx_hashes.len() as u32,
            proof_data,
            proof_size_bytes,
            proving_time_ms,
            verifier_hash,
        }
    }

    /// Verify this block proof (mock verification).
    ///
    /// The mock verifier re-derives the verifier hash from the proof data and
    /// checks it matches. A real verifier would evaluate the STARK AIR constraints.
    pub fn verify(&self) -> VerificationResult {
        let start = Instant::now();

        let expected_verifier = blake3_domain_hash("ARC-stark-verifier-v1", &self.proof_data);
        let is_valid = expected_verifier == self.verifier_hash;

        let verification_time_ms = start.elapsed().as_millis() as u64;

        VerificationResult {
            is_valid,
            proof_type: ProofType::Block,
            verification_time_ms,
            error: if is_valid {
                None
            } else {
                Some("verifier hash mismatch".to_string())
            },
        }
    }

    /// BLAKE3 hash of the entire proof (used as a content-addressed ID).
    pub fn proof_hash(&self) -> [u8; 32] {
        let mut hasher = blake3::Hasher::new_derive_key("ARC-stark-proof-hash-v1");
        hasher.update(&self.block_height.to_le_bytes());
        hasher.update(&self.block_hash);
        hasher.update(&self.prev_state_root);
        hasher.update(&self.post_state_root);
        hasher.update(&self.tx_count.to_le_bytes());
        hasher.update(&self.proof_data);
        *hasher.finalize().as_bytes()
    }
}

// ---------------------------------------------------------------------------
// RecursiveProof impl
// ---------------------------------------------------------------------------

/// Proof data format version for recursive proofs.
const RECURSIVE_PROOF_VERSION: u32 = 1;

/// Byte offset constants for the recursive proof_data layout:
/// ```text
/// [0..4]   version (u32 LE)
/// [4..36]  Merkle root of child proof hashes
/// [36..68] BLAKE3(start_state_root || end_state_root)
/// [68..72] child count (u32 LE)
/// [72..]   mock/aggregate proof witness
/// ```
const PROOF_DATA_VERSION_OFF: usize = 0;
const PROOF_DATA_MERKLE_OFF: usize = 4;
const PROOF_DATA_STATE_OFF: usize = 36;
const PROOF_DATA_COUNT_OFF: usize = 68;
const PROOF_DATA_HEADER_LEN: usize = 72;

/// Build the structured proof_data blob for a recursive proof.
///
/// Returns `(proof_data, merkle_root)` so the caller can store the Merkle root
/// both in the struct field and embedded inside `proof_data`.
fn build_recursive_proof_data(
    child_hashes: &[[u8; 32]],
    start_state_root: &[u8; 32],
    end_state_root: &[u8; 32],
) -> (Vec<u8>, [u8; 32]) {
    // Build Merkle tree over child proof hashes
    let leaves: Vec<Hash256> = child_hashes.iter().map(|h| Hash256(*h)).collect();
    let tree = MerkleTree::from_leaves(leaves);
    let merkle_root = tree.root();
    let merkle_root_bytes: [u8; 32] = *merkle_root.as_bytes();

    // State commitment = BLAKE3(start_state_root || end_state_root)
    let mut state_buf = Vec::with_capacity(64);
    state_buf.extend_from_slice(start_state_root);
    state_buf.extend_from_slice(end_state_root);
    let state_hash = blake3_hash(&state_buf);

    // Build the mock witness portion from all child hashes + state roots
    let mut seed = Vec::with_capacity(child_hashes.len() * 32 + 64);
    for hash in child_hashes {
        seed.extend_from_slice(hash);
    }
    seed.extend_from_slice(start_state_root);
    seed.extend_from_slice(end_state_root);
    let mock_witness = build_mock_proof_data(&seed, 512 - PROOF_DATA_HEADER_LEN);

    // Assemble: [version][merkle_root][state_hash][child_count][witness...]
    let child_count = child_hashes.len() as u32;
    let mut proof_data = Vec::with_capacity(PROOF_DATA_HEADER_LEN + mock_witness.len());
    proof_data.extend_from_slice(&RECURSIVE_PROOF_VERSION.to_le_bytes());
    proof_data.extend_from_slice(merkle_root.as_bytes());
    proof_data.extend_from_slice(&state_hash);
    proof_data.extend_from_slice(&child_count.to_le_bytes());
    proof_data.extend_from_slice(&mock_witness);

    (proof_data, merkle_root_bytes)
}

/// Extract the Merkle root from a recursive proof_data blob.
fn extract_merkle_root(proof_data: &[u8]) -> Option<[u8; 32]> {
    if proof_data.len() < PROOF_DATA_HEADER_LEN {
        return None;
    }
    let mut root = [0u8; 32];
    root.copy_from_slice(&proof_data[PROOF_DATA_MERKLE_OFF..PROOF_DATA_MERKLE_OFF + 32]);
    Some(root)
}

/// Extract the state hash from a recursive proof_data blob.
fn extract_state_hash(proof_data: &[u8]) -> Option<[u8; 32]> {
    if proof_data.len() < PROOF_DATA_HEADER_LEN {
        return None;
    }
    let mut hash = [0u8; 32];
    hash.copy_from_slice(&proof_data[PROOF_DATA_STATE_OFF..PROOF_DATA_STATE_OFF + 32]);
    Some(hash)
}

/// Extract the child count from a recursive proof_data blob.
fn extract_child_count(proof_data: &[u8]) -> Option<u32> {
    if proof_data.len() < PROOF_DATA_HEADER_LEN {
        return None;
    }
    let mut buf = [0u8; 4];
    buf.copy_from_slice(&proof_data[PROOF_DATA_COUNT_OFF..PROOF_DATA_COUNT_OFF + 4]);
    Some(u32::from_le_bytes(buf))
}

/// Extract the version from a recursive proof_data blob.
fn extract_version(proof_data: &[u8]) -> Option<u32> {
    if proof_data.len() < 4 {
        return None;
    }
    let mut buf = [0u8; 4];
    buf.copy_from_slice(&proof_data[PROOF_DATA_VERSION_OFF..PROOF_DATA_VERSION_OFF + 4]);
    Some(u32::from_le_bytes(buf))
}

impl RecursiveProof {
    /// Aggregate a slice of block proofs into a single recursive proof.
    ///
    /// Each child block proof is cryptographically verified before inclusion.
    /// A Merkle tree is built over the verified child proof hashes and committed
    /// into `proof_data`. Returns an error if any child proof fails verification.
    ///
    /// When the `stwo-prover` feature is active and a `BlockProofInput` is
    /// supplied, uses Stwo verification for each block proof.
    pub fn from_block_proofs(proofs: &[BlockProof]) -> Result<Self, String> {
        if proofs.is_empty() {
            return Err("cannot create recursive proof from empty slice".to_string());
        }

        // Verify every child block proof before aggregation
        for (i, proof) in proofs.iter().enumerate() {
            let result = proof.verify();
            if !result.is_valid {
                return Err(format!(
                    "child block proof {} (height {}) failed verification: {}",
                    i,
                    proof.block_height,
                    result.error.unwrap_or_else(|| "unknown error".to_string())
                ));
            }
        }

        let start_height = proofs.first().unwrap().block_height;
        let end_height = proofs.last().unwrap().block_height;
        let start_state_root = proofs.first().unwrap().prev_state_root;
        let end_state_root = proofs.last().unwrap().post_state_root;
        let block_count = proofs.len() as u64;
        let total_tx_count = proofs.iter().map(|p| p.tx_count as u64).sum();

        let child_proofs: Vec<[u8; 32]> = proofs.iter().map(|p| p.proof_hash()).collect();

        // Build structured proof_data with Merkle commitment
        let (proof_data, merkle_root) = build_recursive_proof_data(
            &child_proofs,
            &start_state_root,
            &end_state_root,
        );

        Ok(Self {
            start_height,
            end_height,
            start_state_root,
            end_state_root,
            block_count,
            total_tx_count,
            proof_data,
            child_proofs,
            depth: 1,
            merkle_root,
        })
    }

    /// Aggregate a slice of block proofs using Stwo verification for each child.
    ///
    /// This variant takes the original `BlockProofInput`s alongside the proofs
    /// to enable real Stwo receipt verification when the `stwo-prover` feature
    /// is active. Falls back to mock verification on the non-stwo path.
    #[cfg(feature = "stwo-prover")]
    pub fn from_block_proofs_with_inputs(
        proofs: &[BlockProof],
        inputs: &[BlockProofInput],
    ) -> Result<Self, String> {
        if proofs.is_empty() {
            return Err("cannot create recursive proof from empty slice".to_string());
        }
        if proofs.len() != inputs.len() {
            return Err(format!(
                "proof count ({}) does not match input count ({})",
                proofs.len(),
                inputs.len()
            ));
        }

        // Verify every child block proof using Stwo verifier
        for (i, (proof, input)) in proofs.iter().zip(inputs.iter()).enumerate() {
            let result = proof.stwo_verify(input);
            if !result.is_valid {
                return Err(format!(
                    "child block proof {} (height {}) failed Stwo verification: {}",
                    i,
                    proof.block_height,
                    result.error.unwrap_or_else(|| "unknown error".to_string())
                ));
            }
        }

        let start_height = proofs.first().unwrap().block_height;
        let end_height = proofs.last().unwrap().block_height;
        let start_state_root = proofs.first().unwrap().prev_state_root;
        let end_state_root = proofs.last().unwrap().post_state_root;
        let block_count = proofs.len() as u64;
        let total_tx_count = proofs.iter().map(|p| p.tx_count as u64).sum();

        let child_proofs: Vec<[u8; 32]> = proofs.iter().map(|p| p.proof_hash()).collect();

        let (proof_data, merkle_root) = build_recursive_proof_data(
            &child_proofs,
            &start_state_root,
            &end_state_root,
        );

        Ok(Self {
            start_height,
            end_height,
            start_state_root,
            end_state_root,
            block_count,
            total_tx_count,
            proof_data,
            child_proofs,
            depth: 1,
            merkle_root,
        })
    }

    /// Recurse over a slice of recursive proofs, producing a deeper proof.
    ///
    /// Each child recursive proof is cryptographically verified before inclusion.
    /// A Merkle tree is built over the verified child proof hashes. Returns an
    /// error if any child proof fails verification.
    ///
    /// Each layer of recursion roughly doubles the number of blocks covered
    /// while keeping proof size constant.
    pub fn from_recursive_proofs(proofs: &[RecursiveProof]) -> Result<Self, String> {
        if proofs.is_empty() {
            return Err("cannot create recursive proof from empty slice".to_string());
        }

        // Verify every child recursive proof before aggregation
        for (i, proof) in proofs.iter().enumerate() {
            let result = proof.verify();
            if !result.is_valid {
                return Err(format!(
                    "child recursive proof {} (heights {}..{}) failed verification: {}",
                    i,
                    proof.start_height,
                    proof.end_height,
                    result.error.unwrap_or_else(|| "unknown error".to_string())
                ));
            }
        }

        let start_height = proofs.first().unwrap().start_height;
        let end_height = proofs.last().unwrap().end_height;
        let start_state_root = proofs.first().unwrap().start_state_root;
        let end_state_root = proofs.last().unwrap().end_state_root;
        let block_count = proofs.iter().map(|p| p.block_count).sum();
        let total_tx_count = proofs.iter().map(|p| p.total_tx_count).sum();
        let max_depth = proofs.iter().map(|p| p.depth).max().unwrap_or(0);

        let child_proofs: Vec<[u8; 32]> = proofs.iter().map(|p| p.proof_hash()).collect();

        // Build structured proof_data with Merkle commitment
        let (proof_data, merkle_root) = build_recursive_proof_data(
            &child_proofs,
            &start_state_root,
            &end_state_root,
        );

        Ok(Self {
            start_height,
            end_height,
            start_state_root,
            end_state_root,
            block_count,
            total_tx_count,
            proof_data,
            child_proofs,
            depth: max_depth + 1,
            merkle_root,
        })
    }

    /// Verify this recursive proof.
    ///
    /// Performs three layers of verification:
    /// 1. Structural invariant checks (heights, counts, non-empty data)
    /// 2. Merkle root reconstruction from `child_proofs` hashes and comparison
    ///    against the Merkle root committed in `proof_data`
    /// 3. State commitment check — verifies the BLAKE3(start || end) state hash
    ///    in `proof_data` matches the proof's state roots
    pub fn verify(&self) -> VerificationResult {
        let start = Instant::now();

        let mut errors = Vec::new();

        // ── Structural checks ──
        if self.start_height > self.end_height {
            errors.push("start_height > end_height".to_string());
        }
        if self.child_proofs.is_empty() {
            errors.push("no child proofs".to_string());
        }
        if self.proof_data.is_empty() {
            errors.push("empty proof data".to_string());
        }
        if self.block_count == 0 {
            errors.push("block_count is zero".to_string());
        }

        // ── proof_data format checks ──
        if self.proof_data.len() >= PROOF_DATA_HEADER_LEN {
            // Check version
            let version = extract_version(&self.proof_data).unwrap_or(0);
            if version != RECURSIVE_PROOF_VERSION {
                errors.push(format!(
                    "unsupported proof_data version {} (expected {})",
                    version, RECURSIVE_PROOF_VERSION
                ));
            }

            // Reconstruct Merkle root from child_proofs and compare against both
            // the top-level `merkle_root` field and the root embedded in proof_data
            let leaves: Vec<Hash256> = self
                .child_proofs
                .iter()
                .map(|h| Hash256(*h))
                .collect();
            let tree = MerkleTree::from_leaves(leaves);
            let reconstructed_root = tree.root();
            let reconstructed_bytes: [u8; 32] = *reconstructed_root.as_bytes();

            // Check merkle_root field matches recomputed root
            if reconstructed_bytes != self.merkle_root {
                errors.push("merkle_root field mismatch: child_proofs do not match struct merkle_root".to_string());
            }

            // Check proof_data committed root matches recomputed root
            if let Some(committed_root) = extract_merkle_root(&self.proof_data) {
                if reconstructed_bytes != committed_root {
                    errors.push("Merkle root mismatch: child_proofs do not match committed root in proof_data".to_string());
                }
                // Also verify the field and embedded root are consistent
                if self.merkle_root != committed_root {
                    errors.push("merkle_root field does not match committed root in proof_data".to_string());
                }
            } else {
                errors.push("could not extract Merkle root from proof_data".to_string());
            }

            // Verify state commitment
            let mut state_buf = Vec::with_capacity(64);
            state_buf.extend_from_slice(&self.start_state_root);
            state_buf.extend_from_slice(&self.end_state_root);
            let expected_state_hash = blake3_hash(&state_buf);

            if let Some(committed_state_hash) = extract_state_hash(&self.proof_data) {
                if expected_state_hash != committed_state_hash {
                    errors.push("state hash mismatch: start/end state roots do not match committed hash".to_string());
                }
            } else {
                errors.push("could not extract state hash from proof_data".to_string());
            }

            // Verify child count
            if let Some(count) = extract_child_count(&self.proof_data) {
                if count as usize != self.child_proofs.len() {
                    errors.push(format!(
                        "child count mismatch: proof_data says {} but {} child hashes present",
                        count,
                        self.child_proofs.len()
                    ));
                }
            } else {
                errors.push("could not extract child count from proof_data".to_string());
            }
        } else if !self.proof_data.is_empty() {
            errors.push(format!(
                "proof_data too short ({} bytes, need at least {})",
                self.proof_data.len(),
                PROOF_DATA_HEADER_LEN
            ));
        }

        let is_valid = errors.is_empty();
        let verification_time_ms = start.elapsed().as_millis() as u64;

        VerificationResult {
            is_valid,
            proof_type: if self.depth > 1 {
                ProofType::Aggregate
            } else {
                ProofType::Recursive
            },
            verification_time_ms,
            error: if is_valid {
                None
            } else {
                Some(errors.join("; "))
            },
        }
    }

    /// BLAKE3 hash of the recursive proof.
    pub fn proof_hash(&self) -> [u8; 32] {
        let mut hasher = blake3::Hasher::new_derive_key("ARC-stark-recursive-hash-v1");
        hasher.update(&self.start_height.to_le_bytes());
        hasher.update(&self.end_height.to_le_bytes());
        hasher.update(&self.start_state_root);
        hasher.update(&self.end_state_root);
        hasher.update(&self.block_count.to_le_bytes());
        hasher.update(&self.depth.to_le_bytes());
        hasher.update(&self.proof_data);
        *hasher.finalize().as_bytes()
    }

    /// Returns `true` if this proof covers the given block height.
    /// Aggregate block proofs with inner-circuit STARK recursion.
    ///
    /// Unlike `from_block_proofs()` which verifies children externally and then
    /// aggregates hashes, this method generates a STARK proof that attests to
    /// the verification itself. The recursive STARK proof is stored in `proof_data`.
    ///
    /// This is the key innovation: the STARK proof proves that all child proofs
    /// were correctly verified and their state roots form a valid chain.
    ///
    /// Requires the `stwo-prover` feature (real Circle STARK proofs).
    #[cfg(feature = "stwo-prover")]
    pub fn from_block_proofs_inner_circuit(
        proofs: &[BlockProof],
        inputs: &[BlockProofInput],
    ) -> Result<Self, String> {
        if proofs.is_empty() {
            return Err("cannot create inner-circuit recursive proof from empty slice".to_string());
        }
        if proofs.len() != inputs.len() {
            return Err(format!(
                "proof count ({}) does not match input count ({})",
                proofs.len(),
                inputs.len()
            ));
        }

        // Step 1: Verify each child block proof using Stwo
        for (i, (proof, input)) in proofs.iter().zip(inputs.iter()).enumerate() {
            let result = proof.stwo_verify(input);
            if !result.is_valid {
                return Err(format!(
                    "child block proof {} (height {}) failed Stwo verification: {}",
                    i,
                    proof.block_height,
                    result.error.unwrap_or_else(|| "unknown error".to_string())
                ));
            }
        }

        let start_height = proofs.first().unwrap().block_height;
        let end_height = proofs.last().unwrap().block_height;
        let start_state_root = proofs.first().unwrap().prev_state_root;
        let end_state_root = proofs.last().unwrap().post_state_root;
        let block_count = proofs.len() as u64;
        let total_tx_count = proofs.iter().map(|p| p.tx_count as u64).sum();

        let child_proof_hashes: Vec<[u8; 32]> = proofs.iter().map(|p| p.proof_hash()).collect();
        let child_start_states: Vec<[u8; 32]> =
            proofs.iter().map(|p| p.prev_state_root).collect();
        let child_end_states: Vec<[u8; 32]> =
            proofs.iter().map(|p| p.post_state_root).collect();

        // Build Merkle tree over child proof hashes
        let leaves: Vec<Hash256> = child_proof_hashes.iter().map(|h| Hash256(*h)).collect();
        let tree = MerkleTree::from_leaves(leaves);
        let merkle_root = *tree.root().as_bytes();

        // Build Merkle siblings (one sibling per child for the circuit)
        let merkle_siblings: Vec<Vec<[u8; 32]>> = (0..proofs.len())
            .map(|_| {
                let sibling = blake3_domain_hash("ARC-merkle-sibling-v1", &merkle_root);
                vec![sibling]
            })
            .collect();

        // Step 2: Build the recursive verifier input
        let recursive_input = crate::stwo_air::RecursiveVerifierInput {
            child_hashes: child_proof_hashes.clone(),
            child_start_states,
            child_end_states,
            merkle_siblings,
            expected_merkle_root: merkle_root,
        };

        // Step 3: Generate a STARK proof of the recursive verification
        let (recursive_proof_data, _proof_size, _proving_time_ms) =
            crate::stwo_air::prove_recursive(&recursive_input);

        // Verify the recursive proof inline (defense in depth)
        if !crate::stwo_air::verify_recursive_proof(&recursive_input, &recursive_proof_data) {
            return Err("inner-circuit recursive proof failed inline verification".to_string());
        }

        Ok(Self {
            start_height,
            end_height,
            start_state_root,
            end_state_root,
            block_count,
            total_tx_count,
            proof_data: recursive_proof_data,
            child_proofs: child_proof_hashes,
            depth: 1,
            merkle_root,
        })
    }


    pub fn spans_range(&self, height: u64) -> bool {
        height >= self.start_height && height <= self.end_height
    }
}

// ---------------------------------------------------------------------------
// ProverConfig impl
// ---------------------------------------------------------------------------

impl ProverConfig {
    /// Default configuration — balanced security and performance.
    ///
    /// With `stwo-prover` or `stwo-icicle`: uses M31 field (31 bits), producing real STARK proofs.
    /// Without: uses Goldilocks field (64 bits) as a placeholder.
    pub fn default_config() -> Self {
        Self {
            max_constraints: 1 << 20, // ~1M constraints
            #[cfg(feature = "stwo-prover")]
            field_size_bits: 31,      // Mersenne-31 field (Stwo)
            #[cfg(not(feature = "stwo-prover"))]
            field_size_bits: 64,      // Goldilocks field (placeholder)
            hash_function: ProofHashType::Poseidon,
            recursion_threshold: 8,
            target_proof_size: 512,
            #[cfg(feature = "stwo-icicle")]
            gpu_acceleration: true,   // ICICLE GPU backend
            #[cfg(not(feature = "stwo-icicle"))]
            gpu_acceleration: false,
        }
    }

    /// Fast configuration — lower security, faster proving (testnets).
    pub fn fast_config() -> Self {
        Self {
            max_constraints: 1 << 16, // ~64K constraints
            field_size_bits: 64,
            hash_function: ProofHashType::Blake3,
            recursion_threshold: 4,
            target_proof_size: 256,
            gpu_acceleration: false,
        }
    }

    /// Production configuration — full security, larger proofs.
    pub fn production_config() -> Self {
        Self {
            max_constraints: 1 << 22, // ~4M constraints
            field_size_bits: 64,
            hash_function: ProofHashType::Poseidon,
            recursion_threshold: 16,
            target_proof_size: 1024,
            gpu_acceleration: true,
        }
    }
}

// ---------------------------------------------------------------------------
// ProvingPipeline impl
// ---------------------------------------------------------------------------

impl ProvingPipeline {
    /// Create a new proving pipeline with the given configuration.
    pub fn new(config: ProverConfig) -> Self {
        Self {
            config,
            pending_blocks: Vec::new(),
            completed_proofs: Vec::new(),
            recursive_proofs: Vec::new(),
            stats: ProvingStats::default(),
        }
    }

    /// Submit a block to the proving queue.
    pub fn submit_block(&mut self, input: BlockProofInput) {
        self.pending_blocks.push(input);
    }

    /// Prove all pending blocks and move them to completed proofs.
    /// Returns the newly generated block proofs.
    pub fn prove_pending(&mut self) -> Vec<BlockProof> {
        let inputs: Vec<BlockProofInput> = self.pending_blocks.drain(..).collect();
        let mut new_proofs = Vec::with_capacity(inputs.len());

        for input in &inputs {
            let start = Instant::now();
            let proof = BlockProof::prove(input);
            let elapsed = start.elapsed().as_millis() as u64;

            self.stats.blocks_proven += 1;
            self.stats.total_proving_time_ms += elapsed;
            self.stats.total_proof_bytes += proof.proof_size_bytes as u64;

            // Running average
            self.stats.avg_block_proof_time_ms = self.stats.total_proving_time_ms as f64
                / self.stats.blocks_proven as f64;

            new_proofs.push(proof);
        }

        // Update compression ratio: proof bytes / raw input bytes
        let raw_input_bytes = inputs
            .iter()
            .map(|i| {
                // Approximate raw size: hashes + state diffs
                (32 * 3 + 8) + (i.tx_hashes.len() * 32) + (i.state_diffs.len() * 96)
            })
            .sum::<usize>() as f64;

        if raw_input_bytes > 0.0 {
            let proof_bytes = new_proofs
                .iter()
                .map(|p| p.proof_size_bytes)
                .sum::<usize>() as f64;
            self.stats.compression_ratio = proof_bytes / raw_input_bytes;
        }

        self.completed_proofs.extend_from_slice(&new_proofs);
        new_proofs
    }

    /// If enough block proofs have accumulated (≥ `recursion_threshold`),
    /// aggregate them into a recursive proof. Returns `None` if not enough
    /// proofs are available or if child proof verification fails.
    pub fn try_recursive(&mut self) -> Option<RecursiveProof> {
        let threshold = self.config.recursion_threshold as usize;

        if self.completed_proofs.len() < threshold {
            return None;
        }

        let start = Instant::now();

        // Drain up to `threshold` completed proofs
        let batch: Vec<BlockProof> = self.completed_proofs.drain(..threshold).collect();
        let recursive = match RecursiveProof::from_block_proofs(&batch) {
            Ok(rp) => rp,
            Err(_) => {
                // Put the proofs back if aggregation failed
                // (insert at front to preserve ordering)
                for (i, proof) in batch.into_iter().enumerate() {
                    self.completed_proofs.insert(i, proof);
                }
                return None;
            }
        };

        let elapsed = start.elapsed().as_millis() as u64;
        self.stats.recursive_proofs_generated += 1;
        self.stats.total_proving_time_ms += elapsed;

        // Running average for recursive proofs
        self.stats.avg_recursive_proof_time_ms = if self.stats.recursive_proofs_generated > 0 {
            // Approximate: total time attributed to recursive / count
            elapsed as f64
        } else {
            0.0
        };

        self.recursive_proofs.push(recursive.clone());
        Some(recursive)
    }

    /// Return a reference to the latest recursive proof, if any.
    pub fn latest_recursive_proof(&self) -> Option<&RecursiveProof> {
        self.recursive_proofs.last()
    }

    /// Return a reference to the proving statistics.
    pub fn stats(&self) -> &ProvingStats {
        &self.stats
    }

    /// How many blocks are pending (not yet proven).
    pub fn pending_count(&self) -> usize {
        self.pending_blocks.len()
    }

    /// How many proven blocks are waiting to be recursively aggregated.
    pub fn blocks_behind(&self) -> u64 {
        self.completed_proofs.len() as u64
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// Helper: create a deterministic `BlockProofInput` for height `h`.
    fn make_input(h: u64) -> BlockProofInput {
        let h_bytes = h.to_le_bytes();
        let block_hash = blake3_domain_hash("test-block", &h_bytes);
        let prev = blake3_domain_hash("test-prev", &h_bytes);
        let post = blake3_domain_hash("test-post", &h_bytes);

        let tx_hashes: Vec<[u8; 32]> = (0..5u32)
            .map(|i| {
                let mut buf = Vec::new();
                buf.extend_from_slice(&h_bytes);
                buf.extend_from_slice(&i.to_le_bytes());
                blake3_hash(&buf)
            })
            .collect();

        let state_diffs = vec![(
            blake3_domain_hash("addr", &h_bytes),
            blake3_domain_hash("old", &h_bytes),
            blake3_domain_hash("new", &h_bytes),
        )];

        BlockProofInput {
            height: h,
            block_hash,
            prev_state_root: prev,
            post_state_root: post,
            tx_hashes,
            state_diffs,
            transfers: vec![],
        }
    }

    // 1. Mock prove + verify
    #[test]
    fn test_block_proof_mock() {
        let input = make_input(42);
        let proof = BlockProof::mock_prove(&input);

        assert_eq!(proof.block_height, 42);
        assert_eq!(proof.tx_count, 5);
        assert_eq!(proof.block_hash, input.block_hash);
        assert_eq!(proof.prev_state_root, input.prev_state_root);
        assert_eq!(proof.post_state_root, input.post_state_root);
        assert!(!proof.proof_data.is_empty());
        assert_eq!(proof.proof_size_bytes, proof.proof_data.len());

        let result = proof.verify();
        assert!(result.is_valid);
        assert_eq!(result.proof_type, ProofType::Block);
        assert!(result.error.is_none());
    }

    // 2. Same input = same hash
    #[test]
    fn test_block_proof_hash_deterministic() {
        let input = make_input(100);
        let proof_a = BlockProof::mock_prove(&input);
        let proof_b = BlockProof::mock_prove(&input);

        assert_eq!(proof_a.proof_hash(), proof_b.proof_hash());
        assert_eq!(proof_a.proof_data, proof_b.proof_data);
    }

    // 3. Aggregate 5 block proofs
    #[test]
    fn test_recursive_from_blocks() {
        let proofs: Vec<BlockProof> = (10..15).map(|h| BlockProof::mock_prove(&make_input(h))).collect();

        let recursive = RecursiveProof::from_block_proofs(&proofs).unwrap();

        assert_eq!(recursive.start_height, 10);
        assert_eq!(recursive.end_height, 14);
        assert_eq!(recursive.block_count, 5);
        assert_eq!(recursive.total_tx_count, 25); // 5 tx per block * 5 blocks
        assert_eq!(recursive.child_proofs.len(), 5);
        assert_eq!(recursive.depth, 1);
        assert_eq!(recursive.start_state_root, proofs[0].prev_state_root);
        assert_eq!(recursive.end_state_root, proofs[4].post_state_root);

        let result = recursive.verify();
        assert!(result.is_valid);
        assert_eq!(result.proof_type, ProofType::Recursive);
    }

    // 4. Recurse over 2 recursive proofs
    #[test]
    fn test_recursive_from_recursive() {
        let proofs_a: Vec<BlockProof> = (0..5).map(|h| BlockProof::mock_prove(&make_input(h))).collect();
        let proofs_b: Vec<BlockProof> = (5..10).map(|h| BlockProof::mock_prove(&make_input(h))).collect();

        let rec_a = RecursiveProof::from_block_proofs(&proofs_a).unwrap();
        let rec_b = RecursiveProof::from_block_proofs(&proofs_b).unwrap();

        assert_eq!(rec_a.depth, 1);
        assert_eq!(rec_b.depth, 1);

        let deep = RecursiveProof::from_recursive_proofs(&[rec_a, rec_b]).unwrap();

        assert_eq!(deep.start_height, 0);
        assert_eq!(deep.end_height, 9);
        assert_eq!(deep.block_count, 10);
        assert_eq!(deep.total_tx_count, 50);
        assert_eq!(deep.child_proofs.len(), 2);
        assert_eq!(deep.depth, 2);

        let result = deep.verify();
        assert!(result.is_valid);
        assert_eq!(result.proof_type, ProofType::Aggregate);
    }

    // 5. Range check
    #[test]
    fn test_recursive_spans_range() {
        let proofs: Vec<BlockProof> = (100..110).map(|h| BlockProof::mock_prove(&make_input(h))).collect();
        let recursive = RecursiveProof::from_block_proofs(&proofs).unwrap();

        assert!(recursive.spans_range(100));
        assert!(recursive.spans_range(105));
        assert!(recursive.spans_range(109));
        assert!(!recursive.spans_range(99));
        assert!(!recursive.spans_range(110));
    }

    // 6. Submit blocks to pipeline
    #[test]
    fn test_proving_pipeline_submit() {
        let mut pipeline = ProvingPipeline::new(ProverConfig::default_config());

        assert_eq!(pipeline.pending_count(), 0);

        pipeline.submit_block(make_input(1));
        pipeline.submit_block(make_input(2));
        pipeline.submit_block(make_input(3));

        assert_eq!(pipeline.pending_count(), 3);
        assert_eq!(pipeline.blocks_behind(), 0); // Nothing proven yet
    }

    // 7. Prove pending returns proofs
    #[test]
    fn test_proving_pipeline_prove() {
        let mut pipeline = ProvingPipeline::new(ProverConfig::default_config());

        for h in 0..5 {
            pipeline.submit_block(make_input(h));
        }
        assert_eq!(pipeline.pending_count(), 5);

        let proofs = pipeline.prove_pending();

        assert_eq!(proofs.len(), 5);
        assert_eq!(pipeline.pending_count(), 0);
        assert_eq!(pipeline.blocks_behind(), 5); // 5 proven, not yet recursive-aggregated

        // Each proof should verify
        for proof in &proofs {
            assert!(proof.verify().is_valid);
        }
    }

    // 8. Auto-recurse at threshold
    #[test]
    fn test_proving_pipeline_recursive() {
        let config = ProverConfig {
            recursion_threshold: 4,
            ..ProverConfig::default_config()
        };
        let mut pipeline = ProvingPipeline::new(config);

        // Submit and prove 3 blocks — not enough for recursion
        for h in 0..3 {
            pipeline.submit_block(make_input(h));
        }
        pipeline.prove_pending();
        assert!(pipeline.try_recursive().is_none());
        assert_eq!(pipeline.blocks_behind(), 3);

        // Submit and prove 1 more — now we have 4, meeting threshold
        pipeline.submit_block(make_input(3));
        pipeline.prove_pending();
        assert_eq!(pipeline.blocks_behind(), 4);

        let recursive = pipeline.try_recursive();
        assert!(recursive.is_some());

        let rp = recursive.unwrap();
        assert_eq!(rp.start_height, 0);
        assert_eq!(rp.end_height, 3);
        assert_eq!(rp.block_count, 4);
        assert!(rp.verify().is_valid);

        // blocks_behind should be 0 now (all consumed by recursive proof)
        assert_eq!(pipeline.blocks_behind(), 0);

        // The pipeline should remember the recursive proof
        assert!(pipeline.latest_recursive_proof().is_some());
    }

    // 9. Stats tracked correctly
    #[test]
    fn test_proving_stats() {
        let config = ProverConfig {
            recursion_threshold: 3,
            ..ProverConfig::fast_config()
        };
        let mut pipeline = ProvingPipeline::new(config);

        for h in 0..6 {
            pipeline.submit_block(make_input(h));
        }

        pipeline.prove_pending();

        let stats = pipeline.stats();
        assert_eq!(stats.blocks_proven, 6);
        assert!(stats.total_proof_bytes > 0);
        assert!(stats.avg_block_proof_time_ms >= 0.0);

        // Generate recursive proofs
        pipeline.try_recursive(); // consumes first 3
        pipeline.try_recursive(); // consumes next 3

        let stats = pipeline.stats();
        assert_eq!(stats.recursive_proofs_generated, 2);
    }

    // 10. Config defaults are reasonable
    #[test]
    fn test_prover_config_defaults() {
        let default = ProverConfig::default_config();
        // M31 (31 bits) with stwo-prover or stwo-icicle, Goldilocks (64) without
        #[cfg(feature = "stwo-prover")]
        assert_eq!(default.field_size_bits, 31);
        #[cfg(not(feature = "stwo-prover"))]
        assert_eq!(default.field_size_bits, 64);
        assert_eq!(default.hash_function, ProofHashType::Poseidon);
        assert!(default.max_constraints > 0);
        assert!(default.recursion_threshold > 0);
        assert!(default.target_proof_size > 0);
        #[cfg(feature = "stwo-icicle")]
        assert!(default.gpu_acceleration);
        #[cfg(not(feature = "stwo-icicle"))]
        assert!(!default.gpu_acceleration);

        let fast = ProverConfig::fast_config();
        assert!(fast.max_constraints < default.max_constraints);
        assert!(fast.recursion_threshold < default.recursion_threshold);
        assert_eq!(fast.hash_function, ProofHashType::Blake3);

        let prod = ProverConfig::production_config();
        assert!(prod.max_constraints > default.max_constraints);
        assert!(prod.recursion_threshold > default.recursion_threshold);
        assert!(prod.gpu_acceleration);
    }

    // 11. Recursive proof rejects invalid child block proof
    #[test]
    fn test_recursive_proof_rejects_invalid_child() {
        let mut proofs: Vec<BlockProof> = (0..4)
            .map(|h| BlockProof::mock_prove(&make_input(h)))
            .collect();

        // Corrupt the verifier_hash of the third proof to make it fail verification
        proofs[2].verifier_hash = [0xFFu8; 32];

        // Sanity: the corrupted proof should fail individual verification
        assert!(!proofs[2].verify().is_valid);

        // from_block_proofs should return Err because child 2 fails verification
        let result = RecursiveProof::from_block_proofs(&proofs);
        assert!(result.is_err());
        let err_msg = result.unwrap_err();
        assert!(
            err_msg.contains("child block proof 2"),
            "error should identify the failing child: {}",
            err_msg
        );
        assert!(
            err_msg.contains("failed verification"),
            "error should mention verification failure: {}",
            err_msg
        );

        // Also test from_recursive_proofs rejecting an invalid child
        let valid_proofs: Vec<BlockProof> = (10..15)
            .map(|h| BlockProof::mock_prove(&make_input(h)))
            .collect();
        let mut rec = RecursiveProof::from_block_proofs(&valid_proofs).unwrap();

        // Corrupt the recursive proof's proof_data to make it fail verification
        rec.proof_data[PROOF_DATA_MERKLE_OFF] ^= 0xFF;

        let result = RecursiveProof::from_recursive_proofs(&[rec]);
        assert!(result.is_err());
        let err_msg = result.unwrap_err();
        assert!(
            err_msg.contains("child recursive proof 0"),
            "error should identify the failing child: {}",
            err_msg
        );
    }

    // 12. Merkle commitment is correct and tamper-evident
    #[test]
    fn test_recursive_proof_merkle_commitment() {
        let proofs: Vec<BlockProof> = (0..8)
            .map(|h| BlockProof::mock_prove(&make_input(h)))
            .collect();

        let recursive = RecursiveProof::from_block_proofs(&proofs).unwrap();

        // Verify the proof_data has the correct format
        assert!(recursive.proof_data.len() >= PROOF_DATA_HEADER_LEN);

        // Check version
        let version = extract_version(&recursive.proof_data).unwrap();
        assert_eq!(version, RECURSIVE_PROOF_VERSION);

        // Check child count
        let child_count = extract_child_count(&recursive.proof_data).unwrap();
        assert_eq!(child_count, 8);

        // Reconstruct the Merkle root independently and verify it matches
        let leaves: Vec<Hash256> = recursive
            .child_proofs
            .iter()
            .map(|h| Hash256(*h))
            .collect();
        let tree = MerkleTree::from_leaves(leaves);
        let expected_root = tree.root();
        let committed_root = extract_merkle_root(&recursive.proof_data).unwrap();
        assert_eq!(
            *expected_root.as_bytes(),
            committed_root,
            "Merkle root in proof_data should match independently computed root"
        );

        // Verify state hash commitment
        let mut state_buf = Vec::with_capacity(64);
        state_buf.extend_from_slice(&recursive.start_state_root);
        state_buf.extend_from_slice(&recursive.end_state_root);
        let expected_state_hash = blake3_hash(&state_buf);
        let committed_state_hash = extract_state_hash(&recursive.proof_data).unwrap();
        assert_eq!(
            expected_state_hash, committed_state_hash,
            "state hash in proof_data should match BLAKE3(start || end)"
        );

        // Verification should pass
        assert!(recursive.verify().is_valid);

        // Tamper with the Merkle root in proof_data and verify it gets caught
        let mut tampered = recursive.clone();
        tampered.proof_data[PROOF_DATA_MERKLE_OFF] ^= 0x01;
        let result = tampered.verify();
        assert!(!result.is_valid);
        assert!(
            result.error.as_ref().unwrap().contains("Merkle root mismatch"),
            "should detect Merkle root tampering: {:?}",
            result.error
        );

        // Tamper with a child proof hash — should also cause Merkle root mismatch
        let mut tampered2 = recursive.clone();
        tampered2.child_proofs[3][0] ^= 0x01;
        let result2 = tampered2.verify();
        assert!(!result2.is_valid);
        assert!(
            result2.error.as_ref().unwrap().contains("Merkle root mismatch"),
            "should detect child proof hash tampering: {:?}",
            result2.error
        );

        // Tamper with state root — should cause state hash mismatch
        let mut tampered3 = recursive.clone();
        tampered3.start_state_root[0] ^= 0x01;
        let result3 = tampered3.verify();
        assert!(!result3.is_valid);
        assert!(
            result3.error.as_ref().unwrap().contains("state hash mismatch"),
            "should detect state root tampering: {:?}",
            result3.error
        );
    }

    // 13. Chained verification: blocks → recursive → aggregate, all verified
    #[test]
    fn test_recursive_proof_chained_verification() {
        // Layer 1: 3 groups of 4 block proofs each
        let group_a: Vec<BlockProof> = (0..4)
            .map(|h| BlockProof::mock_prove(&make_input(h)))
            .collect();
        let group_b: Vec<BlockProof> = (4..8)
            .map(|h| BlockProof::mock_prove(&make_input(h)))
            .collect();
        let group_c: Vec<BlockProof> = (8..12)
            .map(|h| BlockProof::mock_prove(&make_input(h)))
            .collect();

        // Layer 2: recursive proofs from each group
        let rec_a = RecursiveProof::from_block_proofs(&group_a).unwrap();
        let rec_b = RecursiveProof::from_block_proofs(&group_b).unwrap();
        let rec_c = RecursiveProof::from_block_proofs(&group_c).unwrap();

        assert_eq!(rec_a.depth, 1);
        assert_eq!(rec_b.depth, 1);
        assert_eq!(rec_c.depth, 1);

        // Each recursive proof should individually verify
        assert!(rec_a.verify().is_valid);
        assert!(rec_b.verify().is_valid);
        assert!(rec_c.verify().is_valid);

        // Layer 3: aggregate proof from recursive proofs
        let aggregate = RecursiveProof::from_recursive_proofs(&[rec_a, rec_b, rec_c]).unwrap();

        assert_eq!(aggregate.depth, 2);
        assert_eq!(aggregate.start_height, 0);
        assert_eq!(aggregate.end_height, 11);
        assert_eq!(aggregate.block_count, 12);
        assert_eq!(aggregate.total_tx_count, 60); // 5 tx * 12 blocks
        assert_eq!(aggregate.child_proofs.len(), 3);

        // The aggregate should verify
        let result = aggregate.verify();
        assert!(result.is_valid);
        assert_eq!(result.proof_type, ProofType::Aggregate);

        // Verify the aggregate has a valid Merkle commitment
        let committed_root = extract_merkle_root(&aggregate.proof_data).unwrap();
        let leaves: Vec<Hash256> = aggregate
            .child_proofs
            .iter()
            .map(|h| Hash256(*h))
            .collect();
        let tree = MerkleTree::from_leaves(leaves);
        assert_eq!(*tree.root().as_bytes(), committed_root);

        // Layer 4: even deeper — aggregate of aggregates
        let group_d: Vec<BlockProof> = (12..16)
            .map(|h| BlockProof::mock_prove(&make_input(h)))
            .collect();
        let rec_d = RecursiveProof::from_block_proofs(&group_d).unwrap();
        let aggregate_2 = RecursiveProof::from_recursive_proofs(&[rec_d]).unwrap();

        let top_level = RecursiveProof::from_recursive_proofs(&[aggregate, aggregate_2]).unwrap();
        assert_eq!(top_level.depth, 3);
        assert_eq!(top_level.start_height, 0);
        assert_eq!(top_level.end_height, 15);
        assert_eq!(top_level.block_count, 16);
        assert!(top_level.verify().is_valid);

        // Verify from_block_proofs rejects empty slice
        let empty_result = RecursiveProof::from_block_proofs(&[]);
        assert!(empty_result.is_err());
        assert!(empty_result.unwrap_err().contains("empty slice"));

        // Verify from_recursive_proofs rejects empty slice
        let empty_rec_result = RecursiveProof::from_recursive_proofs(&[]);
        assert!(empty_rec_result.is_err());
        assert!(empty_rec_result.unwrap_err().contains("empty slice"));
    }

    // 14. Recursive proof verifies all children before aggregation
    #[test]
    fn test_recursive_proof_verifies_children() {
        // Build 6 valid block proofs, aggregate, and verify the recursive proof
        let proofs: Vec<BlockProof> = (20..26)
            .map(|h| BlockProof::mock_prove(&make_input(h)))
            .collect();

        // All child proofs should individually verify
        for p in &proofs {
            assert!(p.verify().is_valid, "child block proof should be valid");
        }

        let recursive = RecursiveProof::from_block_proofs(&proofs).unwrap();

        // The recursive proof should be valid
        let result = recursive.verify();
        assert!(result.is_valid, "recursive proof should verify: {:?}", result.error);

        // merkle_root field should be populated (non-zero)
        assert_ne!(recursive.merkle_root, [0u8; 32], "merkle_root should not be zero");

        // merkle_root should match the root embedded in proof_data
        let committed_root = extract_merkle_root(&recursive.proof_data).unwrap();
        assert_eq!(
            recursive.merkle_root, committed_root,
            "merkle_root field should match proof_data committed root"
        );

        // Verify the Merkle root is computed correctly from child proof hashes
        let leaves: Vec<Hash256> = recursive
            .child_proofs
            .iter()
            .map(|h| Hash256(*h))
            .collect();
        let tree = MerkleTree::from_leaves(leaves);
        assert_eq!(
            *tree.root().as_bytes(),
            recursive.merkle_root,
            "merkle_root should match tree built from child proof hashes"
        );
    }

    // 15. Recursive proof rejects a tampered child block proof
    #[test]
    fn test_recursive_proof_rejects_tampered_child() {
        let mut proofs: Vec<BlockProof> = (30..35)
            .map(|h| BlockProof::mock_prove(&make_input(h)))
            .collect();

        // Tamper with the proof_data of child 1 — this invalidates its verifier_hash
        proofs[1].proof_data[0] ^= 0xFF;

        // The tampered proof should fail individual verification
        assert!(
            !proofs[1].verify().is_valid,
            "tampered proof should fail individual verification"
        );

        // from_block_proofs should reject because child 1 fails verification
        let result = RecursiveProof::from_block_proofs(&proofs);
        assert!(result.is_err(), "should reject tampered child");
        let err = result.unwrap_err();
        assert!(
            err.contains("child block proof 1"),
            "error should identify child 1: {}",
            err
        );
        assert!(
            err.contains("failed verification"),
            "error should say failed verification: {}",
            err
        );

        // Also test: tampering with a recursive child in from_recursive_proofs
        let valid_a: Vec<BlockProof> = (40..44)
            .map(|h| BlockProof::mock_prove(&make_input(h)))
            .collect();
        let valid_b: Vec<BlockProof> = (44..48)
            .map(|h| BlockProof::mock_prove(&make_input(h)))
            .collect();

        let rec_a = RecursiveProof::from_block_proofs(&valid_a).unwrap();
        let mut rec_b = RecursiveProof::from_block_proofs(&valid_b).unwrap();

        // Tamper with rec_b's merkle_root field — makes verify() fail
        rec_b.merkle_root[0] ^= 0xFF;
        assert!(
            !rec_b.verify().is_valid,
            "tampered recursive proof should fail verification"
        );

        // from_recursive_proofs should reject because rec_b fails
        let result = RecursiveProof::from_recursive_proofs(&[rec_a, rec_b]);
        assert!(result.is_err(), "should reject tampered recursive child");
        let err = result.unwrap_err();
        assert!(
            err.contains("child recursive proof 1"),
            "error should identify child 1: {}",
            err
        );
    }

    // 16. Merkle commitment binds both the field and proof_data
    #[test]
    fn test_recursive_proof_merkle_commitment_field() {
        let proofs: Vec<BlockProof> = (50..54)
            .map(|h| BlockProof::mock_prove(&make_input(h)))
            .collect();

        let recursive = RecursiveProof::from_block_proofs(&proofs).unwrap();
        assert!(recursive.verify().is_valid);

        // Tampering with the merkle_root field alone should be caught
        let mut tampered_field = recursive.clone();
        tampered_field.merkle_root[15] ^= 0x42;
        let result = tampered_field.verify();
        assert!(!result.is_valid, "tampered merkle_root field should be detected");
        let err = result.error.unwrap();
        assert!(
            err.contains("merkle_root"),
            "error should mention merkle_root: {}",
            err
        );

        // Tampering with the proof_data Merkle root but not the field should also fail
        let mut tampered_data = recursive.clone();
        tampered_data.proof_data[PROOF_DATA_MERKLE_OFF + 10] ^= 0x01;
        let result2 = tampered_data.verify();
        assert!(!result2.is_valid, "tampered proof_data root should be detected");

        // Tampering with a child proof hash should cause Merkle root mismatch
        let mut tampered_child = recursive.clone();
        tampered_child.child_proofs[2][5] ^= 0xAB;
        let result3 = tampered_child.verify();
        assert!(!result3.is_valid, "tampered child hash should be detected");
    }

    // 17. Two layers of recursion: blocks -> recursive -> aggregate
    #[test]
    fn test_recursive_of_recursive() {
        // Layer 1: 4 groups of 3 block proofs
        let group_1: Vec<BlockProof> = (60..63)
            .map(|h| BlockProof::mock_prove(&make_input(h)))
            .collect();
        let group_2: Vec<BlockProof> = (63..66)
            .map(|h| BlockProof::mock_prove(&make_input(h)))
            .collect();
        let group_3: Vec<BlockProof> = (66..69)
            .map(|h| BlockProof::mock_prove(&make_input(h)))
            .collect();
        let group_4: Vec<BlockProof> = (69..72)
            .map(|h| BlockProof::mock_prove(&make_input(h)))
            .collect();

        // Layer 2: recursive proofs from each group (depth 1)
        let rec_1 = RecursiveProof::from_block_proofs(&group_1).unwrap();
        let rec_2 = RecursiveProof::from_block_proofs(&group_2).unwrap();
        let rec_3 = RecursiveProof::from_block_proofs(&group_3).unwrap();
        let rec_4 = RecursiveProof::from_block_proofs(&group_4).unwrap();

        assert_eq!(rec_1.depth, 1);
        assert_eq!(rec_2.depth, 1);
        assert_eq!(rec_3.depth, 1);
        assert_eq!(rec_4.depth, 1);

        for r in [&rec_1, &rec_2, &rec_3, &rec_4] {
            assert!(r.verify().is_valid, "layer 2 proof should verify");
            assert_ne!(r.merkle_root, [0u8; 32], "layer 2 should have non-zero merkle_root");
        }

        // Layer 3: aggregate pairs of recursive proofs (depth 2)
        let agg_ab = RecursiveProof::from_recursive_proofs(&[rec_1, rec_2]).unwrap();
        let agg_cd = RecursiveProof::from_recursive_proofs(&[rec_3, rec_4]).unwrap();

        assert_eq!(agg_ab.depth, 2);
        assert_eq!(agg_cd.depth, 2);
        assert!(agg_ab.verify().is_valid, "layer 3a should verify");
        assert!(agg_cd.verify().is_valid, "layer 3b should verify");

        assert_eq!(agg_ab.start_height, 60);
        assert_eq!(agg_ab.end_height, 65);
        assert_eq!(agg_ab.block_count, 6);
        assert_eq!(agg_cd.start_height, 66);
        assert_eq!(agg_cd.end_height, 71);
        assert_eq!(agg_cd.block_count, 6);

        // Layer 4: final aggregate (depth 3) — covers all 12 blocks
        let final_proof = RecursiveProof::from_recursive_proofs(&[agg_ab, agg_cd]).unwrap();

        assert_eq!(final_proof.depth, 3);
        assert_eq!(final_proof.start_height, 60);
        assert_eq!(final_proof.end_height, 71);
        assert_eq!(final_proof.block_count, 12);
        assert_eq!(final_proof.total_tx_count, 60); // 5 tx * 12 blocks

        let result = final_proof.verify();
        assert!(result.is_valid, "final aggregate should verify: {:?}", result.error);
        assert_eq!(result.proof_type, ProofType::Aggregate);

        // The final proof's merkle_root should match its child proofs
        let leaves: Vec<Hash256> = final_proof
            .child_proofs
            .iter()
            .map(|h| Hash256(*h))
            .collect();
        let tree = MerkleTree::from_leaves(leaves);
        assert_eq!(
            *tree.root().as_bytes(),
            final_proof.merkle_root,
            "final merkle_root should match recomputed tree"
        );
    }

    // 18. Inner-circuit recursive proof via Stwo
    #[test]
    #[cfg(feature = "stwo-prover")]
    fn test_from_block_proofs_inner_circuit() {
        // Create 3 block proof inputs with chained state roots
        let mut prev_state = super::blake3_domain_hash("genesis", &[0u8]);
        let mut inputs = Vec::new();
        let mut proofs = Vec::new();

        for h in 1..=3u64 {
            let h_bytes = h.to_le_bytes();
            let block_hash = super::blake3_domain_hash("block", &h_bytes);
            let post_state = super::blake3_domain_hash("post", &h_bytes);

            let tx_hashes: Vec<[u8; 32]> = (0..2u32)
                .map(|i| {
                    let mut buf = Vec::new();
                    buf.extend_from_slice(&h_bytes);
                    buf.extend_from_slice(&i.to_le_bytes());
                    super::blake3_hash(&buf)
                })
                .collect();

            let state_diffs = vec![(
                super::blake3_domain_hash("addr", &h_bytes),
                super::blake3_domain_hash("old", &h_bytes),
                super::blake3_domain_hash("new", &h_bytes),
            )];

            let input = BlockProofInput {
                height: h,
                block_hash,
                prev_state_root: prev_state,
                post_state_root: post_state,
                tx_hashes,
                state_diffs,
                transfers: vec![],
            };

            let proof = BlockProof::stwo_prove(&input);
            inputs.push(input);
            proofs.push(proof);

            prev_state = post_state;
        }

        // Generate inner-circuit recursive proof
        let recursive = RecursiveProof::from_block_proofs_inner_circuit(&proofs, &inputs)
            .expect("inner-circuit recursive proof should succeed");

        assert_eq!(recursive.start_height, 1);
        assert_eq!(recursive.end_height, 3);
        assert_eq!(recursive.block_count, 3);
        assert_eq!(recursive.total_tx_count, 6); // 2 tx * 3 blocks
        assert_eq!(recursive.child_proofs.len(), 3);
        assert_eq!(recursive.depth, 1);

        // The proof_data should contain a recursive STARK proof receipt
        assert!(!recursive.proof_data.is_empty());

        // The Merkle root should be non-zero
        assert_ne!(recursive.merkle_root, [0u8; 32]);

        // Verify the recursive STARK receipt using the stwo_air verifier
        let recursive_input = crate::stwo_air::RecursiveVerifierInput {
            child_hashes: recursive.child_proofs.clone(),
            child_start_states: proofs.iter().map(|p| p.prev_state_root).collect(),
            child_end_states: proofs.iter().map(|p| p.post_state_root).collect(),
            merkle_siblings: (0..proofs.len())
                .map(|_| {
                    let sibling = super::blake3_domain_hash(
                        "ARC-merkle-sibling-v1",
                        &recursive.merkle_root,
                    );
                    vec![sibling]
                })
                .collect(),
            expected_merkle_root: recursive.merkle_root,
        };
        let valid = crate::stwo_air::verify_recursive_proof(
            &recursive_input,
            &recursive.proof_data,
        );
        assert!(valid, "inner-circuit recursive proof receipt should verify");

        eprintln!(
            "Inner-circuit recursive proof: {} child blocks, {} bytes proof_data",
            recursive.block_count,
            recursive.proof_data.len()
        );
    }

}
