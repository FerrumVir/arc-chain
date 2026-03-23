//! Cached Integer Model — Production-speed deterministic inference with INT8 weights.
//!
//! Stores weights as INT8 (1 byte per parameter) with **per-row** Q16 scale factors.
//! Per-row quantization: each output row of a weight matrix gets its own scale,
//! dramatically improving precision compared to per-tensor quantization.
//!
//! 7B model: ~7GB instead of 56GB with i64. Fits in 8GB RAM.
//! Forward pass: i8 weight × i64 activation → accumulate in i64 → per-row scale → Q16.
//! Pure integer arithmetic during inference. Deterministic on all platforms.
//! Float used ONLY at model load time (GGUF dequant → per-row i8 quantization).

use crate::integer_lut::*;
use arc_crypto::Hash256;
use rayon::prelude::*;
use tracing::info;

// ─── INT8 Weight Storage (Per-Row Quantization) ───────────────────────────────

/// Per-row symmetric INT8 quantized weight matrix.
///
/// Each row has its own scale factor, so every row uses the full [-127, 127] range.
/// This eliminates the precision loss from outlier weights in other rows.
///
/// Layout: data is row-major [n_rows × n_cols] as i8.
/// scales[i] = Q16 representation of (abs_max_of_row_i / 127).
/// Reconstruction: real_value[i][j] ≈ data[i*cols+j] * scales[i] / ONE
pub struct I8Weights {
    pub data: Vec<i8>,
    pub scales: Vec<i64>,  // Per-row scale in Q16 (one per output row)
    pub n_rows: usize,
    pub n_cols: usize,
}

impl I8Weights {
    /// Quantize f32 matrix [n_rows × n_cols] to per-row symmetric INT8.
    pub fn quantize_f32(values: &[f32], n_rows: usize, n_cols: usize) -> Self {
        assert_eq!(values.len(), n_rows * n_cols);

        let mut data = Vec::with_capacity(n_rows * n_cols);
        let mut scales = Vec::with_capacity(n_rows);

        for i in 0..n_rows {
            let row = &values[i * n_cols..(i + 1) * n_cols];

            // Per-row abs_max
            let abs_max = row.iter().map(|x| x.abs()).fold(0.0f32, f32::max);
            let abs_max = abs_max.max(1e-10);

            let inv_abs_max = 127.0 / abs_max;
            for &x in row {
                data.push((x * inv_abs_max).round().clamp(-127.0, 127.0) as i8);
            }

            // Per-row scale = abs_max / 127 in Q16
            let scale = ((abs_max as f64 / 127.0) * ONE as f64).round() as i64;
            scales.push(scale.max(1));
        }

        Self { data, scales, n_rows, n_cols }
    }

    /// Memory usage in bytes.
    pub fn memory_bytes(&self) -> usize {
        self.data.len() + self.scales.len() * 8 + 16
    }
}

// ─── Layer and Model Structs ──────────────────────────────────────────────────

/// Pre-loaded transformer layer weights in per-row INT8 with Q16 norms.
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
    pub k: Vec<Vec<i64>>,
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
    pub rope_cos: Vec<i64>,
    pub rope_sin: Vec<i64>,
    pub max_seq: usize,
}

/// Fully cached integer model with per-row INT8 weights.
pub struct CachedIntegerModel {
    pub config: ModelConfig,
    pub embedding: I8Weights,     // [vocab × d_model]
    pub layers: Vec<CachedLayer>,
    pub final_norm: Vec<i64>,
    pub output_weight: I8Weights, // [vocab × d_model]
    pub vocab: Vec<String>,
}

// ─── INT8 Matmul (Per-Row Scale, Optimized) ───────────────────────────────────

/// Core i8×i64 dot product for a single row. Unsafe for speed (no bounds checks).
/// 8-element unroll for ILP. This is the hot inner loop.
#[inline(always)]
unsafe fn dot_i8_i64(row: *const i8, input: *const i64, len: usize) -> i64 {
    let mut acc0: i64 = 0;
    let mut acc1: i64 = 0;
    let mut acc2: i64 = 0;
    let mut acc3: i64 = 0;
    let full = len / 8 * 8;
    let mut j = 0usize;
    while j < full {
        acc0 += (*row.add(j) as i64) * (*input.add(j));
        acc1 += (*row.add(j + 1) as i64) * (*input.add(j + 1));
        acc2 += (*row.add(j + 2) as i64) * (*input.add(j + 2));
        acc3 += (*row.add(j + 3) as i64) * (*input.add(j + 3));
        acc0 += (*row.add(j + 4) as i64) * (*input.add(j + 4));
        acc1 += (*row.add(j + 5) as i64) * (*input.add(j + 5));
        acc2 += (*row.add(j + 6) as i64) * (*input.add(j + 6));
        acc3 += (*row.add(j + 7) as i64) * (*input.add(j + 7));
        j += 8;
    }
    let mut acc = acc0 + acc1 + acc2 + acc3;
    while j < len {
        acc += (*row.add(j) as i64) * (*input.add(j));
        j += 1;
    }
    acc
}

