//! Q4 direct computation — 4-bit weights, half the memory, ~2x speed.
//!
//! Symmetric per-row 4-bit quantization: each weight stored as signed nibble [-7, 7].
//! Packed 2 values per byte (low nibble first, biased by 8 for unsigned storage).
//! Memory: 0.5 bytes/param + 8 bytes/row scale = ~3.4 GB for 7B.
//!
//! Matmul inner loop extracts nibbles and multiplies with i8 input.
//! NEON: process 32 Q4 values (16 bytes) per iteration.

use crate::integer_lut::*;
use crate::cached_integer_model::{I8Weights, QuantizedInput};
use rayon::prelude::*;

/// Q4 per-row quantized weight matrix.
/// Each byte stores 2 values: low nibble = v0 + 8, high nibble = v1 + 8.
/// Real value ≈ (nibble - 8) * scale[row].
pub struct Q4Weights {
    pub data: Vec<u8>,       // packed nibbles [n_rows × n_cols/2]
    pub scales: Vec<i64>,    // per-row Q16 scale
    pub n_rows: usize,
    pub n_cols: usize,       // must be even
}

impl Q4Weights {
    /// Quantize from per-row INT8 weights to Q4.
    /// Uses the same per-row scale but reduces to 4-bit resolution.
    pub fn from_i8(i8w: &I8Weights) -> Self {
        let n_rows = i8w.n_rows;
        let n_cols = i8w.n_cols;
        assert!(n_cols % 2 == 0, "Q4 requires even column count");

        let mut data = Vec::with_capacity(n_rows * n_cols / 2);
        let mut scales = Vec::with_capacity(n_rows);

        for i in 0..n_rows {
            let row = &i8w.data[i * n_cols..(i + 1) * n_cols];

            // Find abs max of i8 row
            let abs_max = row.iter().map(|&x| (x as i16).abs() as u8).max().unwrap_or(1).max(1);

            // Q4 scale: maps [-7, 7] to [-abs_max, abs_max]
            // scale_i64 = abs_max_real / 7 in Q16 terms
            // Since i8 values are in [-127, 127] with their own scale,
            // and Q4 values will be in [-7, 7]:
            // Q4 scale = i8_scale * (abs_max_i8 / 7)
            let q4_per_unit = (abs_max as i64 + 6) / 7; // ceiling division
            let q4_scale = (i8w.scales[i] * q4_per_unit.max(1)) >> 0; // combined scale

            // Quantize each i8 value to 4-bit [-7, 7], store as [1, 15] (bias +8)
            for pair in 0..(n_cols / 2) {
                let j = pair * 2;
                let v0 = ((row[j] as i16) / q4_per_unit as i16).clamp(-7, 7) as i8;
                let v1 = ((row[j + 1] as i16) / q4_per_unit as i16).clamp(-7, 7) as i8;
                // Pack: low nibble = v0+8, high nibble = v1+8 (both in [1,15])
                let byte = ((v0 + 8) as u8) | (((v1 + 8) as u8) << 4);
                data.push(byte);
            }

            scales.push(q4_scale);
        }

        Self { data, scales, n_rows, n_cols }
    }

    pub fn memory_bytes(&self) -> usize {
        self.data.len() + self.scales.len() * 8 + 16
    }
}

/// Q4 × i64 scalar matmul with per-row scales.
pub fn matmul_q4(weights: &Q4Weights, input: &[i64], in_size: usize, out_size: usize) -> Vec<i64> {
    let data = &weights.data;
    let scales = &weights.scales;
    let half_cols = in_size / 2;

    let mut output = vec![0i64; out_size];
    output.par_chunks_mut(512).enumerate().for_each(|(ci, chunk)| {
        let base = ci * 512;
        for (li, out) in chunk.iter_mut().enumerate() {
            let i = base + li;
            let row_off = i * half_cols;
            let mut acc: i64 = 0;

            for p in 0..half_cols {
                let byte = data[row_off + p];
                let q0 = ((byte & 0x0F) as i64) - 8;  // low nibble [-7..7]
                let q1 = ((byte >> 4) as i64) - 8;     // high nibble [-7..7]
                let j = p * 2;
                acc += q0 * input[j];
                acc += q1 * input[j + 1];
            }

            *out = (acc * scales[i]) >> FRAC_BITS;
        }
    });
    output
}

