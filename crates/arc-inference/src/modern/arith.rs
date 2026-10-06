//! Operators of the dyadic profile (spec §5), integer-only and order-free.
//!
//! Every function computes exact integer values and refuses (returns
//! [`ModernError::Domain`]) instead of wrapping. Reduction order, thread count
//! and SIMD width cannot change a result: sums are exact and the only
//! roundings are explicit shifts or truncating divisions of exact values.

use rayon::prelude::*;

use super::ModernError;
use super::tables::{EXP_STEPS, EXP_TABLE, isqrt_u128};

/// Q16 fractional bits.
pub const FRAC_BITS: u32 = 16;
/// 1.0 in Q16.
pub const ONE: i64 = 1 << FRAC_BITS;
/// Largest magnitude of any stored activation (spec §9).
pub const ACTIVATION_LIMIT: u128 = 1 << 62;
/// Row chunk for parallel projections.
const ROW_CHUNK: usize = 64;

/// Per-row INT8 matrix with dyadic scales `mu * 2^-k` (spec §3, §4.2).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DyadicMatrix {
    pub rows: usize,
    pub cols: usize,
    /// Row-major weights in `[-127, 127]`.
    pub q: Vec<i8>,
    /// Scale mantissas: 0 for an all-zero row, else in `[2^30, 2^31)`.
    pub mu: Vec<i32>,
    /// Scale shifts in `[16, 62]` (16 for an all-zero row).
    pub k: Vec<u8>,
}

impl DyadicMatrix {
    /// Check the shape and every value range the profile requires.
    pub fn validate(&self, name: &str) -> Result<(), ModernError> {
        let expected = self.rows.checked_mul(self.cols);
        if self.rows == 0
            || self.cols == 0
            || expected != Some(self.q.len())
            || self.mu.len() != self.rows
            || self.k.len() != self.rows
        {
            return Err(ModernError::Invalid(format!(
                "{name}: inconsistent dyadic matrix shape {}x{} (q {}, mu {}, k {})",
                self.rows,
                self.cols,
                self.q.len(),
                self.mu.len(),
                self.k.len()
            )));
        }
        if self.q.contains(&i8::MIN) {
            return Err(ModernError::Invalid(format!(
                "{name}: weight -128 is not a profile value"
            )));
        }
        for (row, (&mu, &k)) in self.mu.iter().zip(&self.k).enumerate() {
            let zero_scale = mu == 0 && k == 16;
            let normal_scale = mu >= (1 << 30) && (16..=62).contains(&k);
            if !(zero_scale || normal_scale) {
                return Err(ModernError::Invalid(format!(
                    "{name}: row {row} has an invalid dyadic scale ({mu}, {k})"
                )));
            }
            if mu == 0
                && self.q[row * self.cols..(row + 1) * self.cols]
                    .iter()
                    .any(|&w| w != 0)
            {
                return Err(ModernError::Invalid(format!(
                    "{name}: row {row} has a zero scale but nonzero weights"
                )));
            }
        }
        Ok(())
    }

    fn row(&self, r: usize) -> &[i8] {
        &self.q[r * self.cols..(r + 1) * self.cols]
    }
}

fn domain(what: &str) -> ModernError {
    ModernError::Domain(what.to_string())
}

/// Narrow an exact wide value to a stored activation, refusing `|v| > 2^62`.
#[inline]
pub fn to_activation(value: i128, what: &str) -> Result<i64, ModernError> {
    if value.unsigned_abs() > ACTIVATION_LIMIT {
        return Err(domain(what));
    }
    Ok(value as i64)
}

/// `exp(x)` in Q16 for `x <= 0` (spec §5.1).
#[inline]
pub fn exp_q16(x: i64) -> i64 {
    if x >= 0 {
        return ONE;
    }
    if x <= -(16 * ONE) {
        return 0;
    }
    let offset = x + 16 * ONE;
    let index = (offset >> 8) as usize;
    let fraction = offset & 255;
    if index >= EXP_STEPS {
        return EXP_TABLE[EXP_STEPS];
    }
    let low = EXP_TABLE[index];
    let high = EXP_TABLE[index + 1];
    low + (((high - low) * fraction) >> 8)
}

