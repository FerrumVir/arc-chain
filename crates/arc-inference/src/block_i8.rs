use crate::integer_lut::{FRAC_BITS, ONE};

pub const BLOCK_SIZE: usize = 32;

pub struct BlockI8Weights {
    pub data: Vec<i8>,
    pub scales: Vec<i32>,
    pub n_rows: usize,
    pub n_cols: usize,
}

impl BlockI8Weights {
    pub fn quantize_f32(values: &[f32], n_rows: usize, n_cols: usize) -> Self {
        assert_eq!(values.len(), n_rows * n_cols, "shape mismatch");
        assert_eq!(
            n_cols % BLOCK_SIZE,
            0,
            "n_cols ({n_cols}) must be a multiple of BLOCK_SIZE ({BLOCK_SIZE})"
        );

        let blocks_per_row = n_cols / BLOCK_SIZE;
        let mut data = Vec::with_capacity(n_rows * n_cols);
        let mut scales = Vec::with_capacity(n_rows * blocks_per_row);

        for row_idx in 0..n_rows {
            let row = &values[row_idx * n_cols..(row_idx + 1) * n_cols];
            for block_idx in 0..blocks_per_row {
                let block = &row[block_idx * BLOCK_SIZE..(block_idx + 1) * BLOCK_SIZE];

                let abs_max = block
                    .iter()
                    .map(|x| x.abs())
                    .fold(0.0f32, f32::max)
                    .max(1e-10);

                let inv_abs_max = 127.0 / abs_max;
                for &x in block {
                    data.push((x * inv_abs_max).round().clamp(-127.0, 127.0) as i8);
                }

                let scale = ((abs_max as f64 / 127.0) * ONE as f64).round();
                let scale = scale.clamp(1.0, i32::MAX as f64) as i32;
                scales.push(scale);
            }
        }

        Self {
            data,
            scales,
            n_rows,
            n_cols,
        }
    }

    pub fn empty() -> Self {
        Self {
            data: Vec::new(),
            scales: Vec::new(),
            n_rows: 0,
            n_cols: 0,
        }
    }

    pub fn memory_bytes(&self) -> usize {
        self.data.len() + self.scales.len() * 4 + 32
    }

    pub fn dequant(&self, row: usize, col: usize) -> f64 {
        debug_assert!(row < self.n_rows && col < self.n_cols);
        let block_idx = row * (self.n_cols / BLOCK_SIZE) + col / BLOCK_SIZE;
        let scale = self.scales[block_idx] as f64 / ONE as f64;
        self.data[row * self.n_cols + col] as f64 * scale
    }
}

pub fn matmul_block_i8_into(weights: &BlockI8Weights, input: &[i64], output: &mut [i64]) {
    let n_rows = weights.n_rows;
    let n_cols = weights.n_cols;
    if n_rows == 0 || n_cols == 0 || weights.data.is_empty() {
        for o in output.iter_mut() {
            *o = 0;
        }
        return;
    }
    assert_eq!(input.len(), n_cols, "input width mismatch");
    assert_eq!(output.len(), n_rows, "output row count mismatch");

    let blocks_per_row = n_cols / BLOCK_SIZE;

    for (row_idx, out_row) in output.iter_mut().enumerate() {
        let row_data = &weights.data[row_idx * n_cols..(row_idx + 1) * n_cols];
        let row_scales = &weights.scales[row_idx * blocks_per_row..(row_idx + 1) * blocks_per_row];

        let mut acc: i128 = 0;
        for (block_idx, &row_scale) in row_scales.iter().enumerate() {
            let block_start = block_idx * BLOCK_SIZE;
            let block_end = block_start + BLOCK_SIZE;
            let wblock = &row_data[block_start..block_end];
            let iblock = &input[block_start..block_end];

            let mut block_dot: i64 = 0;
            for j in 0..BLOCK_SIZE {
                block_dot += (wblock[j] as i64) * iblock[j];
            }

            let scale = row_scale as i128;
            acc += (block_dot as i128) * scale;
        }

        *out_row = (acc >> FRAC_BITS as i128) as i64;
    }
}

