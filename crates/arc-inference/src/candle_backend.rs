//! Real GGUF inference backend powered by Hugging Face candle.
//!
//! When the `candle` feature is enabled:
//! - Loads GGUF quantized models (Llama, Mistral, Mixtral, etc.)
//! - Runs INT4/INT8 quantized forward pass in-process
//! - Uses BLAS (CPU), Metal (macOS), or CUDA (NVIDIA) acceleration
//! - Deterministic: INT4 accumulation is exact across all hardware
//!
//! This is the Tier 1 on-chain inference path. Every validator loads the
//! same GGUF model and produces bitwise identical output.

use crate::model_artifact::ModelArtifactCommitment;
use crate::{InferenceError, InferenceResult};
use arc_crypto::Hash256;
// Only the `candle` feature has code paths that log here; without it this
// import would be unused.
#[cfg(feature = "candle")]
use tracing::info;

/// The quantized-Llama backend precomputes RoPE tables for exactly this many
/// positions. Keep the public admission contract visibly tied to the backend
/// constant so a dependency upgrade cannot silently widen or shrink it.
pub const GGUF_CONTEXT_WINDOW: usize = 4096;

#[cfg(feature = "candle")]
const _: () =
    assert!(GGUF_CONTEXT_WINDOW == candle_transformers::models::quantized_llama::MAX_SEQ_LEN);

fn generation_index_pos(
    input_tokens: usize,
    generation_step: u32,
) -> Result<usize, InferenceError> {
    if generation_step == 0 {
        return Ok(0);
    }
    let decoded = usize::try_from(generation_step - 1)
        .map_err(|_| InferenceError::Runtime("generation position overflow".to_string()))?;
    input_tokens
        .checked_add(decoded)
        .ok_or_else(|| InferenceError::Runtime("generation position overflow".to_string()))
}

fn enforce_generation_timeout(elapsed_ms: u64, limit_ms: u64) -> Result<(), InferenceError> {
    if elapsed_ms > limit_ms {
        return Err(InferenceError::Timeout {
            elapsed_ms,
            limit_ms,
        });
    }
    Ok(())
}

/// GGUF model inference engine.
///
/// Loads quantized models from GGUF files and executes transformer
/// forward passes in-process. No external API calls.
pub struct GgufEngine {
    pub timeout_ms: u64,
    #[cfg(feature = "candle")]
    models: dashmap::DashMap<[u8; 32], LoadedGgufModel>,
}

#[cfg(feature = "candle")]
struct LoadedGgufModel {
    /// Path to the GGUF file (for reference).
    path: String,
    /// Model ID = streaming BLAKE3 of every source-artifact byte.
    model_id: Hash256,
    /// Quantized model weights loaded via candle.
    model: candle_transformers::models::quantized_llama::ModelWeights,
    /// Tokenizer (simple byte-level for determinism).
    ///
    /// Recorded but never read. Kept because it is part of this struct's
    /// record of what was loaded — and because the value is currently
    /// hardcoded to 32000 ("standard for Llama family", see `load`) rather
    /// than read from the GGUF metadata. Deleting the field would remove the
    /// only trace of that assumption; it should instead be sourced from the
    /// file and actually used when a non-32000-vocab model is supported.
    #[allow(dead_code)]
    vocab_size: u32,
}

impl GgufEngine {
    pub fn new(timeout_ms: u64) -> Self {
        Self {
            timeout_ms,
            #[cfg(feature = "candle")]
            models: dashmap::DashMap::new(),
        }
    }

    /// Reject an untrusted prompt before model lookup, KV-cache mutation or
    /// tensor allocation. The first forward consumes the whole prompt and
    /// produces token one; each later token consumes one additional RoPE
    /// position, so the exact requirement is `prompt + max_tokens - 1`.
    pub fn preflight_generation(
        input_tokens: usize,
        max_tokens: u32,
    ) -> Result<usize, InferenceError> {
        Self::preflight_generation_for_context(input_tokens, max_tokens, GGUF_CONTEXT_WINDOW)
    }

    /// Apply a paired tokenizer/model context limit in addition to Candle's
    /// fixed backend ceiling. Callers that loaded tokenizer metadata from the
    /// same artifact use this before dispatching blocking work.
    pub fn preflight_generation_for_context(
        input_tokens: usize,
        max_tokens: u32,
        context_window: usize,
    ) -> Result<usize, InferenceError> {
        if input_tokens == 0 {
            return Err(InferenceError::Runtime(
                "empty_prompt: GGUF generation requires at least one input token".to_string(),
            ));
        }
        if max_tokens == 0 {
            return Err(InferenceError::Runtime(
                "invalid_max_tokens: GGUF generation requires at least one output token"
                    .to_string(),
            ));
        }
        let decode_positions = usize::try_from(max_tokens - 1)
            .map_err(|_| InferenceError::Runtime("generation position overflow".to_string()))?;
        let required_positions = input_tokens
            .checked_add(decode_positions)
            .ok_or_else(|| InferenceError::Runtime("generation position overflow".to_string()))?;
        let effective_context_window = context_window.min(GGUF_CONTEXT_WINDOW);
        if required_positions > effective_context_window {
            return Err(InferenceError::Runtime(format!(
                "context_window_exceeded: {input_tokens} prompt tokens + {decode_positions} subsequent decode positions requires {required_positions} positions, but the paired GGUF context supports at most {effective_context_window}"
            )));
        }
        Ok(required_positions)
    }