/// Exact `sum_j row[j] * x[j]`; the caller has checked the projection domain.
#[inline]
pub fn dot_i8_i64(row: &[i8], x: &[i64]) -> i64 {
    let mut lanes = [0i64; 4];
    let mut row_chunks = row.chunks_exact(4);
    let mut x_chunks = x.chunks_exact(4);
    for (w, v) in (&mut row_chunks).zip(&mut x_chunks) {
        lanes[0] += i64::from(w[0]) * v[0];
        lanes[1] += i64::from(w[1]) * v[1];
        lanes[2] += i64::from(w[2]) * v[2];
        lanes[3] += i64::from(w[3]) * v[3];
    }
    let mut total = lanes[0] + lanes[1] + lanes[2] + lanes[3];
    for (&w, &v) in row_chunks.remainder().iter().zip(x_chunks.remainder()) {
        total += i64::from(w) * v;
    }
    total
}

/// Projection precondition `127 * sum |x_j| < 2^63` (spec §5.2).
pub fn check_projection_input(x: &[i64]) -> Result<(), ModernError> {
    let total: u128 = x.iter().map(|v| u128::from(v.unsigned_abs())).sum();
    if total * 127 >= 1u128 << 63 {
        return Err(domain("projection input magnitude (127 * sum |x| >= 2^63)"));
    }
    Ok(())
}

/// Dyadic epilogue `(acc * mu) >> k` (spec §5.2).
#[inline]
pub fn dyadic_epilogue(acc: i64, mu: i32, k: u8) -> Result<i64, ModernError> {
    let product = i128::from(acc) * i128::from(mu);
    to_activation(product >> k, "projection output beyond 2^62")
}

/// `out = W x` for one dyadic matrix (spec §5.2).
///
/// When the opt-in limb kernel is enabled (`ARC_FAST_CANONICAL_KERNEL=1` or
/// `canonical_simd::set_fast_canonical_kernel`) it computes the exact row dot
/// products; on refusal, or when it is off, the scalar kernel computes the same
/// integers. The epilogue is shared, so both paths produce identical outputs.
pub fn project(m: &DyadicMatrix, x: &[i64], out: &mut [i64]) -> Result<(), ModernError> {
    if x.len() != m.cols || out.len() != m.rows {
        return Err(ModernError::Invalid(format!(
            "projection shape: matrix {}x{}, input {}, output {}",
            m.rows,
            m.cols,
            x.len(),
            out.len()
        )));
    }
    check_projection_input(x)?;
    let simd = crate::canonical_simd::fast_canonical_kernel_enabled()
        && crate::canonical_simd::exact_row_dots_fast(&m.q, m.rows, m.cols, x, out);
    if !simd {
        out.par_chunks_mut(ROW_CHUNK)
            .enumerate()
            .for_each(|(chunk_index, chunk)| {
                let start = chunk_index * ROW_CHUNK;
                for (offset, slot) in chunk.iter_mut().enumerate() {
                    *slot = dot_i8_i64(m.row(start + offset), x);
                }
            });
    }
    for ((slot, &mu), &k) in out.iter_mut().zip(&m.mu).zip(&m.k) {
        *slot = dyadic_epilogue(*slot, mu, k)?;
    }
    Ok(())
}

/// Embedding row `e_j = (q_tj * mu_t) >> (k_t - 16)` (spec §5.3).
pub fn embed_row(m: &DyadicMatrix, token: usize) -> Result<Vec<i64>, ModernError> {
    if token >= m.rows {
        return Err(domain("token id is outside the vocabulary"));
    }
    let mu = i64::from(m.mu[token]);
    let shift = u32::from(m.k[token]).saturating_sub(FRAC_BITS);
    Ok(m.row(token)
        .iter()
        .map(|&w| (i64::from(w) * mu) >> shift)
        .collect())
}

/// RMS normalisation with an exact square root (spec §5.4).
pub fn rms_norm(x: &[i64], gain: &[i64], eps_q32: i64) -> Result<Vec<i64>, ModernError> {
    if x.is_empty() || gain.len() != x.len() || eps_q32 < 1 {
        return Err(ModernError::Invalid(format!(
            "rms_norm shape: input {}, gain {}, eps_q32 {eps_q32}",
            x.len(),
            gain.len()
        )));
    }
    let mut squares: i128 = 0;
    for &v in x {
        let v = i128::from(v);
        squares = squares
            .checked_add(v * v)
            .ok_or_else(|| domain("rms_norm sum of squares beyond 2^127"))?;
    }
    let mean = squares / x.len() as i128 + i128::from(eps_q32);
    if mean > 1i128 << 92 {
        return Err(domain("rms_norm mean square beyond 2^92"));
    }
    let inverse_rms = isqrt_u128((1u128 << 92) / mean as u128) as i128;
    x.iter()
        .zip(gain)
        .map(|(&v, &g)| {
            let product = i128::from(v)
                .checked_mul(inverse_rms)
                .and_then(|p| p.checked_mul(i128::from(g)))
                .ok_or_else(|| domain("rms_norm product beyond 2^127"))?;
            to_activation(product >> 46, "rms_norm output beyond 2^62")
        })
        .collect()
}

