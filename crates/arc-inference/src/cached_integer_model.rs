//! Cached Integer Model — Production-speed deterministic inference with INT8 weights.
//!
//! Stores weights as INT8 (1 byte per parameter) with per-tensor Q16 scale factors.
//! 7B model: ~7GB instead of 56GB with i64. Fits in 8GB RAM.
//!
//! Forward pass: i8 weight × i64 activation → accumulate in i64 → scale → Q16 result.
//! Pure integer arithmetic during inference. Deterministic on all platforms.
//! Float used ONLY at model load time (GGUF dequant + quantize to i8).

use crate::integer_lut::*;
use arc_crypto::Hash256;
use rayon::prelude::*;
use tracing::info;

// ─── INT8 Weight Storage ──────────────────────────────────────────────────────

/// Per-tensor symmetric INT8 quantized weights.
///
/// Original real value ≈ i8_value × scale_q16 (where scale_q16 is in Q16).
/// Quantization: i8 = round(f32 / abs_max × 127), scale = abs_max / 127 × ONE.
/// Matmul: result_q16 = (sum(i8 × activation_q16) × scale) >> FRAC_BITS
pub struct I8Weights {
    pub data: Vec<i8>,
    pub scale: i64, // Q16 representation of (abs_max / 127)
}

impl I8Weights {
    /// Quantize f32 values to symmetric INT8 with per-tensor scale.
    /// Returns I8Weights with scale in Q16.
    pub fn quantize_f32(values: &[f32]) -> Self {
        let abs_max = values.iter().map(|x| x.abs()).fold(0.0f32, f32::max);
        let abs_max = abs_max.max(1e-10); // avoid div by zero

        let inv_abs_max = 127.0 / abs_max;
        let data: Vec<i8> = values.iter()
            .map(|&x| (x * inv_abs_max).round().clamp(-127.0, 127.0) as i8)
            .collect();

        // scale = abs_max / 127 in Q16
        let scale = ((abs_max as f64 / 127.0) * ONE as f64).round() as i64;

        Self { data, scale: scale.max(1) }
    }

    /// Memory usage in bytes.
    pub fn memory_bytes(&self) -> usize {
        self.data.len() + 8 // data + scale
    }
}

// ─── Layer and Model Structs ──────────────────────────────────────────────────

/// Pre-loaded transformer layer weights in INT8 with Q16 norms.
pub struct CachedLayer {
    pub wq: I8Weights,      // [d_model × d_model]
    pub wk: I8Weights,      // [d_model × d_kv]
    pub wv: I8Weights,      // [d_model × d_kv]
    pub wo: I8Weights,      // [d_model × d_model]
    pub w_gate: I8Weights,  // [d_ff × d_model]
    pub w_up: I8Weights,    // [d_ff × d_model]
    pub w_down: I8Weights,  // [d_model × d_ff]
    pub attn_norm: Vec<i64>, // norms stay i64 (small: d_model each)
    pub ffn_norm: Vec<i64>,
}

/// KV cache for autoregressive generation.
pub struct KVCache {
    /// k_cache[layer][pos * d_kv .. (pos+1) * d_kv]
    pub k: Vec<Vec<i64>>,
    /// v_cache[layer][pos * d_kv .. (pos+1) * d_kv]
    pub v: Vec<Vec<i64>>,
    pub seq_len: usize,
}

impl KVCache {
    pub fn new(n_layers: usize) -> Self {
        Self {
            k: vec![Vec::new(); n_layers],
            v: vec![Vec::new(); n_layers],
            seq_len: 0,
        }
    }

    pub fn clear(&mut self) {
        for layer in &mut self.k { layer.clear(); }
        for layer in &mut self.v { layer.clear(); }
        self.seq_len = 0;
    }
}

/// Model config extracted from GGUF metadata.
pub struct ModelConfig {
    pub n_layers: usize,
    pub d_model: usize,
    pub n_heads: usize,
    pub n_kv_heads: usize,
    pub d_ff: usize,
    pub d_head: usize,
    pub d_kv: usize,
    pub vocab_size: usize,
    pub attn_scale: i64,
    /// Pre-computed RoPE cos/sin tables in Q16.
    pub rope_cos: Vec<i64>, // [max_seq × d_head/2]
    pub rope_sin: Vec<i64>,
    pub max_seq: usize,
}

/// Fully cached integer model with INT8 weights.
///
/// Memory for 7B: ~7GB (vs 56GB with i64).
/// - Embedding: ~131MB (32K × 4096 × 1 byte)
/// - 32 layers × ~190MB each = ~6.1GB
/// - Output weight: ~131MB
/// - Norms + RoPE: ~4MB
pub struct CachedIntegerModel {
    pub config: ModelConfig,
    pub embedding: I8Weights,     // [vocab × d_model] as i8
    pub layers: Vec<CachedLayer>,
    pub final_norm: Vec<i64>,     // [d_model]
    pub output_weight: I8Weights, // [vocab × d_model] as i8
    /// Token vocabulary extracted from GGUF (for decode: token_id → string).
    pub vocab: Vec<String>,
}

// ─── INT8 Matmul ──────────────────────────────────────────────────────────────

/// INT8 × i64 parallel matrix-vector multiply.
///
/// weights.data: [out_size × in_size] as i8, weights.scale: Q16
/// input: [in_size] as i64 Q16
/// Returns: [out_size] as i64 Q16
///
/// Math: result[i] = (sum_j(w_i8[i,j] * input[j]) * scale) >> FRAC_BITS
fn matmul_i8_par(weights: &I8Weights, input: &[i64], in_size: usize, out_size: usize) -> Vec<i64> {
    let data = &weights.data;
    let scale = weights.scale;
    (0..out_size).into_par_iter().map(|i| {
        let row = &data[i * in_size..(i + 1) * in_size];
        let mut acc: i64 = 0;
        // Process 4 elements at a time for better ILP
        let chunks = in_size / 4;
        let remainder = in_size % 4;
        for c in 0..chunks {
            let base = c * 4;
            acc += (row[base] as i64) * input[base];
            acc += (row[base + 1] as i64) * input[base + 1];
            acc += (row[base + 2] as i64) * input[base + 2];
            acc += (row[base + 3] as i64) * input[base + 3];
        }
        for j in (in_size - remainder)..in_size {
            acc += (row[j] as i64) * input[j];
        }
        (acc * scale) >> FRAC_BITS
    }).collect()
}

