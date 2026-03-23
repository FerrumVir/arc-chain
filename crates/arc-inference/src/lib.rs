//! On-chain inference runtime with VRF committee selection and EIP-1559 gas lane.
//!
//! This crate provides:
//! - `Int4Runtime`: Deterministic INT4 inference execution
//! - `committee`: VRF-based committee selection for tiered inference
//! - `gas`: EIP-1559-style inference gas lane with separate base fee

pub mod committee;
pub mod gas;
pub mod candle_backend;
pub mod integer_lut;
pub mod integer_engine;

use arc_crypto::Hash256;
use serde::{Deserialize, Serialize};
use thiserror::Error;

// ─── Errors ──────────────────────────────────────────────────────────────────

#[derive(Error, Debug)]
pub enum InferenceError {
    #[error("model not found: {0}")]
    ModelNotFound(String),

    #[error("model too large for tier {tier}: {size} params, max {max}")]
    ModelTooLarge { tier: u8, size: u64, max: u64 },

    #[error("execution timeout: {elapsed_ms}ms > {limit_ms}ms")]
    Timeout { elapsed_ms: u64, limit_ms: u64 },

    #[error("determinism violation: output mismatch")]
    DeterminismViolation,

    #[error("insufficient stake for tier {tier}: have {have}, need {need}")]
    InsufficientStake { tier: u8, have: u64, need: u64 },

    #[error("inference error: {0}")]
    Runtime(String),
}

// ─── Types ───────────────────────────────────────────────────────────────────

/// Hardware tiers for inference capability.
///
/// Validators register for a tier based on their hardware. The chain uses
/// this to determine which validators can execute which models.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum InferenceTier {
    /// ≤20B INT4 parameters. Min 16GB. Every validator runs it.
    Tier1 = 1,
    /// 20-50B INT4 parameters. Min 64GB. VRF committee of 7.
    Tier2 = 2,
    /// 50-100B INT4 parameters. Min 128GB+. VRF committee of 7.
    Tier3 = 3,
    /// 100B+ parameters. H100/Mac Studio 192GB. VRF committee of 7.
    Tier4 = 4,
}

impl InferenceTier {
    /// Minimum stake required for this tier (in ARC base units).
    pub fn min_stake(&self) -> u64 {
        match self {
            Self::Tier1 => 1_000,
            Self::Tier2 => 5_000,
            Self::Tier3 => 10_000,
            Self::Tier4 => 25_000,
        }
    }

    /// Maximum model parameters (in billions) for this tier.
    pub fn max_params_b(&self) -> u64 {
        match self {
            Self::Tier1 => 20,
            Self::Tier2 => 50,
            Self::Tier3 => 100,
            Self::Tier4 => u64::MAX, // unlimited
        }
    }

    /// Whether this tier requires VRF committee selection.
    pub fn requires_committee(&self) -> bool {
        matches!(self, Self::Tier2 | Self::Tier3 | Self::Tier4)
    }

    /// From u8 value.
    pub fn from_u8(v: u8) -> Option<Self> {
        match v {
            1 => Some(Self::Tier1),
            2 => Some(Self::Tier2),
            3 => Some(Self::Tier3),
            4 => Some(Self::Tier4),
            _ => None,
        }
    }
}

/// An inference request to be executed on-chain.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InferenceRequest {
    /// BLAKE3 hash of model weights (content-addressed).
    pub model_id: Hash256,
    /// Input data (prompt bytes, token IDs, etc.).
    pub input: Vec<u8>,
    /// Which tier this model belongs to.
    pub tier: InferenceTier,
    /// Maximum tokens to generate.
    pub max_tokens: u32,
}

/// Result of an inference execution.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InferenceResult {
    /// BLAKE3 hash of the output.
    pub output_hash: Hash256,
    /// Raw output bytes.
    pub output: Vec<u8>,
    /// Number of tokens consumed/generated.
    pub tokens_used: u32,
    /// Execution time in milliseconds.
    pub elapsed_ms: u64,
    /// Whether this was deterministic (INT4 path).
    pub deterministic: bool,
}

// ─── INT4 Runtime ────────────────────────────────────────────────────────────