// Wrapper to send raw pointers across rayon threads (data lives for duration of call)
struct SendPtr<T>(*const T);
unsafe impl<T> Send for SendPtr<T> {}
unsafe impl<T> Sync for SendPtr<T> {}

/// Per-row INT8 × i64 parallel matmul — optimized with chunked parallelism.
fn matmul_i8_par(weights: &I8Weights, input: &[i64], in_size: usize, out_size: usize) -> Vec<i64> {
    let data = &weights.data;
    let scales = &weights.scales;
    let mut output = vec![0i64; out_size];
    output.par_chunks_mut(64).enumerate().for_each(|(chunk_idx, chunk)| {
        let start = chunk_idx * 64;
        for (local_i, out) in chunk.iter_mut().enumerate() {
            let i = start + local_i;
            let acc = unsafe {
                dot_i8_i64(data.as_ptr().add(i * in_size), input.as_ptr(), in_size)
            };
            *out = (acc * scales[i]) >> FRAC_BITS;
        }
    });
    output
}

/// Sequential variant for small dimensions.
fn matmul_i8_seq(weights: &I8Weights, input: &[i64], in_size: usize, out_size: usize) -> Vec<i64> {
    let data = &weights.data;
    let scales = &weights.scales;
    let mut output = Vec::with_capacity(out_size);
    for i in 0..out_size {
        let acc = unsafe { dot_i8_i64(data.as_ptr().add(i * in_size), input.as_ptr(), in_size) };
        output.push((acc * scales[i]) >> FRAC_BITS);
    }
    output
}

fn matmul_i8(weights: &I8Weights, input: &[i64], in_size: usize, out_size: usize) -> Vec<i64> {
    if out_size >= 256 {
        matmul_i8_par(weights, input, in_size, out_size)
    } else {
        matmul_i8_seq(weights, input, in_size, out_size)
    }
}

// ─── SIMD INT8 Matmul (Cross-Platform Deterministic) ──────────────────────────

/// NEON i8×i8→i32 SIMD matmul with per-row scales.
/// Processes 32 elements per iteration (2 × 16-byte NEON loads).
#[cfg(target_arch = "aarch64")]
fn matmul_i8xi8_simd(weights: &I8Weights, input: &[i64], in_size: usize, out_size: usize) -> Vec<i64> {
    use std::arch::aarch64::*;

    let input_abs_max = input.iter().map(|x| x.abs()).max().unwrap_or(1).max(1);
    let input_scale_factor = (input_abs_max / 127).max(1);
    let input_i8: Vec<i8> = input.iter()
        .map(|&x| (x / input_scale_factor).clamp(-127, 127) as i8)
        .collect();

    let data = &weights.data;
    let inp_slice = &input_i8;
    let scales = &weights.scales;

    let mut output = vec![0i64; out_size];
    output.par_chunks_mut(64).enumerate().for_each(|(chunk_idx, chunk)| {
        let start = chunk_idx * 64;
        for (local_i, out) in chunk.iter_mut().enumerate() {
            let i = start + local_i;
            let row = unsafe { data.as_ptr().add(i * in_size) };
            let mut acc: i64;
            let simd_len = in_size / 32 * 32;

            unsafe {
                let mut vacc0 = vdupq_n_s32(0);
                let mut vacc1 = vdupq_n_s32(0);
                let mut vacc2 = vdupq_n_s32(0);
                let mut vacc3 = vdupq_n_s32(0);
                let mut j = 0usize;
                while j < simd_len {
                    let vw0 = vld1q_s8(row.add(j));
                    let vi0 = vld1q_s8(inp_slice.as_ptr().add(j));
                    vacc0 = vpadalq_s16(vacc0, vmull_s8(vget_low_s8(vw0), vget_low_s8(vi0)));
                    vacc1 = vpadalq_s16(vacc1, vmull_s8(vget_high_s8(vw0), vget_high_s8(vi0)));
                    let vw1 = vld1q_s8(row.add(j + 16));
                    let vi1 = vld1q_s8(inp_slice.as_ptr().add(j + 16));
                    vacc2 = vpadalq_s16(vacc2, vmull_s8(vget_low_s8(vw1), vget_low_s8(vi1)));
                    vacc3 = vpadalq_s16(vacc3, vmull_s8(vget_high_s8(vw1), vget_high_s8(vi1)));
                    j += 32;
                }
                vacc0 = vaddq_s32(vacc0, vacc1);
                vacc2 = vaddq_s32(vacc2, vacc3);
                vacc0 = vaddq_s32(vacc0, vacc2);
                acc = vaddvq_s32(vacc0) as i64;

                while j < in_size {
                    acc += (*row.add(j) as i64) * (*inp_slice.as_ptr().add(j) as i64);
                    j += 1;
                }
            }

            let combined = (scales[i] * input_scale_factor) >> FRAC_BITS;
            *out = acc * combined;
        }
    });
    output
}

