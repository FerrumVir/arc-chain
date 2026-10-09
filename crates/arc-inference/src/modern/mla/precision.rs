//! Versioned, per-class BF16 matrix precision. See INT16-CONTRACT.md.
use crate::canonical_simd::LimbBlocks;
use crate::modern::{ModernError, arith::dyadic_epilogue, convert::bf16_parts};
use rayon::prelude::*;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::ops::Range;

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

/// BF16 encodings of the inclusive lower / exclusive upper nonzero row maximum.
/// This admission rule applies only to dyadic INT16 matrices, not router/norms.
pub const INT16_MIN_MAX: u16 = 0x3700; // 2^-17
pub const INT16_MAX_MAX: u16 = 0x4e80; // 2^30

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
    // Validate before writing q: an unsupported nonzero row must never look
    // like a successfully converted (or silently flushed) row to a caller.
    if !(INT16_MIN_MAX..INT16_MAX_MAX).contains(&maximum) {
        return Err(ModernError::Invalid(format!(
            "INT16 nonzero row maximum BF16 0x{maximum:04x} outside [2^-17,2^30); no flush/clamp/fallback"
        )));
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
    for (index, row) in bits.chunks_exact(cols).enumerate() {
        let (mu, k) = quantize_row(row, &mut q)
            .map_err(|e| ModernError::Invalid(format!("conversion row {index}: {e}")))?;
        m.q.extend(q.iter().flat_map(|v| v.to_le_bytes()));
        m.mu.push(mu);
        m.k.push(k);
    }
    Ok(m)
}

/// Row-major little-endian INT16 matrix weights, every value in
/// `[-32767, 32767]`.
///
/// -32768 is not a profile value: the converter never writes it, and the
/// projection's accumulator bound (`32767 * sum|x| < 2^63`) needs
/// `|w| <= 32767`. The rule is checked once, when weights are admitted, so no
/// projection rescans its matrix. [`I16Weights::new`] scans the bytes it is
/// given. The stage loader (`model.rs`, `Loader::mat`) scans every INT16
/// matrix of a package before the only caller of `I16Weights::admitted`
/// (`MatRef::view`) can view it, and debug builds repeat that scan at every
/// view.
///
/// Like the INT8 `-128` rule, the check covers the bytes as admitted. A
/// mapped package changed on disk afterwards is outside the profile: it can
/// change values, never memory safety.
#[derive(Debug, Clone, Copy)]
pub struct I16Weights<'a> {
    bytes: &'a [u8],
}

impl<'a> I16Weights<'a> {
    /// Admit `bytes` after a full scan: an odd length or any -32768 refuses.
    pub fn new(bytes: &'a [u8]) -> Result<Self, ModernError> {
        if !bytes.len().is_multiple_of(2) {
            return Err(ModernError::Invalid(
                "INT16 weights need an even byte count".into(),
            ));
        }
        if holds_int16_min(bytes) {
            return Err(ModernError::Invalid(
                "INT16 weight/scale domain: -32768 is not a profile value".into(),
            ));
        }
        Ok(Self { bytes })
    }

    /// Bytes of a package matrix that the stage loader already scanned.
    pub(super) fn admitted(bytes: &'a [u8]) -> Self {
        debug_assert!(
            bytes.len().is_multiple_of(2) && !holds_int16_min(bytes),
            "INT16 view of bytes the stage loader did not admit"
        );
        Self { bytes }
    }

    /// The little-endian bytes, two per weight.
    pub fn as_bytes(&self) -> &'a [u8] {
        self.bytes
    }

    /// Rows `rows` of these weights read as a matrix of `cols` columns. Whole
    /// rows of admitted weights start at even offsets, so they are admitted
    /// too: no scan.
    ///
    /// # Panics
    /// If the rows are not inside the weights.
    pub fn rows(&self, rows: Range<usize>, cols: usize) -> Self {
        Self {
            bytes: &self.bytes[rows.start * cols * 2..rows.end * cols * 2],
        }
    }
}

/// Whether any little-endian INT16 value of `bytes`, read at even offsets,
/// is -32768. The parallel chunks have an even size, so pairs stay aligned.
pub(crate) fn holds_int16_min(bytes: &[u8]) -> bool {
    bytes
        .par_chunks(1 << 20)
        .any(|chunk| chunk.chunks_exact(2).any(|v| v == [0, 128]))
}

/// Rows per task of the INT16 dots, as in the INT8 projections.
const ROW_CHUNK: usize = 64;
/// Columns per weight-limb block: one row's three i8 limb blocks live on the
/// stack (6 KiB) next to one block of activation digits (8 KiB).
const COL_BLOCK: usize = 2048;
/// Smaller work runs on the calling thread: below about a quarter million
/// weights a thread-pool round trip costs more than it saves. The per-head MLA
/// `wk_b`/`wv_b` slices are smaller, so an INT16 layer runs its heads in
/// parallel instead (`model.rs`, `StageModel::head_schedule`).
const PARALLEL_MIN_WEIGHTS: usize = 1 << 18;

/// How independent tasks (the row chunks of a projection, the heads of a
/// layer) are scheduled. Each task writes only its own outputs as exact
/// integer sums, so no schedule can change a value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Schedule {
    /// Tasks one after another on the calling thread, with no pool.
    Serial,
    /// Tasks of the current rayon pool: the global pool (sized by
    /// `arc-mla --threads N`) unless the caller installed another one.
    Pool,
}

impl Schedule {
    /// The pool for work of at least 2^18 weights, the calling thread below.
    pub(crate) fn for_work(weights: usize) -> Self {
        if weights >= PARALLEL_MIN_WEIGHTS {
            Schedule::Pool
        } else {
            Schedule::Serial
        }
    }
}

