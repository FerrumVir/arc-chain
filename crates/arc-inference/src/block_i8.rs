//! Block-wise INT8 quantization - llama.cpp Q8_0 structure with integer scales.
//!
//! Each `BLOCK_SIZE`-weight chunk shares one scale. Scale is stored as i32
//! Q16 fixed-point (the real scale value × ONE) so matmul stays pure-integer
//! and bitwise deterministic across all hardware. No f16 or f32 arithmetic.
//!
//! Quality ceiling: matches llama.cpp's Q8_0 PPL (~5.5 on Llama-2-7B) because
//! the format is structurally identical. Our differentiation is the integer
//! pipeline discipline: no float ops anywhere in the forward path, every
//! matmul, every scale application, every softmax step produces the same
//! bit pattern on ARM/x86/AVX-512/NEON/phone.
//!
//! See `project_i16_ppl_bug.md` for why the previous per-row scheme topped
//! out at PPL 107 - per-row scales couldn't capture the dynamic-range
//! structure of Llama's output/ffn_down tensors. Per-32-weight blocks solve
//! that without giving up integer determinism.

use crate::integer_lut::{FRAC_BITS, ONE};
use rayon::prelude::*;

/// Weights per quantization block. 32 matches GGUF Q8_0.
pub const BLOCK_SIZE: usize = 32;

/// Per-block INT8 weight tensor. Row-major, with each row split into
/// `n_cols / BLOCK_SIZE` blocks, each carrying its own Q16 scale.
pub struct BlockI8Weights {
    /// Flattened i8 weights, `n_rows * n_cols` entries.
    pub data: Vec<i8>,
    /// Per-block scales in Q16 (real_scale * ONE, stored as i32 to halve
    /// the scale-tensor memory vs i64). One entry per block.
    pub scales: Vec<i32>,
    pub n_rows: usize,
    pub n_cols: usize,
}

impl BlockI8Weights {
    /// Quantize an f32 row-major matrix into per-block INT8.
    ///
    /// `n_cols` MUST be a multiple of `BLOCK_SIZE` for now. Llama-family
    /// dimensions (4096, 11008, 32000) all satisfy this.
    pub fn quantize_f32(values: &[f32], n_rows: usize, n_cols: usize) -> Self {
        assert_eq!(values.len(), n_rows * n_cols, "shape mismatch");
        assert_eq!(n_cols % BLOCK_SIZE, 0,
            "n_cols ({n_cols}) must be a multiple of BLOCK_SIZE ({BLOCK_SIZE})");

        let blocks_per_row = n_cols / BLOCK_SIZE;
        let mut data = Vec::with_capacity(n_rows * n_cols);
        let mut scales = Vec::with_capacity(n_rows * blocks_per_row);

        for row_idx in 0..n_rows {
            let row = &values[row_idx * n_cols..(row_idx + 1) * n_cols];
            for block_idx in 0..blocks_per_row {
                let block = &row[block_idx * BLOCK_SIZE..(block_idx + 1) * BLOCK_SIZE];

                // abs_max per block - in f64 to preserve precision for
                // sub-integer values (the trap that broke the old per-row
                // scheme). Floor at 1e-10 so a pure-zero block still
                // produces a valid i16 scale.
                let abs_max = block.iter().map(|x| x.abs()).fold(0.0f32, f32::max).max(1e-10);

                // Quantize the block.
                let inv_abs_max = 127.0 / abs_max;
                for &x in block {
                    data.push((x * inv_abs_max).round().clamp(-127.0, 127.0) as i8);
                }

                // Block scale: (abs_max / 127) * ONE in f64, clamped to i32
                // range so it fits alongside the data without bloating
                // memory. At abs_max=100, scale ≈ 51_600 - comfortably
                // inside i32 max (2.1e9).
                let scale = ((abs_max as f64 / 127.0) * ONE as f64).round();
                let scale = scale.clamp(1.0, i32::MAX as f64) as i32;
                scales.push(scale);
            }
        }

        Self { data, scales, n_rows, n_cols }
    }