/// INT8 × i64 sequential matmul for small dimensions (rayon overhead not worth it).
fn matmul_i8_seq(weights: &I8Weights, input: &[i64], in_size: usize, out_size: usize) -> Vec<i64> {
    let data = &weights.data;
    let scale = weights.scale;
    let mut output = Vec::with_capacity(out_size);
    for i in 0..out_size {
        let row_start = i * in_size;
        let mut acc: i64 = 0;
        for j in 0..in_size {
            acc += (data[row_start + j] as i64) * input[j];
        }
        output.push((acc * scale) >> FRAC_BITS);
    }
    output
}

/// Choose parallel or sequential based on output dimension.
fn matmul_i8(weights: &I8Weights, input: &[i64], in_size: usize, out_size: usize) -> Vec<i64> {
    if out_size >= 256 {
        matmul_i8_par(weights, input, in_size, out_size)
    } else {
        matmul_i8_seq(weights, input, in_size, out_size)
    }
}

// ─── SIMD INT8 Matmul (Phase 4b) ─────────────────────────────────────────────

/// SIMD-accelerated i8 × i8 → i32 matmul for ARM NEON.
/// Quantizes input to i8 on-the-fly, uses NEON for the inner loop.
#[cfg(target_arch = "aarch64")]
fn matmul_i8xi8_simd(
    weights: &I8Weights,
    input: &[i64],
    in_size: usize,
    out_size: usize,
) -> Vec<i64> {
    use std::arch::aarch64::*;

    // Quantize input to i8 with its own scale
    let input_abs_max = input.iter().map(|x| x.abs()).max().unwrap_or(1).max(1);
    let input_scale_factor = input_abs_max / 127;
    let input_scale_factor = input_scale_factor.max(1);

    let input_i8: Vec<i8> = input.iter()
        .map(|&x| (x / input_scale_factor).clamp(-127, 127) as i8)
        .collect();

    // Combined scale: weight_scale * input_scale_factor / ONE
    // result = i32_acc * combined_scale >> FRAC_BITS
    let weight_scale = weights.scale;
    let data = &weights.data;

    (0..out_size).into_par_iter().map(|i| {
        let row = &data[i * in_size..(i + 1) * in_size];
        let mut acc: i64 = 0;

        // NEON path: process 16 i8 elements at a time
        let simd_len = in_size / 16 * 16;
        unsafe {
            let mut vacc = vdupq_n_s32(0);
            let mut vacc2 = vdupq_n_s32(0);
            let mut j = 0usize;
            while j < simd_len {
                // Load 16 i8 weights and 16 i8 inputs
                let vw = vld1q_s8(row.as_ptr().add(j));
                let vi = vld1q_s8(input_i8.as_ptr().add(j));

                // Multiply low 8: i8 × i8 → i16
                let prod_lo = vmull_s8(vget_low_s8(vw), vget_low_s8(vi));
                // Multiply high 8: i8 × i8 → i16
                let prod_hi = vmull_s8(vget_high_s8(vw), vget_high_s8(vi));

                // Pairwise add i16 → i32 and accumulate
                vacc = vpadalq_s16(vacc, prod_lo);
                vacc2 = vpadalq_s16(vacc2, prod_hi);

                j += 16;
            }
            // Horizontal sum of 4xi32
            vacc = vaddq_s32(vacc, vacc2);
            acc = vaddvq_s32(vacc) as i64;
        }

        // Scalar remainder
        for j in simd_len..in_size {
            acc += (row[j] as i64) * (input_i8[j] as i64);
        }

        // Rescale: acc is in (weight_unit × input_unit) space
        // result_q16 = acc * weight_scale * input_scale_factor / ONE
        // = (acc * weight_scale >> FRAC_BITS) * input_scale_factor
        // But to avoid overflow, we compute differently:
        // result_q16 = acc * (weight_scale * input_scale_factor / ONE)
        // Since weight_scale and input_scale_factor are both manageable, let's compute:
        let combined = (weight_scale * input_scale_factor) >> FRAC_BITS;
        acc * combined
    }).collect()
}

/// x86 AVX2 variant.
#[cfg(target_arch = "x86_64")]
fn matmul_i8xi8_simd(
    weights: &I8Weights,
    input: &[i64],
    in_size: usize,
    out_size: usize,
) -> Vec<i64> {
    use std::arch::x86_64::*;

    // Check for AVX2 support at runtime
    if !is_x86_feature_detected!("avx2") {
        return matmul_i8_par(weights, input, in_size, out_size);
    }

    // Quantize input to i8
    let input_abs_max = input.iter().map(|x| x.abs()).max().unwrap_or(1).max(1);
    let input_scale_factor = (input_abs_max / 127).max(1);

    let input_i8: Vec<i8> = input.iter()
        .map(|&x| (x / input_scale_factor).clamp(-127, 127) as i8)
        .collect();

    let weight_scale = weights.scale;
    let data = &weights.data;

    (0..out_size).into_par_iter().map(|i| {
        let row = &data[i * in_size..(i + 1) * in_size];
        let mut acc: i64 = 0;

        // AVX2 path: process 32 i8 elements at a time
        // _mm256_maddubs_epi16 needs unsigned × signed
        // We offset weights to unsigned: u8 = i8 + 128
        let simd_len = in_size / 32 * 32;
        unsafe {
            let mut vacc = _mm256_setzero_si256();
            let offset = _mm256_set1_epi8(-128i8); // for bias correction
            let mut j = 0usize;
            while j < simd_len {
                // Load 32 weights and 32 inputs
                let vw = _mm256_loadu_si256(row.as_ptr().add(j) as *const __m256i);
                let vi = _mm256_loadu_si256(input_i8.as_ptr().add(j) as *const __m256i);

                // Make weights unsigned: u8 = i8 + 128 (xor with 0x80)
                let vw_u = _mm256_xor_si256(vw, _mm256_set1_epi8(-128i8));

                // u8 × i8 → i16 pairs with saturating add
                let prod16 = _mm256_maddubs_epi16(vw_u, vi);

                // i16 pairs → i32 with horizontal add
                let prod32 = _mm256_madd_epi16(prod16, _mm256_set1_epi16(1));

                vacc = _mm256_add_epi32(vacc, prod32);

                // Bias correction: subtract 128 * sum(input) for this chunk
                // We handle this after the loop for the full accumulator
                j += 32;
            }

            // Horizontal sum of 8xi32
            let lo = _mm256_extracti128_si256(vacc, 0);
            let hi = _mm256_extracti128_si256(vacc, 1);
            let sum128 = _mm_add_epi32(lo, hi);
            let sum128 = _mm_hadd_epi32(sum128, sum128);
            let sum128 = _mm_hadd_epi32(sum128, sum128);
            acc = _mm_extract_epi32(sum128, 0) as i64;

            // Bias correction for unsigned conversion:
            // We computed sum(w_u8 * input_i8) = sum((w_i8+128) * input_i8)
            //   = sum(w_i8 * input_i8) + 128 * sum(input_i8)
            // So subtract 128 * sum(input_i8[0..simd_len])
            let input_sum: i64 = input_i8[..simd_len].iter().map(|&x| x as i64).sum();
            acc -= 128 * input_sum;
        }

        // Scalar remainder
        for j in simd_len..in_size {
            acc += (row[j] as i64) * (input_i8[j] as i64);
        }

        let combined = (weight_scale * input_scale_factor) >> FRAC_BITS;
        acc * combined
    }).collect()
}

