//! GGUF → Integer Engine bridge.
//!
//! Loads GGUF models via candle, runs forward pass with i64 integer arithmetic.
//! Weights stay quantized in memory. Dequantization to i64 happens per-layer
//! during the forward pass, so memory usage equals GGUF size + activations.
//!
//! Works for ANY model size — 1B, 8B, 70B — as long as the GGUF fits in RAM.

#[cfg(feature = "candle")]
use candle_core::{Device, Tensor};
#[cfg(feature = "candle")]
use candle_core::quantized::gguf_file;

use crate::integer_lut::*;
use crate::integer_engine::*;
use crate::InferenceError;

/// Convert an f32 tensor to i64 Q16 fixed-point.
/// Deterministic: IEEE-754 round-to-nearest for values in typical weight range.
#[cfg(feature = "candle")]
fn tensor_to_i64(tensor: &Tensor) -> Result<Vec<i64>, InferenceError> {
    let flat = tensor.flatten_all()
        .map_err(|e| InferenceError::Runtime(format!("Flatten: {e}")))?
        .to_vec1::<f32>()
        .map_err(|e| InferenceError::Runtime(format!("ToVec: {e}")))?;
    Ok(flat.iter().map(|&x| (x * ONE as f32).round() as i64).collect())
}

/// Extract a quantized tensor from GGUF, dequantize to f32, convert to i64 Q16.
#[cfg(feature = "candle")]
fn extract_tensor(
    content: &gguf_file::Content,
    reader: &mut std::fs::File,
    device: &Device,
    name: &str,
) -> Result<Vec<i64>, InferenceError> {
    let qtensor = content.tensor(reader, name, device)
        .map_err(|e| InferenceError::Runtime(format!("Tensor '{name}': {e}")))?;
    let dequant = qtensor.dequantize(device)
        .map_err(|e| InferenceError::Runtime(format!("Dequant '{name}': {e}")))?;
    tensor_to_i64(&dequant)
}

/// Try to extract a tensor, return zeros if not found (some models don't have all tensors).
#[cfg(feature = "candle")]
fn extract_tensor_or_ones(
    content: &gguf_file::Content,
    reader: &mut std::fs::File,
    device: &Device,
    name: &str,
    size: usize,
) -> Vec<i64> {
    extract_tensor(content, reader, device, name).unwrap_or_else(|_| vec![ONE; size])
}

/// Get a u32 metadata value from GGUF.
#[cfg(feature = "candle")]
fn get_meta_u32(content: &gguf_file::Content, key: &str) -> Result<u32, InferenceError> {
    match content.metadata.get(key) {
        Some(gguf_file::Value::U32(v)) => Ok(*v),
        Some(gguf_file::Value::U64(v)) => Ok(*v as u32),
        Some(gguf_file::Value::I32(v)) => Ok(*v as u32),
        _ => Err(InferenceError::Runtime(format!("Missing GGUF metadata: {key}"))),
    }
}