/// ARC-INT4 model format header.
///
/// Integer-rational scale factors for determinism:
/// `real_value = int4_value * numerator / denominator`
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelHeader {
    /// Model identifier (BLAKE3 of full weights).
    pub model_id: Hash256,
    /// Number of parameters.
    pub num_params: u64,
    /// Number of layers.
    pub num_layers: u32,
    /// Hidden dimension.
    pub hidden_dim: u32,
    /// Vocabulary size.
    pub vocab_size: u32,
    /// Scale factor numerator (integer rational for determinism).
    pub scale_numerator: i64,
    /// Scale factor denominator (integer rational for determinism).
    pub scale_denominator: i64,
}

/// Deterministic INT4 inference runtime.
///
/// All arithmetic is integer-only for cross-platform determinism.
/// INT4 values are stored as nibbles, unpacked to i8 for computation.
/// Scale factors use integer rationals (numerator/denominator) instead
/// of floating point.
pub struct Int4Runtime {
    /// Maximum execution time per inference call.
    pub timeout_ms: u64,
    /// Loaded model headers (model_id → header).
    models: dashmap::DashMap<[u8; 32], ModelHeader>,
}

impl Int4Runtime {
    pub fn new(timeout_ms: u64) -> Self {
        Self {
            timeout_ms,
            models: dashmap::DashMap::new(),
        }
    }

    /// Register a model header (weights are loaded separately via content-addressed storage).
    pub fn register_model(&self, header: ModelHeader) {
        self.models.insert(header.model_id.0, header);
    }

    /// Check if a model is registered.
    pub fn has_model(&self, model_id: &Hash256) -> bool {
        self.models.contains_key(&model_id.0)
    }

    /// Execute inference on a registered model.
    ///
    /// This is the deterministic INT4 execution path. All arithmetic is integer-only.
    /// The output is guaranteed to be identical across all platforms (x86, ARM, GPU).
    pub fn execute(&self, request: &InferenceRequest) -> Result<InferenceResult, InferenceError> {
        let start = std::time::Instant::now();

        let header = self
            .models
            .get(&request.model_id.0)
            .ok_or_else(|| InferenceError::ModelNotFound(hex::encode(request.model_id.0)))?;

        // Verify model fits in requested tier
        let params_b = header.num_params / 1_000_000_000;
        if params_b > request.tier.max_params_b() {
            return Err(InferenceError::ModelTooLarge {
                tier: request.tier as u8,
                size: header.num_params,
                max: request.tier.max_params_b() * 1_000_000_000,
            });
        }

        // Deterministic INT4 forward pass.
        //
        // When candle integration is complete (week 11-12), this will:
        // 1. Load GGUF quantized weights from content-addressed storage
        // 2. Unpack INT4 → i8
        // 3. Matrix multiply using i32 accumulation (exact on all hardware)
        // 4. Apply integer-rational scaling: out = acc * numerator / denominator
        // 5. Argmax for next token (deterministic tie-breaking by index)
        //
        // For now, use deterministic hash-based output generation that
        // produces consistent output given the same model + input.
        let output = deterministic_mock_inference(
            &request.model_id,
            &request.input,
            request.max_tokens,
            header.scale_numerator,
            header.scale_denominator,
        );

        let elapsed_ms = start.elapsed().as_millis() as u64;

        if elapsed_ms > self.timeout_ms {
            return Err(InferenceError::Timeout {
                elapsed_ms,
                limit_ms: self.timeout_ms,
            });
        }

        let output_hash = arc_crypto::hash_bytes(&output);

        Ok(InferenceResult {
            output_hash,
            output,
            tokens_used: request.max_tokens.min(256),
            elapsed_ms,
            deterministic: true,
        })
    }
}

/// Deterministic mock inference using BLAKE3 hashing.
///
/// Produces identical output for the same (model_id, input, max_tokens)
/// across all platforms. This is a placeholder for the real candle INT4 backend.
fn deterministic_mock_inference(
    model_id: &Hash256,
    input: &[u8],
    max_tokens: u32,
    scale_num: i64,
    scale_denom: i64,
) -> Vec<u8> {
    let mut output = Vec::new();
    let tokens = max_tokens.min(256);

    // Generate deterministic token IDs via iterated BLAKE3
    let mut state = Vec::with_capacity(32 + input.len() + 8);
    state.extend_from_slice(&model_id.0);
    state.extend_from_slice(input);
    state.extend_from_slice(&scale_num.to_le_bytes());
    state.extend_from_slice(&scale_denom.to_le_bytes());

    for i in 0..tokens {
        state.extend_from_slice(&i.to_le_bytes());
        let hash = blake3::hash(&state);
        let token_bytes = &hash.as_bytes()[..4];
        output.extend_from_slice(token_bytes);
        // Feed hash back into state for next token
        state.truncate(32 + input.len() + 16);
        state.extend_from_slice(hash.as_bytes());
    }

    output
}