/// Rotate one head in split-half pairing at one position (spec §5.5).
pub fn rope_split_half(head: &mut [i64], cos: &[i32], sin: &[i32]) -> Result<(), ModernError> {
    let half = head.len() / 2;
    if head.len() != 2 * half || cos.len() != half || sin.len() != half {
        return Err(ModernError::Invalid(format!(
            "rope shape: head {}, cos {}, sin {}",
            head.len(),
            cos.len(),
            sin.len()
        )));
    }
    let (low, high) = head.split_at_mut(half);
    for (((a, b), &c), &s) in low.iter_mut().zip(high.iter_mut()).zip(cos).zip(sin) {
        let (av, bv) = (i128::from(*a), i128::from(*b));
        let (c, s) = (i128::from(c), i128::from(s));
        let rotated_a = (av * c - bv * s) >> FRAC_BITS;
        let rotated_b = (av * s + bv * c) >> FRAC_BITS;
        *a = to_activation(rotated_a, "rope output beyond 2^62")?;
        *b = to_activation(rotated_b, "rope output beyond 2^62")?;
    }
    Ok(())
}

/// The cached keys and values one query head reads.
#[derive(Clone, Copy)]
pub struct HeadCache<'a> {
    /// Keys, `positions * stride` values.
    pub keys: &'a [i32],
    /// Values, same layout as `keys`.
    pub values: &'a [i32],
    /// Positions `0 .. positions` are attended.
    pub positions: usize,
    /// Values per cached position (all KV heads).
    pub stride: usize,
    /// Offset of this head's KV group inside a position.
    pub offset: usize,
}

/// Two-pass attention for one query head (spec §5.6).
pub fn attention_head(
    q: &[i64],
    cache: HeadCache<'_>,
    lambda: i64,
    out: &mut [i64],
) -> Result<(), ModernError> {
    let width = q.len();
    let last = cache
        .positions
        .checked_sub(1)
        .and_then(|p| p.checked_mul(cache.stride))
        .and_then(|base| base.checked_add(cache.offset + width));
    if cache.positions == 0
        || out.len() != width
        || last.is_none_or(|end| end > cache.keys.len() || end > cache.values.len())
    {
        return Err(ModernError::Invalid("attention cache shape".into()));
    }
    // |k| < 2^31, so sum |q_t| < 2^32 bounds every dot below 2^63.
    let q_mass: u128 = q.iter().map(|v| u128::from(v.unsigned_abs())).sum();
    let narrow = q_mass < (1u128 << 32);
    let mut scores = Vec::with_capacity(cache.positions);
    for position in 0..cache.positions {
        let base = position * cache.stride + cache.offset;
        let key = &cache.keys[base..base + width];
        let dot: i128 = if narrow {
            let mut sum = 0i64;
            for (&a, &b) in q.iter().zip(key) {
                sum += a * i64::from(b);
            }
            i128::from(sum)
        } else {
            let mut sum = 0i128;
            for (&a, &b) in q.iter().zip(key) {
                sum += i128::from(a) * i128::from(b);
            }
            sum
        };
        let scaled = dot
            .checked_mul(i128::from(lambda))
            .ok_or_else(|| domain("attention score product beyond 2^127"))?;
        scores.push(to_activation(scaled >> 46, "attention score beyond 2^62")?);
    }
    let max_score = scores.iter().copied().max().unwrap_or(0);
    let mut total: i64 = 0;
    let mut weighted = vec![0i64; width];
    for (position, &score) in scores.iter().enumerate() {
        let weight = exp_q16(score - max_score);
        if weight == 0 {
            continue;
        }
        total += weight;
        let base = position * cache.stride + cache.offset;
        for (acc, &v) in weighted.iter_mut().zip(&cache.values[base..base + width]) {
            *acc += weight * i64::from(v);
        }
    }
    for (slot, &acc) in out.iter_mut().zip(&weighted) {
        *slot = acc / total;
    }
    Ok(())
}