/// Exact INT16 projection: row dots, then the shared dyadic epilogue.
///
/// The weights were admitted once ([`I16Weights`]), so no call rescans them.
/// Matrices of at least 2^18 weights compute their rows in parallel over
/// disjoint 64-row chunks of the current rayon pool. With the opt-in limb
/// kernel, activation digits are split once per projection and each weight
/// block is split on the stack into three signed base-128 limbs for the
/// existing AVX2/NEON exact INT8 kernels. Otherwise, or on a refusal, the
/// scalar kernel computes the same integers. A conservative bound prevents
/// every i64 partial overflow.
#[allow(clippy::too_many_arguments)]
pub fn project_i16(
    q: I16Weights<'_>,
    rows: usize,
    cols: usize,
    mu: &[i32],
    k: &[u8],
    x: &[i64],
    out: &mut [i64],
) -> Result<(), ModernError> {
    let schedule = Schedule::for_work(rows.saturating_mul(cols));
    project_i16_scheduled(q, rows, cols, mu, k, x, out, schedule)
}

/// [`project_i16`] with an explicit schedule, so tests can run every one.
#[allow(clippy::too_many_arguments)]
pub(crate) fn project_i16_scheduled(
    q: I16Weights<'_>,
    rows: usize,
    cols: usize,
    mu: &[i32],
    k: &[u8],
    x: &[i64],
    out: &mut [i64],
    schedule: Schedule,
) -> Result<(), ModernError> {
    let q = q.as_bytes();
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
    // The weight half of this domain is carried by `I16Weights`.
    if mu
        .iter()
        .zip(k)
        .any(|(&m, &s)| !((m == 0 && s == 16) || (m >= 1 << 30 && (16..=62).contains(&s))))
    {
        return Err(ModernError::Invalid("INT16 weight/scale domain".into()));
    }
    // Opt-in exact Metal GEMV: compiled only with the `metal-exact` feature,
    // and used only when ARC_METAL_EXACT_I16=1 (or set_metal_exact_i16) is on
    // AND the call runs inside a MetalI16Model scope that uploaded these
    // weights. It computes the same exact row dots as the kernels below, or
    // refuses without writing and leaves them to the kernels below; the shared
    // epilogue finishes either. See `super::metal_i16`.
    #[cfg(all(feature = "metal-exact", target_os = "macos", target_arch = "aarch64"))]
    if super::metal_i16::metal_exact_i16_requested()
        && super::metal_i16::try_dots(q, rows, cols, x, out)
    {
        return epilogue(out, mu, k);
    }
    // Activation digits once per projection. `None` (kernel off, no vector
    // backend, or an activation outside the four-digit domain) means scalar.
    let digits = if crate::canonical_simd::fast_canonical_kernel_enabled() {
        LimbBlocks::new(x, COL_BLOCK)
    } else {
        None
    };
    let digits = digits.as_ref();
    let task = |chunk_index: usize, dots: &mut [i64]| {
        let start = chunk_index * ROW_CHUNK;
        let weights = &q[start * cols * 2..(start + dots.len()) * cols * 2];
        row_dots(weights, cols, x, digits, dots);
    };
    match schedule {
        Schedule::Serial => out
            .chunks_mut(ROW_CHUNK)
            .enumerate()
            .for_each(|(chunk_index, dots)| task(chunk_index, dots)),
        Schedule::Pool => out
            .par_chunks_mut(ROW_CHUNK)
            .enumerate()
            .for_each(|(chunk_index, dots)| task(chunk_index, dots)),
    }
    epilogue(out, mu, k)
}

/// The dyadic epilogue of every INT16 projection path, row by row in place:
/// `floor(dot * mu / 2^k)` in i128, refusing an output beyond 2^62.
fn epilogue(out: &mut [i64], mu: &[i32], k: &[u8]) -> Result<(), ModernError> {
    for ((v, &m), &s) in out.iter_mut().zip(mu).zip(k) {
        *v = dyadic_epilogue(*v, m, s)?;
    }
    Ok(())
}

/// Exact `dots[r] = sum_j w[r][j] * x[j]` for the rows of one task.
fn row_dots(weights: &[u8], cols: usize, x: &[i64], digits: Option<&LimbBlocks>, dots: &mut [i64]) {
    let rows = weights.chunks_exact(cols * 2);
    let Some(digits) = digits else {
        for (dot, row) in dots.iter_mut().zip(rows) {
            *dot = dot_i16(row, x);
        }
        return;
    };
    // One stack scratch per task (6 KiB), reused by every row: no allocation.
    let mut limbs = [[0i8; COL_BLOCK]; 3];
    for (dot, row) in dots.iter_mut().zip(rows) {
        *dot = dot_i16_limbs(row, digits, &mut limbs);
    }
}

/// Exact `sum_j w_j x_j` of one little-endian INT16 row. `|w| <= 32767` and
/// the caller's `32767 * sum|x| < 2^63` bound every partial sum, in any order.
fn dot_i16(row: &[u8], x: &[i64]) -> i64 {
    let mut lanes = [0i64; 4];
    let mut row_chunks = row.chunks_exact(8);
    let mut x_chunks = x.chunks_exact(4);
    for (w, v) in (&mut row_chunks).zip(&mut x_chunks) {
        lanes[0] += i64::from(i16::from_le_bytes([w[0], w[1]])) * v[0];
        lanes[1] += i64::from(i16::from_le_bytes([w[2], w[3]])) * v[1];
        lanes[2] += i64::from(i16::from_le_bytes([w[4], w[5]])) * v[2];
        lanes[3] += i64::from(i16::from_le_bytes([w[6], w[7]])) * v[3];
    }
    let mut total = lanes[0] + lanes[1] + lanes[2] + lanes[3];
    for (w, &v) in row_chunks
        .remainder()
        .chunks_exact(2)
        .zip(x_chunks.remainder())
    {
        total += i64::from(i16::from_le_bytes([w[0], w[1]])) * v;
    }
    total
}