#[cfg(test)]
mod tests {
    use super::*;
    use arc_crypto::hash_bytes;

    #[test]
    fn test_tier_properties() {
        assert!(!InferenceTier::Tier1.requires_committee());
        assert!(InferenceTier::Tier2.requires_committee());
        assert!(InferenceTier::Tier3.requires_committee());
        assert!(InferenceTier::Tier4.requires_committee());

        assert_eq!(InferenceTier::Tier1.min_stake(), 1_000);
        assert_eq!(InferenceTier::Tier4.min_stake(), 25_000);
    }

    #[test]
    fn test_tier_from_u8() {
        assert_eq!(InferenceTier::from_u8(1), Some(InferenceTier::Tier1));
        assert_eq!(InferenceTier::from_u8(4), Some(InferenceTier::Tier4));
        assert_eq!(InferenceTier::from_u8(0), None);
        assert_eq!(InferenceTier::from_u8(5), None);
    }

    #[test]
    fn test_int4_runtime_register_and_execute() {
        let runtime = Int4Runtime::new(5000);
        let model_id = hash_bytes(b"test-model-7b");

        let header = ModelHeader {
            model_id,
            num_params: 7_000_000_000,
            num_layers: 32,
            hidden_dim: 4096,
            vocab_size: 32000,
            scale_numerator: 1,
            scale_denominator: 127,
        };
        runtime.register_model(header);
        assert!(runtime.has_model(&model_id));

        let request = InferenceRequest {
            model_id,
            input: b"What is the capital of France?".to_vec(),
            tier: InferenceTier::Tier1,
            max_tokens: 10,
        };

        let result = runtime.execute(&request).unwrap();
        assert!(result.deterministic);
        assert!(!result.output.is_empty());
    }

    #[test]
    fn test_deterministic_output() {
        let runtime = Int4Runtime::new(5000);
        let model_id = hash_bytes(b"determinism-test");

        runtime.register_model(ModelHeader {
            model_id,
            num_params: 1_000_000_000,
            num_layers: 12,
            hidden_dim: 768,
            vocab_size: 50000,
            scale_numerator: 1,
            scale_denominator: 15,
        });

        let request = InferenceRequest {
            model_id,
            input: b"hello world".to_vec(),
            tier: InferenceTier::Tier1,
            max_tokens: 50,
        };

        // Run twice — must produce identical output
        let r1 = runtime.execute(&request).unwrap();
        let r2 = runtime.execute(&request).unwrap();
        assert_eq!(r1.output, r2.output);
        assert_eq!(r1.output_hash, r2.output_hash);
    }

    #[test]
    fn test_model_too_large_for_tier() {
        let runtime = Int4Runtime::new(5000);
        let model_id = hash_bytes(b"big-model");

        runtime.register_model(ModelHeader {
            model_id,
            num_params: 50_000_000_000, // 50B
            num_layers: 64,
            hidden_dim: 8192,
            vocab_size: 100000,
            scale_numerator: 1,
            scale_denominator: 15,
        });

        let request = InferenceRequest {
            model_id,
            input: b"test".to_vec(),
            tier: InferenceTier::Tier1, // Tier1 max is 20B
            max_tokens: 1,
        };

        assert!(matches!(
            runtime.execute(&request),
            Err(InferenceError::ModelTooLarge { .. })
        ));
    }

    #[test]
    fn test_model_not_found() {
        let runtime = Int4Runtime::new(5000);
        let request = InferenceRequest {
            model_id: hash_bytes(b"nonexistent"),
            input: b"test".to_vec(),
            tier: InferenceTier::Tier1,
            max_tokens: 1,
        };

        assert!(matches!(
            runtime.execute(&request),
            Err(InferenceError::ModelNotFound(_))
        ));
    }
}