pub fn matmul_block_i8(weights: &BlockI8Weights, input: &[i64]) -> Vec<i64> {
    let mut out = vec![0i64; weights.n_rows];
    matmul_block_i8_into(weights, input, &mut out);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_single_block_matches_f64() {
        let weights_f32: Vec<f32> = (0..BLOCK_SIZE)
            .map(|i| ((i as f32) - 16.0) / 10.0)
            .collect();
        let input_f32: Vec<f32> = (0..BLOCK_SIZE).map(|i| ((i as f32) - 8.0) / 5.0).collect();

        let truth: f64 = weights_f32
            .iter()
            .zip(input_f32.iter())
            .map(|(w, x)| (*w as f64) * (*x as f64))
            .sum();

        let w = BlockI8Weights::quantize_f32(&weights_f32, 1, BLOCK_SIZE);
        let input_q16: Vec<i64> = input_f32
            .iter()
            .map(|&x| (x as f64 * ONE as f64).round() as i64)
            .collect();
        let out = matmul_block_i8(&w, &input_q16);
        let reconstructed = out[0] as f64 / ONE as f64;

        let rel_err = (reconstructed - truth).abs() / truth.abs().max(1e-10);
        assert!(
            rel_err < 0.02,
            "rel_err = {:.4} truth={:.4} got={:.4}",
            rel_err,
            truth,
            reconstructed
        );
    }

    #[test]
    fn test_llama_shaped_matches_f64() {
        let n_rows = 32;
        let n_cols = 4096;

        let mut weights_f32 = Vec::with_capacity(n_rows * n_cols);
        for r in 0..n_rows {
            let row_abs_max = 0.05 + 0.45 * ((r as f32) / (n_rows as f32));
            for c in 0..n_cols {
                let x = (((r * n_cols + c) as u64)
                    .wrapping_mul(2862933555777941757)
                    .wrapping_add(3037000493)) as i64;
                let unit = ((x >> 33) as f64 / u32::MAX as f64) - 0.5;
                weights_f32.push((unit as f32) * 2.0 * row_abs_max);
            }
        }

        let mut input_f32 = Vec::with_capacity(n_cols);
        let mut s: u64 = 42;
        for _ in 0..n_cols {
            s = s
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            let u1 = ((s >> 33) as f64 / u32::MAX as f64) - 0.5;
            s = s
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            let u2 = ((s >> 33) as f64 / u32::MAX as f64) - 0.5;
            input_f32.push(((u1 + u2) as f32) * 1.4);
        }

        let truth: Vec<f64> = (0..n_rows)
            .map(|r| {
                weights_f32[r * n_cols..(r + 1) * n_cols]
                    .iter()
                    .zip(input_f32.iter())
                    .map(|(w, x)| (*w as f64) * (*x as f64))
                    .sum()
            })
            .collect();

        let w = BlockI8Weights::quantize_f32(&weights_f32, n_rows, n_cols);
        let input_q16: Vec<i64> = input_f32
            .iter()
            .map(|&x| (x as f64 * ONE as f64).round() as i64)
            .collect();
        let out = matmul_block_i8(&w, &input_q16);

        let rel_errs: Vec<f64> = out
            .iter()
            .zip(truth.iter())
            .map(|(&got, &t)| {
                let got_real = got as f64 / ONE as f64;
                (got_real - t).abs() / t.abs().max(1e-6)
            })
            .collect();
        let mean_rel = rel_errs.iter().sum::<f64>() / rel_errs.len() as f64;
        let max_rel = rel_errs.iter().cloned().fold(0.0f64, f64::max);

        assert!(mean_rel < 0.02, "mean rel err = {:.4}", mean_rel);
        assert!(max_rel < 0.05, "max rel err = {:.4}", max_rel);
    }

    #[test]
    fn test_deterministic_1000() {
        let weights_f32: Vec<f32> = (0..4 * BLOCK_SIZE)
            .map(|i| ((i as f32) - 64.0) / 32.0)
            .collect();
        let input_f32: Vec<f32> = (0..BLOCK_SIZE)
            .map(|i| ((i as f32) - 16.0) / 10.0)
            .collect();

        let w = BlockI8Weights::quantize_f32(&weights_f32, 4, BLOCK_SIZE);
        let input_q16: Vec<i64> = input_f32
            .iter()
            .map(|&x| (x as f64 * ONE as f64).round() as i64)
            .collect();

        let reference = matmul_block_i8(&w, &input_q16);
        for _ in 0..1000 {
            let result = matmul_block_i8(&w, &input_q16);
            assert_eq!(result, reference, "non-deterministic matmul output");
        }
    }

    #[test]
    fn test_empty_weights() {
        let w = BlockI8Weights::empty();
        let mut out: Vec<i64> = Vec::new();
        matmul_block_i8_into(&w, &[], &mut out);
        assert_eq!(out.len(), 0);
    }

    #[test]
    fn empty_placeholder_with_live_width_zeroes_instead_of_panicking() {
        let w = BlockI8Weights::empty();
        let input = vec![ONE; 4 * BLOCK_SIZE];
        let mut out = vec![7i64; 16];
        matmul_block_i8_into(&w, &input, &mut out);
        assert_eq!(out, vec![0i64; 16]);
    }
}
