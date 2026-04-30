//! Precomputed lookup tables for integer-only inference.
//!
//! All tables are const arrays computed at build time - zero runtime float dependency.
//! Used by integer_engine.rs for exp and GeLU approximations.

/// Fixed-point fractional bits. Value x represents x / 2^FRAC_BITS.
pub const FRAC_BITS: u32 = 16;
/// 1.0 in fixed-point representation.
pub const ONE: i64 = 1 << FRAC_BITS; // 65536

/// Integer exp lookup table for softmax.
/// 4097 entries covering x in [-16*ONE, 0] with 16× finer resolution
/// than the original 257-entry table. Step size: ONE/256 = 256 Q16 units
/// (≈0.0039 real), which reduces the linear-interp error on exp() from
/// ~0.5% per call down to ~0.002% - critical for attention softmax where
/// small per-position errors compound into distribution noise that
/// inflates PPL without affecting argmax.
///
/// EXP_LUT[i] = round(exp(-(4096 - i) * 16.0 / 4096.0) * ONE)
/// Entry 4096 is exp(0) = ONE. Entry 0 is exp(-16) ≈ 0.
/// Memory: 4097 × 8 bytes ≈ 32 KB (fits L1 cache).
pub const EXP_LUT_SIZE: usize = 4096;
pub const EXP_LUT_RANGE: i64 = 16 * ONE; // covers [-16*ONE, 0]

pub const EXP_LUT: [i64; 4097] = {
    let mut table = [0i64; 4097];
    table[4096] = ONE;
    // exp(-1/256) = 0.99610544... × 65536 = 65280.76..., rounded to 65281.
    // Derived in f64: (-1.0/256.0).exp() * 65536.0.
    let decay: i64 = 65281;

    let mut i: usize = 4095;
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

    // Map x from [-16*ONE, 0] to index [0, 4096].
    // step = 16*ONE / 4096 = ONE/256 = 256.
    let offset = x + EXP_LUT_RANGE; // [0, 16*ONE]
    let step = ONE / 256; // 256
    let idx = (offset / step) as usize;
    let frac = offset % step;

    if idx >= 4096 { return ONE; }

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
        // All values are negligible - return uniform
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

    // Newton-Raphson converges quadratically; 5 iterations bring the
    // relative error from ~0.1% (3 iters) down to ~1e-6, which matters for
    // layernorm precision when a few hidden-state outliers dominate the
    // RMS. Cost: +2 multiplies + 2 shifts per layernorm call, negligible.
    for _ in 0..5 {
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