/// AVX2 i8×i8→i32 SIMD matmul — sign-extend + i32 accumulation (no i16 saturation).
/// Uses chunked parallelism (64 rows per chunk) to reduce rayon overhead.
#[cfg(target_arch = "x86_64")]
fn matmul_i8xi8_simd(weights: &I8Weights, input: &[i64], in_size: usize, out_size: usize) -> Vec<i64> {
    use std::arch::x86_64::*;

    if !is_x86_feature_detected!("avx2") {
        return matmul_i8_par(weights, input, in_size, out_size);
    }

    let input_abs_max = input.iter().map(|x| x.abs()).max().unwrap_or(1).max(1);
    let input_scale_factor = (input_abs_max / 127).max(1);
    let input_i8: Vec<i8> = input.iter()
        .map(|&x| (x / input_scale_factor).clamp(-127, 127) as i8)
        .collect();

    let data = &weights.data;
    let inp_slice = &input_i8;
    let scales = &weights.scales;

    let mut output = vec![0i64; out_size];
    output.par_chunks_mut(64).enumerate().for_each(|(chunk_idx, chunk)| {
        let start = chunk_idx * 64;
        for (local_i, out) in chunk.iter_mut().enumerate() {
            let i = start + local_i;
            let row = unsafe { data.as_ptr().add(i * in_size) };
            let mut acc: i64 = 0;
            let simd_len = in_size / 32 * 32;

            unsafe {
                let mut vacc = _mm256_setzero_si256();
                let mut vacc2 = _mm256_setzero_si256();

                let mut j = 0usize;
                while j < simd_len {
                    let vw = _mm256_loadu_si256(row.add(j) as *const __m256i);
                    let vi = _mm256_loadu_si256(inp_slice.as_ptr().add(j) as *const __m256i);

                    let vw_lo = _mm256_castsi256_si128(vw);
                    let vw_hi = _mm256_extracti128_si256(vw, 1);
                    let vi_lo = _mm256_castsi256_si128(vi);
                    let vi_hi = _mm256_extracti128_si256(vi, 1);

                    vacc = _mm256_add_epi32(vacc, _mm256_madd_epi16(
                        _mm256_cvtepi8_epi16(vw_lo), _mm256_cvtepi8_epi16(vi_lo)));
                    vacc2 = _mm256_add_epi32(vacc2, _mm256_madd_epi16(
                        _mm256_cvtepi8_epi16(vw_hi), _mm256_cvtepi8_epi16(vi_hi)));
                    j += 32;
                }

                vacc = _mm256_add_epi32(vacc, vacc2);
                let lo = _mm256_extracti128_si256(vacc, 0);
                let hi = _mm256_extracti128_si256(vacc, 1);
                let sum128 = _mm_add_epi32(lo, hi);
                let sum128 = _mm_hadd_epi32(sum128, sum128);
                let sum128 = _mm_hadd_epi32(sum128, sum128);
                acc = _mm_extract_epi32(sum128, 0) as i64;

                let mut jj = simd_len;
                while jj < in_size {
                    acc += (*row.add(jj) as i64) * (*inp_slice.as_ptr().add(jj) as i64);
                    jj += 1;
                }
            }

            let combined = (scales[i] * input_scale_factor) >> FRAC_BITS;
            *out = acc * combined;
        }
    });
    output
}

/// Dispatch: SIMD for large matmuls, scalar for small.
pub fn matmul_fast(weights: &I8Weights, input: &[i64], in_size: usize, out_size: usize) -> Vec<i64> {
    #[cfg(any(target_arch = "aarch64", target_arch = "x86_64"))]
    if in_size >= 512 && out_size >= 256 {
        return matmul_i8xi8_simd(weights, input, in_size, out_size);
    }
    matmul_i8(weights, input, in_size, out_size)
}

// ─── Helpers ──────────────────────────────────────────────────────────────────

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

fn silu_i64(x: i64) -> i64 {
    if x > 0 { x } else { x >> 2 }
}

// ─── Binary Weight Cache ──────────────────────────────────────────────────────

impl I8Weights {
    fn write_to(&self, w: &mut impl std::io::Write) -> std::io::Result<()> {
        w.write_all(&(self.n_rows as u64).to_le_bytes())?;
        w.write_all(&(self.n_cols as u64).to_le_bytes())?;
        // Per-row scales
        for &s in &self.scales {
            w.write_all(&s.to_le_bytes())?;
        }
        // Data
        let bytes: &[u8] = unsafe {
            std::slice::from_raw_parts(self.data.as_ptr() as *const u8, self.data.len())
        };
        w.write_all(bytes)
    }

