use crate::cached_integer_model::I8Weights;
use crate::integer_lut::FRAC_BITS;

/// Ternary weight tensor: each weight is quantized to {-1, 0, 1}.
///
/// Quantization threshold = mean(|w|) / 2 per row. Values below the
/// threshold become 0; others map to their sign. Scale inherits the
/// per-row Q16 value from the source I8Weights.
pub struct TernaryWeights {
    pub data: Vec<i8>,
    pub scales: Vec<i64>,
    pub n_rows: usize,
    pub n_cols: usize,
}

impl TernaryWeights {
    pub fn from_i8(weights: &I8Weights) -> Self {
        let n_rows = weights.n_rows;
        let n_cols = weights.n_cols;
        let mut data = Vec::with_capacity(n_rows * n_cols);

        for row_idx in 0..n_rows {
            let row = &weights.data[row_idx * n_cols..(row_idx + 1) * n_cols];
            let sum_abs: u64 = row.iter().map(|&v| v.unsigned_abs() as u64).sum();
            let threshold = ((sum_abs / n_cols.max(1) as u64) / 2).max(1) as u8;
            for &w in row {
                data.push(if (w.unsigned_abs()) < threshold { 0 } else if w > 0 { 1 } else { -1 });
            }
        }

        Self { data, scales: weights.scales.clone(), n_rows, n_cols }
    }
}

pub fn matmul_ternary_into(
    weights: &TernaryWeights,
    input: &[i64],
    _in_sz: usize,
    output: &mut [i64],
) {
    if weights.n_rows == 0 || weights.data.is_empty() {
        for o in output.iter_mut() { *o = 0; }
        return;
    }
    for row_idx in 0..weights.n_rows {
        let row = &weights.data[row_idx * weights.n_cols..(row_idx + 1) * weights.n_cols];
        let mut acc: i128 = 0;
        for (&w, &x) in row.iter().zip(input.iter()) {
            if w != 0 {
                acc += (w as i128) * (x as i128);
            }
        }
        output[row_idx] = ((acc * weights.scales[row_idx] as i128) >> FRAC_BITS) as i64;
    }
}