/// Q4 × i8 SIMD matmul (pre-quantized input).
pub fn matmul_q4_preq(weights: &Q4Weights, input_q: &QuantizedInput, in_size: usize, output: &mut [i64]) {
    let data = &weights.data;
    let scales = &weights.scales;
    let half_cols = in_size / 2;
    let isf = input_q.scale_factor;

    output.par_chunks_mut(512).enumerate().for_each(|(ci, chunk)| {
        let base = ci * 512;
        for (li, out) in chunk.iter_mut().enumerate() {
            let i = base + li;
            let row_off = i * half_cols;
            let mut acc: i64 = 0;

            #[cfg(target_arch = "aarch64")]
            {
                use std::arch::aarch64::*;
                let inp = &input_q.data;
                let simd_len = half_cols / 16 * 16; // 16 bytes = 32 Q4 values
                unsafe {
                    let bias = vdupq_n_s8(8);
                    let mut vacc0 = vdupq_n_s32(0);
                    let mut vacc1 = vdupq_n_s32(0);
                    let mask_lo = vdupq_n_u8(0x0F);

                    let mut p = 0usize;
                    while p < simd_len {
                        // Load 16 packed bytes = 32 Q4 values
                        let packed = vld1q_u8(data.as_ptr().add(row_off + p));

                        // Extract low and high nibbles
                        let lo = vreinterpretq_s8_u8(vandq_u8(packed, mask_lo));
                        let hi = vreinterpretq_s8_u8(vshrq_n_u8(packed, 4));

                        // Subtract bias (8) to get signed [-7..7]
                        let q_lo = vsubq_s8(lo, bias);
                        let q_hi = vsubq_s8(hi, bias);

                        // Load 32 input i8 values
                        let i_lo = vld1q_s8(inp.as_ptr().add(p * 2));
                        let i_hi = vld1q_s8(inp.as_ptr().add(p * 2 + 16));

                        // Multiply and accumulate: i8×i8→i16→i32
                        vacc0 = vpadalq_s16(vacc0, vmull_s8(vget_low_s8(q_lo), vget_low_s8(i_lo)));
                        vacc0 = vpadalq_s16(vacc0, vmull_s8(vget_high_s8(q_lo), vget_high_s8(i_lo)));
                        vacc1 = vpadalq_s16(vacc1, vmull_s8(vget_low_s8(q_hi), vget_low_s8(i_hi)));
                        vacc1 = vpadalq_s16(vacc1, vmull_s8(vget_high_s8(q_hi), vget_high_s8(i_hi)));

                        p += 16;
                    }
                    vacc0 = vaddq_s32(vacc0, vacc1);
                    acc = vaddvq_s32(vacc0) as i64;

                    // Scalar remainder
                    for pp in simd_len..half_cols {
                        let byte = data[row_off + pp];
                        let q0 = ((byte & 0x0F) as i64) - 8;
                        let q1 = ((byte >> 4) as i64) - 8;
                        let j = pp * 2;
                        acc += q0 * (inp[j] as i64);
                        acc += q1 * (inp[j + 1] as i64);
                    }
                }
            }

            #[cfg(not(target_arch = "aarch64"))]
            {
                let inp = &input_q.data;
                for p in 0..half_cols {
                    let byte = data[row_off + p];
                    let q0 = ((byte & 0x0F) as i64) - 8;
                    let q1 = ((byte >> 4) as i64) - 8;
                    let j = p * 2;
                    acc += q0 * (inp[j] as i64);
                    acc += q1 * (inp[j + 1] as i64);
                }
            }

            *out = acc * ((scales[i] * isf) >> FRAC_BITS);
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_q4_roundtrip() {
        let i8w = I8Weights::quantize_f32(
            &[1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0, 8.0], 2, 4);
        let q4 = Q4Weights::from_i8(&i8w);
        assert_eq!(q4.data.len(), 2 * 2); // 2 rows × 4/2 = 4 bytes
        assert_eq!(q4.memory_bytes() < i8w.memory_bytes(), true);
    }

    #[test]
    fn test_q4_matmul_basic() {
        let i8w = I8Weights::quantize_f32(
            &[1.0, 2.0, 3.0, 4.0, 5.0, 6.0], 2, 3);
        // Q4 requires even cols — pad to 4
        let i8w = I8Weights::quantize_f32(
            &[1.0, 2.0, 3.0, 0.0, 4.0, 5.0, 6.0, 0.0], 2, 4);
        let q4 = Q4Weights::from_i8(&i8w);
        let input = vec![ONE, ONE, ONE, 0]; // [1, 1, 1, 0]
        let result = matmul_q4(&q4, &input, 4, 2);
        // Row 0: 1+2+3+0 = 6, Row 1: 4+5+6+0 = 15
        let tolerance = ONE * 2; // Q4 loses precision
        assert!((result[0] - 6 * ONE).abs() < tolerance, "Row 0: {}", result[0]);
        assert!((result[1] - 15 * ONE).abs() < tolerance, "Row 1: {}", result[1]);
    }
}