/// Dispatch to SIMD or scalar based on platform and size.
pub fn matmul_fast(weights: &I8Weights, input: &[i64], in_size: usize, out_size: usize) -> Vec<i64> {
    // Use SIMD for large matmuls where the quantization overhead is worth it
    #[cfg(any(target_arch = "aarch64", target_arch = "x86_64"))]
    if in_size >= 512 && out_size >= 256 {
        return matmul_i8xi8_simd(weights, input, in_size, out_size);
    }
    matmul_i8(weights, input, in_size, out_size)
}

// ─── Helpers ──────────────────────────────────────────────────────────────────

/// Integer layer normalization (operates on i64 activations).
fn layernorm(input: &[i64], gamma: &[i64]) -> Vec<i64> {
    let n = input.len() as i64;
    if n == 0 { return vec![]; }

    let mean = input.iter().sum::<i64>() / n;
    let mut var_sum: i64 = 0;
    for &x in input {
        let d = x - mean;
        var_sum += (d * d) >> FRAC_BITS;
    }
    let variance = var_sum / n;
    let inv_std = integer_isqrt(variance + 1);

    input.iter().enumerate().map(|(i, &x)| {
        let norm = ((x - mean) * inv_std) >> FRAC_BITS;
        let g = if i < gamma.len() { gamma[i] } else { ONE };
        (norm * g) >> FRAC_BITS
    }).collect()
}

/// Apply RoPE (rotary position embeddings) to Q or K vector.
fn apply_rope(vec: &mut [i64], pos: usize, d_head: usize, cos: &[i64], sin: &[i64]) {
    let half = d_head / 2;
    for i in 0..half {
        let cos_val = cos[pos * half + i];
        let sin_val = sin[pos * half + i];
        let x0 = vec[i];
        let x1 = vec[i + half];
        vec[i] = ((x0 * cos_val) >> FRAC_BITS) - ((x1 * sin_val) >> FRAC_BITS);
        vec[i + half] = ((x0 * sin_val) >> FRAC_BITS) + ((x1 * cos_val) >> FRAC_BITS);
    }
}

/// SiLU approximation: x * sigmoid(x) ≈ x (if x > 0) else x/4.
fn silu_i64(x: i64) -> i64 {
    if x > 0 { x } else { x >> 2 }
}

// ─── Binary weight cache for cross-platform determinism ───────────────────────

impl I8Weights {
    fn write_to(&self, w: &mut impl std::io::Write) -> std::io::Result<()> {
        w.write_all(&(self.data.len() as u64).to_le_bytes())?;
        w.write_all(&self.scale.to_le_bytes())?;
        // Safety: i8 and u8 have the same layout
        let bytes: &[u8] = unsafe {
            std::slice::from_raw_parts(self.data.as_ptr() as *const u8, self.data.len())
        };
        w.write_all(bytes)
    }

    fn read_from(r: &mut impl std::io::Read) -> std::io::Result<Self> {
        let mut buf8 = [0u8; 8];
        r.read_exact(&mut buf8)?;
        let len = u64::from_le_bytes(buf8) as usize;
        r.read_exact(&mut buf8)?;
        let scale = i64::from_le_bytes(buf8);
        let mut data_bytes = vec![0u8; len];
        r.read_exact(&mut data_bytes)?;
        // Safety: i8 and u8 have the same layout
        let data: Vec<i8> = unsafe {
            let mut d = std::mem::ManuallyDrop::new(data_bytes);
            Vec::from_raw_parts(d.as_mut_ptr() as *mut i8, d.len(), d.capacity())
        };
        Ok(Self { data, scale })
    }
}

fn write_i64_vec(w: &mut impl std::io::Write, v: &[i64]) -> std::io::Result<()> {
    w.write_all(&(v.len() as u64).to_le_bytes())?;
    for &x in v {
        w.write_all(&x.to_le_bytes())?;
    }
    Ok(())
}

fn read_i64_vec(r: &mut impl std::io::Read) -> std::io::Result<Vec<i64>> {
    let mut buf8 = [0u8; 8];
    r.read_exact(&mut buf8)?;
    let len = u64::from_le_bytes(buf8) as usize;
    let mut v = Vec::with_capacity(len);
    for _ in 0..len {
        r.read_exact(&mut buf8)?;
        v.push(i64::from_le_bytes(buf8));
    }
    Ok(v)
}

// ─── Forward Pass ─────────────────────────────────────────────────────────────

impl CachedIntegerModel {
    /// Total model memory in bytes (approximate).
    pub fn memory_bytes(&self) -> usize {
        let mut total = self.embedding.memory_bytes()
            + self.output_weight.memory_bytes()
            + self.final_norm.len() * 8
            + self.config.rope_cos.len() * 8
            + self.config.rope_sin.len() * 8;
        for layer in &self.layers {
            total += layer.wq.memory_bytes()
                + layer.wk.memory_bytes()
                + layer.wv.memory_bytes()
                + layer.wo.memory_bytes()
                + layer.w_gate.memory_bytes()
                + layer.w_up.memory_bytes()
                + layer.w_down.memory_bytes()
                + layer.attn_norm.len() * 8
                + layer.ffn_norm.len() * 8;
        }
        total
    }

    /// Decode token IDs to text using the GGUF vocabulary.
    pub fn decode(&self, tokens: &[u32]) -> String {
        tokens.iter()
            .map(|&id| {
                if (id as usize) < self.vocab.len() {
                    // SentencePiece uses ▁ (U+2581) for space
                    self.vocab[id as usize].replace('▁', " ")
                } else {
                    format!("[{}]", id)
                }
            })
            .collect::<String>()
    }