    /// Empty placeholder for shard slots this node doesn't hold.
    pub fn empty() -> Self {
        Self { data: Vec::new(), scales: Vec::new(), n_rows: 0, n_cols: 0 }
    }

    /// Memory footprint in bytes.
    pub fn memory_bytes(&self) -> usize {
        self.data.len() + self.scales.len() * 4 + 32
    }

    /// Dequantize a single weight back to f64 - for reference / testing only.
    /// Production math never dequantizes; the matmul consumes blocks directly.
    pub fn dequant(&self, row: usize, col: usize) -> f64 {
        debug_assert!(row < self.n_rows && col < self.n_cols);
        let block_idx = row * (self.n_cols / BLOCK_SIZE) + col / BLOCK_SIZE;
        let scale = self.scales[block_idx] as f64 / ONE as f64;
        self.data[row * self.n_cols + col] as f64 * scale
    }
}

/// Exact 32-wide block dot: `sum_{j=0..32} (w[j] as i64) * x[j]`.
///
/// Portable scalar reference. Pure i64 arithmetic → identical bit pattern on
/// every platform (two's-complement multiply/add is architecture-independent).
/// This is the determinism ground truth the NEON path is validated against.
#[cfg(not(target_arch = "aarch64"))]
#[inline(always)]
fn block_dot32(w: &[i8], x: &[i64]) -> i64 {
    let mut acc: i64 = 0;
    for j in 0..BLOCK_SIZE {
        acc += (w[j] as i64) * x[j];
    }
    acc
}

/// NEON 32-wide block dot. Widens each i8 weight to i32 and narrows the i64
/// Q16 input to i32, then accumulates i32×i32→i64 via `vmlal_s32`.
///
/// Bit-identical to the scalar `block_dot32` reference whenever `|x| < 2^31`
/// — the exact invariant the shipping `dot_i16_i64_neon` already relies on for
/// cross-platform determinism (Q16 hidden states peak around 2^28). Under that
/// invariant `(x as i32) as i64 == x`, so every lane product matches the
/// scalar i64 product, and i64 addition is associative so the lane-wise
/// partial sums recombine to the identical total regardless of grouping.
/// A unit test (`test_block_i8_neon_matches_scalar_reference`) pins this.
#[cfg(target_arch = "aarch64")]
#[inline(always)]
unsafe fn block_dot32_neon(w: *const i8, x: *const i64) -> i64 {
    use std::arch::aarch64::*;
    let mut acc = vdupq_n_s64(0);
    let mut k = 0usize;
    while k < BLOCK_SIZE {
        let w8 = vld1_s8(w.add(k));                     // 8× i8
        let w16 = vmovl_s8(w8);                         // 8× i16
        let w32_lo = vmovl_s16(vget_low_s16(w16));      // 4× i32
        let w32_hi = vmovl_s16(vget_high_s16(w16));     // 4× i32
        let x0 = vld1q_s64(x.add(k));
        let x1 = vld1q_s64(x.add(k + 2));
        let x2 = vld1q_s64(x.add(k + 4));
        let x3 = vld1q_s64(x.add(k + 6));
        let x32_lo = vcombine_s32(vmovn_s64(x0), vmovn_s64(x1)); // 4× i32 (truncate)
        let x32_hi = vcombine_s32(vmovn_s64(x2), vmovn_s64(x3)); // 4× i32 (truncate)
        acc = vmlal_s32(acc, vget_low_s32(w32_lo), vget_low_s32(x32_lo));
        acc = vmlal_high_s32(acc, w32_lo, x32_lo);
        acc = vmlal_s32(acc, vget_low_s32(w32_hi), vget_low_s32(x32_hi));
        acc = vmlal_high_s32(acc, w32_hi, x32_hi);
        k += 8;
    }
    vgetq_lane_s64(acc, 0) + vgetq_lane_s64(acc, 1)
}

