//! Operators of the MLA + MoE profile (spec §4.3, §4.4, §5.1–§5.6).
//!
//! Like [`crate::modern::arith`], every function computes exact integer
//! values and refuses ([`ModernError::Domain`]) instead of wrapping. Sums are
//! exact, so evaluation order, thread count, SIMD width and the split of work
//! between devices cannot change a result.

use rayon::prelude::*;

use crate::modern::ModernError;
use crate::modern::arith::{
    self, FRAC_BITS, check_projection_input, dot_i8_i64, dyadic_epilogue, exp_q16, gated_silu,
    sigmoid_q16, to_activation,
};
use crate::modern::convert::bf16_parts;

/// Row chunk for parallel projections.
const ROW_CHUNK: usize = 64;

fn domain(what: &str) -> ModernError {
    ModernError::Domain(what.to_string())
}

fn invalid(what: impl Into<String>) -> ModernError {
    ModernError::Invalid(what.into())
}

/// A borrowed per-row dyadic INT8 matrix (dyadic v1 §5.2): weights may live
/// in a memory-mapped package, scales in memory.
#[derive(Debug, Clone, Copy)]
pub struct QView<'a> {
    pub rows: usize,
    pub cols: usize,
    /// Row-major weights in `[-127, 127]`.
    pub q: &'a [i8],
    pub mu: &'a [i32],
    pub k: &'a [u8],
}

impl<'a> QView<'a> {
    fn consistent(&self) -> bool {
        self.rows > 0
            && self.cols > 0
            && self.q.len() == self.rows * self.cols
            && self.mu.len() == self.rows
            && self.k.len() == self.rows
    }

    /// Row `r` of the weights.
    pub fn row(&self, r: usize) -> &'a [i8] {
        &self.q[r * self.cols..(r + 1) * self.cols]
    }

    /// `out = W x` (dyadic v1 §5.2). The opt-in limb kernel computes the same
    /// exact row dots as the scalar loop; the epilogue is shared.
    pub fn project(&self, x: &[i64], out: &mut [i64]) -> Result<(), ModernError> {
        if !self.consistent() || x.len() != self.cols || out.len() != self.rows {
            return Err(invalid(format!(
                "projection shape: matrix {}x{}, input {}, output {}",
                self.rows,
                self.cols,
                x.len(),
                out.len()
            )));
        }
        check_projection_input(x)?;
        let simd = crate::canonical_simd::fast_canonical_kernel_enabled()
            && crate::canonical_simd::exact_row_dots_fast(self.q, self.rows, self.cols, x, out);
        if !simd {
            let (q, cols) = (self.q, self.cols);
            out.par_chunks_mut(ROW_CHUNK)
                .enumerate()
                .for_each(|(chunk_index, chunk)| {
                    let start = chunk_index * ROW_CHUNK;
                    for (offset, slot) in chunk.iter_mut().enumerate() {
                        let r = start + offset;
                        *slot = dot_i8_i64(&q[r * cols..(r + 1) * cols], x);
                    }
                });
        }
        for ((slot, &mu), &k) in out.iter_mut().zip(self.mu).zip(self.k) {
            *slot = dyadic_epilogue(*slot, mu, k)?;
        }
        Ok(())
    }

    /// Embedding row `e_j = (q_tj * mu_t) >> (k_t - 16)` (dyadic v1 §5.3).
    pub fn embed_row(&self, token: usize) -> Result<Vec<i64>, ModernError> {
        if token >= self.rows {
            return Err(domain("token id is outside the vocabulary"));
        }
        let mu = i64::from(self.mu[token]);
        let shift = u32::from(self.k[token]).saturating_sub(FRAC_BITS);
        Ok(self
            .row(token)
            .iter()
            .map(|&w| (i64::from(w) * mu) >> shift)
            .collect())
    }
}

/// `ffn(W, x) = W.down · gated_silu(W.gate x, W.up x)` (spec §5.6).
pub fn gated_ffn(
    gate: QView<'_>,
    up: QView<'_>,
    down: QView<'_>,
    x: &[i64],
) -> Result<Vec<i64>, ModernError> {
    let mut g = vec![0i64; gate.rows];
    gate.project(x, &mut g)?;
    let mut u = vec![0i64; up.rows];
    up.project(x, &mut u)?;
    for (gi, &ui) in g.iter_mut().zip(&u) {
        *gi = gated_silu(*gi, ui)?;
    }
    let mut y = vec![0i64; down.rows];
    down.project(&g, &mut y)?;
    Ok(y)
}