    fn read_from(r: &mut impl std::io::Read) -> std::io::Result<Self> {
        let mut buf8 = [0u8; 8];
        r.read_exact(&mut buf8)?;
        let n_rows = u64::from_le_bytes(buf8) as usize;
        r.read_exact(&mut buf8)?;
        let n_cols = u64::from_le_bytes(buf8) as usize;
        let mut scales = Vec::with_capacity(n_rows);
        for _ in 0..n_rows {
            r.read_exact(&mut buf8)?;
            scales.push(i64::from_le_bytes(buf8));
        }
        let mut data_bytes = vec![0u8; n_rows * n_cols];
        r.read_exact(&mut data_bytes)?;
        let data: Vec<i8> = unsafe {
            let mut d = std::mem::ManuallyDrop::new(data_bytes);
            Vec::from_raw_parts(d.as_mut_ptr() as *mut i8, d.len(), d.capacity())
        };
        Ok(Self { data, scales, n_rows, n_cols })
    }
}

fn write_i64_vec(w: &mut impl std::io::Write, v: &[i64]) -> std::io::Result<()> {
    w.write_all(&(v.len() as u64).to_le_bytes())?;
    for &x in v { w.write_all(&x.to_le_bytes())?; }
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
    pub fn memory_bytes(&self) -> usize {
        let mut total = self.embedding.memory_bytes()
            + self.output_weight.memory_bytes()
            + self.final_norm.len() * 8
            + self.config.rope_cos.len() * 8 * 2;
        for layer in &self.layers {
            total += layer.wq.memory_bytes() + layer.wk.memory_bytes()
                + layer.wv.memory_bytes() + layer.wo.memory_bytes()
                + layer.w_gate.memory_bytes() + layer.w_up.memory_bytes()
                + layer.w_down.memory_bytes()
                + (layer.attn_norm.len() + layer.ffn_norm.len()) * 8;
        }
        total
    }

    pub fn decode(&self, tokens: &[u32]) -> String {
        tokens.iter()
            .map(|&id| {
                if (id as usize) < self.vocab.len() {
                    self.vocab[id as usize].replace('▁', " ")
                } else {
                    format!("[{}]", id)
                }
            })
            .collect::<String>()
    }

    pub fn encode(&self, text: &str) -> Vec<u32> {
        if self.vocab.is_empty() { return vec![]; }
        let mut tokens = Vec::new();
        let sp_text = format!("▁{}", text.replace(' ', "▁"));
        let bytes = sp_text.as_bytes();
        let mut pos = 0;
        while pos < bytes.len() {
            let mut best_len = 0;
            let mut best_id = 0u32;
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
                let byte_tok = format!("<0x{:02X}>", bytes[pos]);
                if let Some(id) = self.vocab.iter().position(|v| v == &byte_tok) {
                    tokens.push(id as u32);
                }
                pos += 1;
            }
        }
        tokens
    }

    /// Forward pass for a single new token with KV cache.
    pub fn forward_one_token(&self, token: u32, cache: &mut KVCache) -> Vec<i64> {
        let cfg = &self.config;
        let d = cfg.d_model;
        let pos = cache.seq_len;

        // Embed: per-row scale means each token's embedding uses full i8 range
        let idx = (token as usize).min(cfg.vocab_size - 1);
        let emb_start = idx * d;
        let emb_scale = self.embedding.scales[idx];
        let mut hidden: Vec<i64> = self.embedding.data[emb_start..emb_start + d]
            .iter().map(|&w| (w as i64) * emb_scale).collect();

        for (layer_idx, layer) in self.layers.iter().enumerate() {
            let normed = layernorm(&hidden, &layer.attn_norm);

            let mut q = matmul_fast(&layer.wq, &normed, d, d);
            let k = matmul_fast(&layer.wk, &normed, d, cfg.d_kv);
            let v = matmul_fast(&layer.wv, &normed, d, cfg.d_kv);

            for h in 0..cfg.n_heads {
                apply_rope(&mut q[h * cfg.d_head..(h + 1) * cfg.d_head],
                    pos, cfg.d_head, &cfg.rope_cos, &cfg.rope_sin);
            }
            let mut k = k;
            for h in 0..cfg.n_kv_heads {
                apply_rope(&mut k[h * cfg.d_head..(h + 1) * cfg.d_head],
                    pos, cfg.d_head, &cfg.rope_cos, &cfg.rope_sin);
            }

            cache.k[layer_idx].extend_from_slice(&k);
            cache.v[layer_idx].extend_from_slice(&v);

            let full_seq = pos + 1;
            let head_results: Vec<Vec<i64>> = (0..cfg.n_heads).into_par_iter().map(|h| {
                let kv_h = h * cfg.n_kv_heads / cfg.n_heads;
                let dh = cfg.d_head;
                let q_head = &q[h * dh..(h + 1) * dh];

                let mut scores = Vec::with_capacity(full_seq);
                for j in 0..full_seq {
                    let k_head = &cache.k[layer_idx][j * cfg.d_kv + kv_h * dh..j * cfg.d_kv + (kv_h + 1) * dh];
                    let mut dot: i64 = 0;
                    for dd in 0..dh { dot += q_head[dd] * k_head[dd]; }
                    scores.push((dot >> FRAC_BITS) * cfg.attn_scale >> FRAC_BITS);
                }

                let attn_weights = softmax_i64(&scores);

                let mut out = vec![0i64; dh];
                for j in 0..full_seq {
                    let v_head = &cache.v[layer_idx][j * cfg.d_kv + kv_h * dh..j * cfg.d_kv + (kv_h + 1) * dh];
                    for dd in 0..dh {
                        out[dd] += (attn_weights[j] * v_head[dd]) >> FRAC_BITS;
                    }
                }
                out
            }).collect();

            let mut attn_out = vec![0i64; d];
            for (h, head_out) in head_results.iter().enumerate() {
                attn_out[h * cfg.d_head..(h + 1) * cfg.d_head].copy_from_slice(head_out);
            }

            let projected = matmul_fast(&layer.wo, &attn_out, d, d);
            for i in 0..d { hidden[i] += projected[i]; }

            let normed_ff = layernorm(&hidden, &layer.ffn_norm);
            let gate = matmul_fast(&layer.w_gate, &normed_ff, d, cfg.d_ff);
            let up = matmul_fast(&layer.w_up, &normed_ff, d, cfg.d_ff);

            let gated: Vec<i64> = gate.iter().zip(up.iter())
                .map(|(&g, &u)| (silu_i64(g) * u) >> FRAC_BITS).collect();

            let ff_out = matmul_fast(&layer.w_down, &gated, cfg.d_ff, d);
            for i in 0..d { hidden[i] += ff_out[i]; }
        }

        cache.seq_len = pos + 1;
        let normed = layernorm(&hidden, &self.final_norm);
        matmul_fast(&self.output_weight, &normed, d, cfg.vocab_size)
    }

    pub fn generate(&self, prompt: &[u32], max_tokens: u32, eos_tokens: &[u32]) -> (Vec<u32>, Hash256) {
        let mut cache = KVCache::new(self.config.n_layers);
        let mut generated = Vec::new();

        for &tok in prompt {
            let _logits = self.forward_one_token(tok, &mut cache);
        }

        for _ in 0..max_tokens {
            let last_token = generated.last().copied()
                .unwrap_or(*prompt.last().unwrap_or(&0));
            let logits = self.forward_one_token(last_token, &mut cache);
            let next = argmax_i64(&logits) as u32;
            generated.push(next);
            if eos_tokens.contains(&next) { break; }
        }

        let output_bytes: Vec<u8> = generated.iter()
            .flat_map(|t| t.to_le_bytes()).collect();
        let hash = arc_crypto::hash_bytes(&output_bytes);
        (generated, hash)
    }

    /// Save weights to binary .arc-int8 file for cross-platform distribution.
    pub fn save_weights(&self, path: &str) -> std::io::Result<()> {
        use std::io::Write;
        let mut f = std::io::BufWriter::new(std::fs::File::create(path)?);
        f.write_all(b"ARC-INT8\x02\x00")?; // v2: per-row scales

        let cfg = &self.config;
        for &v in &[cfg.n_layers, cfg.d_model, cfg.n_heads, cfg.n_kv_heads,
                     cfg.d_ff, cfg.d_head, cfg.d_kv, cfg.vocab_size, cfg.max_seq] {
            f.write_all(&(v as u64).to_le_bytes())?;
        }
        f.write_all(&cfg.attn_scale.to_le_bytes())?;
        write_i64_vec(&mut f, &cfg.rope_cos)?;
        write_i64_vec(&mut f, &cfg.rope_sin)?;

        self.embedding.write_to(&mut f)?;
        self.output_weight.write_to(&mut f)?;
        write_i64_vec(&mut f, &self.final_norm)?;

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

        let vocab_json = serde_json::to_string(&self.vocab).unwrap_or_default();
        let vb = vocab_json.as_bytes();
        f.write_all(&(vb.len() as u64).to_le_bytes())?;
        f.write_all(vb)?;
        f.flush()
    }

    /// BLAKE3 hash of all weights for cross-platform identity verification.
    pub fn weight_hash(&self) -> Hash256 {
        let mut hasher = blake3::Hasher::new();
        let hash_i8w = |h: &mut blake3::Hasher, w: &I8Weights| {
            let bytes: &[u8] = unsafe {
                std::slice::from_raw_parts(w.data.as_ptr() as *const u8, w.data.len())
            };
            h.update(bytes);
            for &s in &w.scales { h.update(&s.to_le_bytes()); }
        };
        hash_i8w(&mut hasher, &self.embedding);
        hash_i8w(&mut hasher, &self.output_weight);
        for layer in &self.layers {
            for w in [&layer.wq, &layer.wk, &layer.wv, &layer.wo,
                      &layer.w_gate, &layer.w_up, &layer.w_down] {
                hash_i8w(&mut hasher, w);
            }
        }
        let hash = hasher.finalize();
        Hash256(*hash.as_bytes())
    }
}