/// `sigma(g)` in Q16 (spec §5.7).
#[inline]
pub fn sigmoid_q16(g: i64) -> i64 {
    if g >= 0 {
        (1i64 << 32) / (ONE + exp_q16(-g))
    } else {
        let e = exp_q16(g);
        (e << FRAC_BITS) / (ONE + e)
    }
}

/// Gated SiLU `(g * sigma(g) * u) >> 32` (spec §5.7).
#[inline]
pub fn gated_silu(g: i64, u: i64) -> Result<i64, ModernError> {
    let product = i128::from(g)
        .checked_mul(i128::from(sigmoid_q16(g)))
        .and_then(|p| p.checked_mul(i128::from(u)))
        .ok_or_else(|| domain("gated SiLU product beyond 2^127"))?;
    to_activation(product >> 32, "gated SiLU output beyond 2^62")
}

/// `h += delta`, exactly, refusing results beyond 2^62.
pub fn add_residual(h: &mut [i64], delta: &[i64]) -> Result<(), ModernError> {
    if h.len() != delta.len() {
        return Err(ModernError::Invalid("residual shape".into()));
    }
    for (a, &b) in h.iter_mut().zip(delta) {
        *a = to_activation(i128::from(*a) + i128::from(b), "residual beyond 2^62")?;
    }
    Ok(())
}

/// Token selection rules (spec §6.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Selection {
    /// First index of the maximum.
    Argmax,
    /// ARC repetition penalty over the 64 most recent generated tokens, then argmax.
    Rp64Argmax,
}

impl Selection {
    pub fn name(self) -> &'static str {
        match self {
            Selection::Argmax => "argmax",
            Selection::Rp64Argmax => "rp64-argmax",
        }
    }

    pub fn parse(name: &str) -> Result<Self, ModernError> {
        match name {
            "argmax" => Ok(Selection::Argmax),
            "rp64-argmax" => Ok(Selection::Rp64Argmax),
            other => Err(ModernError::Invalid(format!("unknown selection {other}"))),
        }
    }

    /// The generation semantics identity this selection belongs to.
    pub fn semantics(self) -> &'static str {
        match self {
            Selection::Argmax => super::GENERATION_ARGMAX,
            Selection::Rp64Argmax => super::GENERATION_RP64,
        }
    }
}

/// First index holding the maximum value.
pub fn argmax(values: &[i64]) -> usize {
    let mut best = 0usize;
    for (index, &value) in values.iter().enumerate() {
        if value > values[best] {
            best = index;
        }
    }
    best
}

/// Select the next token from `logits` given the tokens generated so far.
pub fn select(logits: &[i64], generated: &[u32], selection: Selection) -> Result<u32, ModernError> {
    if logits.is_empty() {
        return Err(ModernError::Invalid("empty logits".into()));
    }
    let index = match selection {
        Selection::Argmax => argmax(logits),
        Selection::Rp64Argmax => {
            let mut penalised = logits.to_vec();
            for &token in generated.iter().rev().take(64) {
                if let Some(value) = penalised.get_mut(token as usize) {
                    let wide = i128::from(*value);
                    let next = if wide > 0 { wide * 5 / 6 } else { wide * 6 / 5 };
                    *value = to_activation(next, "repetition penalty beyond 2^62")?;
                }
            }
            argmax(&penalised)
        }
    };
    u32::try_from(index).map_err(|_| ModernError::Invalid("vocabulary beyond u32".into()))
}

/// BLAKE3 of logits as little-endian i64 (spec §6.3).
pub fn logits_hash(logits: &[i64]) -> [u8; 32] {
    let bytes: Vec<u8> = logits.iter().flat_map(|v| v.to_le_bytes()).collect();
    *blake3::hash(&bytes).as_bytes()
}

/// BLAKE3 of the concatenated per-position logits hashes (spec §6.3).
pub fn logits_digest(hashes: &[[u8; 32]]) -> [u8; 32] {
    let mut hasher = blake3::Hasher::new();
    for hash in hashes {
        hasher.update(hash);
    }
    *hasher.finalize().as_bytes()
}