/// Load a GGUF model and run integer-only inference.
///
/// The model stays quantized in memory. Each layer is dequantized to i64
/// on-the-fly during the forward pass, then dropped. This means:
/// - Memory = GGUF size + one layer of i64 activations
/// - Works for any model size (1B, 8B, 70B)
/// - Forward pass is pure i64 — deterministic on all platforms
#[cfg(feature = "candle")]
pub fn generate_integer_from_gguf(
    path: &str,
    input_tokens: &[u32],
    max_tokens: u32,
    eos_tokens: &[u32],
    timeout_ms: u64,
) -> Result<(Vec<u32>, arc_crypto::Hash256), InferenceError> {
    use std::time::Instant;
    let start = Instant::now();
    let device = Device::Cpu;

    // Parse GGUF
    let mut reader = std::fs::File::open(path)
        .map_err(|e| InferenceError::Runtime(format!("Open: {e}")))?;
    let content = gguf_file::Content::read(&mut reader)
        .map_err(|e| InferenceError::Runtime(format!("GGUF: {e}")))?;

    // Read model config from metadata
    let n_layers = get_meta_u32(&content, "llama.block_count")? as usize;
    let d_model = get_meta_u32(&content, "llama.embedding_length")? as usize;
    let n_heads = get_meta_u32(&content, "llama.attention.head_count")? as usize;
    let n_kv_heads = get_meta_u32(&content, "llama.attention.head_count_kv").unwrap_or(n_heads as u32) as usize;
    let d_ff = get_meta_u32(&content, "llama.feed_forward_length")? as usize;
    let vocab_size = content.tensor_infos.get("token_embd.weight")
        .map(|t| t.shape.dims()[0] as usize)
        .unwrap_or(32000);
    let d_head = d_model / n_heads;
    let d_kv = d_head * n_kv_heads;

    tracing::info!(
        n_layers, d_model, n_heads, n_kv_heads, d_ff, vocab_size, d_head,
        "GGUF integer engine: model config loaded"
    );

    // Load token embeddings (kept in memory for all tokens)
    let embedding = extract_tensor(&content, &mut reader, &device, "token_embd.weight")?;
    tracing::info!("Embeddings loaded: {} values", embedding.len());

    // Load output head + final norm
    let output_weight = extract_tensor(&content, &mut reader, &device, "output.weight")
        .or_else(|_| {
            // Some models tie embeddings to output
            Ok(embedding.clone())
        })?;
    let final_norm = extract_tensor_or_ones(&content, &mut reader, &device, "output_norm.weight", d_model);
    let final_norm_beta = vec![0i64; d_model];

    // Attention scale: round(ONE / sqrt(d_head))
    let attn_scale = {
        let isqrt = integer_isqrt((d_head as i64) * ONE);
        (ONE * ONE) / isqrt.max(1)
    };

    // Autoregressive generation
    let mut all_tokens = input_tokens.to_vec();
    let mut generated = Vec::new();

    for token_step in 0..max_tokens {
        let seq_len = all_tokens.len();

        // Embed all tokens
        let mut hidden: Vec<i64> = Vec::with_capacity(seq_len * d_model);
        for &tok in &all_tokens {
            let idx = (tok as usize).min(vocab_size - 1);
            hidden.extend_from_slice(&embedding[idx * d_model..(idx + 1) * d_model]);
        }

        // Process each transformer layer (streaming — load, run, drop)
        for layer in 0..n_layers {
            let prefix = format!("blk.{layer}");

            // Load layer weights as i64 (dequantized from Q4)
            let wq = extract_tensor(&content, &mut reader, &device, &format!("{prefix}.attn_q.weight"))?;
            let wk = extract_tensor(&content, &mut reader, &device, &format!("{prefix}.attn_k.weight"))?;
            let wv = extract_tensor(&content, &mut reader, &device, &format!("{prefix}.attn_v.weight"))?;
            let wo = extract_tensor(&content, &mut reader, &device, &format!("{prefix}.attn_output.weight"))?;
            let w_gate = extract_tensor(&content, &mut reader, &device, &format!("{prefix}.ffn_gate.weight"))?;
            let w_up = extract_tensor(&content, &mut reader, &device, &format!("{prefix}.ffn_up.weight"))?;
            let w_down = extract_tensor(&content, &mut reader, &device, &format!("{prefix}.ffn_down.weight"))?;
            let attn_norm = extract_tensor_or_ones(&content, &mut reader, &device, &format!("{prefix}.attn_norm.weight"), d_model);
            let ffn_norm = extract_tensor_or_ones(&content, &mut reader, &device, &format!("{prefix}.ffn_norm.weight"), d_model);
            let zero_beta = vec![0i64; d_model];

            // === Attention block ===
            // Pre-norm
            let mut normed = Vec::with_capacity(seq_len * d_model);
            for pos in 0..seq_len {
                normed.extend(layernorm_i64(
                    &hidden[pos * d_model..(pos + 1) * d_model],
                    &attn_norm, &zero_beta,
                ));
            }

            // Q, K, V projections
            let mut all_q = Vec::with_capacity(seq_len * d_model);
            let mut all_k = Vec::with_capacity(seq_len * d_kv);
            let mut all_v = Vec::with_capacity(seq_len * d_kv);
            for pos in 0..seq_len {
                let x = &normed[pos * d_model..(pos + 1) * d_model];
                all_q.extend(matmul_i64(&wq, &[], x, d_model, d_model));
                all_k.extend(matmul_i64(&wk, &[], x, d_model, d_kv));
                all_v.extend(matmul_i64(&wv, &[], x, d_model, d_kv));
            }

            // Multi-head attention (with GQA support)
            let mut attn_out = vec![0i64; seq_len * d_model];
            for h in 0..n_heads {
                let kv_h = h * n_kv_heads / n_heads; // GQA: map head to kv head
                let mut hq = vec![0i64; seq_len * d_head];
                let mut hk = vec![0i64; seq_len * d_head];
                let mut hv = vec![0i64; seq_len * d_head];

                for pos in 0..seq_len {
                    for d in 0..d_head {
                        hq[pos * d_head + d] = all_q[pos * d_model + h * d_head + d];
                        hk[pos * d_head + d] = all_k[pos * d_kv + kv_h * d_head + d];
                        hv[pos * d_head + d] = all_v[pos * d_kv + kv_h * d_head + d];
                    }
                }

                let head_out = attention_i64(&hq, &hk, &hv, seq_len, d_head, attn_scale, true);

                for pos in 0..seq_len {
                    for d in 0..d_head {
                        attn_out[pos * d_model + h * d_head + d] = head_out[pos * d_head + d];
                    }
                }
            }

            // Output projection + residual
            let mut after_attn = Vec::with_capacity(seq_len * d_model);
            for pos in 0..seq_len {
                let projected = matmul_i64(&wo, &[], &attn_out[pos * d_model..(pos + 1) * d_model], d_model, d_model);
                for d in 0..d_model {
                    after_attn.push(hidden[pos * d_model + d] + projected[d]);
                }
            }

            // === FFN block ===
            let mut output = Vec::with_capacity(seq_len * d_model);
            for pos in 0..seq_len {
                let x = &after_attn[pos * d_model..(pos + 1) * d_model];
                let normed_ff = layernorm_i64(x, &ffn_norm, &zero_beta);

                // SwiGLU: gate = silu(x @ w_gate), up = x @ w_up, out = (gate * up) @ w_down
                let gate = matmul_i64(&w_gate, &[], &normed_ff, d_model, d_ff);
                let up = matmul_i64(&w_up, &[], &normed_ff, d_model, d_ff);

                // SiLU(x) = x * sigmoid(x) ≈ x * (x > 0 ? 1 : exp(x)) — approximate
                // For integer: use ReLU as approximation (loses SiLU curve but is deterministic)
                let mut gated = Vec::with_capacity(d_ff);
                for i in 0..d_ff {
                    let silu_approx = if gate[i] > 0 { gate[i] } else { gate[i] / 4 }; // leaky approximation
                    gated.push((silu_approx * up[i]) >> FRAC_BITS);
                }

                let ff_out = matmul_i64(&w_down, &[], &gated, d_ff, d_model);

                for d in 0..d_model {
                    output.push(x[d] + ff_out[d]);
                }
            }

            hidden = output;
            // Layer weights dropped here — memory freed

            if layer % 4 == 0 {
                tracing::debug!("Layer {}/{} complete (step {})", layer + 1, n_layers, token_step);
            }
        }

        // Final norm (last position only)
        let last_pos = &hidden[(seq_len - 1) * d_model..seq_len * d_model];
        let normed = layernorm_i64(last_pos, &final_norm, &final_norm_beta);

        // LM head → logits
        let logits = matmul_i64(&output_weight, &[], &normed, d_model, vocab_size);

        // Argmax (deterministic: lowest index wins on tie)
        let next_token = argmax_i64(&logits) as u32;
        generated.push(next_token);
        all_tokens.push(next_token);

        tracing::info!("Token {}: {} (step {}ms)", token_step, next_token, start.elapsed().as_millis());

        if eos_tokens.contains(&next_token) { break; }
        if start.elapsed().as_millis() as u64 > timeout_ms { break; }
    }

    // Compute output hash
    let output_bytes: Vec<u8> = generated.iter()
        .flat_map(|t| t.to_le_bytes())
        .collect();
    let hash = arc_crypto::hash_bytes(&output_bytes);

    Ok((generated, hash))
}

#[cfg(not(feature = "candle"))]
pub fn generate_integer_from_gguf(
    _path: &str,
    _input_tokens: &[u32],
    _max_tokens: u32,
    _eos_tokens: &[u32],
    _timeout_ms: u64,
) -> Result<(Vec<u32>, arc_crypto::Hash256), InferenceError> {
    Err(InferenceError::Runtime("candle feature not enabled".into()))
}