/// Interleaved RoPE on one vector at one position (spec §5.1): pairs
/// `(u[2i], u[2i+1])` rotated by table entry `i`, one floor shift each.
pub fn rope_interleaved(u: &mut [i64], cos: &[i32], sin: &[i32]) -> Result<(), ModernError> {
    let half = u.len() / 2;
    if u.len() != 2 * half || cos.len() != half || sin.len() != half {
        return Err(invalid(format!(
            "rope shape: vector {}, cos {}, sin {}",
            u.len(),
            cos.len(),
            sin.len()
        )));
    }
    for ((pair, &c), &s) in u.chunks_exact_mut(2).zip(cos).zip(sin) {
        let (a, b) = (i128::from(pair[0]), i128::from(pair[1]));
        let (c, s) = (i128::from(c), i128::from(s));
        pair[0] = to_activation((a * c - b * s) >> FRAC_BITS, "rope output beyond 2^62")?;
        pair[1] = to_activation((a * s + b * c) >> FRAC_BITS, "rope output beyond 2^62")?;
    }
    Ok(())
}

/// Quantise one router row to INT16 with a power-of-two scale (spec §4.3).
/// Returns the shift `k`; the row's value is `q * 2^-k`.
pub fn quantize_router_row(bits: &[u16], q: &mut [i16]) -> Result<u8, ModernError> {
    if bits.len() != q.len() {
        return Err(invalid("router row length mismatch"));
    }
    let mut max_magnitude = 0u16;
    for &b in bits {
        if (b >> 7) & 0xFF == 0xFF {
            return Err(invalid("BF16 infinity or NaN in a router row"));
        }
        max_magnitude = max_magnitude.max(b & 0x7FFF);
    }
    if max_magnitude == 0 {
        q.fill(0);
        return Ok(16);
    }
    let (_, _, e_a) = bf16_parts(max_magnitude)?;
    let k = 7 - e_a;
    if !(0..=62).contains(&k) {
        return Err(invalid(format!(
            "router row scale shift {k} is outside [0, 62]"
        )));
    }
    for (slot, &b) in q.iter_mut().zip(bits) {
        let (negative, m, e) = bf16_parts(b)?;
        let m = m as i32;
        let shift = e + k;
        let magnitude = if m == 0 {
            0
        } else if shift >= 0 {
            // e <= e_A, so shift <= 7 and m << shift <= 255 * 128.
            m << shift
        } else {
            let c = -shift;
            // For c >= 9, 2m + 2^c < 2^(c+1): the rounded value is 0.
            if c >= 16 {
                0
            } else {
                (2 * m + (1 << c)) >> (c + 1)
            }
        };
        let value = if negative { -magnitude } else { magnitude };
        *slot = i16::try_from(value).map_err(|_| invalid("router value beyond i16"))?;
    }
    Ok(k as u8)
}

/// `rha(M * 2^(e + frac))` with sign, refusing magnitudes beyond 2^62.
fn round_scaled(negative: bool, m: u64, e: i32, frac: i32) -> Result<i64, ModernError> {
    let shift = e + frac;
    let magnitude: u128 = if m == 0 {
        0
    } else if shift >= 0 {
        if shift > 62 {
            return Err(domain("correction bias beyond 2^62"));
        }
        u128::from(m) << shift
    } else {
        let c = (-shift) as u32;
        // m < 2^24, so for c > 100 the rounded value is 0.
        if c > 100 {
            0
        } else {
            (2 * u128::from(m) + (1u128 << c)) >> (c + 1)
        }
    };
    if magnitude > 1u128 << 62 {
        return Err(domain("correction bias beyond 2^62"));
    }
    let value = magnitude as i64;
    Ok(if negative { -value } else { value })
}

