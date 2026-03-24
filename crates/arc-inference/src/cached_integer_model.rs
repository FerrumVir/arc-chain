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

/// INT8 quantized KV cache — 8x less memory than i64.
/// Each position's K/V vector is quantized to i8 with a per-position scale.
/// 7B at 2048 context: 512 MB (vs 4 GB with i64).
pub struct KVCache {
    /// k_data[layer]: flat i8 array, [pos * d_kv .. (pos+1) * d_kv]
    pub k_data: Vec<Vec<i8>>,
    /// k_scales[layer][pos]: Q16 scale for position pos
    pub k_scales: Vec<Vec<i64>>,
    /// v_data[layer]: flat i8 array
    pub v_data: Vec<Vec<i8>>,
    pub v_scales: Vec<Vec<i64>>,
    pub seq_len: usize,
}

impl KVCache {
    pub fn new(n_layers: usize) -> Self {
        Self {
            k_data: vec![Vec::new(); n_layers],
            k_scales: vec![Vec::new(); n_layers],
            v_data: vec![Vec::new(); n_layers],
            v_scales: vec![Vec::new(); n_layers],
            seq_len: 0,
        }
    }

    pub fn clear(&mut self) {
        for l in 0..self.k_data.len() {
            self.k_data[l].clear();
            self.k_scales[l].clear();
            self.v_data[l].clear();
            self.v_scales[l].clear();
        }
        self.seq_len = 0;
    }

    /// Quantize and append a K vector for one position.
    fn push_k(&mut self, layer: usize, k: &[i64]) {
        let (data, scale) = quantize_vec_i8(k);
        self.k_data[layer].extend_from_slice(&data);
        self.k_scales[layer].push(scale);
    }

    /// Quantize and append a V vector for one position.
    fn push_v(&mut self, layer: usize, v: &[i64]) {
        let (data, scale) = quantize_vec_i8(v);
        self.v_data[layer].extend_from_slice(&data);
        self.v_scales[layer].push(scale);
    }
}