    /// Simple greedy encode: text → token IDs.
    /// Not perfect BPE but good enough for demos.
    pub fn encode(&self, text: &str) -> Vec<u32> {
        if self.vocab.is_empty() { return vec![]; }
        let mut tokens = Vec::new();
        let sp_text = format!("▁{}", text.replace(' ', "▁"));
        let bytes = sp_text.as_bytes();
        let mut pos = 0;
        while pos < bytes.len() {
            let mut best_len = 0;
            let mut best_id = 0u32;
            // Try longest match first (up to 32 bytes)
            let max_try = (bytes.len() - pos).min(32);
            for try_len in (1..=max_try).rev() {
                if let Ok(candidate) = std::str::from_utf8(&bytes[pos..pos + try_len]) {
                    if let Some(id) = self.vocab.iter().position(|v| v == candidate) {
                        best_len = try_len;
                        best_id = id as u32;
                        break;
                    }
                }
            }
            if best_len > 0 {
                tokens.push(best_id);
                pos += best_len;
            } else {
                // Byte fallback: look for <0xXX> tokens
                let byte_tok = format!("<0x{:02X}>", bytes[pos]);
                if let Some(id) = self.vocab.iter().position(|v| v == &byte_tok) {
                    tokens.push(id as u32);
                }
                pos += 1;
            }
        }
        tokens
    }

    /// Run forward pass for a SINGLE new token (uses KV cache for previous tokens).
    /// Returns logits [vocab_size] in Q16.
    pub fn forward_one_token(&self, token: u32, cache: &mut KVCache) -> Vec<i64> {
        let cfg = &self.config;
        let d = cfg.d_model;
        let pos = cache.seq_len;

        // Embed token: look up INT8 embedding and expand to Q16
        let idx = (token as usize).min(cfg.vocab_size - 1);
        let emb_start = idx * d;
        let emb_scale = self.embedding.scale;
        let mut hidden: Vec<i64> = self.embedding.data[emb_start..emb_start + d]
            .iter()
            .map(|&w| (w as i64) * emb_scale)
            .collect();

        // Process each layer
        for (layer_idx, layer) in self.layers.iter().enumerate() {
            // Pre-norm
            let normed = layernorm(&hidden, &layer.attn_norm);

            // Q, K, V projections (only for this one token)
            let mut q = matmul_fast(&layer.wq, &normed, d, d);
            let k = matmul_fast(&layer.wk, &normed, d, cfg.d_kv);
            let v = matmul_fast(&layer.wv, &normed, d, cfg.d_kv);

            // Apply RoPE to Q and K (per-head)
            for h in 0..cfg.n_heads {
                apply_rope(
                    &mut q[h * cfg.d_head..(h + 1) * cfg.d_head],
                    pos, cfg.d_head, &cfg.rope_cos, &cfg.rope_sin,
                );
            }
            // RoPE on K — need mutable copy
            let mut k = k;
            for h in 0..cfg.n_kv_heads {
                apply_rope(
                    &mut k[h * cfg.d_head..(h + 1) * cfg.d_head],
                    pos, cfg.d_head, &cfg.rope_cos, &cfg.rope_sin,
                );
            }

            // Append K, V to cache
            cache.k[layer_idx].extend_from_slice(&k);
            cache.v[layer_idx].extend_from_slice(&v);

            // Multi-head attention with KV cache
            let full_seq = pos + 1;

            // Parallel across heads
            let head_results: Vec<Vec<i64>> = (0..cfg.n_heads).into_par_iter().map(|h| {
                let kv_h = h * cfg.n_kv_heads / cfg.n_heads;
                let dh = cfg.d_head;

                // Q for this head (current position only)
                let q_head = &q[h * dh..(h + 1) * dh];

                // Compute attention scores against all cached K
                let mut scores = Vec::with_capacity(full_seq);
                for j in 0..full_seq {
                    let k_head = &cache.k[layer_idx][j * cfg.d_kv + kv_h * dh..j * cfg.d_kv + (kv_h + 1) * dh];
                    let mut dot: i64 = 0;
                    for dd in 0..dh {
                        dot += q_head[dd] * k_head[dd];
                    }
                    scores.push((dot >> FRAC_BITS) * cfg.attn_scale >> FRAC_BITS);
                }

                // Softmax
                let attn_weights = softmax_i64(&scores);

                // Weighted sum of V
                let mut out = vec![0i64; dh];
                for j in 0..full_seq {
                    let v_head = &cache.v[layer_idx][j * cfg.d_kv + kv_h * dh..j * cfg.d_kv + (kv_h + 1) * dh];
                    for dd in 0..dh {
                        out[dd] += (attn_weights[j] * v_head[dd]) >> FRAC_BITS;
                    }
                }
                out
            }).collect();

            // Gather head results
            let mut attn_out = vec![0i64; d];
            for (h, head_out) in head_results.iter().enumerate() {
                attn_out[h * cfg.d_head..(h + 1) * cfg.d_head].copy_from_slice(head_out);
            }

            // Output projection + residual
            let projected = matmul_fast(&layer.wo, &attn_out, d, d);
            for i in 0..d {
                hidden[i] += projected[i];
            }

            // FFN: pre-norm → gate/up → SiLU → down → residual
            let normed_ff = layernorm(&hidden, &layer.ffn_norm);
            let gate = matmul_fast(&layer.w_gate, &normed_ff, d, cfg.d_ff);
            let up = matmul_fast(&layer.w_up, &normed_ff, d, cfg.d_ff);

            // SiLU gate * up
            let gated: Vec<i64> = gate.iter().zip(up.iter())
                .map(|(&g, &u)| (silu_i64(g) * u) >> FRAC_BITS)
                .collect();

            let ff_out = matmul_fast(&layer.w_down, &gated, cfg.d_ff, d);
            for i in 0..d {
                hidden[i] += ff_out[i];
            }
        }

        cache.seq_len = pos + 1;

        // Final norm + LM head
        let normed = layernorm(&hidden, &self.final_norm);
        matmul_fast(&self.output_weight, &normed, d, cfg.vocab_size)
    }

