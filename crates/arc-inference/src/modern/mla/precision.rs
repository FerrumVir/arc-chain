//! Versioned, per-class BF16 matrix precision. See INT16-CONTRACT.md.
use crate::modern::{ModernError, arith::dyadic_epilogue, convert::bf16_parts};
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Bits {
    Int8,
    Int16,
}

/// All fields are mandatory, and unknown fields/versions are rejected.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Precision {
    pub version: u8,
    pub attention: Bits,
    pub dense: Bits,
    pub shared: Bits,
    pub embedding: Bits,
    pub head: Bits,
}
impl Precision {
    pub fn all_int16() -> Self {
        Self {
            version: 1,
            attention: Bits::Int16,
            dense: Bits::Int16,
            shared: Bits::Int16,
            embedding: Bits::Int16,
            head: Bits::Int16,
        }
    }
    pub fn from_json(v: &Value) -> Result<Self, ModernError> {
        let p: Self = serde_json::from_value(v.clone())
            .map_err(|e| ModernError::Invalid(format!("precision: {e}")))?;
        p.validate()?;
        Ok(p)
    }
    pub fn validate(&self) -> Result<(), ModernError> {
        if self.version != 1 {
            return Err(ModernError::Invalid("precision version must be 1".into()));
        }
        Ok(())
    }
    /// Canonical package tensor name, including .q/.mu/.k suffixes.
    pub fn canonical(&self, name: &str) -> Bits {
        if name.starts_with("embed.") {
            self.embedding
        } else if name.starts_with("lm_head.") {
            self.head
        } else if name.contains(".experts.") || name.contains("router") {
            Bits::Int8
        } else if name.contains(".shared.") {
            self.shared
        } else if [".w_gate.", ".w_up.", ".w_down."]
            .iter()
            .any(|s| name.contains(s))
        {
            self.dense
        } else {
            self.attention
        }
    }
    pub fn source(&self, name: &str) -> Bits {
        if name == "model.embed_tokens.weight" {
            self.embedding
        } else if name == "lm_head.weight" {
            self.head
        } else if name.contains(".experts.") {
            Bits::Int8
        } else if name.contains(".shared_experts.") {
            self.shared
        } else if name.contains(".mlp.") {
            self.dense
        } else {
            self.attention
        }
    }
}
const PROFILES: [(&str, &str); 5] = [
    (
        super::PROFILE,
        "arc.hf-deepseek-v3.mla-moe.mixed-dyadic-row.q16.v1",
    ),
    (
        super::PROFILE_I4G32,
        "arc.hf-deepseek-v3.mla-moe.mixed-dyadic-row.i4g32-experts.q16.v1",
    ),
    (
        super::yarn::PROFILE,
        "arc.kimi-k26.mla-moe.mixed-dyadic-row.i4g32.yarn.q16.v1",
    ),
    (
        super::yarn::PROBE_PROFILE,
        "arc.experimental.kimi-k26.early-layers-head.mixed-dyadic-row.i4g32.yarn.q16.v1",
    ),
    (
        super::yarn::FIXTURE_PROFILE,
        "arc.synthetic.kimi-k26-yarn.mixed-dyadic-row.i4g32.q16.v1",
    ),
];
pub fn legacy_profile(p: &str) -> &str {
    PROFILES
        .iter()
        .find(|(_, mixed)| *mixed == p)
        .map_or(p, |(base, _)| base)
}
pub fn mixed_profile(p: &'static str) -> &'static str {
    PROFILES
        .iter()
        .find(|(base, _)| *base == p)
        .expect("known MLA profile")
        .1
}

/// INT16 symmetric row calibration, all integer; ties away from zero.
/// Scales are mu * 2^-k, mu in [2^30,2^31), k in [16,62].
pub fn quantize_row(bits: &[u16], q: &mut [i16]) -> Result<(i32, u8), ModernError> {
    if bits.len() != q.len() || bits.is_empty() {
        return Err(ModernError::Invalid("INT16 row shape".into()));
    }
    let mut maximum = 0;
    for &b in bits {
        bf16_parts(b)?;
        maximum = maximum.max(b & 0x7fff);
    }
    if maximum == 0 {
        q.fill(0);
        return Ok((0, 16));
    }
    let (_, ma, ea) = bf16_parts(maximum)?;
    let ma = u64::from(ma);
    for (slot, &b) in q.iter_mut().zip(bits) {
        let (negative, mj, ej) = bf16_parts(b)?;
        let d = ea - ej;
        if mj == 0 || d > 60 {
            *slot = 0;
            continue;
        }
        let denominator = u128::from(ma) << d;
        let magnitude = ((2 * 32767 * u128::from(mj) + denominator) / (2 * denominator)) as i16;
        *slot = if negative { -magnitude } else { magnitude };
    }
    let mut t = 0;
    while (ma << t) < 32767u64 << 30 {
        t += 1;
    }
    let mut mu = (2 * (ma << t) + 32767) / 65534;
    if mu == 1 << 31 {
        mu = 1 << 30;
        t -= 1;
    }
    let k = t - ea;
    if !(16..=62).contains(&k) {
        return Err(ModernError::Invalid(format!(
            "INT16 row scale shift {k} outside [16,62]"
        )));
    }
    Ok((mu as i32, k as u8))
}
pub struct Matrix {
    pub q: Vec<u8>,
    pub mu: Vec<i32>,
    pub k: Vec<u8>,
}
pub fn quantize_matrix(
    bits: &[u16],
    rows: usize,
    cols: usize,
    precision: Bits,
) -> Result<Matrix, ModernError> {
    if rows == 0 || cols == 0 || rows.checked_mul(cols) != Some(bits.len()) {
        return Err(ModernError::Invalid("matrix shape".into()));
    }
    if precision == Bits::Int8 {
        let m = crate::modern::convert::quantize_matrix(bits, rows, cols)?;
        return Ok(Matrix {
            q: crate::modern::package::i8_bytes(&m.q),
            mu: m.mu,
            k: m.k,
        });
    }
    let mut m = Matrix {
        q: Vec::with_capacity(bits.len() * 2),
        mu: Vec::with_capacity(rows),
        k: Vec::with_capacity(rows),
    };
    let mut q = vec![0i16; cols];
    for row in bits.chunks_exact(cols) {
        let (mu, k) = quantize_row(row, &mut q)?;
        m.q.extend(q.iter().flat_map(|v| v.to_le_bytes()));
        m.mu.push(mu);
        m.k.push(k);
    }
    Ok(m)
}

/// Exact INT16 dots. Three signed base-128 weight limbs use existing AVX2/NEON
/// kernels. At most 64 rows of limbs are materialized; unsupported hosts fall
/// back to scalar. A conservative bound prevents every i64 partial overflow.
#[allow(clippy::too_many_arguments)]
pub fn project_i16(
    q: &[u8],
    rows: usize,
    cols: usize,
    mu: &[i32],
    k: &[u8],
    x: &[i64],
    out: &mut [i64],
) -> Result<(), ModernError> {
    if rows.checked_mul(cols).and_then(|n| n.checked_mul(2)) != Some(q.len())
        || x.len() != cols
        || out.len() != rows
        || mu.len() != rows
        || k.len() != rows
        || cols == 0
        || rows == 0
    {
        return Err(ModernError::Invalid("INT16 projection shape".into()));
    }
    let sum: u128 = x.iter().map(|v| u128::from(v.unsigned_abs())).sum();
    if sum >= (1u128 << 63) / 32767 {
        return Err(ModernError::Domain(
            "INT16 projection accumulator bound".into(),
        ));
    }
    if q.chunks_exact(2).any(|v| v == [0, 128])
        || mu
            .iter()
            .zip(k)
            .any(|(&m, &s)| !((m == 0 && s == 16) || (m >= 1 << 30 && (16..=62).contains(&s))))
    {
        return Err(ModernError::Invalid("INT16 weight/scale domain".into()));
    }
    for (chunk_index, chunk) in out.chunks_mut(64).enumerate() {
        let start = chunk_index * 64;
        let weights = &q[start * cols * 2..(start + chunk.len()) * cols * 2];
        let mut used_simd = false;
        if crate::canonical_simd::fast_canonical_kernel_enabled() {
            let mut limbs = [
                vec![0i8; chunk.len() * cols],
                vec![0i8; chunk.len() * cols],
                vec![0i8; chunk.len() * cols],
            ];
            for (i, bytes) in weights.chunks_exact(2).enumerate() {
                let mut v = i32::from(i16::from_le_bytes([bytes[0], bytes[1]]));
                for limb in &mut limbs {
                    limb[i] = (v % 128) as i8;
                    v /= 128;
                }
            }
            let mut dots = [
                vec![0i64; chunk.len()],
                vec![0i64; chunk.len()],
                vec![0i64; chunk.len()],
            ];
            used_simd = limbs.iter().zip(&mut dots).all(|(limb, dot)| {
                crate::canonical_simd::exact_row_dots_fast(limb, chunk.len(), cols, x, dot)
            });
            if used_simd {
                for (i, slot) in chunk.iter_mut().enumerate() {
                    *slot = dots[0][i] + 128 * dots[1][i] + 16384 * dots[2][i];
                }
            }
        }
        if !used_simd {
            for (row, slot) in weights.chunks_exact(cols * 2).zip(chunk.iter_mut()) {
                *slot = row
                    .chunks_exact(2)
                    .zip(x)
                    .map(|(v, &x)| i64::from(i16::from_le_bytes([v[0], v[1]])) * x)
                    .sum();
            }
        }
    }
    for ((v, &m), &s) in out.iter_mut().zip(mu).zip(k) {
        *v = dyadic_epilogue(*v, m, s)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn int16_conversion_ties_zeros_nonfinite_and_scale_domains() {
        let mut q = [0; 6];
        let scale = quantize_row(&[0x3f80, 0xbf80, 0x3f00, 0xbf00, 0, 0x8000], &mut q).unwrap();
        assert_eq!(q, [32767, -32767, 16384, -16384, 0, 0]);
        // Direct rational reconstruction of the positive row maximum.
        assert_eq!(scale, (1073774593, 45));
        assert_eq!(quantize_row(&[0, 0x8000], &mut [7; 2]).unwrap(), (0, 16));
        for bad in [0x7f80, 0xff80, 0x7fc1, 0x7f7f, 1] {
            assert!(quantize_row(&[bad], &mut [0]).is_err());
        }
        // Exhaust all finite BF16 entries against an independent exact rational
        // rule for a row with maximum 1. Includes signed subnormals and ties.
        let bits: Vec<u16> = (0..=u16::MAX).filter(|b| b & 0x7fff <= 0x3f80).collect();
        let mut output = vec![0; bits.len()];
        quantize_row(&bits, &mut output).unwrap();
        for (&b, &got) in bits.iter().zip(&output) {
            let (neg, m, e) = bf16_parts(b).unwrap();
            let want = if e < -100 {
                0
            } else {
                let n = u128::from(m) * 32767;
                let divisor = 1u128 << (-e);
                ((n + divisor / 2) / divisor) as i16
            };
            assert_eq!(got, if neg { -want } else { want }, "{b:x}");
        }
    }
    #[test]
    fn int16_dots_match_i128_oracle_scalar_simd_and_reject_overflow() {
        let _guard = crate::canonical_simd::kernel_switch_guard();
        let cols = 33;
        let values: Vec<i16> = (0..cols * 67)
            .map(|i| ((i * 997) % 65535) as i32 - 32767)
            .map(|v| v as i16)
            .collect();
        let q: Vec<u8> = values.iter().flat_map(|v| v.to_le_bytes()).collect();
        for x in [
            vec![1234567; cols],
            (0..cols).map(|i| (i as i64 - 16) * 456789).collect(),
            vec![1 << 36; cols],
        ] {
            let expected: Vec<i64> = values
                .chunks_exact(cols)
                .map(|row| {
                    let dot: i128 = row
                        .iter()
                        .zip(&x)
                        .map(|(&w, &a)| i128::from(w) * i128::from(a))
                        .sum();
                    ((dot * (1 << 30)) >> 45) as i64
                })
                .collect();
            for fast in [false, true] {
                crate::canonical_simd::set_fast_canonical_kernel(fast);
                let mut got = vec![0; 67];
                project_i16(&q, 67, cols, &[1 << 30; 67], &[45; 67], &x, &mut got).unwrap();
                assert_eq!(got, expected);
            }
        }
        crate::canonical_simd::set_fast_canonical_kernel(false);
        assert!(project_i16(&[1, 0], 1, 1, &[1 << 30], &[16], &[i64::MAX], &mut [0]).is_err());
        assert!(project_i16(&[0, 128], 1, 1, &[1 << 30], &[45], &[1], &mut [0]).is_err());
        assert!(project_i16(&[1, 0], 1, 1, &[1 << 30], &[255], &[1], &mut [0]).is_err());
    }
    #[test]
    fn precision_schema_is_exact_and_versioned() {
        let p = Precision::all_int16();
        let v = serde_json::to_value(&p).unwrap();
        assert_eq!(Precision::from_json(&v).unwrap(), p);
        for field in [
            "attention",
            "dense",
            "shared",
            "embedding",
            "head",
            "version",
        ] {
            let mut bad = v.clone();
            bad.as_object_mut().unwrap().remove(field);
            assert!(Precision::from_json(&bad).is_err());
        }
        for (field, value) in [
            ("version", serde_json::json!(2)),
            ("experts", serde_json::json!("int16")),
            ("head", serde_json::json!("bf16")),
        ] {
            let mut bad = v.clone();
            bad[field] = value;
            assert!(Precision::from_json(&bad).is_err());
        }
        assert_eq!(p.canonical("layers.1.experts.w_gate.q"), Bits::Int8);
    }
}