/// Block-wise INT8 × i64(Q16) matmul, writing results in Q16 into `output`.
///
/// Per-block math:
///   block_dot = sum_{j in block} (weight_i8[j] × input_q16[j])
///   block_contribution = (block_dot × scale_q16) >> FRAC_BITS
///   out_row = sum_of_all_block_contributions_for_row
///
/// The scale is i32 Q16, so `block_dot × scale` fits in i128 (block_dot peaks
/// at 127 × 2^28 × 32 = 2^39, × i32 scale = 2^71, inside i128's 2^127 budget).
///
/// Integer-only - no f32 anywhere. Produces bit-identical output on any
/// platform where i128 arithmetic is supported (all modern CPUs, GPUs via
/// emulation, and phones).
///
/// PARALLELISM: rows are distributed across rayon workers with
/// `par_chunks_mut`. Every output row is an independent reduction whose block
/// order (0,1,2,…) and within-block summation are preserved exactly, so the
/// result is bit-identical to the serial version for any thread count — the
/// parallel split touches only which core computes which row, never the
/// arithmetic order inside a row.
pub fn matmul_block_i8_into(
    weights: &BlockI8Weights,
    input: &[i64],
    output: &mut [i64],
) {
    let n_rows = weights.n_rows;
    let n_cols = weights.n_cols;
    // Empty-weight guard FIRST: shard placeholders carry a zero-sized weight
    // but callers still pass a full-width input/output, so asserting shapes
    // before this check would panic on every non-held layer. Zero-fill to
    // match matmul_i8_into / matmul_i16_into behaviour.
    if n_rows == 0 || n_cols == 0 {
        for o in output.iter_mut() { *o = 0; }
        return;
    }
    assert_eq!(input.len(), n_cols, "input width mismatch");
    assert_eq!(output.len(), n_rows, "output row count mismatch");

    let blocks_per_row = n_cols / BLOCK_SIZE;
    let data = &weights.data;
    let scales = &weights.scales;

    // Chunk 256 matches matmul_i16_into's tuned rayon granularity on M2:
    // enough tasks to saturate all cores on 2048/5632/32000-wide outputs
    // without per-task scheduling cost dominating.
    output.par_chunks_mut(256).enumerate().for_each(|(chunk_idx, chunk)| {
        let base = chunk_idx * 256;
        for (local_i, out) in chunk.iter_mut().enumerate() {
            let row_idx = base + local_i;
            let row_off = row_idx * n_cols;
            let row_data = &data[row_off..row_off + n_cols];
            let row_scales = &scales[row_idx * blocks_per_row
                ..(row_idx + 1) * blocks_per_row];

            let mut acc: i128 = 0;
            for block_idx in 0..blocks_per_row {
                let bs = block_idx * BLOCK_SIZE;

                #[cfg(target_arch = "aarch64")]
                let block_dot = unsafe {
                    block_dot32_neon(row_data.as_ptr().add(bs), input.as_ptr().add(bs))
                };
                #[cfg(not(target_arch = "aarch64"))]
                let block_dot = block_dot32(
                    &row_data[bs..bs + BLOCK_SIZE],
                    &input[bs..bs + BLOCK_SIZE],
                );

                acc += (block_dot as i128) * (row_scales[block_idx] as i128);
            }

            // Finalize Q16: (row_acc >> FRAC_BITS) gives the Q16 output
            // equivalent of sum(weight_real × input_real) × ONE.
            *out = (acc >> FRAC_BITS as i128) as i64;
        }
    });
}