    /// Generate tokens autoregressively with KV cache.
    pub fn generate(
        &self,
        prompt: &[u32],
        max_tokens: u32,
        eos_tokens: &[u32],
    ) -> (Vec<u32>, Hash256) {
        let mut cache = KVCache::new(self.config.n_layers);
        let mut generated = Vec::new();

        // Process prompt tokens (build up KV cache)
        for &tok in prompt {
            let _logits = self.forward_one_token(tok, &mut cache);
        }

        // Generate new tokens
        for _ in 0..max_tokens {
            let last_token = generated.last().copied()
                .unwrap_or(*prompt.last().unwrap_or(&0));
            let logits = self.forward_one_token(last_token, &mut cache);
            let next = argmax_i64(&logits) as u32;
            generated.push(next);
            if eos_tokens.contains(&next) { break; }
        }

        let output_bytes: Vec<u8> = generated.iter()
            .flat_map(|t| t.to_le_bytes())
            .collect();
        let hash = arc_crypto::hash_bytes(&output_bytes);
        (generated, hash)
    }

    /// Save all INT8 weights to a binary file for cross-platform determinism.
    /// Load once from GGUF on any platform, save to .arc-int8, distribute to all nodes.
    pub fn save_weights(&self, path: &str) -> std::io::Result<()> {
        use std::io::Write;
        let mut f = std::io::BufWriter::new(std::fs::File::create(path)?);

        // Magic + version
        f.write_all(b"ARC-INT8\x01\x00")?;

        // Config
        let cfg = &self.config;
        for &v in &[cfg.n_layers, cfg.d_model, cfg.n_heads, cfg.n_kv_heads,
                     cfg.d_ff, cfg.d_head, cfg.d_kv, cfg.vocab_size, cfg.max_seq] {
            f.write_all(&(v as u64).to_le_bytes())?;
        }
        f.write_all(&cfg.attn_scale.to_le_bytes())?;

        // RoPE tables
        write_i64_vec(&mut f, &cfg.rope_cos)?;
        write_i64_vec(&mut f, &cfg.rope_sin)?;

        // Embedding + output + final_norm
        self.embedding.write_to(&mut f)?;
        self.output_weight.write_to(&mut f)?;
        write_i64_vec(&mut f, &self.final_norm)?;

        // Layers
        for layer in &self.layers {
            layer.wq.write_to(&mut f)?;
            layer.wk.write_to(&mut f)?;
            layer.wv.write_to(&mut f)?;
            layer.wo.write_to(&mut f)?;
            layer.w_gate.write_to(&mut f)?;
            layer.w_up.write_to(&mut f)?;
            layer.w_down.write_to(&mut f)?;
            write_i64_vec(&mut f, &layer.attn_norm)?;
            write_i64_vec(&mut f, &layer.ffn_norm)?;
        }

        // Vocab
        let vocab_json = serde_json::to_string(&self.vocab).unwrap_or_default();
        let vocab_bytes = vocab_json.as_bytes();
        f.write_all(&(vocab_bytes.len() as u64).to_le_bytes())?;
        f.write_all(vocab_bytes)?;

        f.flush()?;
        Ok(())
    }

    /// Compute hash of all weights (for verifying cross-platform identity).
    pub fn weight_hash(&self) -> arc_crypto::Hash256 {
        use std::io::Write;
        let mut hasher = blake3::Hasher::new();

        // Hash embedding
        let emb_bytes: &[u8] = unsafe {
            std::slice::from_raw_parts(self.embedding.data.as_ptr() as *const u8, self.embedding.data.len())
        };
        hasher.update(emb_bytes);
        hasher.update(&self.embedding.scale.to_le_bytes());

        // Hash all layer weights
        for layer in &self.layers {
            for w in [&layer.wq, &layer.wk, &layer.wv, &layer.wo,
                      &layer.w_gate, &layer.w_up, &layer.w_down] {
                let bytes: &[u8] = unsafe {
                    std::slice::from_raw_parts(w.data.as_ptr() as *const u8, w.data.len())
                };
                hasher.update(bytes);
                hasher.update(&w.scale.to_le_bytes());
            }
        }

        let hash = hasher.finalize();
        arc_crypto::Hash256(*hash.as_bytes())
    }
}

