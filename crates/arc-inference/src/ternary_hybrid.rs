use crate::cached_integer_model::I8Weights;
use crate::integer_lut::FRAC_BITS;

/// Hybrid ternary + sparse outlier INT8 weights.
///
/// Most weights are ternary {-1, 0, 1}; the top `outlier_pct` fraction by
/// absolute magnitude are kept as full i8 "outliers". The matmul adds both
/// contributions for improved quality over pure ternary.
pub struct TernaryHybridWeights {
    pub ternary: Vec<i8>,
    pub outlier_data: Vec<i8>,
    pub outlier_indices: Vec<u32>,
    pub scales: Vec<i64>,
    pub n_rows: usize,
    pub n_cols: usize,
}

impl TernaryHybridWeights {
    pub fn from_i8(weights: &I8Weights, outlier_pct: f32) -> Self {
        let n_rows = weights.n_rows;
        let n_cols = weights.n_cols;
        let outlier_frac = outlier_pct.clamp(0.0, 1.0);
        let outliers_per_row = ((n_cols as f32 * outlier_frac) as usize).max(0);

        let mut ternary = Vec::with_capacity(n_rows * n_cols);
        let mut outlier_data = Vec::new();
        let mut outlier_indices = Vec::new();

        for row_idx in 0..n_rows {
            let row = &weights.data[row_idx * n_cols..(row_idx + 1) * n_cols];

            // Sort indices by absolute value descending to pick outliers
            let mut indexed: Vec<(usize, u8)> = row.iter()
                .enumerate()
                .map(|(i, &v)| (i, v.unsigned_abs()))
                .collect();
            indexed.sort_unstable_by(|a, b| b.1.cmp(&a.1));

            let outlier_set: std::collections::HashSet<usize> = indexed.iter()
                .take(outliers_per_row)
                .map(|&(i, _)| i)
                .collect();

            let sum_abs: u64 = row.iter()
                .enumerate()
                .filter(|(i, _)| !outlier_set.contains(i))
                .map(|(_, &v)| v.unsigned_abs() as u64)
                .sum();
            let non_outlier_count = (n_cols - outlier_set.len()).max(1) as u64;
            let threshold = ((sum_abs / non_outlier_count) / 2).max(1) as u8;

            for (col_idx, &w) in row.iter().enumerate() {
                if outlier_set.contains(&col_idx) {
                    ternary.push(0i8);
                    outlier_data.push(w);
                    outlier_indices.push((row_idx * n_cols + col_idx) as u32);
                } else {
                    let t = if w.unsigned_abs() < threshold { 0 } else if w > 0 { 1 } else { -1 };
                    ternary.push(t);
                }
            }
        }

        Self { ternary, outlier_data, outlier_indices, scales: weights.scales.clone(), n_rows, n_cols }
    }
}

pub fn matmul_ternary_hybrid_into(
    weights: &TernaryHybridWeights,
    input: &[i64],
    _in_sz: usize,
    output: &mut [i64],
) {
    if weights.n_rows == 0 || weights.ternary.is_empty() {
        for o in output.iter_mut() { *o = 0; }
        return;
    }

    for row_idx in 0..weights.n_rows {
        let row = &weights.ternary[row_idx * weights.n_cols..(row_idx + 1) * weights.n_cols];
        let mut acc: i128 = 0;
        for (&w, &x) in row.iter().zip(input.iter()) {
            if w != 0 {
                acc += (w as i128) * (x as i128);
            }
        }
        output[row_idx] = ((acc * weights.scales[row_idx] as i128) >> FRAC_BITS) as i64;
    }

    // Add outlier corrections
    for (idx_offset, (&w, &flat_idx)) in weights.outlier_data.iter()
        .zip(weights.outlier_indices.iter())
        .enumerate()
    {
        let flat = flat_idx as usize;
        let row_idx = flat / weights.n_cols;
        let col_idx = flat % weights.n_cols;
        if row_idx < output.len() && col_idx < input.len() {
            let correction = ((w as i128) * (input[col_idx] as i128)
                * weights.scales[row_idx] as i128) >> FRAC_BITS;
            output[row_idx] = output[row_idx].saturating_add(correction as i64);
        }
        let _ = idx_offset;
    }
}