/// Quantize an i64 Q16 vector to i8 with per-vector scale.
#[inline]
fn quantize_vec_i8(v: &[i64]) -> (Vec<i8>, i64) {
    let abs_max = v.iter().map(|x| x.abs()).max().unwrap_or(1).max(1);
    let scale_factor = (abs_max / 127).max(1);
    let data: Vec<i8> = v.iter()
        .map(|&x| (x / scale_factor).clamp(-127, 127) as i8)
        .collect();
    (data, scale_factor)
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

/// Pre-converted Q4 layer weights (optional, converted at runtime).
pub struct Q4Layer {
    pub wq: Q4WeightsX86,
    pub wk: Q4WeightsX86,
    pub wv: Q4WeightsX86,
    pub wo: Q4WeightsX86,
    pub w_gate: Q4WeightsX86,
    pub w_up: Q4WeightsX86,
    pub w_down: Q4WeightsX86,
}

/// Fully cached integer model with per-row INT8 weights.
pub struct CachedIntegerModel {
    pub config: ModelConfig,
    /// Embeddings stored at full Q16 precision (not INT8).
    /// Embedding values can be extremely small (1e-6) and INT8 destroys them.
    /// This is just a lookup table, not a matmul — no performance impact.
    pub embedding_q16: Vec<i64>,  // [vocab × d_model] in Q16
    pub embedding_i8: I8Weights,  // kept for weight_hash and save_weights
    pub layers: Vec<CachedLayer>,
    pub final_norm: Vec<i64>,
    pub output_weight: I8Weights, // [vocab × d_model]
    pub vocab: Vec<String>,
    /// Q4 weights — converted from I8 on enable_q4(). Halves bandwidth.
    pub q4_layers: Option<Vec<Q4Layer>>,
    pub q4_output: Option<Q4WeightsX86>,
}

impl CachedIntegerModel {
    /// Convert all weights to Q4 (4-bit). Halves memory bandwidth.
    /// Call once after loading model. Original I8 weights kept for fallback.
    pub fn enable_q4(&mut self) {
        let q4_layers: Vec<Q4Layer> = self.layers.iter().map(|l| Q4Layer {
            wq: Q4WeightsX86::from_i8(&l.wq),
            wk: Q4WeightsX86::from_i8(&l.wk),
            wv: Q4WeightsX86::from_i8(&l.wv),
            wo: Q4WeightsX86::from_i8(&l.wo),
            w_gate: Q4WeightsX86::from_i8(&l.w_gate),
            w_up: Q4WeightsX86::from_i8(&l.w_up),
            w_down: Q4WeightsX86::from_i8(&l.w_down),
        }).collect();
        self.q4_output = Some(Q4WeightsX86::from_i8(&self.output_weight));
        self.q4_layers = Some(q4_layers);
    }
}

// ─── Cached Input Quantization ────────────────────────────────────────────────

/// Pre-quantized i8 input — computed once, reused for multiple matmuls.
pub struct QuantizedInput {
    pub data: Vec<i8>,
    pub scale_factor: i64,
}

impl QuantizedInput {
    /// Quantize i64 Q16 input to i8. Call once, pass to multiple matmuls.
    #[inline]
    pub fn from_i64(input: &[i64]) -> Self {
        let abs_max = input.iter().map(|x| x.abs()).max().unwrap_or(1).max(1);
        let scale_factor = (abs_max / 127).max(1);
        let data: Vec<i8> = input.iter()
            .map(|&x| (x / scale_factor).clamp(-127, 127) as i8)
            .collect();
        Self { data, scale_factor }
    }
}

// ─── INT8 Matmul (Per-Row Scale, Optimized) ───────────────────────────────────

/// Core i8×i64 dot product. Unsafe, 8-element unroll, 4 independent accumulators.
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

/// Write matmul result into pre-allocated output buffer (zero-alloc).
/// Parallel with 512-row chunks to minimize rayon scheduling overhead.
fn matmul_i8_into(weights: &I8Weights, input: &[i64], in_size: usize, output: &mut [i64]) {
    let data = &weights.data;
    let scales = &weights.scales;
    output.par_chunks_mut(512).enumerate().for_each(|(chunk_idx, chunk)| {
        let start = chunk_idx * 512;
        for (local_i, out) in chunk.iter_mut().enumerate() {
            let i = start + local_i;
            let acc = unsafe {
                dot_i8_i64(data.as_ptr().add(i * in_size), input.as_ptr(), in_size)
            };
            *out = (acc * scales[i]) >> FRAC_BITS;
        }
    });
}

/// Allocating matmul (for compatibility and small outputs).
fn matmul_i8(weights: &I8Weights, input: &[i64], in_size: usize, out_size: usize) -> Vec<i64> {
    let mut output = vec![0i64; out_size];
    if out_size >= 256 {
        matmul_i8_into(weights, input, in_size, &mut output);
    } else {
        let data = &weights.data;
        let scales = &weights.scales;
        for i in 0..out_size {
            let acc = unsafe { dot_i8_i64(data.as_ptr().add(i * in_size), input.as_ptr(), in_size) };
            output[i] = (acc * scales[i]) >> FRAC_BITS;
        }
    }
    output
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

/// x86 SIMD matmul — llama.cpp sign trick for AVX2, AVX-512.
/// sign trick: abs(w) × sign_corrected(input) → safe maddubs (no i16 saturation)
/// Processes 32 bytes at once (AVX2) or 64 (AVX-512), no sign extension needed.
#[cfg(target_arch = "x86_64")]
fn matmul_i8xi8_simd(weights: &I8Weights, input: &[i64], in_size: usize, out_size: usize) -> Vec<i64> {
    use std::arch::x86_64::*;

    if !is_x86_feature_detected!("avx2") {
        return matmul_i8(weights, input, in_size, out_size);
    }

    let use_avx512 = is_x86_feature_detected!("avx512bw") && is_x86_feature_detected!("avx512f");

    // Quantize input once — reused across all output rows
    let input_abs_max = input.iter().map(|x| x.abs()).max().unwrap_or(1).max(1);
    let input_scale_factor = (input_abs_max / 127).max(1);
    // Align to 64 bytes for AVX-512 loads (pad with zeros)
    let aligned_len = (in_size + 63) & !63;
    let mut input_i8 = vec![0i8; aligned_len];
    for (i, &x) in input.iter().enumerate() {
        input_i8[i] = (x / input_scale_factor).clamp(-127, 127) as i8;
    }

    let data = &weights.data;
    let inp_slice = &input_i8;
    let scales = &weights.scales;

    let mut output = vec![0i64; out_size];
    output.par_chunks_mut(64).enumerate().for_each(|(chunk_idx, chunk)| {
        let start = chunk_idx * 64;
        for (local_i, out) in chunk.iter_mut().enumerate() {
            let i = start + local_i;
            let row = unsafe { data.as_ptr().add(i * in_size) };
            let inp_ptr = inp_slice.as_ptr();
            let mut acc: i64 = 0;

            if use_avx512 {
                // AVX-512: 4 independent 512-bit accumulators for ILP
                // + software prefetch 256 bytes ahead
                let simd_len = in_size / 128 * 128; // process 128 per iteration
                unsafe {
                    let mut vacc0 = _mm512_setzero_si512();
                    let mut vacc1 = _mm512_setzero_si512();
                    let mut vacc2 = _mm512_setzero_si512();
                    let mut vacc3 = _mm512_setzero_si512();

                    let mut j = 0usize;
                    while j < simd_len {
                        // Prefetch next iteration's data into L1
                        _mm_prefetch(row.add(j + 256) as *const i8, _MM_HINT_T0);
                        _mm_prefetch(inp_ptr.add(j + 256) as *const i8, _MM_HINT_T0);

                        // First 64 elements
                        let vw0 = _mm512_loadu_si512(row.add(j) as *const __m512i);
                        let vi0 = _mm512_loadu_si512(inp_ptr.add(j) as *const __m512i);
                        let vw0_lo = _mm512_castsi512_si256(vw0);
                        let vw0_hi = _mm512_extracti64x4_epi64(vw0, 1);
                        let vi0_lo = _mm512_castsi512_si256(vi0);
                        let vi0_hi = _mm512_extracti64x4_epi64(vi0, 1);
                        vacc0 = _mm512_add_epi32(vacc0, _mm512_madd_epi16(
                            _mm512_cvtepi8_epi16(vw0_lo), _mm512_cvtepi8_epi16(vi0_lo)));
                        vacc1 = _mm512_add_epi32(vacc1, _mm512_madd_epi16(
                            _mm512_cvtepi8_epi16(vw0_hi), _mm512_cvtepi8_epi16(vi0_hi)));

                        // Second 64 elements (independent accumulators for ILP)
                        let vw1 = _mm512_loadu_si512(row.add(j + 64) as *const __m512i);
                        let vi1 = _mm512_loadu_si512(inp_ptr.add(j + 64) as *const __m512i);
                        let vw1_lo = _mm512_castsi512_si256(vw1);
                        let vw1_hi = _mm512_extracti64x4_epi64(vw1, 1);
                        let vi1_lo = _mm512_castsi512_si256(vi1);
                        let vi1_hi = _mm512_extracti64x4_epi64(vi1, 1);
                        vacc2 = _mm512_add_epi32(vacc2, _mm512_madd_epi16(
                            _mm512_cvtepi8_epi16(vw1_lo), _mm512_cvtepi8_epi16(vi1_lo)));
                        vacc3 = _mm512_add_epi32(vacc3, _mm512_madd_epi16(
                            _mm512_cvtepi8_epi16(vw1_hi), _mm512_cvtepi8_epi16(vi1_hi)));
                        j += 128;
                    }

                    vacc0 = _mm512_add_epi32(_mm512_add_epi32(vacc0, vacc1),
                                             _mm512_add_epi32(vacc2, vacc3));
                    acc = _mm512_reduce_add_epi32(vacc0) as i64;

                    // 64-element remainder
                    if j + 64 <= in_size {
                        let vw = _mm512_loadu_si512(row.add(j) as *const __m512i);
                        let vi = _mm512_loadu_si512(inp_ptr.add(j) as *const __m512i);
                        let vw_lo = _mm512_castsi512_si256(vw);
                        let vw_hi = _mm512_extracti64x4_epi64(vw, 1);
                        let vi_lo = _mm512_castsi512_si256(vi);
                        let vi_hi = _mm512_extracti64x4_epi64(vi, 1);
                        let mut vr = _mm512_madd_epi16(
                            _mm512_cvtepi8_epi16(vw_lo), _mm512_cvtepi8_epi16(vi_lo));
                        vr = _mm512_add_epi32(vr, _mm512_madd_epi16(
                            _mm512_cvtepi8_epi16(vw_hi), _mm512_cvtepi8_epi16(vi_hi)));
                        acc += _mm512_reduce_add_epi32(vr) as i64;
                        j += 64;
                    }

                    // Scalar remainder
                    while j < in_size {
                        acc += (*row.add(j) as i64) * (*inp_ptr.add(j) as i64);
                        j += 1;
                    }
                }
            } else {
                // AVX2: 4 independent accumulators + sign trick + prefetch
                let simd_len = in_size / 128 * 128; // 4×32 per iteration for ILP
                unsafe {
                    let mut vacc0 = _mm256_setzero_si256();
                    let mut vacc1 = _mm256_setzero_si256();
                    let mut vacc2 = _mm256_setzero_si256();
                    let mut vacc3 = _mm256_setzero_si256();
                    let ones = _mm256_set1_epi16(1);

                    let mut j = 0usize;
                    while j < simd_len {
                        _mm_prefetch(row.add(j + 256) as *const i8, _MM_HINT_T0);
                        _mm_prefetch(inp_ptr.add(j + 256) as *const i8, _MM_HINT_T0);

                        // 4 independent 32-element blocks per iteration
                        let vw0 = _mm256_loadu_si256(row.add(j) as *const __m256i);
                        let vi0 = _mm256_loadu_si256(inp_ptr.add(j) as *const __m256i);
                        let ax0 = _mm256_sign_epi8(vw0, vw0);
                        let sy0 = _mm256_sign_epi8(vi0, vw0);
                        vacc0 = _mm256_add_epi32(vacc0, _mm256_madd_epi16(_mm256_maddubs_epi16(ax0, sy0), ones));

                        let vw1 = _mm256_loadu_si256(row.add(j + 32) as *const __m256i);
                        let vi1 = _mm256_loadu_si256(inp_ptr.add(j + 32) as *const __m256i);
                        let ax1 = _mm256_sign_epi8(vw1, vw1);
                        let sy1 = _mm256_sign_epi8(vi1, vw1);
                        vacc1 = _mm256_add_epi32(vacc1, _mm256_madd_epi16(_mm256_maddubs_epi16(ax1, sy1), ones));

                        let vw2 = _mm256_loadu_si256(row.add(j + 64) as *const __m256i);
                        let vi2 = _mm256_loadu_si256(inp_ptr.add(j + 64) as *const __m256i);
                        let ax2 = _mm256_sign_epi8(vw2, vw2);
                        let sy2 = _mm256_sign_epi8(vi2, vw2);
                        vacc2 = _mm256_add_epi32(vacc2, _mm256_madd_epi16(_mm256_maddubs_epi16(ax2, sy2), ones));

                        let vw3 = _mm256_loadu_si256(row.add(j + 96) as *const __m256i);
                        let vi3 = _mm256_loadu_si256(inp_ptr.add(j + 96) as *const __m256i);
                        let ax3 = _mm256_sign_epi8(vw3, vw3);
                        let sy3 = _mm256_sign_epi8(vi3, vw3);
                        vacc3 = _mm256_add_epi32(vacc3, _mm256_madd_epi16(_mm256_maddubs_epi16(ax3, sy3), ones));

                        j += 128;
                    }

                    // Merge 4 accumulators
                    vacc0 = _mm256_add_epi32(_mm256_add_epi32(vacc0, vacc1),
                                             _mm256_add_epi32(vacc2, vacc3));

                    // 32-element remainder blocks
                    while j + 32 <= in_size {
                        let vw = _mm256_loadu_si256(row.add(j) as *const __m256i);
                        let vi = _mm256_loadu_si256(inp_ptr.add(j) as *const __m256i);
                        let ax = _mm256_sign_epi8(vw, vw);
                        let sy = _mm256_sign_epi8(vi, vw);
                        vacc0 = _mm256_add_epi32(vacc0, _mm256_madd_epi16(_mm256_maddubs_epi16(ax, sy), ones));
                        j += 32;
                    }

                    // Horizontal sum
                    let lo = _mm256_extracti128_si256(vacc0, 0);
                    let hi = _mm256_extracti128_si256(vacc0, 1);
                    let sum128 = _mm_add_epi32(lo, hi);
                    let sum128 = _mm_hadd_epi32(sum128, sum128);
                    let sum128 = _mm_hadd_epi32(sum128, sum128);
                    acc = _mm_extract_epi32(sum128, 0) as i64;

                    // Scalar remainder
                    while j < in_size {
                        acc += (*row.add(j) as i64) * (*inp_ptr.add(j) as i64);
                        j += 1;
                    }
                }
            }

            let combined = (scales[i] * input_scale_factor) >> FRAC_BITS;
            *out = acc * combined;
        }
    });
    output
}

/// Dispatch: SIMD i8×i8 for large matmuls, scalar for small.
/// NOTE: SIMD path quantizes input to i8 which causes double-quantization precision loss.
/// For models with small weight distributions, use scalar path (i8×i64, full input precision).
pub fn matmul_fast(weights: &I8Weights, input: &[i64], in_size: usize, out_size: usize) -> Vec<i64> {
    // Use scalar i8×i64 path for full precision — SIMD i8×i8 loses too much
    // precision for small models like TinyLlama where weights are near-zero.
    matmul_i8(weights, input, in_size, out_size)
}

/// Zero-alloc matmul — uses scalar i8×i64 for full precision.
pub fn matmul_fast_preq(weights: &I8Weights, _input_q: &QuantizedInput, input_raw: &[i64], in_size: usize, output: &mut [i64]) {
    // Use scalar i8×i64 path for full input precision.
    // The SIMD i8×i8 path loses too much via double quantization.
    matmul_i8_into(weights, input_raw, in_size, output);
}

#[cfg(target_arch = "aarch64")]
fn matmul_simd_preq_neon(weights: &I8Weights, input_q: &QuantizedInput, in_size: usize, output: &mut [i64]) {
    use std::arch::aarch64::*;
    let data = &weights.data;
    let inp = &input_q.data;
    let scales = &weights.scales;
    let isf = input_q.scale_factor;

    output.par_chunks_mut(512).enumerate().for_each(|(ci, chunk)| {
        let base = ci * 512;
        for (li, out) in chunk.iter_mut().enumerate() {
            let i = base + li;
            let row = unsafe { data.as_ptr().add(i * in_size) };
            let simd_len = in_size / 32 * 32;
            let mut acc: i64;
            unsafe {
                let mut v0 = vdupq_n_s32(0);
                let mut v1 = vdupq_n_s32(0);
                let mut v2 = vdupq_n_s32(0);
                let mut v3 = vdupq_n_s32(0);
                let mut j = 0usize;
                while j < simd_len {
                    let w0 = vld1q_s8(row.add(j));
                    let i0 = vld1q_s8(inp.as_ptr().add(j));
                    v0 = vpadalq_s16(v0, vmull_s8(vget_low_s8(w0), vget_low_s8(i0)));
                    v1 = vpadalq_s16(v1, vmull_s8(vget_high_s8(w0), vget_high_s8(i0)));
                    let w1 = vld1q_s8(row.add(j + 16));
                    let i1 = vld1q_s8(inp.as_ptr().add(j + 16));
                    v2 = vpadalq_s16(v2, vmull_s8(vget_low_s8(w1), vget_low_s8(i1)));
                    v3 = vpadalq_s16(v3, vmull_s8(vget_high_s8(w1), vget_high_s8(i1)));
                    j += 32;
                }
                v0 = vaddq_s32(vaddq_s32(v0, v1), vaddq_s32(v2, v3));
                acc = vaddvq_s32(v0) as i64;
                while j < in_size { acc += (*row.add(j) as i64) * (*inp.as_ptr().add(j) as i64); j += 1; }
            }
            *out = acc * ((scales[i] * isf) >> FRAC_BITS);
        }
    });
}

#[cfg(target_arch = "x86_64")]
fn matmul_simd_preq_x86(weights: &I8Weights, input_q: &QuantizedInput, in_size: usize, output: &mut [i64]) {
    use std::arch::x86_64::*;
    let data = &weights.data;
    let inp = &input_q.data;
    let scales = &weights.scales;
    let isf = input_q.scale_factor;
    let use512 = is_x86_feature_detected!("avx512bw") && is_x86_feature_detected!("avx512f");

    output.par_chunks_mut(512).enumerate().for_each(|(ci, chunk)| {
        let base = ci * 512;
        for (li, out) in chunk.iter_mut().enumerate() {
            let i = base + li;
            let row = unsafe { data.as_ptr().add(i * in_size) };
            let ip = inp.as_ptr();
            let mut acc: i64 = 0;
            unsafe {
                if use512 {
                    let sl = in_size / 64 * 64;
                    let mut a0 = _mm512_setzero_si512();
                    let mut a1 = _mm512_setzero_si512();
                    let mut j = 0usize;
                    while j < sl {
                        let vw = _mm512_loadu_si512(row.add(j) as *const __m512i);
                        let vi = _mm512_loadu_si512(ip.add(j) as *const __m512i);
                        a0 = _mm512_add_epi32(a0, _mm512_madd_epi16(
                            _mm512_cvtepi8_epi16(_mm512_castsi512_si256(vw)),
                            _mm512_cvtepi8_epi16(_mm512_castsi512_si256(vi))));
                        a1 = _mm512_add_epi32(a1, _mm512_madd_epi16(
                            _mm512_cvtepi8_epi16(_mm512_extracti64x4_epi64(vw, 1)),
                            _mm512_cvtepi8_epi16(_mm512_extracti64x4_epi64(vi, 1))));
                        j += 64;
                    }
                    acc = _mm512_reduce_add_epi32(_mm512_add_epi32(a0, a1)) as i64;
                    while j < in_size { acc += (*row.add(j) as i64) * (*ip.add(j) as i64); j += 1; }
                } else {
                    let sl = in_size / 32 * 32;
                    let mut a0 = _mm256_setzero_si256();
                    let mut a1 = _mm256_setzero_si256();
                    let mut j = 0usize;
                    while j < sl {
                        let vw = _mm256_loadu_si256(row.add(j) as *const __m256i);
                        let vi = _mm256_loadu_si256(ip.add(j) as *const __m256i);
                        a0 = _mm256_add_epi32(a0, _mm256_madd_epi16(
                            _mm256_cvtepi8_epi16(_mm256_castsi256_si128(vw)),
                            _mm256_cvtepi8_epi16(_mm256_castsi256_si128(vi))));
                        a1 = _mm256_add_epi32(a1, _mm256_madd_epi16(
                            _mm256_cvtepi8_epi16(_mm256_extracti128_si256(vw, 1)),
                            _mm256_cvtepi8_epi16(_mm256_extracti128_si256(vi, 1))));
                        j += 32;
                    }
                    let v = _mm256_add_epi32(a0, a1);
                    let lo = _mm256_extracti128_si256(v, 0);
                    let hi = _mm256_extracti128_si256(v, 1);
                    let s = _mm_hadd_epi32(_mm_add_epi32(lo, hi), _mm_setzero_si128());
                    let s = _mm_hadd_epi32(s, _mm_setzero_si128());
                    acc = _mm_extract_epi32(s, 0) as i64;
                    while j < in_size { acc += (*row.add(j) as i64) * (*ip.add(j) as i64); j += 1; }
                }
            }
            *out = acc * ((scales[i] * isf) >> FRAC_BITS);
        }
    });
}

// ─── Q4 Weights (4-bit, half bandwidth) ──────────────────────────────────────

/// Q4 weight matrix: 4-bit signed values packed 2 per byte.
/// Byte layout: [hi_nibble(4b) | lo_nibble(4b)], both signed [-8, 7].
/// Buffer is half the size of I8Weights → 2x bandwidth reduction.
pub struct Q4WeightsX86 {
    pub data: Vec<u8>,       // packed Q4 bytes (n_rows × n_cols / 2)
    pub scales: Vec<i64>,    // per-row scale factors (same as I8Weights)
    pub n_rows: usize,
    pub n_cols: usize,
}

impl Q4WeightsX86 {
    /// Total memory in bytes.
    pub fn memory_bytes(&self) -> usize {
        self.data.len() + self.scales.len() * 8
    }

    /// Convert I8Weights to Q4WeightsX86 with per-row scaling.
    /// Each i8 row is rescaled to use the full Q4 range [-8, 7].
    /// Per-row q4_per_unit = ceil(abs_max / 7) ensures minimal clamping loss.
    /// Encoding: bias +8 (stored [0, 15], decode by subtracting 8).
    pub fn from_i8(w: &I8Weights) -> Self {
        let n_rows = w.n_rows;
        let n_cols = w.n_cols;
        let mut data = Vec::with_capacity(w.data.len() / 2);
        let mut scales = Vec::with_capacity(n_rows);

        for i in 0..n_rows {
            let row = &w.data[i * n_cols..(i + 1) * n_cols];

            // Per-row abs_max of i8 values
            let abs_max = row.iter().map(|&x| (x as i16).abs() as u8).max().unwrap_or(1).max(1);
            // How many i8 units per Q4 step: ceil(abs_max / 7)
            let q4_per_unit = ((abs_max as i64 + 6) / 7).max(1);

            for pair in row.chunks(2) {
                // Divide by q4_per_unit to use full Q4 range, then clamp
                let v0 = ((pair[0] as i16) / q4_per_unit as i16).clamp(-8, 7);
                let v1 = if pair.len() > 1 {
                    ((pair[1] as i16) / q4_per_unit as i16).clamp(-8, 7)
                } else { 0 };
                // Bias encoding: store value + 8 in nibble [0, 15]
                let lo = ((v0 + 8) as u8) & 0x0F;
                let hi = ((v1 + 8) as u8) & 0x0F;
                data.push(lo | (hi << 4));
            }

            // Combined scale: original i8 scale × q4_per_unit
            scales.push(w.scales[i] * q4_per_unit);
        }

        Q4WeightsX86 { data, scales, n_rows, n_cols }
    }
}

/// Q4×Q8 matmul with pre-quantized input. AVX2/AVX-512 + sign trick.
/// Reads HALF the weight data of matmul_i8xi8 → 2x bandwidth improvement.
#[cfg(target_arch = "x86_64")]
pub fn matmul_q4_preq_x86(q4: &Q4WeightsX86, input_q: &QuantizedInput, output: &mut [i64]) {
    use std::arch::x86_64::*;

    if !is_x86_feature_detected!("avx2") {
        // Scalar fallback
        matmul_q4_scalar(q4, input_q, output);
        return;
    }

    let in_size = q4.n_cols;
    let byte_cols = in_size / 2;
    let data = &q4.data;
    let inp = &input_q.data;
    let scales = &q4.scales;
    let isf = input_q.scale_factor;

    output.par_chunks_mut(64).enumerate().for_each(|(ci, chunk)| {
        let base = ci * 64;
        for (li, out) in chunk.iter_mut().enumerate() {
            let i = base + li;
            let row = unsafe { data.as_ptr().add(i * byte_cols) };
            let ip = inp.as_ptr();
            let mut acc: i64 = 0;

            unsafe {
                // AVX2: process 16 Q4 bytes (32 values) per iteration
                // Unpack nibbles → sign-extend → multiply with Q8 input via sign trick
                let simd_len = byte_cols / 64 * 64; // 4×16 per iteration for ILP
                let mask_lo = _mm_set1_epi8(0x0F);
                let bias = _mm256_set1_epi8(8);
                let ones = _mm256_set1_epi16(1);
                let mut vacc0 = _mm256_setzero_si256();
                let mut vacc1 = _mm256_setzero_si256();
                let mut vacc2 = _mm256_setzero_si256();
                let mut vacc3 = _mm256_setzero_si256();

                let mut j = 0usize;
                while j < simd_len {
                    _mm_prefetch(row.add(j + 128) as *const i8, _MM_HINT_T0);
                    _mm_prefetch(ip.add(j * 2 + 256) as *const i8, _MM_HINT_T0);

                    // Block 0: 16 Q4 bytes → 32 i8 weights × 32 i8 input
                    let packed0 = _mm_loadu_si128(row.add(j) as *const __m128i);
                    let lo0 = _mm_and_si128(packed0, mask_lo);
                    let hi0 = _mm_and_si128(_mm_srli_epi16(packed0, 4), mask_lo);
                    let interleaved0 = _mm256_sub_epi8(
                        _mm256_set_m128i(_mm_unpackhi_epi8(lo0, hi0), _mm_unpacklo_epi8(lo0, hi0)),
                        bias);
                    let vi0 = _mm256_loadu_si256(ip.add(j * 2) as *const __m256i);
                    let ax0 = _mm256_sign_epi8(interleaved0, interleaved0);
                    let sy0 = _mm256_sign_epi8(vi0, interleaved0);
                    vacc0 = _mm256_add_epi32(vacc0, _mm256_madd_epi16(_mm256_maddubs_epi16(ax0, sy0), ones));

                    // Block 1
                    let packed1 = _mm_loadu_si128(row.add(j + 16) as *const __m128i);
                    let lo1 = _mm_and_si128(packed1, mask_lo);
                    let hi1 = _mm_and_si128(_mm_srli_epi16(packed1, 4), mask_lo);
                    let interleaved1 = _mm256_sub_epi8(
                        _mm256_set_m128i(_mm_unpackhi_epi8(lo1, hi1), _mm_unpacklo_epi8(lo1, hi1)),
                        bias);
                    let vi1 = _mm256_loadu_si256(ip.add(j * 2 + 32) as *const __m256i);
                    let ax1 = _mm256_sign_epi8(interleaved1, interleaved1);
                    let sy1 = _mm256_sign_epi8(vi1, interleaved1);
                    vacc1 = _mm256_add_epi32(vacc1, _mm256_madd_epi16(_mm256_maddubs_epi16(ax1, sy1), ones));

                    // Block 2
                    let packed2 = _mm_loadu_si128(row.add(j + 32) as *const __m128i);
                    let lo2 = _mm_and_si128(packed2, mask_lo);
                    let hi2 = _mm_and_si128(_mm_srli_epi16(packed2, 4), mask_lo);
                    let interleaved2 = _mm256_sub_epi8(
                        _mm256_set_m128i(_mm_unpackhi_epi8(lo2, hi2), _mm_unpacklo_epi8(lo2, hi2)),
                        bias);
                    let vi2 = _mm256_loadu_si256(ip.add(j * 2 + 64) as *const __m256i);
                    let ax2 = _mm256_sign_epi8(interleaved2, interleaved2);
                    let sy2 = _mm256_sign_epi8(vi2, interleaved2);
                    vacc2 = _mm256_add_epi32(vacc2, _mm256_madd_epi16(_mm256_maddubs_epi16(ax2, sy2), ones));

                    // Block 3
                    let packed3 = _mm_loadu_si128(row.add(j + 48) as *const __m128i);
                    let lo3 = _mm_and_si128(packed3, mask_lo);
                    let hi3 = _mm_and_si128(_mm_srli_epi16(packed3, 4), mask_lo);
                    let interleaved3 = _mm256_sub_epi8(
                        _mm256_set_m128i(_mm_unpackhi_epi8(lo3, hi3), _mm_unpacklo_epi8(lo3, hi3)),
                        bias);
                    let vi3 = _mm256_loadu_si256(ip.add(j * 2 + 96) as *const __m256i);
                    let ax3 = _mm256_sign_epi8(interleaved3, interleaved3);
                    let sy3 = _mm256_sign_epi8(vi3, interleaved3);
                    vacc3 = _mm256_add_epi32(vacc3, _mm256_madd_epi16(_mm256_maddubs_epi16(ax3, sy3), ones));

                    j += 64;
                }

                vacc0 = _mm256_add_epi32(_mm256_add_epi32(vacc0, vacc1),
                                         _mm256_add_epi32(vacc2, vacc3));

                // 16-byte remainder
                while j + 16 <= byte_cols {
                    let packed = _mm_loadu_si128(row.add(j) as *const __m128i);
                    let lo = _mm_and_si128(packed, mask_lo);
                    let hi = _mm_and_si128(_mm_srli_epi16(packed, 4), mask_lo);
                    let interleaved = _mm256_sub_epi8(
                        _mm256_set_m128i(_mm_unpackhi_epi8(lo, hi), _mm_unpacklo_epi8(lo, hi)),
                        bias);
                    let vi = _mm256_loadu_si256(ip.add(j * 2) as *const __m256i);
                    let ax = _mm256_sign_epi8(interleaved, interleaved);
                    let sy = _mm256_sign_epi8(vi, interleaved);
                    vacc0 = _mm256_add_epi32(vacc0, _mm256_madd_epi16(_mm256_maddubs_epi16(ax, sy), ones));
                    j += 16;
                }

                let lo128 = _mm256_extracti128_si256(vacc0, 0);
                let hi128 = _mm256_extracti128_si256(vacc0, 1);
                let sum128 = _mm_add_epi32(lo128, hi128);
                let sum128 = _mm_hadd_epi32(sum128, sum128);
                let sum128 = _mm_hadd_epi32(sum128, sum128);
                acc = _mm_extract_epi32(sum128, 0) as i64;

                // Scalar remainder
                while j < byte_cols {
                    let byte = *row.add(j);
                    let w_lo = (byte & 0x0F) as i8 - 8;
                    let w_hi = ((byte >> 4) & 0x0F) as i8 - 8;
                    acc += (w_lo as i64) * (*ip.add(j * 2) as i64)
                         + (w_hi as i64) * (*ip.add(j * 2 + 1) as i64);
                    j += 1;
                }
            }
            *out = acc * ((scales[i] * isf) >> FRAC_BITS);
        }
    });
}

/// Q4×Q8 matmul with NEON SIMD. Same algorithm as x86 AVX2 but uses
/// NEON nibble extraction: vand + vshr for low/high, vsub for bias-8.
/// Processes 16 packed bytes (32 Q4 values) per iteration.
#[cfg(target_arch = "aarch64")]
pub fn matmul_q4_preq_neon(q4: &Q4WeightsX86, input_q: &QuantizedInput, output: &mut [i64]) {
    use std::arch::aarch64::*;

    let in_size = q4.n_cols;
    let byte_cols = in_size / 2;
    let data = &q4.data;
    let inp = &input_q.data;
    let scales = &q4.scales;
    let isf = input_q.scale_factor;

    output.par_chunks_mut(512).enumerate().for_each(|(ci, chunk)| {
        let base = ci * 512;
        for (li, out) in chunk.iter_mut().enumerate() {
            let i = base + li;
            let row_off = i * byte_cols;
            let mut acc: i64;

            unsafe {
                let simd_len = byte_cols / 16 * 16; // 16 bytes = 32 Q4 values
                let bias = vdupq_n_s8(8);
                let mask_lo = vdupq_n_u8(0x0F);
                let mut vacc0 = vdupq_n_s32(0);
                let mut vacc1 = vdupq_n_s32(0);
                let mut vacc2 = vdupq_n_s32(0);
                let mut vacc3 = vdupq_n_s32(0);

                let mut j = 0usize;
                while j < simd_len {
                    // Load 16 packed Q4 bytes = 32 weight values
                    let packed = vld1q_u8(data.as_ptr().add(row_off + j));

                    // Extract low nibbles [0,15] and high nibbles [0,15]
                    let lo = vreinterpretq_s8_u8(vandq_u8(packed, mask_lo));
                    let hi = vreinterpretq_s8_u8(vshrq_n_u8(packed, 4));

                    // Subtract bias 8 → signed [-8, 7]
                    let q_lo = vsubq_s8(lo, bias);
                    let q_hi = vsubq_s8(hi, bias);

                    // Load 32 input i8 values (lo input for lo nibbles, hi for hi)
                    // Layout: lo nibble = even cols, hi nibble = odd cols
                    // Need to interleave: input[j*2], input[j*2+1], input[j*2+2], ...
                    // lo[k] pairs with input[j*2 + k*2], hi[k] with input[j*2 + k*2 + 1]
                    // But NEON zip can interleave: q_lo[0],q_hi[0],q_lo[1],q_hi[1],...
                    // to match sequential input layout

                    // Interleave weights to match sequential input
                    let wlo_lo = vget_low_s8(q_lo);   // 8 low nibbles (even cols)
                    let whi_lo = vget_low_s8(q_hi);   // 8 high nibbles (odd cols)
                    let wlo_hi = vget_high_s8(q_lo);
                    let whi_hi = vget_high_s8(q_hi);

                    let w_interleaved_0 = vzip1q_s8(
                        vcombine_s8(wlo_lo, wlo_hi),
                        vcombine_s8(whi_lo, whi_hi));
                    let w_interleaved_1 = vzip2q_s8(
                        vcombine_s8(wlo_lo, wlo_hi),
                        vcombine_s8(whi_lo, whi_hi));

                    let i0 = vld1q_s8(inp.as_ptr().add(j * 2));
                    let i1 = vld1q_s8(inp.as_ptr().add(j * 2 + 16));

                    // i8×i8 → i16 → pairwise add to i32
                    vacc0 = vpadalq_s16(vacc0, vmull_s8(vget_low_s8(w_interleaved_0), vget_low_s8(i0)));
                    vacc1 = vpadalq_s16(vacc1, vmull_s8(vget_high_s8(w_interleaved_0), vget_high_s8(i0)));
                    vacc2 = vpadalq_s16(vacc2, vmull_s8(vget_low_s8(w_interleaved_1), vget_low_s8(i1)));
                    vacc3 = vpadalq_s16(vacc3, vmull_s8(vget_high_s8(w_interleaved_1), vget_high_s8(i1)));

                    j += 16;
                }

                vacc0 = vaddq_s32(vaddq_s32(vacc0, vacc1), vaddq_s32(vacc2, vacc3));
                acc = vaddvq_s32(vacc0) as i64;

                // Scalar remainder
                while j < byte_cols {
                    let byte = data[row_off + j];
                    let w_lo = (byte & 0x0F) as i8 - 8;
                    let w_hi = ((byte >> 4) & 0x0F) as i8 - 8;
                    acc += (w_lo as i64) * (inp[j * 2] as i64)
                         + (w_hi as i64) * (inp[j * 2 + 1] as i64);
                    j += 1;
                }
            }
            *out = acc * ((scales[i] * isf) >> FRAC_BITS);
        }
    });
}

fn matmul_q4_scalar(q4: &Q4WeightsX86, input_q: &QuantizedInput, output: &mut [i64]) {
    let byte_cols = q4.n_cols / 2;
    let data = &q4.data;
    let inp = &input_q.data;
    let scales = &q4.scales;
    let isf = input_q.scale_factor;

    for (i, out) in output.iter_mut().enumerate() {
        let mut acc: i64 = 0;
        let row_off = i * byte_cols;
        for j in 0..byte_cols {
            let byte = data[row_off + j];
            let w_lo = (byte & 0x0F) as i8 - 8;
            let w_hi = ((byte >> 4) & 0x0F) as i8 - 8;
            acc += (w_lo as i64) * (inp[j * 2] as i64)
                 + (w_hi as i64) * (inp[j * 2 + 1] as i64);
        }
        *out = acc * ((scales[i] * isf) >> FRAC_BITS);
    }
}

// ─── Helpers ──────────────────────────────────────────────────────────────────

/// RMSNorm (what Llama uses — no mean subtraction, just root-mean-square).
/// output[i] = (input[i] / rms) * gamma[i]
/// where rms = sqrt(mean(x²))
fn layernorm(input: &[i64], gamma: &[i64]) -> Vec<i64> {
    let n = input.len() as i64;
    if n == 0 { return vec![]; }
    // RMSNorm: compute mean of squares (NOT variance around mean)
    let mut sq_sum: i64 = 0;
    for &x in input {
        sq_sum += (x * x) >> FRAC_BITS;
    }
    let mean_sq = sq_sum / n;
    let inv_rms = integer_isqrt(mean_sq + 1); // 1/sqrt(mean_sq) in Q16
    input.iter().enumerate().map(|(i, &x)| {
        let norm = (x * inv_rms) >> FRAC_BITS;
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

/// SiLU(x) = x * sigmoid(x) = x / (1 + exp(-x))
/// Uses the integer exp LUT for sigmoid computation.
fn silu_i64(x: i64) -> i64 {
    // sigmoid(x) = 1 / (1 + exp(-x))
    let sig = if x >= 0 {
        let exp_neg = integer_exp(-x);
        (ONE * ONE) / (ONE + exp_neg).max(1)
    } else {
        let exp_pos = integer_exp(x);
        (exp_pos * ONE) / (ONE + exp_pos).max(1)
    };
    // SiLU = x * sigmoid(x)
    (x * sig) >> FRAC_BITS
}

// ─── Fused LayerNorm + Projection ─────────────────────────────────────────────

/// Compute layernorm stats (mean, inv_std) without materializing the normed vector.
#[inline]
fn layernorm_stats(input: &[i64]) -> (i64, i64) {
    let n = input.len() as i64;
    let mean = input.iter().sum::<i64>() / n;
    let mut var_sum: i64 = 0;
    for &x in input {
        let d = x - mean;
        var_sum += (d * d) >> FRAC_BITS;
    }
    let inv_std = integer_isqrt(var_sum / n + 1);
    (mean, inv_std)
}

/// Fused layernorm + i8 matmul projection.
/// Computes: output = matmul(weights, layernorm(input, gamma))
/// Without allocating the intermediate normed vector.
/// One pass over input to compute stats, then stream through weight rows.
fn fused_layernorm_matmul(
    input: &[i64],
    gamma: &[i64],
    weights: &I8Weights,
    in_size: usize,
    out_size: usize,
) -> Vec<i64> {
    let (mean, inv_std) = layernorm_stats(input);
    let scales = &weights.scales;
    let data = &weights.data;

    let mut output = vec![0i64; out_size];
    output.par_chunks_mut(64).enumerate().for_each(|(chunk_idx, chunk)| {
        let start = chunk_idx * 64;
        for (local_i, out) in chunk.iter_mut().enumerate() {
            let i = start + local_i;
            let row = &data[i * in_size..(i + 1) * in_size];
            let mut acc: i64 = 0;
            // Fused: for each j, compute normed[j] on-the-fly and multiply
            for j in 0..in_size {
                let norm = ((input[j] - mean) * inv_std) >> FRAC_BITS;
                let g = if j < gamma.len() { gamma[j] } else { ONE };
                let normed_j = (norm * g) >> FRAC_BITS;
                acc += (row[j] as i64) * normed_j;
            }
            *out = (acc * scales[i]) >> FRAC_BITS;
        }
    });
    output
}

// ─── SIMD Attention KV Dot Products ──────────────────────────────────────────

/// Quantize a Q16 i64 vector to i8 for SIMD dot products.
/// Returns (i8 data, scale factor).
#[inline]
fn quantize_for_dot(v: &[i64]) -> (Vec<i8>, i64) {
    let abs_max = v.iter().map(|x| x.abs()).max().unwrap_or(1).max(1);
    let sf = (abs_max / 127).max(1);
    let data: Vec<i8> = v.iter().map(|&x| (x / sf).clamp(-127, 127) as i8).collect();
    (data, sf)
}

/// SIMD i8×i8 dot product for attention K scores.
/// q_i8: query head quantized to i8, k_ptr: pointer into KV cache i8 data.
/// Returns i32 dot product (caller applies scales).
#[cfg(target_arch = "aarch64")]
#[inline]
unsafe fn dot_i8_kv_neon(q_i8: *const i8, k_ptr: *const i8, d_head: usize) -> i32 {
    use std::arch::aarch64::*;
    unsafe {
        let simd_len = d_head / 32 * 32;
        let mut vacc0 = vdupq_n_s32(0);
        let mut vacc1 = vdupq_n_s32(0);
        let mut vacc2 = vdupq_n_s32(0);
        let mut vacc3 = vdupq_n_s32(0);
        let mut j = 0usize;
        while j < simd_len {
            let vq0 = vld1q_s8(q_i8.add(j));
            let vk0 = vld1q_s8(k_ptr.add(j));
            vacc0 = vpadalq_s16(vacc0, vmull_s8(vget_low_s8(vq0), vget_low_s8(vk0)));
            vacc1 = vpadalq_s16(vacc1, vmull_s8(vget_high_s8(vq0), vget_high_s8(vk0)));
            let vq1 = vld1q_s8(q_i8.add(j + 16));
            let vk1 = vld1q_s8(k_ptr.add(j + 16));
            vacc2 = vpadalq_s16(vacc2, vmull_s8(vget_low_s8(vq1), vget_low_s8(vk1)));
            vacc3 = vpadalq_s16(vacc3, vmull_s8(vget_high_s8(vq1), vget_high_s8(vk1)));
            j += 32;
        }
        vacc0 = vaddq_s32(vaddq_s32(vacc0, vacc1), vaddq_s32(vacc2, vacc3));
        let mut acc = vaddvq_s32(vacc0);
        while j < d_head {
            acc += (*q_i8.add(j) as i32) * (*k_ptr.add(j) as i32);
            j += 1;
        }
        acc
    }
}

/// AVX2 i8×i8 dot for attention K scores with sign trick.
#[cfg(target_arch = "x86_64")]
#[inline]
unsafe fn dot_i8_kv_avx2(q_i8: *const i8, k_ptr: *const i8, d_head: usize) -> i32 {
    use std::arch::x86_64::*;
    if !is_x86_feature_detected!("avx2") {
        let mut acc: i32 = 0;
        for j in 0..d_head {
            acc += (*q_i8.add(j) as i32) * (*k_ptr.add(j) as i32);
        }
        return acc;
    }
    let simd_len = d_head / 32 * 32;
    let ones = _mm256_set1_epi16(1);
    let mut vacc = _mm256_setzero_si256();
    let mut j = 0usize;
    while j < simd_len {
        let vq = _mm256_loadu_si256(q_i8.add(j) as *const __m256i);
        let vk = _mm256_loadu_si256(k_ptr.add(j) as *const __m256i);
        let ax = _mm256_sign_epi8(vq, vq);
        let sy = _mm256_sign_epi8(vk, vq);
        vacc = _mm256_add_epi32(vacc, _mm256_madd_epi16(_mm256_maddubs_epi16(ax, sy), ones));
        j += 32;
    }
    let lo = _mm256_extracti128_si256(vacc, 0);
    let hi = _mm256_extracti128_si256(vacc, 1);
    let sum128 = _mm_hadd_epi32(_mm_add_epi32(lo, hi), _mm_setzero_si128());
    let sum128 = _mm_hadd_epi32(sum128, _mm_setzero_si128());
    let mut acc = _mm_extract_epi32(sum128, 0);
    while j < d_head {
        acc += (*q_i8.add(j) as i32) * (*k_ptr.add(j) as i32);
        j += 1;
    }
    acc
}

/// Cross-platform SIMD dot product dispatch for attention.
#[inline]
fn dot_i8_kv(q_i8: &[i8], k_ptr: &[i8], k_offset: usize, d_head: usize) -> i32 {
    #[cfg(target_arch = "aarch64")]
    { unsafe { dot_i8_kv_neon(q_i8.as_ptr(), k_ptr.as_ptr().add(k_offset), d_head) } }
    #[cfg(target_arch = "x86_64")]
    { unsafe { dot_i8_kv_avx2(q_i8.as_ptr(), k_ptr.as_ptr().add(k_offset), d_head) } }
    #[cfg(not(any(target_arch = "aarch64", target_arch = "x86_64")))]
    {
        let mut acc: i32 = 0;
        for dd in 0..d_head {
            acc += (q_i8[dd] as i32) * (k_ptr[k_offset + dd] as i32);
        }
        acc
    }
}

// ─── Flash Attention (Online Softmax, O(1) Memory) ───────────────────────────

/// Flash attention for a single query head against i8-quantized KV cache.
/// Uses online softmax: processes KV in streaming fashion, never allocates O(n²).
/// Numerically equivalent to standard attention (within integer rounding).
///
/// q_head: [d_head] i64 Q16 — the query for this head at current position
/// k_data: flat i8 array of all cached K for this layer
/// k_scales: per-position scales for K
/// v_data: flat i8 array of all cached V
/// v_scales: per-position scales for V
/// d_kv: total d_kv dimension (d_head * n_kv_heads)
/// kv_h: which KV head to use
/// d_head: dimension per head
/// full_seq: number of positions in cache
/// attn_scale: 1/sqrt(d_head) in Q16
fn flash_attention_i8(
    q_head: &[i64],
    k_data: &[i8], k_scales: &[i64],
    v_data: &[i8], v_scales: &[i64],
    d_kv: usize, kv_h: usize, d_head: usize,
    full_seq: usize, attn_scale: i64,
) -> Vec<i64> {
    // Online softmax: maintain running max, sum of exp, and weighted V sum.
    // Process one position at a time — O(1) extra memory (no scores array).
    let mut running_max: i64 = -8 * ONE; // start very negative
    let mut running_sum: i64 = 0;        // sum of exp(score - max)
    let mut out = vec![0i64; d_head];     // weighted V accumulator

    // Quantize Q to i8 ONCE for SIMD dot products across all positions
    let (q_i8, q_sf) = quantize_for_dot(q_head);

    for j in 0..full_seq {
        let k_off = j * d_kv + kv_h * d_head;
        let k_scale = k_scales[j];

        // SIMD dot product: Q_i8 · K_i8 (both already quantized)
        let dot_i32 = dot_i8_kv(&q_i8, k_data, k_off, d_head);
        let dot = (dot_i32 as i64) * q_sf * k_scale;
        let score = (dot >> (FRAC_BITS * 2)) * attn_scale >> FRAC_BITS;

        // Online softmax update
        if score > running_max {
            // New max — rescale existing accumulator
            let diff = running_max - score; // negative
            let correction = integer_exp(diff); // exp(old_max - new_max) < 1
            // Scale down existing sum and output
            running_sum = (running_sum * correction) >> FRAC_BITS;
            for dd in 0..d_head {
                out[dd] = (out[dd] * correction) >> FRAC_BITS;
            }
            running_max = score;
        }

        // exp(score - running_max)
        let w = integer_exp(score - running_max);
        running_sum += w;

        // Accumulate weighted V (dequantized on-the-fly)
        let v_off = j * d_kv + kv_h * d_head;
        let v_scale = v_scales[j];
        for dd in 0..d_head {
            let v_val = (v_data[v_off + dd] as i64) * v_scale;
            out[dd] += (w * v_val) >> FRAC_BITS;
        }
    }

    // Normalize by sum
    if running_sum > 0 {
        for dd in 0..d_head {
            out[dd] = (out[dd] * ONE) / running_sum;
        }
    }

    out
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
        let mut total = self.embedding_i8.memory_bytes() + self.embedding_q16.len() * 8
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

    /// Forward pass — zero-alloc matmuls with cached input quantization.
    /// Quantize input ONCE, reuse for Q/K/V (3 matmuls) and gate/up (2 matmuls).
    /// Saves 4 input quantizations per layer × 32 layers = 128 saved quantizations.
    /// Uses pre-allocated buffers (q/k/v/attn_out/gate/up/gated/ff_out).
    /// When Q4 weights are enabled (via enable_q4), uses 4-bit matmul on x86_64.
    pub fn forward_one_token(&self, token: u32, cache: &mut KVCache) -> Vec<i64> {
        let cfg = &self.config;
        let d = cfg.d_model;
        let pos = cache.seq_len;

        // Helper macro: dispatch to Q4 matmul on x86_64 when available, else I8.
        // $q4w: Option<&Q4WeightsX86>, $i8w: &I8Weights, $inq: &QuantizedInput,
        // $raw: &[i64], $in_sz: input dim, $out: &mut [i64]
        macro_rules! dispatch_matmul {
            ($q4w:expr, $i8w:expr, $inq:expr, $raw:expr, $in_sz:expr, $out:expr) => {
                {
                    #[cfg(target_arch = "x86_64")]
                    {
                        if let Some(q4w) = $q4w {
                            matmul_q4_preq_x86(q4w, $inq, $out);
                        } else {
                            matmul_fast_preq($i8w, $inq, $raw, $in_sz, $out);
                        }
                    }
                    #[cfg(target_arch = "aarch64")]
                    {
                        if let Some(q4w) = $q4w {
                            matmul_q4_preq_neon(q4w, $inq, $out);
                        } else {
                            matmul_fast_preq($i8w, $inq, $raw, $in_sz, $out);
                        }
                    }
                    #[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
                    {
                        if let Some(q4w) = $q4w {
                            matmul_q4_scalar(q4w, $inq, $out);
                        } else {
                            matmul_fast_preq($i8w, $inq, $raw, $in_sz, $out);
                        }
                    }
                }
            };
        }

        // Embed — use full Q16 precision (INT8 destroys tiny embedding values)
        let idx = (token as usize).min(cfg.vocab_size - 1);
        let emb_start = idx * d;
        let mut hidden: Vec<i64> = self.embedding_q16[emb_start..emb_start + d].to_vec();

        // Pre-allocate buffers (reused across layers)
        let mut q = vec![0i64; d];
        let mut k_buf = vec![0i64; cfg.d_kv];
        let mut v_buf = vec![0i64; cfg.d_kv];
        let mut attn_out = vec![0i64; d];
        let mut projected = vec![0i64; d];
        let mut gate = vec![0i64; cfg.d_ff];
        let mut up = vec![0i64; cfg.d_ff];
        let mut ff_out = vec![0i64; d];

        for (layer_idx, layer) in self.layers.iter().enumerate() {
            // Get Q4 layer ref if available
            let q4_layer = self.q4_layers.as_ref().map(|ql| &ql[layer_idx]);

            // LayerNorm once — result fits in L1 (32KB)
            let normed = layernorm(&hidden, &layer.attn_norm);

            // Quantize normed input ONCE, reuse for Q, K, V projections
            let normed_q = QuantizedInput::from_i64(&normed);

            // Q/K/V with zero-alloc + cached quantized input (Q4 or I8)
            dispatch_matmul!(q4_layer.map(|l| &l.wq), &layer.wq, &normed_q, &normed, d, &mut q);
            dispatch_matmul!(q4_layer.map(|l| &l.wk), &layer.wk, &normed_q, &normed, d, &mut k_buf);
            dispatch_matmul!(q4_layer.map(|l| &l.wv), &layer.wv, &normed_q, &normed, d, &mut v_buf);

            // RoPE
            for h in 0..cfg.n_heads {
                apply_rope(&mut q[h * cfg.d_head..(h + 1) * cfg.d_head],
                    pos, cfg.d_head, &cfg.rope_cos, &cfg.rope_sin);
            }
            for h in 0..cfg.n_kv_heads {
                apply_rope(&mut k_buf[h * cfg.d_head..(h + 1) * cfg.d_head],
                    pos, cfg.d_head, &cfg.rope_cos, &cfg.rope_sin);
            }

            // Store K/V in i8 cache
            cache.push_k(layer_idx, &k_buf);
            cache.push_v(layer_idx, &v_buf);

            // Attention with i8 KV cache (full precision dot products)
            let full_seq = pos + 1;
            let head_results: Vec<Vec<i64>> = (0..cfg.n_heads).into_par_iter().map(|h| {
                let kv_h = h * cfg.n_kv_heads / cfg.n_heads;
                let dh = cfg.d_head;
                let q_head = &q[h * dh..(h + 1) * dh];

                let mut scores = Vec::with_capacity(full_seq);
                for j in 0..full_seq {
                    let k_off = j * cfg.d_kv + kv_h * dh;
                    let k_scale = cache.k_scales[layer_idx][j];
                    // Full precision: i64 × i8 dot product (no quantization of q_head)
                    let mut dot_raw: i64 = 0;
                    for dd in 0..dh {
                        dot_raw += q_head[dd] * (cache.k_data[layer_idx][k_off + dd] as i64);
                    }
                    let dot = (dot_raw >> FRAC_BITS) * k_scale;
                    scores.push((dot >> FRAC_BITS) * cfg.attn_scale >> FRAC_BITS);
                }

                let attn_weights = softmax_i64(&scores);

                let mut out = vec![0i64; dh];
                for j in 0..full_seq {
                    let v_off = j * cfg.d_kv + kv_h * dh;
                    let v_scale = cache.v_scales[layer_idx][j];
                    let w = attn_weights[j];
                    for dd in 0..dh {
                        out[dd] += (w * ((cache.v_data[layer_idx][v_off + dd] as i64) * v_scale)) >> FRAC_BITS;
                    }
                }
                out
            }).collect();

            for val in attn_out.iter_mut() { *val = 0; }
            for (h, head_out) in head_results.iter().enumerate() {
                attn_out[h * cfg.d_head..(h + 1) * cfg.d_head].copy_from_slice(head_out);
            }

            // Wo projection + residual (zero-alloc)
            let attn_out_q = QuantizedInput::from_i64(&attn_out);
            dispatch_matmul!(q4_layer.map(|l| &l.wo), &layer.wo, &attn_out_q, &attn_out, d, &mut projected);
            for i in 0..d { hidden[i] += projected[i]; }

            // FFN: quantize normed_ff ONCE for gate+up
            let normed_ff = layernorm(&hidden, &layer.ffn_norm);
            let normed_ff_q = QuantizedInput::from_i64(&normed_ff);

            dispatch_matmul!(q4_layer.map(|l| &l.w_gate), &layer.w_gate, &normed_ff_q, &normed_ff, d, &mut gate);
            dispatch_matmul!(q4_layer.map(|l| &l.w_up), &layer.w_up, &normed_ff_q, &normed_ff, d, &mut up);

            // SiLU gate * up (in-place)
            for j in 0..cfg.d_ff {
                gate[j] = (silu_i64(gate[j]) * up[j]) >> FRAC_BITS;
            }

            // W_down + residual
            let gate_q = QuantizedInput::from_i64(&gate);
            dispatch_matmul!(q4_layer.map(|l| &l.w_down), &layer.w_down, &gate_q, &gate, cfg.d_ff, &mut ff_out);
            for i in 0..d { hidden[i] += ff_out[i]; }
        }

        cache.seq_len = pos + 1;
        let normed = layernorm(&hidden, &self.final_norm);

        // LM head: Q4 output path if available
        if let Some(q4_out) = &self.q4_output {
            let normed_q = QuantizedInput::from_i64(&normed);
            let mut logits = vec![0i64; cfg.vocab_size];
            #[cfg(target_arch = "x86_64")]
            { matmul_q4_preq_x86(q4_out, &normed_q, &mut logits); }
            #[cfg(target_arch = "aarch64")]
            { matmul_q4_preq_neon(q4_out, &normed_q, &mut logits); }
            #[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
            { matmul_q4_scalar(q4_out, &normed_q, &mut logits); }
            return logits;
        }
        matmul_fast(&self.output_weight, &normed, d, cfg.vocab_size)
    }

    pub fn generate(&self, prompt: &[u32], max_tokens: u32, eos_tokens: &[u32]) -> (Vec<u32>, Hash256) {
        let mut cache = KVCache::new(self.config.n_layers);
        let mut generated = Vec::new();

        // Prepend BOS token (1) — Llama requires it
        let _ = self.forward_one_token(1, &mut cache);

        for &tok in prompt {
            let _logits = self.forward_one_token(tok, &mut cache);
        }

        for _ in 0..max_tokens {
            let last_token = generated.last().copied()
                .unwrap_or(*prompt.last().unwrap_or(&0));
            let mut logits = self.forward_one_token(last_token, &mut cache);

            // Repetition penalty: penalize recently generated tokens deterministically.
            // This prevents INT8 quantized models from getting stuck in loops.
            // Penalty factor: divide logit by 1.2 (multiply by ONE*5/6) for repeated tokens.
            for &prev_tok in generated.iter().rev().take(64) {
                let idx = prev_tok as usize;
                if idx < logits.len() {
                    if logits[idx] > 0 {
                        logits[idx] = logits[idx] * 5 / 6; // reduce positive logit
                    } else {
                        logits[idx] = logits[idx] * 6 / 5; // increase negative logit (make more negative)
                    }
                }
            }

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

        self.embedding_i8.write_to(&mut f)?;
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
        hash_i8w(&mut hasher, &self.embedding_i8);
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

    let embedding_i8 = extract_i8(&mut reader, &content, "token_embd.weight", vocab_size, d_model)?;
    // Also store embeddings at full Q16 precision — INT8 destroys tiny values (common in 1B models)
    let embedding_q16: Vec<i64> = {
        let f = extract_f32(&mut reader, &content, "token_embd.weight")?;
        f.iter().map(|&x| (x as f64 * ONE as f64).round() as i64).collect()
    };
    info!("Embeddings loaded: {} MB Q16 + {} MB INT8",
        embedding_q16.len() * 8 / (1024 * 1024), embedding_i8.memory_bytes() / (1024 * 1024));

    let output_weight = extract_i8(&mut reader, &content, "output.weight", vocab_size, d_model)
        .unwrap_or_else(|_| I8Weights {
            data: embedding_i8.data.clone(), scales: embedding_i8.scales.clone(),
            n_rows: embedding_i8.n_rows, n_cols: embedding_i8.n_cols,
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
    // 1/sqrt(d_head) in Q16 — integer_isqrt already returns ONE/sqrt(x/ONE)
    let attn_scale = integer_isqrt((d_head as i64) * ONE);

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
        embedding_q16, embedding_i8, layers, final_norm, output_weight, vocab,
        q4_layers: None, q4_output: None,
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

    let embedding_i8 = I8Weights::read_from(&mut f).map_err(|e| InferenceError::Runtime(format!("Emb: {e}")))?;
    // Reconstruct Q16 embeddings from i8 + per-row scale
    let embedding_q16: Vec<i64> = {
        let mut q16 = Vec::with_capacity(embedding_i8.n_rows * embedding_i8.n_cols);
        for i in 0..embedding_i8.n_rows {
            let scale = embedding_i8.scales[i];
            for j in 0..embedding_i8.n_cols {
                q16.push((embedding_i8.data[i * embedding_i8.n_cols + j] as i64) * scale);
            }
        }
        q16
    };
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
        embedding_q16, embedding_i8, layers, final_norm, output_weight, vocab,
        q4_layers: None, q4_output: None,
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

        let embedding_i8 = gen_i8(vs, d);
        // For tests, Q16 embedding = i8 * scale (same as real loading)
        let embedding_q16: Vec<i64> = {
            let mut q16 = Vec::with_capacity(vs * d);
            for i in 0..vs {
                let scale = embedding_i8.scales[i];
                for j in 0..d {
                    q16.push((embedding_i8.data[i * d + j] as i64) * scale);
                }
            }
            q16
        };
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
            embedding_q16, embedding_i8, layers, final_norm: vec![ONE; d], output_weight,
            vocab: (0..vs).map(|i| format!("tok_{}", i)).collect(),
            q4_layers: None, q4_output: None,
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

        let scalar = matmul_i8(&weights, &input, 512, 1024);
        let simd = matmul_i8xi8_simd(&weights, &input, 512, 1024);

        for i in 0..1024 {
            let diff = (scalar[i] - simd[i]).abs();
            let tolerance = scalar[i].abs().max(ONE) / 5;
            assert!(diff < tolerance, "Row {}: scalar={}, simd={}, diff={}", i, scalar[i], simd[i], diff);
        }
    }
}
