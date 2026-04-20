//! Precomputed lookup tables for integer-only inference.
//!
//! All tables are const arrays computed at build time — zero runtime float dependency.
//! Used by integer_engine.rs for exp and GeLU approximations.

/// Fixed-point fractional bits. Value x represents x / 2^FRAC_BITS.
pub const FRAC_BITS: u32 = 16;
/// 1.0 in fixed-point representation.
pub const ONE: i64 = 1 << FRAC_BITS; // 65536

/// Integer exp lookup table for softmax.
/// 257 entries covering x in [-16*ONE, 0]. Expanded from the original
/// [-8*ONE, 0] range because block-wise INT8 matmul (see block_i8.rs)
/// produces correctly-scaled Q·K values — attention scores after max
/// subtraction routinely land in [-20, 0] on Llama-family models, and
/// the old 8-real-unit cap was saturating all of those to 0, flattening
/// the softmax into near-uniform noise (root cause of the PPL 107 ceiling).
///
/// EXP_LUT[i] = round(exp(-(256 - i) * 16.0 / 256.0) * ONE)
/// Entry 256 is exp(0) = ONE. Entry 0 is exp(-16) ≈ 1.13e-7 * ONE = 0.
/// Step size: ONE/16 = 4096 (so one LUT cell = 0.0625 real units).
///
/// For x < -16*ONE, exp(x) still maps to 0 — but that's < 1.13e-7, well
/// below any softmax-relevant contribution.
pub const EXP_LUT_SIZE: usize = 256;
pub const EXP_LUT_RANGE: i64 = 16 * ONE; // covers [-16*ONE, 0]

pub const EXP_LUT: [i64; 257] = {
    let mut table = [0i64; 257];
    // Entry i corresponds to x = -(256-i)/16 (in real units), i.e. step = ONE/16.
    // Recurrence: exp(-k/16) = exp(-(k-1)/16) * exp(-1/16).
    // exp(-1/16) = exp(-0.0625) = 0.93941306... × 65536 = 61564.79...
    // So decay = 61565 (rounded).
    table[256] = ONE;
    let decay: i64 = 61565;

    let mut i: usize = 255;
    loop {
        table[i] = (table[i + 1] * decay) >> FRAC_BITS;
        if i == 0 { break; }
        i -= 1;
    }

    table
};

/// Integer exp for x <= 0 (Q16 fixed-point).
/// Returns round(exp(x_real) * ONE) where x_real = x / ONE.
/// Uses lookup table with linear interpolation. Deterministic on all platforms.
pub fn integer_exp(x: i64) -> i64 {
    if x >= 0 { return ONE; }
    if x <= -(EXP_LUT_RANGE) { return 0; }

    // Map x from [-16*ONE, 0] to index [0, 256].
    // step = 16*ONE / 256 = ONE/16 = 4096.
    let offset = x + EXP_LUT_RANGE; // [0, 16*ONE]
    let step = ONE / 16; // 4096
    let idx = (offset / step) as usize;
    let frac = offset % step;

    if idx >= 256 { return ONE; }

    let lo = EXP_LUT[idx];
    let hi = EXP_LUT[idx + 1];
    lo + ((hi - lo) * frac) / step
}

/// Integer softmax: input is slice of Q16 values, output is Q16 values summing to ~ONE.
/// Deterministic on all platforms (no float operations).
pub fn softmax_i64(input: &[i64]) -> Vec<i64> {
    if input.is_empty() { return vec![]; }
    if input.len() == 1 { return vec![ONE]; }

    // Find max for numerical stability
    let max_val = *input.iter().max().unwrap();

    // Compute exp(x - max) for each element
    let exps: Vec<i64> = input.iter()
        .map(|&x| integer_exp(x - max_val))
        .collect();

    // Sum of exps
    let sum: i64 = exps.iter().sum();
    if sum == 0 {
        // All values are negligible — return uniform
        let n = input.len() as i64;
        return vec![ONE / n; input.len()];
    }

    // Normalize: output[i] = exps[i] * ONE / sum
    exps.iter().map(|&e| (e * ONE) / sum).collect()
}

/// Integer argmax: returns index of largest value. Ties broken by lowest index.
pub fn argmax_i64(input: &[i64]) -> usize {
    let mut best_idx = 0;
    let mut best_val = i64::MIN;
    for (i, &v) in input.iter().enumerate() {
        if v > best_val {
            best_val = v;
            best_idx = i;
        }
    }
    best_idx
}

/// Integer ReLU: max(0, x). Pure integer, trivially deterministic.
pub fn relu_i64(x: i64) -> i64 {
    if x > 0 { x } else { 0 }
}