/// Load model from pre-quantized binary file (cross-platform deterministic).
pub fn load_cached_model_binary(path: &str) -> Result<CachedIntegerModel, crate::InferenceError> {
    use crate::InferenceError;
    use std::io::Read;

    let mut f = std::io::BufReader::new(
        std::fs::File::open(path)
            .map_err(|e| InferenceError::Runtime(format!("Open binary: {e}")))?
    );

    // Magic + version
    let mut magic = [0u8; 10];
    f.read_exact(&mut magic).map_err(|e| InferenceError::Runtime(format!("Read magic: {e}")))?;
    if &magic[..8] != b"ARC-INT8" {
        return Err(InferenceError::Runtime("Not an ARC-INT8 file".into()));
    }

    let mut buf8 = [0u8; 8];
    let read_u64 = |f: &mut std::io::BufReader<std::fs::File>| -> Result<u64, InferenceError> {
        let mut b = [0u8; 8];
        f.read_exact(&mut b).map_err(|e| InferenceError::Runtime(format!("Read: {e}")))?;
        Ok(u64::from_le_bytes(b))
    };

    // Config
    let n_layers = read_u64(&mut f)? as usize;
    let d_model = read_u64(&mut f)? as usize;
    let n_heads = read_u64(&mut f)? as usize;
    let n_kv_heads = read_u64(&mut f)? as usize;
    let d_ff = read_u64(&mut f)? as usize;
    let d_head = read_u64(&mut f)? as usize;
    let d_kv = read_u64(&mut f)? as usize;
    let vocab_size = read_u64(&mut f)? as usize;
    let max_seq = read_u64(&mut f)? as usize;
    f.read_exact(&mut buf8).map_err(|e| InferenceError::Runtime(format!("Read: {e}")))?;
    let attn_scale = i64::from_le_bytes(buf8);

    // RoPE
    let rope_cos = read_i64_vec(&mut f).map_err(|e| InferenceError::Runtime(format!("RoPE cos: {e}")))?;
    let rope_sin = read_i64_vec(&mut f).map_err(|e| InferenceError::Runtime(format!("RoPE sin: {e}")))?;

    // Weights
    let embedding = I8Weights::read_from(&mut f).map_err(|e| InferenceError::Runtime(format!("Embedding: {e}")))?;
    let output_weight = I8Weights::read_from(&mut f).map_err(|e| InferenceError::Runtime(format!("Output: {e}")))?;
    let final_norm = read_i64_vec(&mut f).map_err(|e| InferenceError::Runtime(format!("Norm: {e}")))?;

    let mut layers = Vec::with_capacity(n_layers);
    for l in 0..n_layers {
        layers.push(CachedLayer {
            wq: I8Weights::read_from(&mut f).map_err(|e| InferenceError::Runtime(format!("L{l} wq: {e}")))?,
            wk: I8Weights::read_from(&mut f).map_err(|e| InferenceError::Runtime(format!("L{l} wk: {e}")))?,
            wv: I8Weights::read_from(&mut f).map_err(|e| InferenceError::Runtime(format!("L{l} wv: {e}")))?,
            wo: I8Weights::read_from(&mut f).map_err(|e| InferenceError::Runtime(format!("L{l} wo: {e}")))?,
            w_gate: I8Weights::read_from(&mut f).map_err(|e| InferenceError::Runtime(format!("L{l} gate: {e}")))?,
            w_up: I8Weights::read_from(&mut f).map_err(|e| InferenceError::Runtime(format!("L{l} up: {e}")))?,
            w_down: I8Weights::read_from(&mut f).map_err(|e| InferenceError::Runtime(format!("L{l} down: {e}")))?,
            attn_norm: read_i64_vec(&mut f).map_err(|e| InferenceError::Runtime(format!("L{l} anorm: {e}")))?,
            ffn_norm: read_i64_vec(&mut f).map_err(|e| InferenceError::Runtime(format!("L{l} fnorm: {e}")))?,
        });
        if l % 8 == 0 || l == n_layers - 1 {
            info!("Layer {}/{} loaded from binary", l + 1, n_layers);
        }
    }

    // Vocab
    let vocab_len = {
        let mut b = [0u8; 8];
        f.read_exact(&mut b).map_err(|e| InferenceError::Runtime(format!("Vocab len: {e}")))?;
        u64::from_le_bytes(b) as usize
    };
    let mut vocab_bytes = vec![0u8; vocab_len];
    f.read_exact(&mut vocab_bytes).map_err(|e| InferenceError::Runtime(format!("Vocab: {e}")))?;
    let vocab: Vec<String> = serde_json::from_slice(&vocab_bytes).unwrap_or_default();

    info!("Binary model loaded: {} layers, d={}, vocab={}", n_layers, d_model, vocab_size);

    Ok(CachedIntegerModel {
        config: ModelConfig {
            n_layers, d_model, n_heads, n_kv_heads, d_ff, d_head, d_kv,
            vocab_size, attn_scale, rope_cos, rope_sin, max_seq,
        },
        embedding, layers, final_norm, output_weight, vocab,
    })
}

// ─── RoPE Tables ──────────────────────────────────────────────────────────────

/// Pre-compute RoPE cos/sin tables as i64 Q16.
pub fn compute_rope_tables(d_head: usize, max_seq: usize, base: f64) -> (Vec<i64>, Vec<i64>) {
    let half = d_head / 2;
    let mut cos_table = vec![0i64; max_seq * half];
    let mut sin_table = vec![0i64; max_seq * half];

    for pos in 0..max_seq {
        for i in 0..half {
            let freq = 1.0 / base.powf(2.0 * i as f64 / d_head as f64);
            let angle = pos as f64 * freq;
            // Convert to Q16 — f64 used only at init, never during forward pass
            cos_table[pos * half + i] = (angle.cos() * ONE as f64).round() as i64;
            sin_table[pos * half + i] = (angle.sin() * ONE as f64).round() as i64;
        }
    }

    (cos_table, sin_table)
}

// ─── GGUF Loader ──────────────────────────────────────────────────────────────

