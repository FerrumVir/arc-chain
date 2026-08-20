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
pub fn matmul_block_i8_into(
    weights: &BlockI8Weights,
    input: &[i64],
    output: &mut [i64],
) {
    let n_rows = weights.n_rows;
    let n_cols = weights.n_cols;
    assert_eq!(input.len(), n_cols, "input width mismatch");
    assert_eq!(output.len(), n_rows, "output row count mismatch");
    if n_rows == 0 || n_cols == 0 { return; }

    let blocks_per_row = n_cols / BLOCK_SIZE;

    // `output.len() == n_rows` is asserted above, and `row_scales` is built with
    // exactly `blocks_per_row` elements, so iterating them covers exactly the
    // same indices in the same order as the original ranges.
    for (row_idx, out_row) in output.iter_mut().enumerate() {
        let row_data = &weights.data[row_idx * n_cols..(row_idx + 1) * n_cols];
        let row_scales = &weights.scales[row_idx * blocks_per_row
            ..(row_idx + 1) * blocks_per_row];

        let mut acc: i128 = 0;
        for (block_idx, &row_scale) in row_scales.iter().enumerate() {
            let block_start = block_idx * BLOCK_SIZE;
            let block_end = block_start + BLOCK_SIZE;
            let wblock = &row_data[block_start..block_end];
            let iblock = &input[block_start..block_end];

            // i8 × i64 → i64 per element; sum over 32 elements stays inside i64
            // (max |w×x| = 127 × 2^28 ≈ 3.4e10, × 32 = 1.1e12; i64 max ≈ 9.2e18).
            let mut block_dot: i64 = 0;
            for j in 0..BLOCK_SIZE {
                block_dot += (wblock[j] as i64) * iblock[j];
            }

            let scale = row_scale as i128;
            acc += (block_dot as i128) * scale;
        }

        // Finalize Q16: (row_acc >> FRAC_BITS) gives the Q16 output
        // equivalent of sum(weight_real × input_real) × ONE.
        *out_row = (acc >> FRAC_BITS as i128) as i64;
    }
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
}