/// Exact `sum_j w_j x_j` of one row with the limb kernel.
///
/// Each column block of weights is split exactly as before into three signed
/// base-128 limbs, `w = l0 + 128 l1 + 16384 l2` by truncating division, so
/// `|l0|, |l1| <= 127` and `|l2| <= 1`. Each block's combine and the running
/// total are then bounded by `32767 * sum|x| < 2^63`.
fn dot_i16_limbs(row: &[u8], digits: &LimbBlocks, limbs: &mut [[i8; COL_BLOCK]; 3]) -> i64 {
    let [low, middle, top] = limbs;
    let mut total = 0i64;
    for (block, weights) in row.chunks(COL_BLOCK * 2).enumerate() {
        let (pairs, _) = weights.as_chunks::<2>();
        let width = pairs.len();
        let (low, middle, top) = (&mut low[..width], &mut middle[..width], &mut top[..width]);
        split_block(pairs, low, middle, top);
        total += digits.dot(block, low)
            + 128 * digits.dot(block, middle)
            + 16384 * digits.dot(block, top);
    }
    total
}

/// Split one column block of little-endian INT16 weights into its three
/// signed base-128 limbs by truncating division (`(w / 128) / 128` is
/// `w / 16384`). One plain map loop per limb, from fixed-size `[u8; 2]` words
/// into exclusive slices: the form compilers vectorize. Kept out of line so
/// CI can check its machine code for vector instructions.
#[inline(never)]
fn split_block(pairs: &[[u8; 2]], low: &mut [i8], middle: &mut [i8], top: &mut [i8]) {
    for (limb, &pair) in low.iter_mut().zip(pairs) {
        *limb = (i16::from_le_bytes(pair) % 128) as i8;
    }
    for (limb, &pair) in middle.iter_mut().zip(pairs) {
        *limb = ((i16::from_le_bytes(pair) / 128) % 128) as i8;
    }
    for (limb, &pair) in top.iter_mut().zip(pairs) {
        *limb = (i16::from_le_bytes(pair) / 16384) as i8;
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    #[test]
    fn independent_python_row_router_norm_and_projection_oracle() {
        let corpus: Value = serde_json::from_str(include_str!(
            "../../../../../docs/protocol/reference/int16-row-oracle.json"
        ))
        .unwrap();
        let bits = |v: &Value| -> Vec<u16> {
            v["bits"]
                .as_array()
                .unwrap()
                .iter()
                .map(|b| b.as_u64().unwrap() as u16)
                .collect()
        };
        for v in corpus["rows"].as_array().unwrap() {
            let row = bits(v);
            let mut q = vec![123; row.len()];
            match quantize_row(&row, &mut q) {
                Ok((mu, k)) => assert_eq!(
                    serde_json::json!({"q":q,"mu":mu,"k":k}),
                    v["ok"],
                    "{row:x?}"
                ),
                Err(_) => {
                    assert_eq!(v["error"], true, "{row:x?}");
                    assert!(q.iter().all(|&v| v == 123), "rejected row mutated output");
                }
            }
        }
        for v in corpus["routers"].as_array().unwrap() {
            let row = bits(v);
            let mut q = vec![123; row.len()];
            match super::super::ops::quantize_router_row(&row, &mut q) {
                Ok(k) => assert_eq!(serde_json::json!({"q":q,"k":k}), v["ok"], "router {row:x?}"),
                Err(_) => assert_eq!(v["error"], true, "router {row:x?}"),
            }
        }
        for v in corpus["norms"].as_array().unwrap() {
            let b = v["bits"].as_u64().unwrap() as u16;
            match crate::modern::convert::bf16_to_q16(b) {
                Ok(n) => assert_eq!(serde_json::json!(n), v["ok"], "norm {b:x}"),
                Err(_) => assert_eq!(v["error"], true, "norm {b:x}"),
            }
        }
        let _guard = crate::canonical_simd::kernel_switch_guard();
        for fast in [false, true] {
            crate::canonical_simd::set_fast_canonical_kernel(fast);
            for v in corpus["projections"].as_array().unwrap() {
                let q: Vec<u8> = v["q"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .flat_map(|q| (q.as_i64().unwrap() as i16).to_le_bytes())
                    .collect();
                let x: Vec<i64> = v["x"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|v| v.as_i64().unwrap())
                    .collect();
                let mu = [v["mu"].as_i64().unwrap() as i32];
                let k = [v["k"].as_u64().unwrap() as u8];
                let mut output = [0];
                let got = I16Weights::new(&q)
                    .and_then(|q| project_i16(q, 1, x.len(), &mu, &k, &x, &mut output));
                match got {
                    Ok(()) => assert_eq!(serde_json::json!(output[0]), v["ok"], "projection {v}"),
                    Err(_) => assert_eq!(v["error"], true, "projection {v}"),
                }
                // The legacy kernel agrees on every corpus case, errors included.
                let mut legacy = [0];
                let old = legacy_project_i16(&q, 1, x.len(), &mu, &k, &x, &mut legacy);
                assert_eq!(old.is_ok(), got.is_ok(), "projection {v}");
                if old.is_ok() {
                    assert_eq!(legacy, output, "projection {v}");
                }
            }
        }
        crate::canonical_simd::set_fast_canonical_kernel(false);
    }

    #[test]
    fn int16_window_is_per_matrix_row_and_preserves_legacy_admission() {
        for class in ["attention", "dense", "shared", "embedding", "head"] {
            let mut p = serde_json::to_value(Precision {
                version: 1,
                attention: Bits::Int8,
                dense: Bits::Int8,
                shared: Bits::Int8,
                embedding: Bits::Int8,
                head: Bits::Int8,
            })
            .unwrap();
            p[class] = serde_json::json!("int16");
            let p = Precision::from_json(&p).unwrap();
            let name = match class {
                "attention" => "model.layers.0.self_attn.q_proj.weight",
                "dense" => "model.layers.0.mlp.up_proj.weight",
                "shared" => "model.layers.1.mlp.shared_experts.up_proj.weight",
                "embedding" => "model.embed_tokens.weight",
                _ => "lm_head.weight",
            };
            let precision = p.source(name);
            assert_eq!(precision, Bits::Int16);
            // An accepted maximum in row 0 must not mask row 1's small maximum.
            let bad = quantize_matrix(&[0x3f80, 0, 0x36ff, 0], 2, 2, precision)
                .err()
                .unwrap()
                .to_string();
            assert!(bad.contains("conversion row 1") && bad.contains("[2^-17,2^30)"));
            assert!(quantize_matrix(&[0x36ff], 1, 1, Bits::Int8).is_ok());
            for b in [0, 0x3700, 0x3701, 0x4e7f] {
                assert!(quantize_matrix(&[b], 1, 1, precision).is_ok());
            }
            for b in [1, 0x36ff, 0x4e80, 0x4e81, 0x7f80] {
                assert!(quantize_matrix(&[b], 1, 1, precision).is_err());
            }
        }
        // Exact INT8 edges differ from rounded power-of-two summaries.
        for b in [0x32fe, 0x4a7d] {
            assert!(quantize_matrix(&[b], 1, 1, Bits::Int8).is_ok());
        }
        for b in [0x32fd, 0x4a7e] {
            assert!(quantize_matrix(&[b], 1, 1, Bits::Int8).is_err());
        }
        // Router and norm use different scales: neither inherits matrix admission.
        assert!(super::super::ops::quantize_router_row(&[0x3680], &mut [0]).is_ok());
        assert_eq!(crate::modern::convert::bf16_to_q16(0x3680).unwrap(), 0);
        assert_eq!(crate::modern::convert::bf16_to_q16(0x3700).unwrap(), 1);
    }
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
                let weights = I16Weights::new(&q).unwrap();
                project_i16(weights, 67, cols, &[1 << 30; 67], &[45; 67], &x, &mut got).unwrap();
                assert_eq!(got, expected);
            }
        }
        crate::canonical_simd::set_fast_canonical_kernel(false);
        let one_bytes = [1u8, 0];
        let one = I16Weights::new(&one_bytes).unwrap();
        assert!(project_i16(one, 1, 1, &[1 << 30], &[16], &[i64::MAX], &mut [0]).is_err());
        // -32768 is refused once, when the weights are admitted.
        assert!(I16Weights::new(&[0, 128]).is_err());
        assert!(project_i16(one, 1, 1, &[1 << 30], &[255], &[1], &mut [0]).is_err());
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

    /// The projection before the hot-path change (#156 head f0f02dca,
    /// `precision.rs` lines 213-295), kept as the byte-for-byte reference: a
    /// -32768 scan on every call, three limb buffers per 64-row chunk, chunks
    /// one after another. Only the name differs.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn legacy_project_i16(
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

    /// The output values of one projection, or its error text.
    pub(crate) type Outcome = Result<Vec<i64>, String>;

    /// One INT16 projection: little-endian weights, row scales and an input.
    pub(crate) struct Case<'a> {
        pub(crate) q: &'a [u8],
        pub(crate) rows: usize,
        pub(crate) cols: usize,
        pub(crate) mu: &'a [i32],
        pub(crate) k: &'a [u8],
        pub(crate) x: &'a [i64],
    }

    impl Case<'_> {
        pub(crate) fn legacy(&self) -> Outcome {
            let mut out = vec![0; self.rows];
            legacy_project_i16(
                self.q, self.rows, self.cols, self.mu, self.k, self.x, &mut out,
            )
            .map_err(|e| e.to_string())?;
            Ok(out)
        }

        /// This change's projection; `None` is the public entry point, which
        /// picks its schedule by matrix size.
        pub(crate) fn current(&self, schedule: Option<Schedule>) -> Outcome {
            let weights = I16Weights::new(self.q).map_err(|e| e.to_string())?;
            let (rows, cols, mu, k, x) = (self.rows, self.cols, self.mu, self.k, self.x);
            let mut out = vec![0; rows];
            let result = match schedule {
                Some(schedule) => {
                    project_i16_scheduled(weights, rows, cols, mu, k, x, &mut out, schedule)
                }
                None => project_i16(weights, rows, cols, mu, k, x, &mut out),
            };
            result.map_err(|e| e.to_string())?;
            Ok(out)
        }

        /// The legacy scalar outcome, byte for byte, from: the legacy SIMD
        /// path; the serial schedule; and the pooled schedule and the public
        /// entry point on every pool. Both kernels. The caller holds
        /// `kernel_switch_guard`.
        pub(crate) fn assert_matches_legacy(
            &self,
            pools: &[(usize, rayon::ThreadPool)],
            what: &str,
        ) {
            crate::canonical_simd::set_fast_canonical_kernel(false);
            let want = self.legacy();
            for fast in [false, true] {
                crate::canonical_simd::set_fast_canonical_kernel(fast);
                assert_eq!(self.legacy(), want, "{what}: legacy, fast {fast}");
                let serial = self.current(Some(Schedule::Serial));
                assert_eq!(serial, want, "{what}: serial, fast {fast}");
                for (threads, pool) in pools {
                    let pooled = pool.install(|| self.current(Some(Schedule::Pool)));
                    assert_eq!(pooled, want, "{what}: {threads} threads, fast {fast}");
                    let public = pool.install(|| self.current(None));
                    assert_eq!(
                        public, want,
                        "{what}: entry, {threads} threads, fast {fast}"
                    );
                }
            }
            crate::canonical_simd::set_fast_canonical_kernel(false);
        }
    }

    /// Rayon pools of 1, 2 and N threads: N is this machine's parallelism, and
    /// at least 4, so three distinct counts run even on a two-core runner.
    pub(crate) fn thread_pools() -> Vec<(usize, rayon::ThreadPool)> {
        let n = std::thread::available_parallelism()
            .map_or(4, |n| n.get())
            .max(4);
        [1, 2, n]
            .into_iter()
            .map(|threads| {
                let pool = rayon::ThreadPoolBuilder::new()
                    .num_threads(threads)
                    .build()
                    .unwrap();
                (threads, pool)
            })
            .collect()
    }

    /// Deterministic test values (xorshift64).
    struct Xorshift(u64);

    impl Xorshift {
        fn draw(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }

        /// A value in `[-bound, bound]`.
        fn symmetric(&mut self, bound: u64) -> i64 {
            (self.draw() % (2 * bound + 1)) as i64 - bound as i64
        }
    }

    fn le_bytes(values: &[i16]) -> Vec<u8> {
        values.iter().flat_map(|v| v.to_le_bytes()).collect()
    }

    /// Weights at every base-128 limb boundary, and both extremes.
    const EDGE_WEIGHTS: [i16; 19] = [
        32767, -32767, 0, 1, -1, 127, -127, 128, -128, 255, -256, 16383, -16383, 16384, -16384,
        16511, -16511, 32640, -32640,
    ];

    #[test]
    fn int16_weights_refuse_the_minimum_once_at_admission() {
        // Both sides of the 1 MiB boundary of the parallel scan.
        let clean = vec![0u8; (1 << 20) + 6];
        assert!(I16Weights::new(&clean).is_ok());
        for at in [0, 2, (1 << 20) - 2, 1 << 20, (1 << 20) + 4] {
            let mut bad = clean.clone();
            bad[at..at + 2].copy_from_slice(&i16::MIN.to_le_bytes());
            assert!(holds_int16_min(&bad), "{at}");
            assert!(I16Weights::new(&bad).is_err(), "{at}");
        }
        // Neighbours of the minimum are profile values; so is a pair that
        // reads as -32768 only at an odd offset (1 then 128 here).
        for v in [i16::MIN + 1, i16::MAX, -1, 0, 128, -32512] {
            assert!(I16Weights::new(&v.to_le_bytes()).is_ok(), "{v}");
        }
        let misaligned = [0x01, 0x00, 0x80, 0x00];
        assert!(!holds_int16_min(&misaligned));
        assert!(I16Weights::new(&misaligned).is_ok());
        // Odd lengths leave a dangling byte and are refused.
        assert!(I16Weights::new(&[1, 0, 0]).is_err());
        assert!(I16Weights::new(&[]).is_ok());
        // Whole rows of admitted weights are admitted, without a scan.
        let values: Vec<i16> = (0..12).map(|v| v * 1000 - 5000).collect();
        let bytes = le_bytes(&values);
        let matrix = I16Weights::new(&bytes).unwrap();
        assert_eq!(matrix.rows(1..3, 4).as_bytes(), &bytes[8..24]);
        assert!(matrix.rows(3..3, 4).as_bytes().is_empty());
        // The legacy per-call scan refused exactly the same weights.
        let minimum = le_bytes(&[3, i16::MIN, 5]);
        let mut out = [0; 3];
        let x = [1];
        assert!(legacy_project_i16(&minimum, 3, 1, &[1 << 30; 3], &[40; 3], &x, &mut out).is_err());
        assert!(I16Weights::new(&minimum).is_err());
    }

    /// Random and edge-valued matrices against the legacy kernel: limb
    /// boundaries and +-32767, row-chunk edges (63, 64, 65 rows), column
    /// block edges (2047, 2048, 2049), the SIMD activation-domain edges and
    /// one value past them, the largest accepted accumulator mass with every
    /// product of row 0 the same sign (for rows of +-32767 the dot is
    /// 2^63 - 32,775) and the first refused one, zero rows, and scales at
    /// k = 40 and 62 and mu = 2^30 and 2^31 - 1.
    #[test]
    fn int16_projection_matches_the_legacy_kernel_on_random_and_edge_matrices() {
        use crate::canonical_simd::{LIMB_MAX, LIMB_MIN};
        let _guard = crate::canonical_simd::kernel_switch_guard();
        let pools = thread_pools();
        let mut rng = Xorshift(0x1560_F0F0_2DCA_0004);
        let limit = ((1u128 << 63) / 32767) as i64;
        let shapes: [(usize, usize, &[usize]); 13] = [
            (1, 1, &[0, 1, 2]),
            (1, 3, &[0, 1, 2]),
            (3, 7, &[0, 1, 2]),
            (63, 5, &[0, 1, 2]),
            (64, 9, &[0, 1, 2]),
            (65, 2, &[0, 1, 2]),
            (130, 17, &[0, 1, 2]),
            (2, 2047, &[0, 1, 2]),
            (3, 2048, &[0, 1, 2]),
            (2, 2049, &[0, 1, 2]),
            (5, 4100, &[0, 1]),
            (1000, 300, &[0]),
            (70, 64, &[2]),
        ];
        for (rows, cols, patterns) in shapes {
            for &pattern in patterns {
                let mut values: Vec<i16> = (0..rows * cols)
                    .map(|i| match pattern {
                        0 => rng.symmetric(32767) as i16,
                        1 => EDGE_WEIGHTS[(i * 7 + i / cols) % EDGE_WEIGHTS.len()],
                        _ if (i / cols).is_multiple_of(2) => 32767,
                        _ => -32767,
                    })
                    .collect();
                let mut mu: Vec<i32> = (0..rows)
                    .map(|r| match r % 5 {
                        0 => i32::MAX,
                        1 => 1 << 30,
                        _ => ((1u64 << 30) + rng.draw() % (1 << 30)) as i32,
                    })
                    .collect();
                let mut k: Vec<u8> = (0..rows)
                    .map(|r| match r % 3 {
                        0 => 62,
                        1 => 40,
                        _ => 40 + (rng.draw() % 23) as u8,
                    })
                    .collect();
                if pattern == 0 && rows > 2 {
                    values[cols..2 * cols].fill(0);
                    (mu[1], k[1]) = (0, 16);
                }
                let q = le_bytes(&values);
                // Row 0's signs, so every product of the heaviest input is positive.
                let heaviest: Vec<i64> = (0..cols)
                    .map(|j| {
                        let share = (limit - 1) / cols as i64;
                        let magnitude = share + if j == 0 { (limit - 1) % cols as i64 } else { 0 };
                        if values[j] < 0 { -magnitude } else { magnitude }
                    })
                    .collect();
                let mut refused = heaviest.clone();
                refused[0] += if refused[0] < 0 { -1 } else { 1 };
                let typical: Vec<i64> = (0..cols).map(|_| rng.symmetric(1 << 20)).collect();
                let edges: Vec<i64> = (0..cols)
                    .map(|j| match j % 4 {
                        0 => LIMB_MAX,
                        1 => LIMB_MIN,
                        2 => 0,
                        _ => -1,
                    })
                    .collect();
                let mut outside = typical.clone();
                outside[cols / 2] = LIMB_MAX + 1;
                for (name, x) in [
                    ("typical", &typical),
                    ("digit edges", &edges),
                    ("past the digits", &outside),
                    ("heaviest", &heaviest),
                    ("refused", &refused),
                ] {
                    let case = Case {
                        q: &q,
                        rows,
                        cols,
                        mu: &mu,
                        k: &k,
                        x,
                    };
                    let what = format!("{rows}x{cols} pattern {pattern} {name}");
                    case.assert_matches_legacy(&pools, &what);
                    let ok = case.legacy().is_ok();
                    assert_eq!(ok, name != "refused", "{what}");
                }
            }
        }
        // Past the legacy kernel's 131,071-column limb bound the new kernel
        // still takes the limb path (block by block); the legacy one falls back
        // to scalar. Rows of +-32767 against the four-digit maximum: every dot
        // is within 0.4 % of 2^63, with no partial overflow.
        for cols in [131_071, 131_100] {
            let values: Vec<i16> = (0..2 * cols)
                .map(|i| if i < cols { 32767 } else { -32767 })
                .collect();
            let q = le_bytes(&values);
            let x = vec![LIMB_MAX; cols];
            let case = Case {
                q: &q,
                rows: 2,
                cols,
                mu: &[i32::MAX, 1 << 30],
                k: &[62, 62],
                x: &x,
            };
            case.assert_matches_legacy(&pools, &format!("{cols} columns"));
            assert!(case.legacy().is_ok());
        }
    }

    /// Every refusal after admission gives the legacy error.
    #[test]
    fn int16_projection_refusals_match_the_legacy_kernel() {
        let _guard = crate::canonical_simd::kernel_switch_guard();
        let pools = thread_pools();
        let q = le_bytes(&[32767, -32767, 5, -5, 1, 0]);
        let x = [1i64 << 40, -(1 << 40)];
        let normal = [1 << 30; 3];
        for (mu, k, what) in [
            (
                [1 << 30, (1 << 30) - 1, 1 << 30],
                [40, 40, 40],
                "mu below 2^30",
            ),
            ([1 << 30, 0, 1 << 30], [40, 17, 40], "zero mu with k 17"),
            (normal, [40, 63, 40], "k 63"),
            (normal, [15, 40, 40], "k 15"),
            ([i32::MAX; 3], [16, 16, 16], "output beyond 2^62"),
        ] {
            let case = Case {
                q: &q,
                rows: 3,
                cols: 2,
                mu: &mu,
                k: &k,
                x: &x,
            };
            case.assert_matches_legacy(&pools, what);
            assert!(case.legacy().is_err(), "{what}");
        }
        // A wrong input length, a wrong row count, and a valid reshape.
        let (mu, k) = ([1 << 30; 4], [40; 4]);
        for (rows, cols, x) in [
            (3, 2, &[1i64, 2, 3][..]),
            (4, 2, &[1, 2][..]),
            (2, 3, &[1, 2, 3][..]),
        ] {
            let case = Case {
                q: &q,
                rows,
                cols,
                mu: &mu[..rows],
                k: &k[..rows],
                x,
            };
            let what = format!("shape {rows}x{cols}, input {}", x.len());
            case.assert_matches_legacy(&pools, &what);
        }
    }

    /// Timed calls per benchmark row, after one warm-up call.
    const BENCH_RUNS: usize = 5;

    /// Median and minimum milliseconds of `BENCH_RUNS` calls after one
    /// warm-up, and the warm-up's output (every timed output must equal it).
    fn timed(mut call: impl FnMut() -> Vec<i64>) -> (f64, f64, Vec<i64>) {
        let first = call();
        let mut ms: Vec<f64> = (0..BENCH_RUNS)
            .map(|_| {
                let start = std::time::Instant::now();
                let out = std::hint::black_box(call());
                let elapsed = start.elapsed().as_secs_f64() * 1000.0;
                assert_eq!(out, first, "a timed run changed the output");
                elapsed
            })
            .collect();
        ms.sort_by(f64::total_cmp);
        (ms[BENCH_RUNS / 2], ms[0], first)
    }

    /// The rejected design, measured by the benchmark only: the three i8
    /// weight limbs split once, as a load-time copy would hold them (3 bytes
    /// per weight beside the mapped 2-byte weights).
    fn precompute_limbs(q: &[u8]) -> [Vec<i8>; 3] {
        let (pairs, _) = q.as_chunks::<2>();
        let mut limbs = [
            vec![0i8; pairs.len()],
            vec![0i8; pairs.len()],
            vec![0i8; pairs.len()],
        ];
        let [low, middle, top] = &mut limbs;
        split_block(pairs, low, middle, top);
        limbs
    }

    /// A projection over precomputed limbs, with the digits, column blocks,
    /// kernels, row chunks and epilogue of `project_i16`.
    fn project_precomputed(
        limbs: &[Vec<i8>; 3],
        rows: usize,
        cols: usize,
        mu: &[i32],
        k: &[u8],
        x: &[i64],
    ) -> Vec<i64> {
        use rayon::prelude::*;
        let digits = LimbBlocks::new(x, COL_BLOCK).expect("in-domain input");
        let mut out = vec![0i64; rows];
        out.par_chunks_mut(ROW_CHUNK)
            .enumerate()
            .for_each(|(chunk_index, dots)| {
                for (offset, dot) in dots.iter_mut().enumerate() {
                    let row = (chunk_index * ROW_CHUNK + offset) * cols;
                    *dot = (0..cols.div_ceil(COL_BLOCK))
                        .map(|block| {
                            let lo = row + block * COL_BLOCK;
                            let hi = row + cols.min((block + 1) * COL_BLOCK);
                            digits.dot(block, &limbs[0][lo..hi])
                                + 128 * digits.dot(block, &limbs[1][lo..hi])
                                + 16384 * digits.dot(block, &limbs[2][lo..hi])
                        })
                        .sum::<i64>();
                }
            });
        for ((v, &m), &s) in out.iter_mut().zip(mu).zip(k) {
            *v = dyadic_epilogue(*v, m, s).unwrap();
        }
        out
    }

    /// CI-runner timing of one INT16 projection, the legacy kernel ("before")
    /// against this one ("after"), at two pinned Kimi K2.6 shapes
    /// (`docs/protocol/reference/kimi-k26/config.json`): a 16,384-row slice of
    /// the 163,840 x 7,168 LM head, and the attention output projection. Every
    /// timed output is compared with the legacy bytes. These are CI-runner
    /// timings, not product speed. Run in release mode:
    /// `cargo test -p arc-inference --lib --release --locked
    /// int16_projection_benchmark -- --ignored --nocapture`
    #[test]
    #[ignore = "CI timing run: use --release --ignored --nocapture"]
    fn int16_projection_benchmark() {
        use std::hint::black_box;
        let _guard = crate::canonical_simd::kernel_switch_guard();
        let threads = std::thread::available_parallelism().map_or(1, |n| n.get());
        let counts = [1, threads];
        let counts = if threads > 1 {
            &counts[..]
        } else {
            &counts[..1]
        };
        let pools: Vec<(usize, rayon::ThreadPool)> = counts
            .iter()
            .map(|&t| {
                let pool = rayon::ThreadPoolBuilder::new()
                    .num_threads(t)
                    .build()
                    .unwrap();
                (t, pool)
            })
            .collect();
        let simd = crate::canonical_simd::dotprod_available();
        // The build is the CI one, not the shipped fat-LTO, one-unit build.
        let profile = |name: &str| std::env::var(name).unwrap_or_else(|_| "unset".into());
        println!(
            "INT16 projection, CI-runner timings (not product speed): {} {}, {threads} logical CPUs, \
             SIMD backend {}, {BENCH_RUNS} timed runs after one warm-up; build: release profile with \
             CARGO_PROFILE_RELEASE_LTO={} and CARGO_PROFILE_RELEASE_CODEGEN_UNITS={} (unset: \
             Cargo.toml's fat LTO and 1 unit)",
            std::env::consts::OS,
            std::env::consts::ARCH,
            if simd { "available" } else { "unavailable" },
            profile("CARGO_PROFILE_RELEASE_LTO"),
            profile("CARGO_PROFILE_RELEASE_CODEGEN_UNITS"),
        );
        println!(
            "| shape | rows x cols | kernel | build | threads | median ms | min ms | speedup |"
        );
        println!("|---|---|---|---|---|---|---|---|");
        for (shape, rows, cols) in [
            ("K2.6 lm_head slice (16,384 of 163,840 rows)", 16_384, 7_168),
            ("K2.6 attention wo", 7_168, 8_192),
        ] {
            let mut rng = Xorshift(0x0D15_EA5E_0000_0156 ^ (rows * cols) as u64);
            let values: Vec<i16> = (0..rows * cols)
                .map(|_| rng.symmetric(32767) as i16)
                .collect();
            let q = le_bytes(&values);
            drop(values);
            let mu: Vec<i32> = (0..rows)
                .map(|_| ((1u64 << 30) + rng.draw() % (1 << 30)) as i32)
                .collect();
            let k = vec![46u8; rows];
            // Q16 activations of magnitude up to 2 (three base-256 digits).
            let x: Vec<i64> = (0..cols).map(|_| rng.symmetric(1 << 17)).collect();
            let weights = I16Weights::new(&q).unwrap();
            let (scan, _, _) = timed(|| {
                vec![i64::from(
                    black_box(&q).chunks_exact(2).any(|v| v == [0, 128]),
                )]
            });
            println!(
                "| {shape} | {rows} x {cols} | - | before: the per-call -32768 scan alone | 1 | {scan:.1} | | |"
            );
            for fast in [false, true] {
                if fast && !simd {
                    continue;
                }
                crate::canonical_simd::set_fast_canonical_kernel(fast);
                let kernel = if fast { "SIMD limbs" } else { "scalar" };
                let (before, before_min, want) = timed(|| {
                    let mut out = vec![0; rows];
                    legacy_project_i16(&q, rows, cols, &mu, &k, &x, &mut out).unwrap();
                    out
                });
                println!(
                    "| {shape} | {rows} x {cols} | {kernel} | before (legacy) | 1 | {before:.1} | {before_min:.1} | 1.00 |"
                );
                for (t, pool) in &pools {
                    let (after, after_min, got) = timed(|| {
                        pool.install(|| {
                            let mut out = vec![0; rows];
                            project_i16(weights, rows, cols, &mu, &k, &x, &mut out).unwrap();
                            out
                        })
                    });
                    assert_eq!(got, want, "{shape}, {kernel}, {t} threads");
                    println!(
                        "| {shape} | {rows} x {cols} | {kernel} | after | {t} | {after:.1} | {after_min:.1} | {:.2} |",
                        before / after
                    );
                }
                if fast {
                    // The memory tradeoff, measured: limbs precomputed at load.
                    let limbs = precompute_limbs(&q);
                    let mib = limbs.iter().map(Vec::len).sum::<usize>() as f64 / f64::from(1 << 20);
                    for (t, pool) in &pools {
                        let (pre, pre_min, got) = timed(|| {
                            pool.install(|| project_precomputed(&limbs, rows, cols, &mu, &k, &x))
                        });
                        assert_eq!(got, want, "{shape}, precomputed limbs, {t} threads");
                        println!(
                            "| {shape} | {rows} x {cols} | {kernel} | not adopted: limbs precomputed at load (+{mib:.0} MiB) | {t} | {pre:.1} | {pre_min:.1} | {:.2} |",
                            before / pre
                        );
                    }
                }
            }
            crate::canonical_simd::set_fast_canonical_kernel(false);
        }
        bench_layer_heads(&pools, simd);
    }

    /// One K2.6 layer's per-head projections, scheduled by the driver
    /// `layer_forward` uses (`for_each_head`): 64 heads of `wk_b` (512 x 128,
    /// the absorbed query) and `wv_b` (128 x 512, the head output). Each slice
    /// is below the row-parallel threshold, so before head parallelism every
    /// one ran on one thread. Attention itself is unchanged and not timed.
    fn bench_layer_heads(pools: &[(usize, rayon::ThreadPool)], simd: bool) {
        use super::super::model::for_each_head;
        let (n_heads, rank, nope, v_dim) = (64usize, 512usize, 128usize, 128usize);
        let shape = "K2.6 layer heads (64 x wk_b 512x128 + wv_b 128x512)";
        let dims = "64 x 131072";
        let mut rng = Xorshift(0x0D15_EA5E_0000_0064);
        let wk_values: Vec<i16> = (0..n_heads * rank * nope)
            .map(|_| rng.symmetric(32767) as i16)
            .collect();
        let wv_values: Vec<i16> = (0..n_heads * v_dim * rank)
            .map(|_| rng.symmetric(32767) as i16)
            .collect();
        let (wk, wv) = (le_bytes(&wk_values), le_bytes(&wv_values));
        let mut scale = |rows: usize| -> Vec<i32> {
            (0..rows)
                .map(|_| ((1u64 << 30) + rng.draw() % (1 << 30)) as i32)
                .collect()
        };
        let (mu_k, mu_v) = (scale(n_heads * rank), scale(n_heads * v_dim));
        let (k_k, k_v) = (vec![46u8; n_heads * rank], vec![46u8; n_heads * v_dim]);
        let q_nope: Vec<i64> = (0..n_heads * nope)
            .map(|_| rng.symmetric(1 << 17))
            .collect();
        let u: Vec<i64> = (0..n_heads * rank)
            .map(|_| rng.symmetric(1 << 17))
            .collect();
        let (wk_all, wv_all) = (I16Weights::new(&wk).unwrap(), I16Weights::new(&wv).unwrap());
        // Head j's block: its absorbed query (rank values), then its output
        // (v_dim values), so both projections are compared.
        let width = rank + v_dim;
        let (ks, vs) = (
            |j: usize| j * rank..(j + 1) * rank,
            |j: usize| j * v_dim..(j + 1) * v_dim,
        );
        let head = |j: usize, block: &mut [i64]| -> Result<(), ModernError> {
            let (qa, out) = block.split_at_mut(rank);
            let x = &q_nope[j * nope..(j + 1) * nope];
            project_i16(
                wk_all.rows(ks(j), nope),
                rank,
                nope,
                &mu_k[ks(j)],
                &k_k[ks(j)],
                x,
                qa,
            )?;
            let (u_j, wv_j) = (&u[ks(j)], wv_all.rows(vs(j), rank));
            project_i16(wv_j, v_dim, rank, &mu_v[vs(j)], &k_v[vs(j)], u_j, out)
        };
        for fast in [false, true] {
            if fast && !simd {
                continue;
            }
            crate::canonical_simd::set_fast_canonical_kernel(fast);
            let kernel = if fast { "SIMD limbs" } else { "scalar" };
            let (before, before_min, want) = timed(|| {
                let mut all = vec![0i64; n_heads * width];
                for (j, block) in all.chunks_mut(width).enumerate() {
                    let (qa, out) = block.split_at_mut(rank);
                    let wk_j = &wk[j * rank * nope * 2..(j + 1) * rank * nope * 2];
                    let x = &q_nope[j * nope..(j + 1) * nope];
                    legacy_project_i16(wk_j, rank, nope, &mu_k[ks(j)], &k_k[ks(j)], x, qa).unwrap();
                    let wv_j = &wv[j * v_dim * rank * 2..(j + 1) * v_dim * rank * 2];
                    let (mu, k) = (&mu_v[vs(j)], &k_v[vs(j)]);
                    legacy_project_i16(wv_j, v_dim, rank, mu, k, &u[ks(j)], out).unwrap();
                }
                all
            });
            println!(
                "| {shape} | {dims} | {kernel} | before (legacy, heads serial) | 1 | {before:.1} | {before_min:.1} | 1.00 |"
            );
            for (build, schedule) in [
                ("after, heads serial (555faab3)", Schedule::Serial),
                ("after", Schedule::Pool),
            ] {
                for (t, pool) in pools {
                    let (after, after_min, got) = timed(|| {
                        pool.install(|| {
                            let mut all = vec![0i64; n_heads * width];
                            for_each_head(&mut all, width, schedule, head).unwrap();
                            all
                        })
                    });
                    assert_eq!(got, want, "{shape}, {kernel}, {build}, {t} threads");
                    println!(
                        "| {shape} | {dims} | {kernel} | {build} | {t} | {after:.1} | {after_min:.1} | {:.2} |",
                        before / after
                    );
                }
            }
        }
        crate::canonical_simd::set_fast_canonical_kernel(false);
    }
}
