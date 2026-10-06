//! Exact integer tables of the dyadic profile (spec §4.5, §4.7, §5.1).
//!
//! Both tables are defined as correctly rounded real values. They are built
//! here with signed 128-bit Q62 fixed point (`ONE = 2^62`), which reproduces
//! the correctly rounded values exactly; the unit tests pin the BLAKE3 digests
//! computed independently at 200-bit and 60-digit precision. No floating point
//! and no libm are involved, so every platform builds identical tables.

use super::ModernError;

const Q: u32 = 62;
const QONE: i128 = 1 << Q;
const QHALF: i128 = 1 << (Q - 1);

/// Number of intervals of the exp table: steps of 1/256 over [-16, 0].
pub const EXP_STEPS: usize = 4096;

/// `T[i] = round_half_away(e^(-(4096 - i)/256) * 2^16)` for `i` in `0..=4096`.
pub static EXP_TABLE: [i64; EXP_STEPS + 1] = build_exp_table();

const fn build_exp_table() -> [i64; EXP_STEPS + 1] {
    // e^(-1/256) in Q62 from its Taylor series (every term is non-negative
    // before its sign is applied, so `/` is a floor).
    let mut term: i128 = QONE;
    let mut decay: i128 = QONE;
    let mut n: i128 = 1;
    while n < 20 {
        term /= 256 * n;
        if n % 2 == 1 {
            decay -= term;
        } else {
            decay += term;
        }
        n += 1;
    }
    let mut table = [0i64; EXP_STEPS + 1];
    let mut power: i128 = QONE;
    table[EXP_STEPS] = ((power + (1 << 45)) >> 46) as i64;
    let mut m = 1usize;
    while m <= EXP_STEPS {
        power = (power * decay + QHALF) >> Q;
        table[EXP_STEPS - m] = ((power + (1 << 45)) >> 46) as i64;
        m += 1;
    }
    table
}

/// Exact integer square root `floor(sqrt(n))`.
pub fn isqrt_u128(n: u128) -> u128 {
    if n < 2 {
        return n;
    }
    let bits = 128 - n.leading_zeros();
    // 2^ceil(bits/2) >= sqrt(n), and Newton's iteration decreases
    // monotonically from any start at or above floor(sqrt(n)).
    let mut x: u128 = 1u128 << bits.div_ceil(2);
    loop {
        let y = (x + n / x) >> 1;
        if y >= x {
            return x;
        }
        x = y;
    }
}

fn mulq(a: i128, b: i128) -> i128 {
    (a * b + QHALF) >> Q
}

/// `atanh(z)` for `0 <= z <= ONE/3`, Q62.
fn atanh_q62(z: i128) -> i128 {
    let zz = mulq(z, z);
    let mut total = z;
    let mut power = z;
    let mut k: i128 = 1;
    loop {
        power = mulq(power, zz);
        k += 2;
        let t = power / k;
        if t == 0 {
            return total;
        }
        total += t;
    }
}

/// `ln(theta)` for an integer `2 <= theta <= 2^53`, Q62.
fn ln_q62(theta: u64) -> i128 {
    let ln2 = 2 * atanh_q62(QONE / 3);
    let b = 63 - theta.leading_zeros();
    let y = i128::from(theta) << (Q - b);
    let z = ((y - QONE) << Q) / (y + QONE);
    i128::from(b) * ln2 + 2 * atanh_q62(z)
}

/// Taylor series of `e^(-x)` for `0 <= x <= ONE`, Q62.
fn exp_neg_taylor_q62(x: i128) -> i128 {
    let mut term = QONE;
    let mut total = QONE;
    let mut n: i128 = 1;
    loop {
        term = mulq(term, x) / n;
        if term == 0 {
            return total;
        }
        if n % 2 == 1 {
            total -= term;
        } else {
            total += term;
        }
        n += 1;
    }
}

/// `e^(-x)` for `0 <= x < 64 * ONE`, Q62: `e^(-f) * (e^(-1))^n` with
/// `x = n + f`, `0 <= f < 1`.
fn exp_neg_q62(x: i128) -> i128 {
    let whole = x >> Q;
    let mut result = exp_neg_taylor_q62(x - (whole << Q));
    if whole > 0 {
        let e_inv = exp_neg_taylor_q62(QONE);
        for _ in 0..whole {
            result = mulq(result, e_inv);
        }
    }
    result
}

/// `(cos w, sin w)` for `0 <= w <= ONE`, Q62.
fn cos_sin_q62(w: i128) -> (i128, i128) {
    let mut c = QONE;
    let mut s: i128 = 0;
    let mut term = QONE;
    let mut n: i128 = 1;
    loop {
        term = mulq(term, w) / n;
        if term == 0 {
            return (c, s);
        }
        let negative = (n / 2) % 2 == 1;
        if n % 2 == 1 {
            if negative {
                s -= term;
            } else {
                s += term;
            }
        } else if negative {
            c -= term;
        } else {
            c += term;
        }
        n += 1;
    }
}

fn round_q62_to_q16(v: i128) -> i32 {
    let shift = Q - 16;
    let magnitude = (v.unsigned_abs() + (1u128 << (shift - 1))) >> shift;
    // |v| <= ~2^62, so the magnitude is at most 65537.
    let m = magnitude as i32;
    if v < 0 { -m } else { m }
}