/// Exact parts of an IEEE binary32 value: `(negative, M, e)` with value
/// `(-1)^s * M * 2^e` (spec §4.4).
pub fn f32_parts(bits: u32) -> Result<(bool, u64, i32), ModernError> {
    let negative = bits >> 31 == 1;
    let exponent = (bits >> 23) & 0xFF;
    let mantissa = u64::from(bits & 0x7F_FFFF);
    match exponent {
        0xFF => Err(invalid("F32 infinity or NaN")),
        0 => Ok((negative, mantissa, -149)),
        e => Ok((negative, (1 << 23) + mantissa, e as i32 - 150)),
    }
}

/// `rha(v * 2^32)` of a BF16 correction bias (spec §4.4).
pub fn bf16_to_q32(bits: u16) -> Result<i64, ModernError> {
    let (negative, m, e) = bf16_parts(bits)?;
    round_scaled(negative, u64::from(m), e, 32)
}

/// `rha(v * 2^32)` of an F32 correction bias (spec §4.4).
pub fn f32_to_q32(bits: u32) -> Result<i64, ModernError> {
    let (negative, m, e) = f32_parts(bits)?;
    round_scaled(negative, m, e, 32)
}

/// Router logits `l_e = (sum_j q[e][j] x_j) >> k_e` (spec §5.3).
pub fn router_logits(q: &[i16], k: &[u8], x: &[i64], out: &mut [i64]) -> Result<(), ModernError> {
    let (experts, width) = (k.len(), x.len());
    if width == 0 || q.len() != experts * width || out.len() != experts {
        return Err(invalid("router shape"));
    }
    let mass: u128 = x.iter().map(|v| u128::from(v.unsigned_abs())).sum();
    if mass * 32_767 >= 1u128 << 63 {
        return Err(domain("router input magnitude (32767 * sum |x| >= 2^63)"));
    }
    for ((slot, row), &shift) in out.iter_mut().zip(q.chunks_exact(width)).zip(k) {
        // |w| <= 32767 and 32767 * sum |x| < 2^63 bound every partial sum.
        let mut acc = 0i64;
        for (&w, &v) in row.iter().zip(x) {
            acc += i64::from(w) * v;
        }
        *slot = to_activation(i128::from(acc >> shift), "router logit beyond 2^62")?;
    }
    Ok(())
}

/// Sigmoid scores and selection keys `sigma * 2^16 + bias` (spec §5.4).
pub fn selection_keys(logits: &[i64], bias: &[i64]) -> Result<(Vec<i64>, Vec<i64>), ModernError> {
    if logits.len() != bias.len() {
        return Err(invalid("router bias shape"));
    }
    let sigma: Vec<i64> = logits.iter().map(|&l| sigmoid_q16(l)).collect();
    // sigma <= 2^16 and |bias| <= 2^62, so the key fits i64.
    let keys = sigma
        .iter()
        .zip(bias)
        .map(|(&s, &b)| (s << 16) + b)
        .collect();
    Ok((sigma, keys))
}

/// The selected experts (spec §5.4): group limit when `n_group > 1`, then the
/// `top_k` largest keys, ties broken by the lower index; listed by key
/// descending, then index ascending.
pub fn select_experts(
    keys: &[i64],
    top_k: usize,
    n_group: usize,
    topk_group: usize,
) -> Result<Vec<usize>, ModernError> {
    let experts = keys.len();
    if experts == 0
        || n_group == 0
        || !experts.is_multiple_of(n_group)
        || topk_group == 0
        || topk_group > n_group
    {
        return Err(invalid("expert selection shape"));
    }
    let mut eligible = vec![true; experts];
    if n_group > 1 {
        let size = experts / n_group;
        if size < 2 {
            return Err(invalid("expert groups need at least two experts"));
        }
        let mut scores: Vec<(i128, usize)> = (0..n_group)
            .map(|g| {
                let mut group: Vec<i64> = keys[g * size..(g + 1) * size].to_vec();
                group.sort_unstable_by(|a, b| b.cmp(a));
                (i128::from(group[0]) + i128::from(group[1]), g)
            })
            .collect();
        scores.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
        let mut kept = vec![false; n_group];
        for &(_, g) in &scores[..topk_group] {
            kept[g] = true;
        }
        for (e, slot) in eligible.iter_mut().enumerate() {
            *slot = kept[e / size];
        }
    }
    let mut candidates: Vec<usize> = (0..experts).filter(|&e| eligible[e]).collect();
    if candidates.len() < top_k || top_k == 0 {
        return Err(invalid("fewer eligible experts than experts per token"));
    }
    candidates.sort_by(|&a, &b| keys[b].cmp(&keys[a]).then(a.cmp(&b)));
    candidates.truncate(top_k);
    Ok(candidates)
}

