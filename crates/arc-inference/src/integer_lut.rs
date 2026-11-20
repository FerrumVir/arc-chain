pub const FRAC_BITS: u32 = 16;
pub const ONE: i64 = 1 << FRAC_BITS;

pub const EXP_LUT_SIZE: usize = 4096;
pub const EXP_LUT_RANGE: i64 = 16 * ONE;

pub static EXP_LUT: [i64; 4097] = {
    let mut table = [0i64; 4097];
    table[4096] = ONE;
    let decay: i64 = 65281;

    let mut i: usize = 4095;
    loop {
        table[i] = (table[i + 1] * decay) >> FRAC_BITS;
        if i == 0 {
            break;
        }
        i -= 1;
    }

    table
};

pub fn integer_exp(x: i64) -> i64 {
    if x >= 0 {
        return ONE;
    }
    if x <= -(EXP_LUT_RANGE) {
        return 0;
    }

    let offset = x + EXP_LUT_RANGE;
    let step = ONE / 256;
    let idx = (offset / step) as usize;
    let frac = offset % step;

    if idx >= 4096 {
        return ONE;
    }

    let lo = EXP_LUT[idx];
    let hi = EXP_LUT[idx + 1];
    lo + ((hi - lo) * frac) / step
}

pub fn softmax_i64(input: &[i64]) -> Vec<i64> {
    if input.is_empty() {
        return vec![];
    }
    if input.len() == 1 {
        return vec![ONE];
    }

    let max_val = *input.iter().max().unwrap();

    let exps: Vec<i64> = input.iter().map(|&x| integer_exp(x - max_val)).collect();

    let sum: i64 = exps.iter().sum();
    if sum == 0 {
        let n = input.len() as i64;
        return vec![ONE / n; input.len()];
    }

    exps.iter().map(|&e| (e * ONE) / sum).collect()
}

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

pub fn relu_i64(x: i64) -> i64 {
    if x > 0 { x } else { 0 }
}

pub fn integer_isqrt(x: i64) -> i64 {
    if x <= 0 {
        return ONE * 100;
    }

    let bits = 63 - (x as u64).leading_zeros() as i64;
    let mut y = ONE * 256 / (1i64 << ((bits + 1) / 2));
    if y <= 0 {
        y = 1;
    }

    for _ in 0..5 {
        let y2 = (y * y) >> FRAC_BITS;
        let xy2 = (x * y2) >> FRAC_BITS;
        let three_minus = 3 * ONE - xy2;
        y = (y * three_minus) / (2 * ONE);
        if y <= 0 {
            y = 1;
        }
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
        assert_eq!(integer_exp(-20 * ONE), 0);
        assert!(
            integer_exp(-5 * ONE) > 100,
            "exp(-5) = {}",
            integer_exp(-5 * ONE)
        );
        assert!(integer_exp(-3 * ONE) > integer_exp(-5 * ONE));
    }

    #[test]
    fn test_exp_monotonic() {
        let mut prev = 0i64;
        for i in (-16 * ONE)..=0 {
            let val = integer_exp(i);
            assert!(
                val >= prev,
                "exp not monotonic at {}: {} < {}",
                i,
                val,
                prev
            );
            prev = val;
        }
    }

    #[test]
    fn test_softmax_sums_to_one() {
        let input = vec![ONE, 2 * ONE, 3 * ONE];
        let output = softmax_i64(&input);
        let sum: i64 = output.iter().sum();
        assert!(
            (sum - ONE).abs() < ONE / 100,
            "softmax sum {} not close to {}",
            sum,
            ONE
        );
    }

    #[test]
    fn test_softmax_argmax_preserved() {
        let input = vec![ONE, 5 * ONE, 2 * ONE, -ONE];
        let output = softmax_i64(&input);
        assert_eq!(argmax_i64(&output), 1);
    }

    #[test]
    fn test_argmax_tiebreak() {
        let input = vec![ONE, 2 * ONE, 2 * ONE, ONE];
        assert_eq!(argmax_i64(&input), 1);
    }

    #[test]
    fn test_isqrt_one() {
        let result = integer_isqrt(ONE);
        let error = (result - ONE).abs();
        assert!(
            error < ONE / 50,
            "isqrt(ONE) = {} (error {})",
            result,
            error
        );
    }

    #[test]
    fn test_isqrt_four() {
        let result = integer_isqrt(4 * ONE);
        let expected = ONE / 2;
        let error = (result - expected).abs();
        assert!(
            error < ONE / 50,
            "isqrt(4*ONE) = {} (expected {}, error {})",
            result,
            expected,
            error
        );
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