/// Integer inverse square root: returns round(ONE / sqrt(x)) where x is Q16.
/// Uses Newton-Raphson iteration starting from a rough estimate.
/// Deterministic across all platforms (integer-only).
pub fn integer_isqrt(x: i64) -> i64 {
    if x <= 0 { return ONE * 100; } // avoid division by zero, return large value

    // Initial estimate: find leading bit position, compute rough 1/sqrt
    // For Q16 input x, real value is x/ONE. We want ONE/sqrt(x/ONE) = ONE*sqrt(ONE/x) = ONE * sqrt(ONE) / sqrt(x)
    // = ONE * 256 / sqrt(x) (since sqrt(ONE) = sqrt(65536) = 256)

    // Simple initial estimate using bit manipulation
    let bits = 63 - (x as u64).leading_zeros() as i64; // floor(log2(x))
    // sqrt(x) ≈ 2^(bits/2), so 1/sqrt(x) ≈ 2^(-bits/2)
    // In Q16: ONE * 2^(-bits/2) = 65536 >> (bits/2)
    let mut y = ONE * 256 / (1i64 << ((bits + 1) / 2)); // rough estimate
    if y <= 0 { y = 1; }

    // 3 Newton-Raphson iterations: y = y * (3*ONE - x * y * y / ONE) / (2*ONE)
    for _ in 0..3 {
        let y2 = (y * y) >> FRAC_BITS;
        let xy2 = (x * y2) >> FRAC_BITS;
        let three_minus = 3 * ONE - xy2;
        y = (y * three_minus) / (2 * ONE);
        if y <= 0 { y = 1; }
    }

    y
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_exp_at_zero() {
        assert_eq!(integer_exp(0), ONE);
    }

    #[test]
    fn test_exp_very_negative() {
        // With the expanded [-16, 0] range, exp(-20) is well past the cap.
        assert_eq!(integer_exp(-20 * ONE), 0);
        // exp(-5) ≈ 6.7e-3; in Q16 that's ~442, well above 0.
        assert!(integer_exp(-5 * ONE) > 100, "exp(-5) = {}", integer_exp(-5 * ONE));
        // Exp is strictly increasing from -16 to 0 (no Q16 underflow below -13ish).
        assert!(integer_exp(-3 * ONE) > integer_exp(-5 * ONE));
    }

    #[test]
    fn test_exp_monotonic() {
        let mut prev = 0i64;
        for i in (-16 * ONE as i64)..=0 {
            let val = integer_exp(i);
            assert!(val >= prev, "exp not monotonic at {}: {} < {}", i, val, prev);
            prev = val;
        }
    }

    #[test]
    fn test_softmax_sums_to_one() {
        let input = vec![ONE, 2 * ONE, 3 * ONE];
        let output = softmax_i64(&input);
        let sum: i64 = output.iter().sum();
        // Should be within 1% of ONE
        assert!((sum - ONE).abs() < ONE / 100, "softmax sum {} not close to {}", sum, ONE);
    }

    #[test]
    fn test_softmax_argmax_preserved() {
        // The largest input should produce the largest softmax output
        let input = vec![ONE, 5 * ONE, 2 * ONE, -ONE];
        let output = softmax_i64(&input);
        assert_eq!(argmax_i64(&output), 1);
    }

    #[test]
    fn test_argmax_tiebreak() {
        let input = vec![ONE, 2 * ONE, 2 * ONE, ONE];
        assert_eq!(argmax_i64(&input), 1); // lowest index wins
    }

    #[test]
    fn test_isqrt_one() {
        // isqrt(ONE) should be ONE (1/sqrt(1) = 1)
        let result = integer_isqrt(ONE);
        let error = (result - ONE).abs();
        assert!(error < ONE / 50, "isqrt(ONE) = {} (error {})", result, error);
    }

    #[test]
    fn test_isqrt_four() {
        // isqrt(4*ONE) should be ONE/2 (1/sqrt(4) = 0.5)
        let result = integer_isqrt(4 * ONE);
        let expected = ONE / 2;
        let error = (result - expected).abs();
        assert!(error < ONE / 50, "isqrt(4*ONE) = {} (expected {}, error {})", result, expected, error);
    }

    #[test]
    fn test_determinism_softmax_1000() {
        let input = vec![ONE, 2 * ONE, -ONE, 3 * ONE, ONE / 2];
        let first = softmax_i64(&input);
        for _ in 0..1000 {
            let result = softmax_i64(&input);
            assert_eq!(result, first, "softmax not deterministic");
        }
    }
}