/// Q32 routing weights (spec §5.5) from the selected experts' sigmoid scores.
pub fn routing_weights(sigma: &[i64], rho: i64, normalize: bool) -> Vec<i64> {
    let (rho, k) = (i128::from(rho), sigma.len());
    if normalize && k > 1 {
        let total: i128 = sigma.iter().map(|&s| i128::from(s)).sum();
        if total == 0 {
            return vec![0; k];
        }
        // sigma >= 0 and rho > 0: `/` is the floor.
        sigma
            .iter()
            .map(|&s| (i128::from(s) * rho / total) as i64)
            .collect()
    } else {
        sigma
            .iter()
            .map(|&s| ((i128::from(s) * rho) >> 16) as i64)
            .collect()
    }
}

/// `out_j = (sum_e w_e y_ej) >> 32 + s_j` (spec §5.6): one exact sum, one
/// floor shift, so any split of the experts across devices gives these bytes.
pub fn combine(
    weights: &[i64],
    outputs: &[Vec<i64>],
    shared: &[i64],
    out: &mut [i64],
) -> Result<(), ModernError> {
    if weights.len() != outputs.len()
        || shared.len() != out.len()
        || outputs.iter().any(|y| y.len() != out.len())
    {
        return Err(invalid("combine shape"));
    }
    for (j, slot) in out.iter_mut().enumerate() {
        let mut acc: i128 = 0;
        for (&w, y) in weights.iter().zip(outputs) {
            acc += i128::from(w) * i128::from(y[j]);
        }
        let routed = to_activation(acc >> 32, "routed expert sum beyond 2^62")?;
        *slot = to_activation(
            i128::from(routed) + i128::from(shared[j]),
            "MoE output beyond 2^62",
        )?;
    }
    Ok(())
}

/// One layer's cache as seen by attention: latents and RoPE keys (spec §5.2).
#[derive(Debug, Clone, Copy)]
pub struct LatentCache<'a> {
    /// `positions * rank` i32 latents, row-major by position.
    pub latent: &'a [i32],
    /// `positions * rope_dim` i32 RoPE keys, row-major by position.
    pub rope_keys: &'a [i32],
    /// Positions `0 .. positions` are attended.
    pub positions: usize,
    pub rank: usize,
    pub rope_dim: usize,
}