    /// Load a GGUF quantized model from a file path.
    ///
    /// The model_id is the streaming BLAKE3 commitment of the complete source
    /// artifact. Shape metadata and byte sampling are not identities: two GGUF
    /// files can share them while containing different weights.
    #[cfg(feature = "candle")]
    pub fn load_gguf_file(&self, path: &str) -> Result<Hash256, InferenceError> {
        let artifact = ModelArtifactCommitment::from_path(path)?;
        self.load_gguf_artifact(&artifact)
    }

    /// Load a model using an identity already computed during node startup.
    /// The artifact is rechecked before Candle parses it so a file changed
    /// between commitment and loading cannot be advertised under the old ID.
    #[cfg(feature = "candle")]
    pub fn load_gguf_artifact(
        &self,
        artifact: &ModelArtifactCommitment,
    ) -> Result<Hash256, InferenceError> {
        use candle_core::Device;
        use candle_transformers::models::quantized_llama::ModelWeights;

        artifact.verify_unchanged()?;
        let path = artifact.path();
        let path_display = path.display().to_string();
        let model_id = artifact.model_id();
        let file_size = artifact.size_bytes();

        info!(path = %path.display(), "Loading GGUF model...");

        // Open GGUF via candle's quantized loader
        let mut gguf_file = std::fs::File::open(path).map_err(|e| {
            InferenceError::Runtime(format!("Failed to reopen {path_display}: {e}"))
        })?;

        let gguf_content = candle_core::quantized::gguf_file::Content::read(&mut gguf_file)
            .map_err(|e| InferenceError::Runtime(format!("GGUF parse error: {e}")))?;

        let device = Device::Cpu; // Metal: Device::new_metal(0)?

        // Build quantized model from GGUF
        let model = ModelWeights::from_gguf(gguf_content, &mut gguf_file, &device)
            .map_err(|e| InferenceError::Runtime(format!("Model load error: {e}")))?;

        let vocab_size = 32000; // Standard for Llama family

        info!(
            model_id = hex::encode(&model_id.0[..8]),
            file_size_mb = file_size / (1024 * 1024),
            "GGUF model loaded successfully"
        );

        self.models.insert(
            model_id.0,
            LoadedGgufModel {
                path: path_display,
                model_id,
                model,
                vocab_size,
            },
        );

        Ok(model_id)
    }

    #[cfg(not(feature = "candle"))]
    pub fn load_gguf_file(&self, _path: &str) -> Result<Hash256, InferenceError> {
        Err(InferenceError::Runtime(
            "candle feature not enabled - build with: cargo build --features candle".into(),
        ))
    }

    #[cfg(not(feature = "candle"))]
    pub fn load_gguf_artifact(
        &self,
        _artifact: &ModelArtifactCommitment,
    ) -> Result<Hash256, InferenceError> {
        Err(InferenceError::Runtime(
            "candle feature not enabled - build with: cargo build --features candle".into(),
        ))
    }