// ─── RoPE Tables ──────────────────────────────────────────────────────────────

pub fn compute_rope_tables(d_head: usize, max_seq: usize, base: f64) -> (Vec<i64>, Vec<i64>) {
    let half = d_head / 2;
    let mut cos_table = vec![0i64; max_seq * half];
    let mut sin_table = vec![0i64; max_seq * half];
    for pos in 0..max_seq {
        for i in 0..half {
            let freq = 1.0 / base.powf(2.0 * i as f64 / d_head as f64);
            let angle = pos as f64 * freq;
            cos_table[pos * half + i] = (angle.cos() * ONE as f64).round() as i64;
            sin_table[pos * half + i] = (angle.sin() * ONE as f64).round() as i64;
        }
    }
    (cos_table, sin_table)
}

// ─── GGUF Loader ──────────────────────────────────────────────────────────────

#[cfg(feature = "candle")]
pub fn load_cached_model(path: &str) -> Result<CachedIntegerModel, crate::InferenceError> {
    use candle_core::Device;
    use candle_core::quantized::gguf_file;
    use crate::InferenceError;

    let device = Device::Cpu;
    let gguf_path = path.to_string();

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
            .map(|t| t.shape.dims()[0] as usize).unwrap_or(32000);

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
        "Loading GGUF into per-row INT8 cache...");

    // Single file handle + content parse
    let mut reader = std::fs::File::open(&gguf_path)
        .map_err(|e| InferenceError::Runtime(format!("Open: {e}")))?;
    let content = gguf_file::Content::read(&mut reader)
        .map_err(|e| InferenceError::Runtime(format!("GGUF: {e}")))?;

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

    let extract_i8 = |reader: &mut std::fs::File, content: &gguf_file::Content, name: &str, rows: usize, cols: usize| -> Result<I8Weights, InferenceError> {
        let f = extract_f32(reader, content, name)?;
        Ok(I8Weights::quantize_f32(&f, rows, cols))
    };

    let extract_norm = |reader: &mut std::fs::File, content: &gguf_file::Content, name: &str, size: usize| -> Vec<i64> {
        extract_f32(reader, content, name).map(|f| {
            f.iter().map(|&x| (x * ONE as f32).round() as i64).collect()
        }).unwrap_or_else(|_| vec![ONE; size])
    };

    let embedding = extract_i8(&mut reader, &content, "token_embd.weight", vocab_size, d_model)?;
    info!("Embeddings loaded: {} MB per-row INT8", embedding.memory_bytes() / (1024 * 1024));

    let output_weight = extract_i8(&mut reader, &content, "output.weight", vocab_size, d_model)
        .unwrap_or_else(|_| I8Weights {
            data: embedding.data.clone(), scales: embedding.scales.clone(),
            n_rows: embedding.n_rows, n_cols: embedding.n_cols,
        });
    let final_norm = extract_norm(&mut reader, &content, "output_norm.weight", d_model);

    let mut layers = Vec::with_capacity(n_layers);
    for l in 0..n_layers {
        let p = format!("blk.{l}");
        let wq = extract_i8(&mut reader, &content, &format!("{p}.attn_q.weight"), d_model, d_model)?;
        let wk = extract_i8(&mut reader, &content, &format!("{p}.attn_k.weight"), d_kv, d_model)?;
        let wv = extract_i8(&mut reader, &content, &format!("{p}.attn_v.weight"), d_kv, d_model)?;
        let wo = extract_i8(&mut reader, &content, &format!("{p}.attn_output.weight"), d_model, d_model)?;
        let w_gate = extract_i8(&mut reader, &content, &format!("{p}.ffn_gate.weight"), d_ff, d_model)?;
        let w_up = extract_i8(&mut reader, &content, &format!("{p}.ffn_up.weight"), d_ff, d_model)?;
        let w_down = extract_i8(&mut reader, &content, &format!("{p}.ffn_down.weight"), d_model, d_ff)?;

        if l % 8 == 0 || l == n_layers - 1 {
            info!("Layer {}/{} loaded", l + 1, n_layers);
        }

        layers.push(CachedLayer {
            wq, wk, wv, wo, w_gate, w_up, w_down,
            attn_norm: extract_norm(&mut reader, &content, &format!("{p}.attn_norm.weight"), d_model),
            ffn_norm: extract_norm(&mut reader, &content, &format!("{p}.ffn_norm.weight"), d_model),
        });
    }

    let max_seq = 2048;
    let (rope_cos, rope_sin) = compute_rope_tables(d_head, max_seq, 10000.0);
    let attn_scale = {
        let isqrt = integer_isqrt((d_head as i64) * ONE);
        (ONE * ONE) / isqrt.max(1)
    };

    info!("Model loaded: ~{} MB per-row INT8", layers.iter()
        .map(|l| l.wq.memory_bytes() + l.wk.memory_bytes() + l.wv.memory_bytes()
            + l.wo.memory_bytes() + l.w_gate.memory_bytes() + l.w_up.memory_bytes()
            + l.w_down.memory_bytes())
        .sum::<usize>() / (1024 * 1024));

    Ok(CachedIntegerModel {
        config: ModelConfig {
            n_layers, d_model, n_heads, n_kv_heads, d_ff, d_head, d_kv,
            vocab_size, attn_scale, rope_cos, rope_sin, max_seq,
        },
        embedding, layers, final_norm, output_weight, vocab,
    })
}