/// BLAKE3 of token ids as little-endian u32 (spec §6.2).
pub fn tokens_hash(tokens: &[u32]) -> [u8; 32] {
    let bytes: Vec<u8> = tokens.iter().flat_map(|t| t.to_le_bytes()).collect();
    *blake3::hash(&bytes).as_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn matrix(rows: usize, cols: usize, q: Vec<i8>, mu: i32, k: u8) -> DyadicMatrix {
        DyadicMatrix {
            rows,
            cols,
            q,
            mu: vec![mu; rows],
            k: vec![k; rows],
        }
    }

    #[test]
    fn exp_interpolates_the_table_and_saturates() {
        assert_eq!(exp_q16(0), ONE);
        assert_eq!(exp_q16(-(16 * ONE)), 0);
        assert_eq!(exp_q16(-(20 * ONE)), 0);
        assert_eq!(exp_q16(-ONE), EXP_TABLE[3840]);
        // Midway between T[4095] = 65280 and T[4096] = 65536.
        assert_eq!(exp_q16(-128), 65_280 + ((256 * 128) >> 8));
        let mut previous = 0;
        for x in (-(16 * ONE)..=0).step_by(97) {
            let value = exp_q16(x);
            assert!(value >= previous);
            previous = value;
        }
    }

    #[test]
    fn projection_applies_the_dyadic_epilogue_with_a_floor_shift() {
        // scale = 2^30 * 2^-46 = 2^-16: y = acc >> 16.
        let m = matrix(2, 3, vec![1, -2, 3, 127, 0, -127], 1 << 30, 46);
        let x = [ONE, 3 * ONE, -ONE];
        let mut out = [0i64; 2];
        project(&m, &x, &mut out).unwrap();
        // row 0: 1 - 6 - 3 = -8 (Q16) -> floor(-8 * 2^16 / 2^16) = -8
        // row 1: 127 + 127 = 254
        assert_eq!(out, [-8, 254]);
        // A negative accumulator rounds toward -inf: -3 * 2^30 / 2^31 = -1.5 -> -2.
        let m = matrix(1, 1, vec![-1], 1 << 30, 31);
        let mut out = [0i64; 1];
        project(&m, &[3], &mut out).unwrap();
        assert_eq!(out, [-2]);
    }

    #[test]
    fn projection_refuses_inputs_outside_the_domain() {
        let m = matrix(1, 2, vec![1, 1], 1 << 30, 46);
        let mut out = [0i64; 1];
        // 127 * 2^58 >= 2^63.
        let big = 1i64 << 57;
        assert!(matches!(
            project(&m, &[big, big], &mut out),
            Err(ModernError::Domain(_))
        ));
        assert!(matches!(
            project(&m, &[1], &mut out),
            Err(ModernError::Invalid(_))
        ));
    }

    #[test]
    fn scalar_dot_handles_tails() {
        let row: Vec<i8> = (0..11).map(|i| (i as i8) - 5).collect();
        let x: Vec<i64> = (0..11).map(|i| (i as i64) * 1000 - 7).collect();
        let expected: i64 = row.iter().zip(&x).map(|(&w, &v)| i64::from(w) * v).sum();
        assert_eq!(dot_i8_i64(&row, &x), expected);
    }

    #[test]
    fn embedding_expands_with_the_row_scale() {
        // mu = 2^30, k = 40 -> e = q * 2^30 >> 24 = q * 64.
        let m = matrix(2, 2, vec![2, -3, 0, 1], 1 << 30, 40);
        assert_eq!(embed_row(&m, 0).unwrap(), vec![128, -192]);
        assert!(embed_row(&m, 2).is_err());
    }

    #[test]
    fn rms_norm_matches_the_exact_formula() {
        let x = [ONE, -2 * ONE, 3 * ONE, 0];
        let gain = [ONE, ONE, 2 * ONE, ONE];
        let y = rms_norm(&x, &gain, 4295).unwrap();
        let squares: i128 = x.iter().map(|&v| i128::from(v) * i128::from(v)).sum();
        let mean = squares / 4 + 4295;
        let r = isqrt_u128((1u128 << 92) / mean as u128) as i128;
        for ((&v, &g), &out) in x.iter().zip(&gain).zip(&y) {
            assert_eq!(i128::from(out), (i128::from(v) * r * i128::from(g)) >> 46);
        }
        // rms = sqrt(14/4) = 1.8708; 1/rms = 0.5345 -> y0 ~ 0.5345 * ONE.
        assert!((y[0] - 35_030).abs() < 3, "{}", y[0]);
        assert!(rms_norm(&x, &gain[..3], 4295).is_err());
        assert!(rms_norm(&x, &gain, 0).is_err());
    }

    #[test]
    fn rope_rotates_pairs_with_one_rounding() {
        let mut head = [ONE, 3, -ONE, 5];
        // (c, s) = (0, 1): (a, b) -> (-b, a).
        rope_split_half(&mut head, &[0, ONE as i32], &[ONE as i32, 0]).unwrap();
        assert_eq!(head, [ONE, 3, ONE, 5]);
        let mut head = [7, -7];
        // c = s = 0.5: a' = (7*0.5 - (-7)*0.5) = 7, b' = (3.5 - 3.5) = 0.
        rope_split_half(&mut head, &[ONE as i32 / 2], &[ONE as i32 / 2]).unwrap();
        assert_eq!(head, [7, 0]);
        let mut head = [1, 0];
        // a' = floor(1 * 0.5) = 0, b' = floor(1 * -0.5) = -1 (floor, not trunc).
        rope_split_half(&mut head, &[ONE as i32 / 2], &[-(ONE as i32) / 2]).unwrap();
        assert_eq!(head, [0, -1]);
    }

    #[test]
    fn attention_is_two_pass_and_exact() {
        // One head of width 2, three cached positions, stride 2.
        let keys = [ONE as i32, 0, 0, ONE as i32, ONE as i32, ONE as i32];
        let values = [10 * ONE as i32, 0, 0, 10 * ONE as i32, 5, -5];
        let cache = HeadCache {
            keys: &keys,
            values: &values,
            positions: 3,
            stride: 2,
            offset: 0,
        };
        let q = [2 * ONE, 0];
        let lambda = 1i64 << 30; // scale 1.0 in Q30
        let mut out = [0i64; 2];
        attention_head(&q, cache, lambda, &mut out).unwrap();
        // scores: 2.0, 0.0, 2.0 (Q16) -> weights e^0, e^-2, e^0.
        let w_max = exp_q16(0);
        let w_low = exp_q16(-2 * ONE);
        let z = 2 * w_max + w_low;
        let o0 = w_max * 10 * ONE + w_max * 5;
        let o1 = w_low * 10 * ONE + w_max * -5;
        assert_eq!(out, [o0 / z, o1 / z]);
        // The visiting order cannot matter: reversing the positions gives
        // the same answer.
        let keys_rev = [ONE as i32, ONE as i32, 0, ONE as i32, ONE as i32, 0];
        let values_rev = [5, -5, 0, 10 * ONE as i32, 10 * ONE as i32, 0];
        let mut out_rev = [0i64; 2];
        attention_head(
            &q,
            HeadCache {
                keys: &keys_rev,
                values: &values_rev,
                ..cache
            },
            lambda,
            &mut out_rev,
        )
        .unwrap();
        assert_eq!(out, out_rev);
    }

    #[test]
    fn gated_silu_matches_its_definition() {
        assert_eq!(sigmoid_q16(0), ONE / 2);
        assert_eq!(gated_silu(0, ONE).unwrap(), 0);
        let g = 3 * ONE;
        let u = -2 * ONE;
        let expected = (i128::from(g) * i128::from(sigmoid_q16(g)) * i128::from(u)) >> 32;
        assert_eq!(i128::from(gated_silu(g, u).unwrap()), expected);
        // silu(3) = 2.8577: g*sigma(g)*u ~ -5.715 * ONE.
        assert!((gated_silu(g, u).unwrap() + 374_550).abs() < 40);
        let g = -3 * ONE;
        assert!(sigmoid_q16(g) > 0 && sigmoid_q16(g) < ONE / 10);
    }

    #[test]
    fn selection_rules_follow_the_specification() {
        let logits = [5, 9, 9, -4];
        assert_eq!(select(&logits, &[], Selection::Argmax).unwrap(), 1);
        // Penalising token 1 once: 9 -> 7 (truncating), so token 2 wins.
        assert_eq!(select(&logits, &[1], Selection::Rp64Argmax).unwrap(), 2);
        // Negative logits grow: -4 -> -4 (6 * -4 / 5 = -4.8 -> -4).
        let logits = [-5, -4];
        assert_eq!(select(&logits, &[1], Selection::Rp64Argmax).unwrap(), 1);
        // Penalised once per occurrence: -5 -> -6 -> -7, so token 0 wins.
        assert_eq!(
            select(&[-6, -5], &[1, 1], Selection::Rp64Argmax).unwrap(),
            0
        );
        assert_eq!(
            Selection::parse("rp64-argmax").unwrap(),
            Selection::Rp64Argmax
        );
        assert!(Selection::parse("top-p").is_err());
    }
}