/// Absorbed MLA attention of one head (spec §5.2): from the absorbed query
/// `qa` (C values) and the rotated RoPE query `qp` (R values), compute the
/// attention-weighted latent `u` (C values). Two-pass softmax, exact sums.
pub fn mla_attend(
    qa: &[i64],
    qp: &[i64],
    cache: LatentCache<'_>,
    lambda: i64,
    u: &mut [i64],
) -> Result<(), ModernError> {
    let (rank, rope_dim, positions) = (cache.rank, cache.rope_dim, cache.positions);
    if positions == 0
        || qa.len() != rank
        || qp.len() != rope_dim
        || u.len() != rank
        || cache.latent.len() < positions * rank
        || cache.rope_keys.len() < positions * rope_dim
    {
        return Err(invalid("attention cache shape"));
    }
    // Cached entries are i32, so sum |q| < 2^32 bounds every dot below 2^63.
    let mass: u128 = qa
        .iter()
        .chain(qp)
        .map(|v| u128::from(v.unsigned_abs()))
        .sum();
    let narrow = mass < 1u128 << 32;
    let mut scores = Vec::with_capacity(positions);
    for i in 0..positions {
        let latent = &cache.latent[i * rank..(i + 1) * rank];
        let key = &cache.rope_keys[i * rope_dim..(i + 1) * rope_dim];
        let dot: i128 = if narrow {
            let mut sum = 0i64;
            for (&a, &b) in qa.iter().zip(latent) {
                sum += a * i64::from(b);
            }
            for (&a, &b) in qp.iter().zip(key) {
                sum += a * i64::from(b);
            }
            i128::from(sum)
        } else {
            let mut sum = 0i128;
            for (&a, &b) in qa.iter().zip(latent) {
                sum += i128::from(a) * i128::from(b);
            }
            for (&a, &b) in qp.iter().zip(key) {
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
    let mut acc = vec![0i128; rank];
    for (i, &score) in scores.iter().enumerate() {
        let w = exp_q16(score - max_score);
        if w == 0 {
            continue;
        }
        total += w;
        let latent = &cache.latent[i * rank..(i + 1) * rank];
        for (a, &v) in acc.iter_mut().zip(latent) {
            *a += i128::from(w) * i128::from(v);
        }
    }
    // The maximum contributes exp(0) = 2^16, so total > 0; `/` truncates
    // toward zero (tdiv).
    for (slot, &a) in u.iter_mut().zip(&acc) {
        *slot = to_activation(a / i128::from(total), "attention output beyond 2^62")?;
    }
    Ok(())
}

/// Add `delta` to the residual stream exactly (dyadic v1 §5.8).
pub fn add_residual(h: &mut [i64], delta: &[i64]) -> Result<(), ModernError> {
    arith::add_residual(h, delta)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::modern::arith::ONE;

    fn bf16(value: f32) -> u16 {
        (value.to_bits() >> 16) as u16
    }

    #[test]
    fn interleaved_rope_rotates_adjacent_pairs() {
        // (c, s) = (0, 1): (a, b) -> (-b, a) on each pair.
        let mut u = [ONE, 3, -ONE, 5];
        let c = [0, 0];
        let s = [ONE as i32, ONE as i32];
        rope_interleaved(&mut u, &c, &s).unwrap();
        assert_eq!(u, [-3, ONE, -5, -ONE]);
        // Floor, not truncation: (1 * 0.5) >> 0 -> 0, (1 * -0.5) -> -1.
        let mut u = [1, 0];
        rope_interleaved(&mut u, &[ONE as i32 / 2], &[-(ONE as i32) / 2]).unwrap();
        assert_eq!(u, [0, -1]);
        assert!(rope_interleaved(&mut [1, 2, 3], &[0], &[0]).is_err());
    }

    #[test]
    fn router_rows_are_exact_within_seven_binades() {
        // max 1.0 = 128 * 2^-7, so k = 14.
        let row = [
            bf16(1.0),
            bf16(-0.5),
            bf16(2f32.powi(-10)),
            0x3B81, // 129 * 2^-15: 64.5 rounds half away to 65
            0xBB81, // the negative of it: -65
            0x0001, // subnormal far below the maximum
            0x8000, // negative zero
        ];
        let mut q = [0i16; 7];
        let k = quantize_router_row(&row, &mut q).unwrap();
        assert_eq!(k, 14);
        assert_eq!(q, [16_384, -8_192, 16, 65, -65, 0, 0]);
        // The row maximum maps to 128 * M_A.
        let mut q = [0i16; 2];
        assert_eq!(
            quantize_router_row(&[bf16(-1.9921875), bf16(0.25)], &mut q).unwrap(),
            14
        );
        assert_eq!(q, [-32_640, 4_096]);
        // Zero rows and NaN.
        let mut q = [7i16; 2];
        assert_eq!(quantize_router_row(&[0, 0x8000], &mut q).unwrap(), 16);
        assert_eq!(q, [0, 0]);
        assert!(quantize_router_row(&[0x7FC0], &mut [0i16; 1]).is_err());
        // A row maximum of 2^15 or more needs k < 0: refused.
        assert!(quantize_router_row(&[bf16(65_536.0)], &mut [0i16; 1]).is_err());
        assert_eq!(
            quantize_router_row(&[bf16(256.0)], &mut [0i16; 1]).unwrap(),
            6
        );
    }

    #[test]
    fn router_logits_are_floor_shifted_exact_sums() {
        let q = [16_384i16, -8_192, 1, 1];
        let k = [14u8, 0];
        let x = [ONE, ONE];
        let mut out = [0i64; 2];
        router_logits(&q, &k, &x, &mut out).unwrap();
        // 1.0 * 1 - 0.5 * 1 = 0.5; second row: 2 * ONE >> 0.
        assert_eq!(out, [ONE / 2, 2 * ONE]);
        // Floor of a negative value: -3 >> 1 = -2.
        router_logits(&[-3], &[1], &[1], &mut out[..1]).unwrap();
        assert_eq!(out[0], -2);
        let huge = [1i64 << 49, 0];
        assert!(matches!(
            router_logits(&q, &k, &huge, &mut out),
            Err(ModernError::Domain(_))
        ));
    }

    #[test]
    fn biases_round_half_away_in_q32() {
        assert_eq!(bf16_to_q32(bf16(1.0)).unwrap(), 1 << 32);
        assert_eq!(bf16_to_q32(bf16(-0.5)).unwrap(), -(1 << 31));
        assert_eq!(f32_to_q32(1.0f32.to_bits()).unwrap(), 1 << 32);
        // 2^-33 is half a Q32 unit: away from zero.
        assert_eq!(f32_to_q32(2f32.powi(-33).to_bits()).unwrap(), 1);
        assert_eq!(f32_to_q32((-(2f32.powi(-33))).to_bits()).unwrap(), -1);
        assert_eq!(f32_to_q32(2f32.powi(-34).to_bits()).unwrap(), 0);
        // 1.5 * 2^-32 -> 1.5 units -> 2.
        assert_eq!(f32_to_q32((1.5 * 2f32.powi(-32)).to_bits()).unwrap(), 2);
        // A subnormal F32 is 0; 2^31 is too large; NaN is refused.
        assert_eq!(f32_to_q32(1).unwrap(), 0);
        assert!(f32_to_q32(2f32.powi(31).to_bits()).is_err());
        assert!(f32_to_q32(f32::NAN.to_bits()).is_err());
        assert_eq!(f32_parts(0x3F80_0000).unwrap(), (false, 1 << 23, -23));
    }

    #[test]
    fn selection_breaks_ties_by_the_lower_index() {
        let keys = [5, 9, 9, 1, 9];
        assert_eq!(select_experts(&keys, 2, 1, 1).unwrap(), vec![1, 2]);
        assert_eq!(select_experts(&keys, 3, 1, 1).unwrap(), vec![1, 2, 4]);
        assert_eq!(select_experts(&keys, 5, 1, 1).unwrap(), vec![1, 2, 4, 0, 3]);
        assert!(select_experts(&keys, 6, 1, 1).is_err());
    }

    #[test]
    fn group_limited_routing_keeps_the_best_groups() {
        // Groups of two: sums 3, 10, 6, 18 -> keep groups 3 and 1.
        let keys = [1, 2, 10, 0, 3, 3, 9, 9];
        assert_eq!(select_experts(&keys, 3, 4, 2).unwrap(), vec![2, 6, 7]);
        // Tied group scores: the lower group index is kept.
        let keys = [5, 5, 4, 6, 1, 1, 0, 0];
        assert_eq!(select_experts(&keys, 2, 4, 1).unwrap(), vec![0, 1]);
        assert_eq!(select_experts(&keys, 2, 4, 2).unwrap(), vec![3, 0]);
    }

    #[test]
    fn routing_weights_are_floor_divided_q32() {
        let rho = 1i64 << 32;
        let w = routing_weights(&[30_000, 20_000, 10_000], rho, true);
        assert_eq!(w, vec![2_147_483_648, 1_431_655_765, 715_827_882]);
        assert_eq!(routing_weights(&[0, 0], rho, true), vec![0, 0]);
        // Without normalisation (or with k = 1): sigma * rho >> 16.
        assert_eq!(routing_weights(&[ONE / 2], 3 * rho, true), vec![3 << 31]);
        assert_eq!(
            routing_weights(&[ONE, 1], rho, false),
            vec![1 << 32, 1 << 16]
        );
        // Moonlight's rho with two equal scores: half of 2.446 each.
        let rho = 10_505_490_006;
        assert_eq!(
            routing_weights(&[40_000, 40_000], rho, true),
            vec![5_252_745_003, 5_252_745_003]
        );
    }

    #[test]
    fn combine_rounds_once_after_the_exact_sum() {
        let weights = [1i64 << 31, 1 << 30];
        let outputs = vec![vec![4, -4], vec![8, 1]];
        let shared = [1, 1];
        let mut out = [0i64; 2];
        combine(&weights, &outputs, &shared, &mut out).unwrap();
        // 0.5*4 + 0.25*8 = 4; 0.5*-4 + 0.25*1 = -1.75 -> floor -2.
        assert_eq!(out, [5, -1]);
        // The expert order cannot change the result.
        let mut swapped = [0i64; 2];
        combine(
            &[weights[1], weights[0]],
            &[outputs[1].clone(), outputs[0].clone()],
            &shared,
            &mut swapped,
        )
        .unwrap();
        assert_eq!(out, swapped);
        // Rounding each expert separately would differ: floor(-2) + floor(0.25)
        // = -2, but floor(-1.75) = -2 here too; with three terms it differs.
        let w = [1i64 << 31; 3];
        let ys = vec![vec![1], vec![1], vec![1]];
        let mut one = [0i64];
        combine(&w, &ys, &[0], &mut one).unwrap();
        assert_eq!(one, [1]); // floor(1.5) = 1, not 3 * floor(0.5) = 0
    }

    #[test]
    fn mla_attention_is_exact_and_order_free() {
        // Two positions, rank 2, rope width 2.
        let latent = [ONE as i32, 0, 0, 2 * ONE as i32];
        let rope_keys = [0, ONE as i32, ONE as i32, 0];
        let qa = [ONE, ONE];
        let qp = [ONE, 0];
        let lambda = 1i64 << 30;
        let cache = LatentCache {
            latent: &latent,
            rope_keys: &rope_keys,
            positions: 2,
            rank: 2,
            rope_dim: 2,
        };
        let mut u = [0i64; 2];
        mla_attend(&qa, &qp, cache, lambda, &mut u).unwrap();
        // dot_0 = 1*1 + 0 + (1*0 + 0) = 1.0; dot_1 = 2 + 1 = 3.0 (Q32 -> Q16).
        let (s0, s1) = (ONE, 3 * ONE);
        let w0 = exp_q16(s0 - s1);
        let w1 = exp_q16(0);
        let z = w0 + w1;
        // Position 0 contributes only to latent 0, position 1 only to latent 1.
        let expect = [(w0 * ONE) / z, (w1 * 2 * ONE) / z];
        assert_eq!(u, expect);
        // Reversed position order: same result.
        let latent_rev = [0, 2 * ONE as i32, ONE as i32, 0];
        let keys_rev = [ONE as i32, 0, 0, ONE as i32];
        let mut u_rev = [0i64; 2];
        mla_attend(
            &qa,
            &qp,
            LatentCache {
                latent: &latent_rev,
                rope_keys: &keys_rev,
                ..cache
            },
            lambda,
            &mut u_rev,
        )
        .unwrap();
        assert_eq!(u, u_rev);
        // Wide queries take the i128 path with the same answer.
        let big = [1i64 << 40, 0];
        let mut a = [0i64; 2];
        mla_attend(&big, &qp, cache, 1, &mut a).unwrap();
        assert!(a[0] > 0);
    }

    #[test]
    fn projections_from_views_match_the_owned_matrix() {
        let q: Vec<i8> = (0..12).map(|i| (i * 7 % 255 - 127) as i8).collect();
        let mu = vec![1 << 30, (1 << 30) + 12_345, 1 << 30];
        let k = vec![46, 40, 50];
        let view = QView {
            rows: 3,
            cols: 4,
            q: &q,
            mu: &mu,
            k: &k,
        };
        let owned = crate::modern::arith::DyadicMatrix {
            rows: 3,
            cols: 4,
            q: q.clone(),
            mu: mu.clone(),
            k: k.clone(),
        };
        let x = [ONE, -3 * ONE, 77, 5 * ONE];
        let mut a = [0i64; 3];
        let mut b = [0i64; 3];
        view.project(&x, &mut a).unwrap();
        crate::modern::arith::project(&owned, &x, &mut b).unwrap();
        assert_eq!(a, b);
        assert_eq!(
            view.embed_row(1).unwrap(),
            crate::modern::arith::embed_row(&owned, 1).unwrap()
        );
    }
}