/// Allocating convenience wrapper around `matmul_block_i8_into`.
pub fn matmul_block_i8(weights: &BlockI8Weights, input: &[i64]) -> Vec<i64> {
    let mut out = vec![0i64; weights.n_rows];
    matmul_block_i8_into(weights, input, &mut out);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Single row, single block: confirm quantize + matmul reconstructs the
    /// real dot product within quantization noise. Ground truth computed in
    /// f64; we expect relative error < 1% for well-conditioned inputs.
    #[test]
    fn test_single_block_matches_f64() {
        let weights_f32: Vec<f32> = (0..BLOCK_SIZE).map(|i| ((i as f32) - 16.0) / 10.0).collect();
        let input_f32: Vec<f32> = (0..BLOCK_SIZE).map(|i| ((i as f32) - 8.0) / 5.0).collect();

        let truth: f64 = weights_f32.iter().zip(input_f32.iter())
            .map(|(w, x)| (*w as f64) * (*x as f64))
            .sum();

        let w = BlockI8Weights::quantize_f32(&weights_f32, 1, BLOCK_SIZE);
        let input_q16: Vec<i64> = input_f32.iter()
            .map(|&x| (x as f64 * ONE as f64).round() as i64)
            .collect();
        let out = matmul_block_i8(&w, &input_q16);
        let reconstructed = out[0] as f64 / ONE as f64;

        let rel_err = (reconstructed - truth).abs() / truth.abs().max(1e-10);
        assert!(rel_err < 0.02, "rel_err = {:.4} truth={:.4} got={:.4}", rel_err, truth, reconstructed);
    }

    /// Large matrix with realistic Llama-shaped distribution (abs_max per row
    /// well under 1.0). The per-row scheme collapses here; per-block should
    /// reconstruct within ~1% mean relative error.
    #[test]
    fn test_llama_shaped_matches_f64() {
        let n_rows = 32;
        let n_cols = 4096;

        // abs_max per row uniformly in [0.05, 0.5] (Llama ffn_down
        // distribution per probe_i16_real_weights.rs).
        let mut weights_f32 = Vec::with_capacity(n_rows * n_cols);
        for r in 0..n_rows {
            let row_abs_max = 0.05 + 0.45 * ((r as f32) / (n_rows as f32));
            for c in 0..n_cols {
                let x = (((r * n_cols + c) as u64).wrapping_mul(2862933555777941757)
                    .wrapping_add(3037000493)) as i64;
                let unit = ((x >> 33) as f64 / u32::MAX as f64) - 0.5;
                weights_f32.push((unit as f32) * 2.0 * row_abs_max);
            }
        }

        // Input ~ N(0, 0.7)
        let mut input_f32 = Vec::with_capacity(n_cols);
        let mut s: u64 = 42;
        for _ in 0..n_cols {
            s = s.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            let u1 = ((s >> 33) as f64 / u32::MAX as f64) - 0.5;
            s = s.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            let u2 = ((s >> 33) as f64 / u32::MAX as f64) - 0.5;
            input_f32.push(((u1 + u2) as f32) * 1.4);
        }

        // Ground truth in f64
        let truth: Vec<f64> = (0..n_rows).map(|r| {
            weights_f32[r * n_cols..(r + 1) * n_cols].iter()
                .zip(input_f32.iter())
                .map(|(w, x)| (*w as f64) * (*x as f64))
                .sum()
        }).collect();

        // Quantized path
        let w = BlockI8Weights::quantize_f32(&weights_f32, n_rows, n_cols);
        let input_q16: Vec<i64> = input_f32.iter()
            .map(|&x| (x as f64 * ONE as f64).round() as i64)
            .collect();
        let out = matmul_block_i8(&w, &input_q16);

        // Compare
        let rel_errs: Vec<f64> = out.iter().zip(truth.iter()).map(|(&got, &t)| {
            let got_real = got as f64 / ONE as f64;
            (got_real - t).abs() / t.abs().max(1e-6)
        }).collect();
        let mean_rel = rel_errs.iter().sum::<f64>() / rel_errs.len() as f64;
        let max_rel = rel_errs.iter().cloned().fold(0.0f64, f64::max);

        assert!(mean_rel < 0.02, "mean rel err = {:.4}", mean_rel);
        assert!(max_rel < 0.05, "max rel err = {:.4}", max_rel);
    }

    /// Bitwise determinism across 1000 re-runs of the same matmul.
    #[test]
    fn test_deterministic_1000() {
        let weights_f32: Vec<f32> = (0..4 * BLOCK_SIZE).map(|i| ((i as f32) - 64.0) / 32.0).collect();
        let input_f32: Vec<f32> = (0..BLOCK_SIZE).map(|i| ((i as f32) - 16.0) / 10.0).collect();

        let w = BlockI8Weights::quantize_f32(&weights_f32, 4, BLOCK_SIZE);
        let input_q16: Vec<i64> = input_f32.iter()
            .map(|&x| (x as f64 * ONE as f64).round() as i64)
            .collect();

        let reference = matmul_block_i8(&w, &input_q16);
        for _ in 0..1000 {
            let result = matmul_block_i8(&w, &input_q16);
            assert_eq!(result, reference, "non-deterministic matmul output");
        }
    }

    /// Empty weights produces empty output (no panics for shard placeholders).
    #[test]
    fn test_empty_weights() {
        let w = BlockI8Weights::empty();
        let mut out: Vec<i64> = Vec::new();
        matmul_block_i8_into(&w, &[], &mut out);
        assert_eq!(out.len(), 0);
    }

    /// Pure-scalar i64 reference for the block-i8 matmul — the exact
    /// arithmetic the portable (non-aarch64) build runs, and the determinism
    /// ground truth. No SIMD, no narrowing.
    fn matmul_block_i8_scalar_ref(w: &BlockI8Weights, input: &[i64]) -> Vec<i64> {
        let bpr = w.n_cols / BLOCK_SIZE;
        (0..w.n_rows).map(|r| {
            let mut acc: i128 = 0;
            for b in 0..bpr {
                let mut bd: i64 = 0;
                for j in 0..BLOCK_SIZE {
                    bd += (w.data[r * w.n_cols + b * BLOCK_SIZE + j] as i64)
                        * input[b * BLOCK_SIZE + j];
                }
                acc += (bd as i128) * (w.scales[r * bpr + b] as i128);
            }
            (acc >> FRAC_BITS as i128) as i64
        }).collect()
    }

    /// The production (NEON-on-aarch64, rayon-parallel) `matmul_block_i8_into`
    /// must be BIT-IDENTICAL to the pure-scalar i64 reference on inputs inside
    /// the Q16 hidden-state range (|x| < 2^28). This pins the determinism moat:
    /// the SIMD + threaded fast path cannot diverge from the portable path.
    /// Multi-block rows and a row count spanning >1 rayon chunk exercise both
    /// the SIMD body and the parallel row split.
    #[test]
    fn test_block_i8_neon_matches_scalar_reference() {
        let n_rows = 300;   // > 256 → spans a rayon chunk boundary
        let n_cols = 4096;  // 128 blocks/row
        let mut s: u64 = 0x1234_5678_9abc_def0;
        let mut next = || {
            s = s.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            (s >> 33) as i64
        };
        let data: Vec<i8> = (0..n_rows * n_cols)
            .map(|_| (next().rem_euclid(255) - 127) as i8).collect();
        let scales: Vec<i32> = (0..n_rows * (n_cols / BLOCK_SIZE))
            .map(|_| (next().rem_euclid(200_000) + 1) as i32).collect();
        // Inputs in the realistic Q16 magnitude band [-2^27, 2^27).
        let input: Vec<i64> = (0..n_cols)
            .map(|_| next().rem_euclid(1 << 28) - (1 << 27)).collect();

        let w = BlockI8Weights { data, scales, n_rows, n_cols };
        let got = matmul_block_i8(&w, &input);
        let expect = matmul_block_i8_scalar_ref(&w, &input);
        assert_eq!(got, expect,
            "production block-i8 matmul diverged from scalar i64 reference");
    }

    /// Determinism across 500 re-runs of the parallel/SIMD path (guards
    /// against any rayon nondeterminism sneaking in with the new kernel).
    #[test]
    fn test_block_i8_parallel_deterministic() {
        let n_rows = 257;
        let n_cols = 2048;
        let data: Vec<i8> = (0..n_rows * n_cols)
            .map(|i| (((i * 31 + 7) % 255) as i64 - 127) as i8).collect();
        let scales: Vec<i32> = (0..n_rows * (n_cols / BLOCK_SIZE))
            .map(|i| ((i * 13 + 1) % 100_000 + 1) as i32).collect();
        let input: Vec<i64> = (0..n_cols)
            .map(|i| ((i as i64 * 977) % (1 << 26)) - (1 << 25)).collect();
        let w = BlockI8Weights { data, scales, n_rows, n_cols };
        let reference = matmul_block_i8(&w, &input);
        for _ in 0..500 {
            assert_eq!(matmul_block_i8(&w, &input), reference,
                "non-deterministic parallel block-i8 output");
        }
    }
}