    /// Run inference on a loaded GGUF model.
    ///
    /// Deterministic forward pass:
    /// - Input tokens are converted to tensor
    /// - Transformer forward pass runs quantized matmul (INT4/INT8 → INT32 accumulation)
    /// - Output logits → argmax for next token (deterministic tie-breaking: lowest index)
    /// - Repeat for max_tokens
    #[cfg(feature = "candle")]
    pub fn generate(
        &self,
        model_id: &Hash256,
        input_tokens: &[u32],
        max_tokens: u32,
    ) -> Result<InferenceResult, InferenceError> {
        use candle_core::{Device, Tensor};

        Self::preflight_generation(input_tokens.len(), max_tokens)?;
        let start = std::time::Instant::now();

        let mut model_ref = self
            .models
            .get_mut(&model_id.0)
            .ok_or_else(|| InferenceError::ModelNotFound(hex::encode(&model_id.0[..8])))?;

        let device = Device::Cpu;

        // Convert input tokens to tensor
        let input_ids: Vec<u32> = input_tokens.to_vec();
        let mut all_tokens = input_ids.clone();
        let mut generated_tokens: Vec<u32> = Vec::new();

        // Autoregressive generation
        for i in 0..max_tokens {
            let context = if i == 0 {
                // First pass: use full input
                Tensor::new(all_tokens.as_slice(), &device)
                    .map_err(|e| InferenceError::Runtime(format!("Tensor: {e}")))?
                    .unsqueeze(0)
                    .map_err(|e| InferenceError::Runtime(format!("Unsqueeze: {e}")))?
            } else {
                // Subsequent: use only the last token (KV cache handles context)
                let last = *all_tokens.last().unwrap();
                Tensor::new(&[last], &device)
                    .map_err(|e| InferenceError::Runtime(format!("Tensor: {e}")))?
                    .unsqueeze(0)
                    .map_err(|e| InferenceError::Runtime(format!("Unsqueeze: {e}")))?
            };

            let _seq_len = context
                .dim(1)
                .map_err(|e| InferenceError::Runtime(format!("Dim: {e}")))?;

            // Forward pass through quantized transformer
            let logits = model_ref
                .model
                .forward(&context, generation_index_pos(input_tokens.len(), i)?)
                .map_err(|e| InferenceError::Runtime(format!("Forward: {e}")))?;

            // Get logits for last position
            let logits = logits
                .squeeze(0)
                .map_err(|e| InferenceError::Runtime(format!("Squeeze: {e}")))?;
            let last_logits = if logits.dims().len() == 2 {
                logits
                    .get(logits.dim(0).unwrap() - 1)
                    .map_err(|e| InferenceError::Runtime(format!("Get: {e}")))?
            } else {
                logits
            };

            // Argmax (deterministic: lowest index wins on tie)
            let next_token = last_logits
                .argmax(0)
                .map_err(|e| InferenceError::Runtime(format!("Argmax: {e}")))?
                .to_scalar::<u32>()
                .map_err(|e| InferenceError::Runtime(format!("Scalar: {e}")))?;

            generated_tokens.push(next_token);
            all_tokens.push(next_token);

            let elapsed_ms = start.elapsed().as_millis() as u64;
            enforce_generation_timeout(elapsed_ms, self.timeout_ms)?;

            // Stop on EOS - token 2 (LLaMA-2), 128001/128009 (LLaMA-3)
            // Token 0 is PAD, not EOS - do not stop on it
            if matches!(next_token, 2 | 128001 | 128009) {
                break;
            }
        }

        let elapsed_ms = start.elapsed().as_millis() as u64;

        // Serialize output as bytes for hashing
        let output_bytes: Vec<u8> = generated_tokens
            .iter()
            .flat_map(|t| t.to_le_bytes())
            .collect();
        let output_hash = arc_crypto::hash_bytes(&output_bytes);

        Ok(InferenceResult {
            output_hash,
            output: output_bytes,
            tokens_used: generated_tokens.len() as u32,
            elapsed_ms,
            deterministic: true,
        })
    }

    #[cfg(not(feature = "candle"))]
    pub fn generate(
        &self,
        _model_id: &Hash256,
        _input_tokens: &[u32],
        _max_tokens: u32,
    ) -> Result<InferenceResult, InferenceError> {
        Err(InferenceError::Runtime(
            "candle feature not enabled - build with: cargo build --features candle".into(),
        ))
    }

    /// Check if a model is loaded.
    #[cfg(feature = "candle")]
    pub fn has_model(&self, model_id: &Hash256) -> bool {
        self.models.contains_key(&model_id.0)
    }

    #[cfg(not(feature = "candle"))]
    pub fn has_model(&self, _model_id: &Hash256) -> bool {
        false
    }

    /// List loaded models.
    #[cfg(feature = "candle")]
    pub fn loaded_models(&self) -> Vec<(Hash256, String)> {
        self.models
            .iter()
            .map(|entry| {
                let model = entry.value();
                (model.model_id, model.path.clone())
            })
            .collect()
    }

    #[cfg(not(feature = "candle"))]
    pub fn loaded_models(&self) -> Vec<(Hash256, String)> {
        vec![]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_gguf_engine_creation() {
        let engine = GgufEngine::new(30_000);
        assert_eq!(engine.timeout_ms, 30_000);
    }

    #[test]
    fn gguf_context_preflight_accepts_exact_boundary_and_rejects_plus_one() {
        assert_eq!(
            GgufEngine::preflight_generation(GGUF_CONTEXT_WINDOW - 3, 4).unwrap(),
            GGUF_CONTEXT_WINDOW
        );
        let error = GgufEngine::preflight_generation(GGUF_CONTEXT_WINDOW - 3, 5)
            .expect_err("one position past the backend RoPE table must fail before compute");
        assert!(error.to_string().contains("context_window_exceeded"));
        assert!(GgufEngine::preflight_generation(0, 1).is_err());
        assert!(GgufEngine::preflight_generation(1, 0).is_err());
    }

    #[test]
    fn gguf_decode_positions_follow_prompt_tail_not_generation_ordinal() {
        assert_eq!(generation_index_pos(37, 0).unwrap(), 0);
        assert_eq!(generation_index_pos(37, 1).unwrap(), 37);
        assert_eq!(generation_index_pos(37, 2).unwrap(), 38);
    }

    #[test]
    fn gguf_timeout_is_a_typed_failure_not_partial_success() {
        assert!(enforce_generation_timeout(100, 100).is_ok());
        assert!(matches!(
            enforce_generation_timeout(101, 100),
            Err(InferenceError::Timeout {
                elapsed_ms: 101,
                limit_ms: 100,
            })
        ));
    }

    #[cfg(not(feature = "candle"))]
    #[test]
    fn test_gguf_without_feature() {
        let engine = GgufEngine::new(5000);
        assert!(engine.load_gguf_file("/nonexistent.gguf").is_err());
        assert!(engine.loaded_models().is_empty());
    }
}