/// RoPE tables `(cos, sin)`, each `max_seq * d_head/2` Q16 values, row-major
/// by position: `cos[p * (d_head/2) + i] = round_half_away(cos(p * w_i) * 2^16)`
/// with `w_i = theta^(-2i/d_head)`.
pub fn rope_tables(
    theta: u64,
    d_head: usize,
    max_seq: usize,
) -> Result<(Vec<i32>, Vec<i32>), ModernError> {
    if !(2..=(1u64 << 53)).contains(&theta) {
        return Err(ModernError::Invalid(format!(
            "rope_theta {theta} is outside the supported integer range [2, 2^53]"
        )));
    }
    if d_head == 0 || !d_head.is_multiple_of(2) || max_seq == 0 {
        return Err(ModernError::Invalid(format!(
            "RoPE needs an even, nonzero head width and a nonzero context (d_head {d_head}, max_seq {max_seq})"
        )));
    }
    let half = d_head / 2;
    let x = (ln_q62(theta) * 2) / d_head as i128;
    if !(0..64 * QONE).contains(&x) {
        return Err(ModernError::Invalid(format!(
            "rope_theta {theta} with head width {d_head} needs 2 ln(theta)/d_head < 64"
        )));
    }
    let ratio = exp_neg_q62(x);
    let mut cos = vec![0i32; max_seq * half];
    let mut sin = vec![0i32; max_seq * half];
    let mut w = QONE;
    for i in 0..half {
        if i > 0 {
            w = mulq(w, ratio);
        }
        let (cw, sw) = cos_sin_q62(w);
        let mut c = QONE;
        let mut s: i128 = 0;
        for p in 0..max_seq {
            if p > 0 {
                let next_c = (c * cw - s * sw + QHALF) >> Q;
                let next_s = (s * cw + c * sw + QHALF) >> Q;
                c = next_c;
                s = next_s;
            }
            cos[p * half + i] = round_q62_to_q16(c);
            sin[p * half + i] = round_q62_to_q16(s);
        }
    }
    Ok((cos, sin))
}

/// Attention scale `floor(2^30 / sqrt(d_head))`, exactly.
pub fn attention_lambda(d_head: usize) -> i64 {
    let n = (1u128 << 60) / d_head.max(1) as u128;
    isqrt_u128(n) as i64
}

#[cfg(test)]
mod tests {
    use super::*;

    fn digest_i64(values: &[i64]) -> String {
        let bytes: Vec<u8> = values.iter().flat_map(|v| v.to_le_bytes()).collect();
        blake3::hash(&bytes).to_hex().to_string()
    }

    #[test]
    fn exp_table_is_the_pinned_correctly_rounded_table() {
        assert_eq!(EXP_TABLE[EXP_STEPS], 65_536);
        assert_eq!(EXP_TABLE[3840], 24_109);
        assert_eq!(EXP_TABLE[4095], 65_280);
        assert_eq!(&EXP_TABLE[..4], &[0, 0, 0, 0]);
        assert_eq!(
            digest_i64(&EXP_TABLE),
            "3586482438115a39e0e4b822f62451897f0b1b5baaf2e1019bbfb360a61bf0e2"
        );
        assert!(EXP_TABLE.windows(2).all(|pair| pair[0] <= pair[1]));
    }

    #[test]
    fn isqrt_is_exact_at_and_around_squares() {
        for n in 0u128..2000 {
            let r = isqrt_u128(n);
            assert!(r * r <= n && (r + 1) * (r + 1) > n, "n = {n}");
        }
        for r in [
            1u128 << 20,
            (1 << 40) + 12_345,
            (1 << 63) - 1,
            u64::MAX as u128,
        ] {
            assert_eq!(isqrt_u128(r * r), r);
            assert_eq!(isqrt_u128(r * r - 1), r - 1);
            assert_eq!(isqrt_u128(r * r + 2 * r), r);
        }
        assert_eq!(isqrt_u128(u128::MAX), u64::MAX as u128);
        assert_eq!(isqrt_u128(1 << 53), 94_906_265);
    }

    #[test]
    fn attention_lambda_is_floor_of_two_to_the_thirty_over_root_d() {
        assert_eq!(attention_lambda(128), 94_906_265);
        assert_eq!(attention_lambda(64), 134_217_728);
        assert_eq!(attention_lambda(16), 268_435_456);
    }

    #[test]
    fn smollm3_rope_tables_match_the_pinned_digest() {
        let (cos, sin) = rope_tables(5_000_000, 128, 4096).unwrap();
        assert_eq!(&cos[64..67], &[35_409, 46_321, 53_432]);
        assert_eq!(&sin[64..67], &[55_147, 46_361, 37_947]);
        assert_eq!(cos[4095 * 64 + 63], 65_536);
        assert_eq!(sin[4095 * 64 + 63], 68);
        assert!(cos[..64].iter().all(|&c| c == 65_536));
        assert!(sin[..64].iter().all(|&s| s == 0));
        let mut bytes: Vec<u8> = cos.iter().flat_map(|v| v.to_le_bytes()).collect();
        bytes.extend(sin.iter().flat_map(|v| v.to_le_bytes()));
        assert_eq!(
            blake3::hash(&bytes).to_hex().to_string(),
            "2024b1037902e099975b3c1e9fb989fe5e0845761b381401e9f7f295a3c8ea1b"
        );
    }

    #[test]
    fn rope_refuses_unsupported_geometry() {
        assert!(rope_tables(1, 128, 16).is_err());
        assert!(rope_tables(10_000, 127, 16).is_err());
        assert!(rope_tables(10_000, 128, 0).is_err());
        assert!(rope_tables(10_000, 2, 16).is_ok());
    }
}