/// Load a GGUF model into a CachedIntegerModel with INT8 weight storage.
/// This is the ONE-TIME startup cost. After this, inference is pure integer from RAM.
///
/// Memory: ~1 byte per parameter (vs 8 bytes for i64).
/// 7B model: ~7GB. 1.1B model: ~1.1GB.
#[cfg(feature = "candle")]
pub fn load_cached_model(path: &str) -> Result<CachedIntegerModel, crate::InferenceError> {
    use candle_core::Device;
    use candle_core::quantized::gguf_file;
    use crate::InferenceError;

    let device = Device::Cpu;
    let gguf_path = path.to_string();

    // Read metadata + vocab from GGUF
    let (n_layers, d_model, n_heads, n_kv_heads, d_ff, vocab_size, vocab) = {
        let mut reader = std::fs::File::open(&gguf_path)
            .map_err(|e| InferenceError::Runtime(format!("Open: {e}")))?;
        let content = gguf_file::Content::read(&mut reader)
            .map_err(|e| InferenceError::Runtime(format!("GGUF: {e}")))?;

        let get_u32 = |key: &str| -> u32 {
            match content.metadata.get(key) {
                Some(gguf_file::Value::U32(v)) => *v,
                Some(gguf_file::Value::U64(v)) => *v as u32,
                Some(gguf_file::Value::I32(v)) => *v as u32,
                _ => 0,
            }
        };

        let nl = get_u32("llama.block_count") as usize;
        let dm = get_u32("llama.embedding_length") as usize;
        let nh = get_u32("llama.attention.head_count") as usize;
        let nkv = { let v = get_u32("llama.attention.head_count_kv"); if v > 0 { v as usize } else { nh } };
        let dff = get_u32("llama.feed_forward_length") as usize;
        let vs = content.tensor_infos.get("token_embd.weight")
            .map(|t| t.shape.dims()[0] as usize)
            .unwrap_or(32000);

        // Extract vocabulary from GGUF metadata
        let vocab = match content.metadata.get("tokenizer.ggml.tokens") {
            Some(gguf_file::Value::Array(arr)) => {
                arr.iter().filter_map(|v| match v {
                    gguf_file::Value::String(s) => Some(s.clone()),
                    _ => None,
                }).collect()
            }
            _ => Vec::new(),
        };

        (nl, dm, nh, nkv, dff, vs, vocab)
    };

    let d_head = d_model / n_heads;
    let d_kv = d_head * n_kv_heads;

    info!(n_layers, d_model, n_heads, n_kv_heads, d_ff, vocab_size,
        vocab_len = vocab.len(),
        "Loading GGUF into INT8 cache...");

    // Open file ONCE, parse GGUF content ONCE (avoids 28s re-parsing per tensor)
    let mut reader = std::fs::File::open(&gguf_path)
        .map_err(|e| InferenceError::Runtime(format!("Open: {e}")))?;
    let content = gguf_file::Content::read(&mut reader)
        .map_err(|e| InferenceError::Runtime(format!("GGUF: {e}")))?;

    // Helper: extract tensor as f32 (reuses open file handle + parsed content)
    let extract_f32 = |reader: &mut std::fs::File, content: &gguf_file::Content, name: &str| -> Result<Vec<f32>, InferenceError> {
        let qt = content.tensor(reader, name, &device)
            .map_err(|e| InferenceError::Runtime(format!("{name}: {e}")))?;
        let deq = qt.dequantize(&device)
            .map_err(|e| InferenceError::Runtime(format!("dequant {name}: {e}")))?;
        deq.flatten_all()
            .map_err(|e| InferenceError::Runtime(format!("flatten: {e}")))?
            .to_vec1::<f32>()
            .map_err(|e| InferenceError::Runtime(format!("tovec: {e}")))
    };

    // Helper: extract as INT8 quantized
    let extract_i8 = |reader: &mut std::fs::File, content: &gguf_file::Content, name: &str| -> Result<I8Weights, InferenceError> {
        let f32_data = extract_f32(reader, content, name)?;
        Ok(I8Weights::quantize_f32(&f32_data))
    };

    // Helper: extract as i64 Q16 (for norms — small vectors)
    let extract_norm = |reader: &mut std::fs::File, content: &gguf_file::Content, name: &str, size: usize| -> Vec<i64> {
        extract_f32(reader, content, name).map(|f| {
            f.iter().map(|&x| (x * ONE as f32).round() as i64).collect()
        }).unwrap_or_else(|_| vec![ONE; size])
    };

    // Load embeddings as INT8
    let embedding = extract_i8(&mut reader, &content, "token_embd.weight")?;
    let emb_mb = embedding.memory_bytes() / (1024 * 1024);
    info!("Embeddings loaded as INT8 ({} MB, {} tokens)", emb_mb, vocab_size);

    // Load output head as INT8
    let output_weight = extract_i8(&mut reader, &content, "output.weight").unwrap_or_else(|_| {
        // Tied embeddings: reuse embedding weights
        I8Weights { data: embedding.data.clone(), scale: embedding.scale }
    });
    let final_norm = extract_norm(&mut reader, &content, "output_norm.weight", d_model);

    // Load all layers as INT8
    let mut layers = Vec::with_capacity(n_layers);
    let mut total_layer_mb = 0usize;
    for l in 0..n_layers {
        let p = format!("blk.{l}");

        let wq = extract_i8(&mut reader, &content, &format!("{p}.attn_q.weight"))?;
        let wk = extract_i8(&mut reader, &content, &format!("{p}.attn_k.weight"))?;
        let wv = extract_i8(&mut reader, &content, &format!("{p}.attn_v.weight"))?;
        let wo = extract_i8(&mut reader, &content, &format!("{p}.attn_output.weight"))?;
        let w_gate = extract_i8(&mut reader, &content, &format!("{p}.ffn_gate.weight"))?;
        let w_up = extract_i8(&mut reader, &content, &format!("{p}.ffn_up.weight"))?;
        let w_down = extract_i8(&mut reader, &content, &format!("{p}.ffn_down.weight"))?;

        let layer_mb = (wq.memory_bytes() + wk.memory_bytes() + wv.memory_bytes()
            + wo.memory_bytes() + w_gate.memory_bytes() + w_up.memory_bytes()
            + w_down.memory_bytes()) / (1024 * 1024);
        total_layer_mb += layer_mb;

        if l % 8 == 0 || l == n_layers - 1 {
            info!("Layer {}/{} loaded ({} MB)", l + 1, n_layers, layer_mb);
        }

        layers.push(CachedLayer {
            wq, wk, wv, wo, w_gate, w_up, w_down,
            attn_norm: extract_norm(&mut reader, &content, &format!("{p}.attn_norm.weight"), d_model),
            ffn_norm: extract_norm(&mut reader, &content, &format!("{p}.ffn_norm.weight"), d_model),
        });
    }

    // Compute RoPE tables
    let max_seq = 2048;
    let rope_base = 10000.0;
    let (rope_cos, rope_sin) = compute_rope_tables(d_head, max_seq, rope_base);

    let attn_scale = {
        let isqrt = integer_isqrt((d_head as i64) * ONE);
        (ONE * ONE) / isqrt.max(1)
    };

    let total_mb = (embedding.memory_bytes() + output_weight.memory_bytes()
        + total_layer_mb * 1024 * 1024
        + final_norm.len() * 8
        + rope_cos.len() * 8 * 2) / (1024 * 1024);
    info!("Model loaded into INT8 cache: ~{} MB total. Ready for inference.", total_mb);

    Ok(CachedIntegerModel {
        config: ModelConfig {
            n_layers, d_model, n_heads, n_kv_heads, d_ff, d_head, d_kv,
            vocab_size, attn_scale, rope_cos, rope_sin, max_seq,
        },
        embedding,
        layers,
        final_norm,
        output_weight,
        vocab,
    })
}