#[cfg(not(feature = "candle"))]
pub fn load_cached_model(_path: &str) -> Result<CachedIntegerModel, crate::InferenceError> {
    Err(crate::InferenceError::Runtime("candle feature not enabled".into()))
}

/// Load from binary .arc-int8 file.
pub fn load_cached_model_binary(path: &str) -> Result<CachedIntegerModel, crate::InferenceError> {
    use crate::InferenceError;
    use std::io::Read;

    let mut f = std::io::BufReader::new(
        std::fs::File::open(path).map_err(|e| InferenceError::Runtime(format!("Open: {e}")))?
    );

    let mut magic = [0u8; 10];
    f.read_exact(&mut magic).map_err(|e| InferenceError::Runtime(format!("Magic: {e}")))?;
    if &magic[..8] != b"ARC-INT8" {
        return Err(InferenceError::Runtime("Not an ARC-INT8 file".into()));
    }

    let read_u64 = |f: &mut std::io::BufReader<std::fs::File>| -> Result<u64, InferenceError> {
        let mut b = [0u8; 8];
        f.read_exact(&mut b).map_err(|e| InferenceError::Runtime(format!("Read: {e}")))?;
        Ok(u64::from_le_bytes(b))
    };

    let n_layers = read_u64(&mut f)? as usize;
    let d_model = read_u64(&mut f)? as usize;
    let n_heads = read_u64(&mut f)? as usize;
    let n_kv_heads = read_u64(&mut f)? as usize;
    let d_ff = read_u64(&mut f)? as usize;
    let d_head = read_u64(&mut f)? as usize;
    let d_kv = read_u64(&mut f)? as usize;
    let vocab_size = read_u64(&mut f)? as usize;
    let max_seq = read_u64(&mut f)? as usize;
    let mut buf8 = [0u8; 8];
    f.read_exact(&mut buf8).map_err(|e| InferenceError::Runtime(format!("Scale: {e}")))?;
    let attn_scale = i64::from_le_bytes(buf8);

    let rope_cos = read_i64_vec(&mut f).map_err(|e| InferenceError::Runtime(format!("Cos: {e}")))?;
    let rope_sin = read_i64_vec(&mut f).map_err(|e| InferenceError::Runtime(format!("Sin: {e}")))?;

    let embedding = I8Weights::read_from(&mut f).map_err(|e| InferenceError::Runtime(format!("Emb: {e}")))?;
    let output_weight = I8Weights::read_from(&mut f).map_err(|e| InferenceError::Runtime(format!("Out: {e}")))?;
    let final_norm = read_i64_vec(&mut f).map_err(|e| InferenceError::Runtime(format!("Norm: {e}")))?;

    let mut layers = Vec::with_capacity(n_layers);
    for l in 0..n_layers {
        layers.push(CachedLayer {
            wq: I8Weights::read_from(&mut f).map_err(|e| InferenceError::Runtime(format!("L{l}: {e}")))?,
            wk: I8Weights::read_from(&mut f).map_err(|e| InferenceError::Runtime(format!("L{l}: {e}")))?,
            wv: I8Weights::read_from(&mut f).map_err(|e| InferenceError::Runtime(format!("L{l}: {e}")))?,
            wo: I8Weights::read_from(&mut f).map_err(|e| InferenceError::Runtime(format!("L{l}: {e}")))?,
            w_gate: I8Weights::read_from(&mut f).map_err(|e| InferenceError::Runtime(format!("L{l}: {e}")))?,
            w_up: I8Weights::read_from(&mut f).map_err(|e| InferenceError::Runtime(format!("L{l}: {e}")))?,
            w_down: I8Weights::read_from(&mut f).map_err(|e| InferenceError::Runtime(format!("L{l}: {e}")))?,
            attn_norm: read_i64_vec(&mut f).map_err(|e| InferenceError::Runtime(format!("L{l}: {e}")))?,
            ffn_norm: read_i64_vec(&mut f).map_err(|e| InferenceError::Runtime(format!("L{l}: {e}")))?,
        });
    }

    let vocab_len = read_u64(&mut f)? as usize;
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

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn build_test_model(vs: usize, d: usize, nh: usize, dff: usize, nl: usize) -> CachedIntegerModel {
        let dh = d / nh;
        let nkv = nh;
        let dkv = dh * nkv;

        let mut rng: u64 = 42;
        let mut gen_f32 = |size: usize| -> Vec<f32> {
            (0..size).map(|_| {
                rng = rng.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
                ((rng >> 33) as f32 / u32::MAX as f32 - 0.5) * 0.2
            }).collect()
        };
        let mut gen_i8 = |rows: usize, cols: usize| -> I8Weights {
            I8Weights::quantize_f32(&gen_f32(rows * cols), rows, cols)
        };

        let embedding = gen_i8(vs, d);
        let output_weight = gen_i8(vs, d);
        let mut layers = Vec::new();
        for _ in 0..nl {
            layers.push(CachedLayer {
                wq: gen_i8(d, d), wk: gen_i8(dkv, d), wv: gen_i8(dkv, d),
                wo: gen_i8(d, d), w_gate: gen_i8(dff, d), w_up: gen_i8(dff, d),
                w_down: gen_i8(d, dff),
                attn_norm: vec![ONE; d], ffn_norm: vec![ONE; d],
            });
        }

        let (rope_cos, rope_sin) = compute_rope_tables(dh, 512, 10000.0);
        let attn_scale = { let s = integer_isqrt((dh as i64) * ONE); (ONE * ONE) / s.max(1) };

        CachedIntegerModel {
            config: ModelConfig {
                n_layers: nl, d_model: d, n_heads: nh, n_kv_heads: nkv,
                d_ff: dff, d_head: dh, d_kv: dkv, vocab_size: vs,
                attn_scale, rope_cos, rope_sin, max_seq: 512,
            },
            embedding, layers, final_norm: vec![ONE; d], output_weight,
            vocab: (0..vs).map(|i| format!("tok_{}", i)).collect(),
        }
    }

    #[test]
    fn test_per_row_quantize_precision() {
        // Row with large outlier vs row with small values
        let mut values = vec![0.01f32; 8]; // row 0: small
        values.extend(vec![10.0, 0.01, 0.01, 0.01, 0.01, 0.01, 0.01, 0.01]); // row 1: outlier
        let q = I8Weights::quantize_f32(&values, 2, 8);

        // Row 0: abs_max=0.01, scale=0.01/127. Values should be 127 (full range)
        assert_eq!(q.data[0], 127, "Row 0 should use full range");
        // Row 1: abs_max=10.0, scale=10/127. 0.01 → round(0.01/10*127) = 0
        // This is expected — but only affects row 1, not row 0
        assert_eq!(q.data[8], 127, "Row 1 outlier should be 127");
        // Per-row means row 0 is NOT affected by row 1's outlier
        assert!(q.scales[0] < q.scales[1], "Row 0 should have smaller scale");
    }

    #[test]
    fn test_i8_matmul_per_row() {
        let weights = I8Weights::quantize_f32(
            &[1.0, 2.0, 3.0, 4.0, 5.0, 6.0], 2, 3);
        let input = vec![ONE, ONE, ONE];
        let result = matmul_i8(&weights, &input, 3, 2);

        let expected_0 = 6.0 * ONE as f64;
        let expected_1 = 15.0 * ONE as f64;
        let tolerance = ONE as f64 * 0.05;
        assert!((result[0] as f64 - expected_0).abs() < tolerance);
        assert!((result[1] as f64 - expected_1).abs() < tolerance);
    }

    #[test]
    fn test_deterministic_100_runs() {
        let model = build_test_model(50, 32, 2, 64, 1);
        let prompt = vec![1u32, 2, 3];
        let (_, first_hash) = model.generate(&prompt, 4, &[99]);
        for _ in 0..100 {
            let (_, hash) = model.generate(&prompt, 4, &[99]);
            assert_eq!(hash, first_hash, "Determinism broken");
        }
    }

    #[test]
    fn test_model_deterministic() {
        let model = build_test_model(100, 64, 2, 128, 2);
        let prompt = vec![1u32, 5, 10, 15];
        let (t1, h1) = model.generate(&prompt, 8, &[99]);
        let (t2, h2) = model.generate(&prompt, 8, &[99]);
        assert_eq!(t1, t2);
        assert_eq!(h1, h2);
    }

    #[cfg(any(target_arch = "aarch64", target_arch = "x86_64"))]
    #[test]
    fn test_simd_matches_scalar() {
        let weights = I8Weights::quantize_f32(
            &(0..1024 * 512).map(|i| (i as f32 % 200.0 - 100.0) / 100.0).collect::<Vec<_>>(),
            1024, 512,
        );
        let input: Vec<i64> = (0..512).map(|i| (i as i64 - 256) * ONE / 256).collect();

        let scalar = matmul_i8_par(&weights, &input, 512, 1024);
        let simd = matmul_i8xi8_simd(&weights, &input, 512, 1024);

        for i in 0..1024 {
            let diff = (scalar[i] - simd[i]).abs();
            let tolerance = scalar[i].abs().max(ONE) / 5;
            assert!(diff < tolerance, "Row {}: scalar={}, simd={}, diff={}", i, scalar[i], simd[i], diff);
        }
    }
}