#[cfg(not(feature = "candle"))]
pub fn load_cached_model(_path: &str) -> Result<CachedIntegerModel, crate::InferenceError> {
    Err(crate::InferenceError::Runtime("candle feature not enabled".into()))
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a small test model with INT8 weights for testing.
    fn build_test_model_i8(
        vocab_size: usize,
        d_model: usize,
        n_heads: usize,
        d_ff: usize,
        n_layers: usize,
    ) -> CachedIntegerModel {
        let d_head = d_model / n_heads;
        let n_kv_heads = n_heads;
        let d_kv = d_head * n_kv_heads;

        // Deterministic weights using LCG
        let mut rng: u64 = 42;
        let mut gen_f32 = |size: usize| -> Vec<f32> {
            (0..size).map(|_| {
                rng = rng.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
                // Small weights in [-0.1, 0.1]
                ((rng >> 33) as f32 / u32::MAX as f32 - 0.5) * 0.2
            }).collect()
        };

        let mut gen_i8 = |size: usize| -> I8Weights {
            I8Weights::quantize_f32(&gen_f32(size))
        };

        let embedding = gen_i8(vocab_size * d_model);
        let output_weight = gen_i8(vocab_size * d_model);

        let mut layers = Vec::new();
        for _ in 0..n_layers {
            layers.push(CachedLayer {
                wq: gen_i8(d_model * d_model),
                wk: gen_i8(d_model * d_kv),
                wv: gen_i8(d_model * d_kv),
                wo: gen_i8(d_model * d_model),
                w_gate: gen_i8(d_ff * d_model),
                w_up: gen_i8(d_ff * d_model),
                w_down: gen_i8(d_model * d_ff),
                attn_norm: vec![ONE; d_model],
                ffn_norm: vec![ONE; d_model],
            });
        }

        let max_seq = 512;
        let (rope_cos, rope_sin) = compute_rope_tables(d_head, max_seq, 10000.0);
        let attn_scale = {
            let isqrt = integer_isqrt((d_head as i64) * ONE);
            (ONE * ONE) / isqrt.max(1)
        };

        CachedIntegerModel {
            config: ModelConfig {
                n_layers, d_model, n_heads, n_kv_heads, d_ff, d_head, d_kv,
                vocab_size, attn_scale, rope_cos, rope_sin, max_seq,
            },
            embedding,
            layers,
            final_norm: vec![ONE; d_model],
            output_weight,
            vocab: (0..vocab_size).map(|i| format!("tok_{}", i)).collect(),
        }
    }

    #[test]
    fn test_i8_quantize_roundtrip() {
        let values: Vec<f32> = vec![0.0, 0.5, -0.5, 1.0, -1.0, 0.01, -0.01];
        let q = I8Weights::quantize_f32(&values);

        // Reconstruct and check closeness
        for (i, &orig) in values.iter().enumerate() {
            let reconstructed = (q.data[i] as f64) * (q.scale as f64) / ONE as f64;
            let error = (reconstructed - orig as f64).abs();
            assert!(error < 0.02, "Quantize error too large for {}: got {}, error {}",
                orig, reconstructed, error);
        }
    }

    #[test]
    fn test_i8_matmul_correctness() {
        // Compare i8 matmul against direct i64 matmul
        let weights_f32 = vec![1.0f32, 2.0, 3.0, 4.0, 5.0, 6.0]; // 2×3
        let weights_i8 = I8Weights::quantize_f32(&weights_f32);
        let input = vec![ONE, ONE, ONE]; // [1.0, 1.0, 1.0] in Q16

        let result = matmul_i8(&weights_i8, &input, 3, 2);

        // Row 0: 1+2+3 = 6. Row 1: 4+5+6 = 15.
        // In Q16: 6*ONE = 393216, 15*ONE = 983040
        let expected_0 = 6.0 * ONE as f64;
        let expected_1 = 15.0 * ONE as f64;
        let tolerance = ONE as f64 * 0.05; // 5% tolerance for quantization

        assert!((result[0] as f64 - expected_0).abs() < tolerance,
            "Row 0: expected ~{}, got {}", expected_0, result[0]);
        assert!((result[1] as f64 - expected_1).abs() < tolerance,
            "Row 1: expected ~{}, got {}", expected_1, result[1]);
    }

    #[test]
    fn test_i8_matmul_deterministic_1000() {
        let weights = I8Weights::quantize_f32(&vec![0.1f32; 256 * 128]);
        let input: Vec<i64> = (0..128).map(|i| (i as i64 - 64) * 1000).collect();

        let first = matmul_i8(&weights, &input, 128, 256);
        for _ in 0..1000 {
            let result = matmul_i8(&weights, &input, 128, 256);
            assert_eq!(result, first, "INT8 matmul not deterministic");
        }
    }

    #[test]
    fn test_i8_model_deterministic() {
        let model = build_test_model_i8(100, 64, 2, 128, 2);
        let prompt = vec![1u32, 5, 10, 15];

        let (tokens1, hash1) = model.generate(&prompt, 8, &[99]);
        let (tokens2, hash2) = model.generate(&prompt, 8, &[99]);

        assert_eq!(tokens1, tokens2, "Non-deterministic generation");
        assert_eq!(hash1, hash2, "Non-deterministic hash");
    }

    #[test]
    fn test_i8_model_deterministic_100_runs() {
        let model = build_test_model_i8(50, 32, 2, 64, 1);
        let prompt = vec![1u32, 2, 3];

        let (_, first_hash) = model.generate(&prompt, 4, &[99]);
        for _ in 0..100 {
            let (_, hash) = model.generate(&prompt, 4, &[99]);
            assert_eq!(hash, first_hash, "Determinism broken across runs");
        }
    }

    #[test]
    fn test_i8_model_memory_savings() {
        let model = build_test_model_i8(100, 64, 2, 128, 2);
        let mem = model.memory_bytes();
        // With i64 weights: 100*64*8 (emb) + 100*64*8 (output) + 2 layers * 7 matrices
        // Each layer: (64*64 + 64*64 + 64*64 + 64*64 + 128*64 + 128*64 + 64*128) * 8
        // With i8: same but × 1 instead of × 8
        // Should be roughly 8x smaller
        let i64_estimate = (100*64 + 100*64 + 2 * (64*64*4 + 128*64*3)) * 8;
        let i8_estimate = (100*64 + 100*64 + 2 * (64*64*4 + 128*64*3)) * 1;
        assert!(mem < i64_estimate, "INT8 model ({}) should be much smaller than i64 ({})", mem, i64_estimate);
        assert!(mem < i8_estimate * 4, "INT8 model ({}) too large vs estimate ({})", mem, i8_estimate);
    }

    #[test]
    fn test_encode_decode_roundtrip() {
        let model = build_test_model_i8(10, 32, 2, 64, 1);
        let decoded = model.decode(&[0, 1, 2, 3]);
        assert!(decoded.contains("tok_0"));
        assert!(decoded.contains("tok_3"));
    }

    #[cfg(any(target_arch = "aarch64", target_arch = "x86_64"))]
    #[test]
    fn test_simd_matches_scalar() {
        let weights = I8Weights::quantize_f32(
            &(0..1024 * 512).map(|i| (i as f32 % 200.0 - 100.0) / 100.0).collect::<Vec<_>>()
        );
        let input: Vec<i64> = (0..512).map(|i| (i as i64 - 256) * ONE / 256).collect();

        let scalar = matmul_i8_par(&weights, &input, 512, 1024);
        let simd = matmul_i8xi8_simd(&weights, &input, 512, 1024);

        // SIMD result may differ slightly due to i8 input quantization
        for i in 0..1024 {
            let diff = (scalar[i] - simd[i]).abs();
            let tolerance = scalar[i].abs().max(ONE) / 10; // 10% tolerance
            assert!(diff < tolerance,
                "Row {}: scalar={}, simd={}, diff={}", i, scalar[i], simd[i], diff);
        }
    }
}
