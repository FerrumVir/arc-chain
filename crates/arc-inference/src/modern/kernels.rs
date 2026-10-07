//! Exact integer GEMV kernels for the dyadic profile, with forced dispatch.
//!
//! A projection of `arc.hf-llama.i8-dyadic-row.q16.v1` (spec §5.2) needs the
//! exact accumulator `acc_i = Σ_j q_ij · x_j` of an INT8 weight row and an i64
//! Q16 activation. Three kernels compute it, and each returns the same integer
//! for every input it accepts:
//!
//! * [`Kernel::Scalar`]: one `i8 × i64` multiply per weight, on every CPU.
//! * [`Kernel::Avx2`] (x86-64): the activation is split once into balanced
//!   base-2^16 digits (two digits cover `|x| < 2^31`); every 16 weights are
//!   widened to i16 once and multiplied with each digit plane by `vpmaddwd`
//!   (exact `i16 × i16 → i32` pair sums), four rows at a time.
//! * [`Kernel::Neon`] (ARM64 with dotprod): balanced base-256 digits (four
//!   cover `|x| < 2^31`) and `sdot`; four rows times up to four digit planes
//!   give sixteen independent accumulator chains and one pass over each
//!   weight vector.
//!
//! # Why the kernel cannot change a value
//!
//! `x = Σ_l d_l · B^l` exactly, so `Σ_j q_j x_j = Σ_l B^l · (Σ_j q_j d_lj)`.
//! Each inner sum adds exact integer products, and integer addition is
//! associative, so lane order, unrolling, row tiling and thread count cannot
//! change it as long as no partial sum leaves its register:
//!
//! * base 2^16: `|q| ≤ 128` and `|d| ≤ 2^15`, so a `vpmaddwd` lane gains at
//!   most 2^23 per 16 columns; lanes are widened to i64 every 2,048 columns,
//!   before they can pass 2^30;
//! * base 256: an `sdot` lane gains at most `4 · 2^14 = 2^16` per 16 columns,
//!   so inputs of at most 131,071 columns keep every lane, and the sum of the
//!   four lanes, below 2^31;
//! * the digit sums are combined in i128 and must fit i64, which the
//!   projection precondition `127 · Σ|x_j| < 2^63` guarantees for profile
//!   weights (anything else is refused, never wrapped).
//!
//! An input outside a kernel's digit domain is computed by the scalar kernel
//! instead, before any output is written; [`census`] counts the refusals.
//!
//! # Forced dispatch
//!
//! `arc-modern --kernel <spec>` (or `ARC_MODERN_KERNEL=<spec>`, see
//! [`super::engine::Spec`]) selects a forward pass and kernel explicitly, so
//! CI runs every path a CPU supports against the same golden digests
//! (`.github/workflows/cpu-engine-gate.yml`).
//!
//! # Reuse: fused projections and mixture-of-experts layers
//!
//! A [`PreparedInput`] is split once and read by any number of matrices.
//! [`project_many`] and [`project_swiglu_many`] run several matrices, or row
//! ranges of one ([`MatrixRows::range`], e.g. one expert of an `[E·F, D]`
//! tensor), in one parallel region whose tasks own disjoint output rows.

use std::cell::RefCell;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering};
use std::time::Instant;

use rayon::prelude::*;

use super::ModernError;
use super::arith::{self, DyadicMatrix};
use crate::canonical_simd::ProjectionCensus;

/// Which kernel computes the exact row dot products.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum Kernel {
    /// One `i8 × i64` multiply per weight, on every platform.
    #[default]
    Scalar,
    /// x86-64 AVX2: base-2^16 digits and `vpmaddwd`.
    Avx2,
    /// ARM64 NEON with dotprod: base-256 digits and `sdot`.
    Neon,
}

impl Kernel {
    /// Every kernel, in a fixed order.
    pub const ALL: [Kernel; 3] = [Kernel::Scalar, Kernel::Avx2, Kernel::Neon];

    /// Stable name used on the command line, in run files and in CI.
    pub fn name(self) -> &'static str {
        match self {
            Kernel::Scalar => "scalar",
            Kernel::Avx2 => "avx2",
            Kernel::Neon => "neon",
        }
    }

    /// Parse a kernel name; `auto` is the fastest kernel this CPU supports.
    pub fn parse(name: &str) -> Result<Self, ModernError> {
        match name {
            "scalar" => Ok(Kernel::Scalar),
            "avx2" => Ok(Kernel::Avx2),
            "neon" => Ok(Kernel::Neon),
            "auto" => Ok(Kernel::best()),
            other => Err(ModernError::Invalid(format!(
                "unknown kernel {other} (expected scalar, avx2, neon or auto)"
            ))),
        }
    }

    /// Whether this build and CPU can run the kernel.
    pub fn available(self) -> bool {
        match self {
            Kernel::Scalar => true,
            Kernel::Avx2 => avx2_available(),
            Kernel::Neon => neon_available(),
        }
    }

    /// The fastest kernel this CPU supports.
    pub fn best() -> Self {
        if avx2_available() {
            Kernel::Avx2
        } else if neon_available() {
            Kernel::Neon
        } else {
            Kernel::Scalar
        }
    }

    /// Every kernel this CPU supports, scalar first.
    pub fn available_kernels() -> Vec<Kernel> {
        Kernel::ALL.into_iter().filter(|k| k.available()).collect()
    }

    fn code(self) -> u8 {
        match self {
            Kernel::Scalar => 0,
            Kernel::Avx2 => 1,
            Kernel::Neon => 2,
        }
    }

    fn from_code(code: u8) -> Self {
        match code {
            1 => Kernel::Avx2,
            2 => Kernel::Neon,
            _ => Kernel::Scalar,
        }
    }
}

fn avx2_available() -> bool {
    #[cfg(target_arch = "x86_64")]
    {
        std::arch::is_x86_feature_detected!("avx2")
    }
    #[cfg(not(target_arch = "x86_64"))]
    {
        false
    }
}

fn neon_available() -> bool {
    #[cfg(target_arch = "aarch64")]
    {
        std::arch::is_aarch64_feature_detected!("dotprod")
    }
    #[cfg(not(target_arch = "aarch64"))]
    {
        false
    }
}

// ---------------------------------------------------------------- census --

static CENSUS_ON: AtomicBool = AtomicBool::new(false);
static ATTEMPTED: AtomicU64 = AtomicU64::new(0);
static ACCEPTED: AtomicU64 = AtomicU64::new(0);
static REFUSED_UNAVAILABLE: AtomicU64 = AtomicU64::new(0);
static REFUSED_DOMAIN: AtomicU64 = AtomicU64::new(0);
static REFUSED_INNER_DIM: AtomicU64 = AtomicU64::new(0);
/// Accepted inputs by number of digit planes (index = planes, 1..=4).
static LIMBS: [AtomicU64; 5] = [const { AtomicU64::new(0) }; 5];

/// Count prepared inputs (one per activation vector, however many matrices
/// read it). Off by default, so timed runs do no counting unless asked.
pub fn set_census_enabled(on: bool) {
    CENSUS_ON.store(on, Ordering::Relaxed);
}

/// Whether prepared inputs are being counted (see [`set_census_enabled`]).
pub fn census_enabled() -> bool {
    CENSUS_ON.load(Ordering::Relaxed)
}

/// Zero every census counter.
pub fn reset_census() {
    for counter in [
        &ATTEMPTED,
        &ACCEPTED,
        &REFUSED_UNAVAILABLE,
        &REFUSED_DOMAIN,
        &REFUSED_INNER_DIM,
    ] {
        counter.store(0, Ordering::Relaxed);
    }
    for counter in &LIMBS {
        counter.store(0, Ordering::Relaxed);
    }
}

/// SIMD attempts, acceptances and refusals since the last reset, in the
/// field names of the legacy limb kernel's census (the CI report reads them).
pub fn census() -> ProjectionCensus {
    ProjectionCensus {
        attempted: ATTEMPTED.load(Ordering::Relaxed),
        accepted: ACCEPTED.load(Ordering::Relaxed),
        refused_unavailable: REFUSED_UNAVAILABLE.load(Ordering::Relaxed),
        refused_shape: 0,
        refused_inner_dim_above_i32_bound: REFUSED_INNER_DIM.load(Ordering::Relaxed),
        refused_activation_out_of_domain: REFUSED_DOMAIN.load(Ordering::Relaxed),
        refused_scale_multiply_would_overflow: 0,
    }
}

/// Accepted inputs by number of digit planes: index 1 to 4.
pub fn limb_histogram() -> [u64; 5] {
    let mut out = [0u64; 5];
    for (slot, counter) in out.iter_mut().zip(&LIMBS) {
        *slot = counter.load(Ordering::Relaxed);
    }
    out
}

fn count(counter: &AtomicU64) {
    if CENSUS_ON.load(Ordering::Relaxed) {
        counter.fetch_add(1, Ordering::Relaxed);
    }
}

// --------------------------------------------------------------- domains --

/// Most base-2^16 digits the AVX2 kernel uses (`|x|` up to about 2^47).
pub const I16_MAX_LIMBS: usize = 3;
/// Most base-256 digits the NEON kernel uses (`|x|` up to about 2^31).
pub const I8_MAX_LIMBS: usize = 4;
/// Widest input the base-256 kernel accepts (its i32 lanes never flush).
pub const I8_MAX_COLS: usize = crate::canonical_simd::MAX_COLS_FOR_I32;

/// `[min, max]` of the integers `limbs` balanced digits of `digit_bits` bits
/// represent: digits in `[-2^(b-1), 2^(b-1) - 1]`, weights `2^(b·l)`.
pub const fn digit_range(digit_bits: u32, limbs: usize) -> (i64, i64) {
    let half: i128 = 1 << (digit_bits - 1);
    let mut span: i128 = 0;
    let mut weight: i128 = 1;
    let mut i = 0;
    while i < limbs {
        span += weight;
        weight <<= digit_bits;
        i += 1;
    }
    ((-half * span) as i64, ((half - 1) * span) as i64)
}

// The published four-digit base-256 range of the legacy limb kernel, and the
// two-digit base-2^16 range (which stops 2^15 short of i32::MAX).
const _: () = assert!(digit_range(8, 4).0 == crate::canonical_simd::LIMB_MIN);
const _: () = assert!(digit_range(8, 4).1 == crate::canonical_simd::LIMB_MAX);
const _: () = assert!(digit_range(16, 2).0 == -2_147_516_416);
const _: () = assert!(digit_range(16, 2).1 == 2_147_450_879);
const _: () = assert!(digit_range(16, 3).1 == 140_735_340_838_911);

/// Fewest digits that represent every value in `[lo, hi]`, if at most `max`.
#[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
fn limbs_for(lo: i64, hi: i64, digit_bits: u32, max: usize) -> Option<usize> {
    (1..=max).find(|&limbs| {
        let (low, high) = digit_range(digit_bits, limbs);
        lo >= low && hi <= high
    })
}

/// The projection precondition `127 · Σ|x_j| < 2^63` (spec §5.2), plus the
/// input's minimum and maximum.
fn input_range(x: &[i64]) -> Result<(i64, i64), ModernError> {
    if x.is_empty() {
        return Err(ModernError::Invalid("empty projection input".into()));
    }
    let (lo, hi) = x
        .iter()
        .fold((i64::MAX, i64::MIN), |(lo, hi), &v| (lo.min(v), hi.max(v)));
    let max_abs = u128::from(lo.unsigned_abs().max(hi.unsigned_abs()));
    // `127 · n · max|x| < 2^63` implies the precondition; only inputs that
    // fail this cheap bound need the exact sum.
    if 127 * (x.len() as u128) * max_abs >= 1u128 << 63 {
        arith::check_projection_input(x)?;
    }
    Ok((lo, hi))
}

// --------------------------------------------------------- prepared input --

/// How the kernels read a prepared input.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum Plan {
    /// Scalar dot products with the stored activations.
    #[default]
    Scalar,
    /// `limbs` base-2^16 digit planes.
    #[cfg(target_arch = "x86_64")]
    I16 { limbs: usize },
    /// `limbs` base-256 digit planes.
    #[cfg(target_arch = "aarch64")]
    I8 { limbs: usize },
}

/// An activation vector checked against the projection precondition and
/// split once into the digit planes its kernel reads.
///
/// Every matrix that consumes the same vector (Q, K and V; gate and up; the
/// selected experts of a mixture-of-experts layer) reuses one prepared input.
/// The buffers are kept between calls, so preparing allocates nothing once
/// they have grown to the model's widths.
#[derive(Debug, Clone)]
pub struct PreparedInput {
    x: Vec<i64>,
    plan: Plan,
    /// Digits per plane: `x.len()` rounded up to 64, zero padded.
    #[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
    stride: usize,
    #[cfg(target_arch = "x86_64")]
    planes16: Vec<i16>,
    #[cfg(target_arch = "aarch64")]
    planes8: Vec<i8>,
    /// Scratch for the digit split.
    #[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
    rest: Vec<i64>,
}

impl Default for PreparedInput {
    fn default() -> Self {
        Self::new()
    }
}

impl PreparedInput {
    /// An empty input; [`PreparedInput::prepare`] fills it.
    pub const fn new() -> Self {
        Self {
            x: Vec::new(),
            plan: Plan::Scalar,
            #[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
            stride: 0,
            #[cfg(target_arch = "x86_64")]
            planes16: Vec::new(),
            #[cfg(target_arch = "aarch64")]
            planes8: Vec::new(),
            #[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
            rest: Vec::new(),
        }
    }

    /// Number of activations.
    pub fn len(&self) -> usize {
        self.x.len()
    }

    /// Whether nothing has been prepared.
    pub fn is_empty(&self) -> bool {
        self.x.is_empty()
    }

    /// The activations, exactly as given.
    pub fn values(&self) -> &[i64] {
        &self.x
    }

    /// The kernel that computes dot products with this input (scalar after a
    /// refusal).
    pub fn kernel(&self) -> Kernel {
        match self.plan {
            Plan::Scalar => Kernel::Scalar,
            #[cfg(target_arch = "x86_64")]
            Plan::I16 { .. } => Kernel::Avx2,
            #[cfg(target_arch = "aarch64")]
            Plan::I8 { .. } => Kernel::Neon,
        }
    }

    /// Digit planes in use (0 for the scalar kernel).
    pub fn limbs(&self) -> usize {
        match self.plan {
            Plan::Scalar => 0,
            #[cfg(target_arch = "x86_64")]
            Plan::I16 { limbs } => limbs,
            #[cfg(target_arch = "aarch64")]
            Plan::I8 { limbs } => limbs,
        }
    }

    /// Check the projection precondition for `x` and split it for `kernel`.
    ///
    /// Returns [`ModernError::Domain`] exactly when the reference projection
    /// does. A kernel that is unavailable here, or an input outside its digit
    /// domain, falls back to the scalar kernel (counted by [`census`]).
    pub fn prepare(&mut self, x: &[i64], kernel: Kernel) -> Result<(), ModernError> {
        self.plan = Plan::Scalar;
        let (lo, hi) = input_range(x)?;
        self.x.clear();
        self.x.extend_from_slice(x);
        self.split(lo, hi, kernel);
        Ok(())
    }

    /// RMS-normalise `x` (spec §5.4) straight into this input, then split it.
    ///
    /// Same values and the same refusals as [`arith::rms_norm`] followed by
    /// [`PreparedInput::prepare`], without the intermediate vector.
    pub fn prepare_rms_norm(
        &mut self,
        x: &[i64],
        gain: &[i64],
        eps_q32: i64,
        kernel: Kernel,
    ) -> Result<(), ModernError> {
        self.plan = Plan::Scalar;
        self.x.clear();
        self.x.resize(x.len(), 0);
        arith::rms_norm_into(x, gain, eps_q32, &mut self.x)?;
        let (lo, hi) = input_range(&self.x)?;
        self.split(lo, hi, kernel);
        Ok(())
    }

    fn split(&mut self, lo: i64, hi: i64, kernel: Kernel) {
        match kernel {
            Kernel::Scalar => {}
            Kernel::Avx2 => self.split_avx2(lo, hi),
            Kernel::Neon => self.split_neon(lo, hi),
        }
    }

    #[cfg(target_arch = "x86_64")]
    fn split_avx2(&mut self, lo: i64, hi: i64) {
        count(&ATTEMPTED);
        if !avx2_available() {
            count(&REFUSED_UNAVAILABLE);
            return;
        }
        let Some(limbs) = limbs_for(lo, hi, 16, I16_MAX_LIMBS) else {
            count(&REFUSED_DOMAIN);
            return;
        };
        self.stride = self.x.len().div_ceil(64) * 64;
        split_base_2_16(
            &self.x,
            limbs,
            self.stride,
            &mut self.planes16,
            &mut self.rest,
        );
        self.plan = Plan::I16 { limbs };
        count(&ACCEPTED);
        count(&LIMBS[limbs]);
    }

    #[cfg(not(target_arch = "x86_64"))]
    fn split_avx2(&mut self, _lo: i64, _hi: i64) {
        count(&ATTEMPTED);
        count(&REFUSED_UNAVAILABLE);
    }

    #[cfg(target_arch = "aarch64")]
    fn split_neon(&mut self, lo: i64, hi: i64) {
        count(&ATTEMPTED);
        if !neon_available() {
            count(&REFUSED_UNAVAILABLE);
            return;
        }
        if self.x.len() > I8_MAX_COLS {
            count(&REFUSED_INNER_DIM);
            return;
        }
        let Some(limbs) = limbs_for(lo, hi, 8, I8_MAX_LIMBS) else {
            count(&REFUSED_DOMAIN);
            return;
        };
        self.stride = self.x.len().div_ceil(64) * 64;
        split_base_256(
            &self.x,
            limbs,
            self.stride,
            &mut self.planes8,
            &mut self.rest,
        );
        self.plan = Plan::I8 { limbs };
        count(&ACCEPTED);
        count(&LIMBS[limbs]);
    }

    #[cfg(not(target_arch = "aarch64"))]
    fn split_neon(&mut self, _lo: i64, _hi: i64) {
        count(&ATTEMPTED);
        count(&REFUSED_UNAVAILABLE);
    }
}

/// Balanced base-2^16 digit planes, `planes[l * stride + j]`, zero padded.
#[cfg(target_arch = "x86_64")]
fn split_base_2_16(
    x: &[i64],
    limbs: usize,
    stride: usize,
    planes: &mut Vec<i16>,
    rest: &mut Vec<i64>,
) {
    planes.clear();
    planes.resize(limbs * stride, 0);
    rest.clear();
    rest.extend_from_slice(x);
    for plane in planes.chunks_exact_mut(stride) {
        for (digit, value) in plane.iter_mut().zip(rest.iter_mut()) {
            // The low 16 bits read as signed: `value - low` is a multiple of
            // 2^16, so the shift divides exactly.
            let low = *value as i16;
            *digit = low;
            *value = (*value - i64::from(low)) >> 16;
        }
    }
    debug_assert!(rest.iter().all(|&v| v == 0), "digit domain was checked");
}

/// Balanced base-256 digit planes, `planes[l * stride + j]`, zero padded.
#[cfg(target_arch = "aarch64")]
fn split_base_256(
    x: &[i64],
    limbs: usize,
    stride: usize,
    planes: &mut Vec<i8>,
    rest: &mut Vec<i64>,
) {
    planes.clear();
    planes.resize(limbs * stride, 0);
    rest.clear();
    rest.extend_from_slice(x);
    for plane in planes.chunks_exact_mut(stride) {
        for (digit, value) in plane.iter_mut().zip(rest.iter_mut()) {
            let low = *value as i8;
            *digit = low;
            *value = (*value - i64::from(low)) >> 8;
        }
    }
    debug_assert!(rest.iter().all(|&v| v == 0), "digit domain was checked");
}

/// `Σ_l sums[l] · 2^(digit_bits · l)` plus the columns past the last full
/// vector, computed exactly and narrowed to i64.
#[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
fn combine<const L: usize>(
    sums: &[i64; L],
    digit_bits: u32,
    row: &[i8],
    x: &[i64],
    vector_cols: usize,
) -> Result<i64, ModernError> {
    let mut total: i128 = 0;
    for (l, &sum) in sums.iter().enumerate() {
        total += i128::from(sum) * (1i128 << (digit_bits as usize * l));
    }
    for (&w, &v) in row[vector_cols..].iter().zip(&x[vector_cols..]) {
        total += i128::from(w) * i128::from(v);
    }
    i64::try_from(total)
        .map_err(|_| ModernError::Domain("projection accumulator beyond i64".into()))
}

// ---------------------------------------------------------- matrix views --

/// Rows of a dyadic matrix that one projection reads: the whole matrix, or a
/// row range of it (one expert's rows of an `[E·F, D]` tensor).
#[derive(Debug, Clone, Copy)]
pub struct MatrixRows<'a> {
    /// Row-major INT8 weights, `rows * cols` values.
    pub q: &'a [i8],
    /// Scale mantissas, one per row.
    pub mu: &'a [i32],
    /// Scale shifts, one per row.
    pub k: &'a [u8],
    pub rows: usize,
    pub cols: usize,
}

impl<'a> MatrixRows<'a> {
    /// Every row of `m`.
    pub fn of(m: &'a DyadicMatrix) -> Self {
        Self {
            q: &m.q,
            mu: &m.mu,
            k: &m.k,
            rows: m.rows,
            cols: m.cols,
        }
    }

    /// `rows` rows of `m` starting at row `start`.
    pub fn range(m: &'a DyadicMatrix, start: usize, rows: usize) -> Result<Self, ModernError> {
        let end = start
            .checked_add(rows)
            .filter(|&end| rows > 0 && end <= m.rows)
            .ok_or_else(|| {
                ModernError::Invalid(format!(
                    "rows {start}..{start}+{rows} of a {}-row matrix",
                    m.rows
                ))
            })?;
        Ok(Self {
            q: &m.q[start * m.cols..end * m.cols],
            mu: &m.mu[start..end],
            k: &m.k[start..end],
            rows,
            cols: m.cols,
        })
    }

    fn row(&self, r: usize) -> &'a [i8] {
        &self.q[r * self.cols..(r + 1) * self.cols]
    }

    fn check(&self, input_len: usize) -> Result<(), ModernError> {
        let consistent = self.rows > 0
            && self.cols > 0
            && self.rows.checked_mul(self.cols) == Some(self.q.len())
            && self.mu.len() == self.rows
            && self.k.len() == self.rows;
        if !consistent || self.cols != input_len {
            return Err(ModernError::Invalid(format!(
                "projection shape: matrix {}x{} (q {}, mu {}, k {}), input {input_len}",
                self.rows,
                self.cols,
                self.q.len(),
                self.mu.len(),
                self.k.len()
            )));
        }
        Ok(())
    }
}

/// Rows per parallel task: a multiple of every row tile, large enough that
/// scheduling costs little next to streaming the rows.
const TASK_ROWS: usize = 64;

/// Exact accumulators of rows `row0 .. row0 + acc.len()` of `m` for one
/// input, in the configured row order for single-input calls.
///
/// The caller has checked `m` against `input` and keeps the rows in range.
fn row_dots(
    m: MatrixRows<'_>,
    row0: usize,
    input: &PreparedInput,
    acc: &mut [i64],
) -> Result<(), ModernError> {
    row_dots_ordered(m, row0, input, acc, tiling().resolve(input.kernel(), false))
}

/// [`row_dots`] with the row order given (the batched path resolves it for
/// batched calls). Every order computes the same integers.
#[cfg_attr(
    not(any(target_arch = "x86_64", target_arch = "aarch64")),
    allow(unused_variables)
)]
fn row_dots_ordered(
    m: MatrixRows<'_>,
    row0: usize,
    input: &PreparedInput,
    acc: &mut [i64],
    order: Tiling,
) -> Result<(), ModernError> {
    debug_assert!(row0 + acc.len() <= m.rows);
    match input.plan {
        Plan::Scalar => {
            for (offset, slot) in acc.iter_mut().enumerate() {
                *slot = arith::dot_i8_i64(m.row(row0 + offset), &input.x);
            }
            Ok(())
        }
        #[cfg(target_arch = "x86_64")]
        Plan::I16 { limbs } => {
            // SAFETY: an `I16` plan exists only after `avx2_available()`
            // returned true and the planes were split with
            // `stride >= x.len() = m.cols`; the rows read lie inside `m.q`,
            // whose length `check` verified.
            unsafe { x86::row_dots(m, row0, input, limbs, order, acc) }
        }
        #[cfg(target_arch = "aarch64")]
        Plan::I8 { limbs } => {
            // SAFETY: as for `I16`, with `neon_available()` and
            // `m.cols <= I8_MAX_COLS` checked when the plan was made.
            unsafe { arm::row_dots(m, row0, input, limbs, order, acc) }
        }
    }
}

/// One dyadic projection (spec §5.2): `out_i = (acc_i · μ_i) >> k_i`.
pub fn project(
    m: MatrixRows<'_>,
    input: &PreparedInput,
    out: &mut [i64],
) -> Result<(), ModernError> {
    project_many(&[(m, input)], out)
}

/// Several dyadic projections in one parallel region, outputs concatenated
/// in order (fused Q/K/V, or the down projections of several experts, each
/// with its own input).
pub fn project_many(
    parts: &[(MatrixRows<'_>, &PreparedInput)],
    out: &mut [i64],
) -> Result<(), ModernError> {
    let mut total = 0usize;
    for (m, input) in parts {
        m.check(input.len())?;
        total += m.rows;
    }
    if total != out.len() {
        return Err(ModernError::Invalid(format!(
            "projection output: {} values for {total} rows",
            out.len()
        )));
    }
    out.par_chunks_mut(TASK_ROWS)
        .enumerate()
        .try_for_each(|(chunk_index, chunk)| {
            let mut first = chunk_index * TASK_ROWS;
            let mut rest = chunk;
            let mut part_start = 0usize;
            for &(m, input) in parts {
                let part_end = part_start + m.rows;
                if !rest.is_empty() && first < part_end {
                    let local = first - part_start;
                    let take = (part_end - first).min(rest.len());
                    let (head, tail) = std::mem::take(&mut rest).split_at_mut(take);
                    row_dots(m, local, input, head)?;
                    for (offset, slot) in head.iter_mut().enumerate() {
                        let row = local + offset;
                        *slot = arith::dyadic_epilogue(*slot, m.mu[row], m.k[row])?;
                    }
                    rest = tail;
                    first += take;
                }
                part_start = part_end;
            }
            Ok(())
        })
}

/// `out_t = gated_silu((gate·x)_t, (up·x)_t)` (spec §5.2, §5.7) for one
/// gate/up pair: the first half of a SwiGLU feed-forward block.
pub fn project_swiglu(
    gate: MatrixRows<'_>,
    up: MatrixRows<'_>,
    input: &PreparedInput,
    out: &mut [i64],
) -> Result<(), ModernError> {
    project_swiglu_many(&[(gate, up)], input, out)
}

/// [`project_swiglu`] for several gate/up pairs that read the same input
/// (the selected experts of a mixture-of-experts layer), outputs
/// concatenated in order, in one parallel region.
///
/// Every gate and up row passes the same epilogue and gated-SiLU checks as
/// the reference, so the call refuses exactly when the reference does.
pub fn project_swiglu_many(
    parts: &[(MatrixRows<'_>, MatrixRows<'_>)],
    input: &PreparedInput,
    out: &mut [i64],
) -> Result<(), ModernError> {
    let mut total = 0usize;
    for (gate, up) in parts {
        gate.check(input.len())?;
        up.check(input.len())?;
        if gate.rows != up.rows {
            return Err(ModernError::Invalid(format!(
                "gate has {} rows, up has {}",
                gate.rows, up.rows
            )));
        }
        total += gate.rows;
    }
    if total != out.len() {
        return Err(ModernError::Invalid(format!(
            "SwiGLU output: {} values for {total} rows",
            out.len()
        )));
    }
    out.par_chunks_mut(TASK_ROWS)
        .enumerate()
        .try_for_each(|(chunk_index, chunk)| {
            let mut up_acc = [0i64; TASK_ROWS];
            let mut first = chunk_index * TASK_ROWS;
            let mut rest = chunk;
            let mut part_start = 0usize;
            for &(gate, up) in parts {
                let part_end = part_start + gate.rows;
                if !rest.is_empty() && first < part_end {
                    let local = first - part_start;
                    let take = (part_end - first).min(rest.len());
                    let (head, tail) = std::mem::take(&mut rest).split_at_mut(take);
                    let up_head = &mut up_acc[..take];
                    row_dots(gate, local, input, head)?;
                    row_dots(up, local, input, up_head)?;
                    for (offset, (g, &u)) in head.iter_mut().zip(up_head.iter()).enumerate() {
                        let row = local + offset;
                        let g_out = arith::dyadic_epilogue(*g, gate.mu[row], gate.k[row])?;
                        let u_out = arith::dyadic_epilogue(u, up.mu[row], up.k[row])?;
                        *g = arith::gated_silu(g_out, u_out)?;
                    }
                    rest = tail;
                    first += take;
                }
                part_start = part_end;
            }
            Ok(())
        })
}

// --------------------------------------------------------------- batches --

/// Most inputs (tokens) one batched call takes.
pub const MAX_BATCH: usize = 16;
/// Rows a batched call computes for every input before moving on: small
/// enough that the rows stay in the core's cache while every input reads them.
const BATCH_TILE_ROWS: usize = 4;

fn check_batch(inputs: &[PreparedInput]) -> Result<usize, ModernError> {
    if inputs.is_empty() || inputs.len() > MAX_BATCH {
        return Err(ModernError::Invalid(format!(
            "a batch takes 1 to {MAX_BATCH} inputs, not {}",
            inputs.len()
        )));
    }
    Ok(inputs.len())
}

/// Rows `row0 .. row0 + acc.len() / inputs.len()` of `m` for every input:
/// `acc[i * inputs.len() + t]`. Each tile of rows is read from memory once
/// and then from cache by every input; the arithmetic is [`row_dots`]'s.
fn row_dots_batch(
    m: MatrixRows<'_>,
    row0: usize,
    inputs: &[PreparedInput],
    acc: &mut [i64],
) -> Result<(), ModernError> {
    let tokens = inputs.len();
    let rows = acc.len() / tokens;
    let order = tiling();
    let mut tile = [0i64; BATCH_TILE_ROWS];
    let mut i = 0usize;
    while i < rows {
        let n = (rows - i).min(BATCH_TILE_ROWS);
        for (t, input) in inputs.iter().enumerate() {
            let resolved = order.resolve(input.kernel(), true);
            row_dots_ordered(m, row0 + i, input, &mut tile[..n], resolved)?;
            for (r, &value) in tile[..n].iter().enumerate() {
                acc[(i + r) * tokens + t] = value;
            }
        }
        i += n;
    }
    Ok(())
}

/// [`project_many`] for several tokens that read the same matrices (prompt
/// prefill, batched verification): `out[row * inputs.len() + t]` is row
/// `row` of the concatenated parts for token `t`. Weight traffic falls by the
/// number of tokens; every value is the single-token value.
pub fn project_batch(
    parts: &[MatrixRows<'_>],
    inputs: &[PreparedInput],
    out: &mut [i64],
) -> Result<(), ModernError> {
    let tokens = check_batch(inputs)?;
    let mut total = 0usize;
    for m in parts {
        for input in inputs {
            m.check(input.len())?;
        }
        total += m.rows;
    }
    if total.checked_mul(tokens) != Some(out.len()) {
        return Err(ModernError::Invalid(format!(
            "batched projection output: {} values for {total} rows x {tokens} inputs",
            out.len()
        )));
    }
    out.par_chunks_mut(TASK_ROWS * tokens)
        .enumerate()
        .try_for_each(|(chunk_index, chunk)| {
            let mut first = chunk_index * TASK_ROWS;
            let mut rest = chunk;
            let mut part_start = 0usize;
            for &m in parts {
                let part_end = part_start + m.rows;
                if !rest.is_empty() && first < part_end {
                    let local = first - part_start;
                    let take = (part_end - first).min(rest.len() / tokens);
                    let (head, tail) = std::mem::take(&mut rest).split_at_mut(take * tokens);
                    row_dots_batch(m, local, inputs, head)?;
                    for (offset, row_out) in head.chunks_exact_mut(tokens).enumerate() {
                        let row = local + offset;
                        for slot in row_out {
                            *slot = arith::dyadic_epilogue(*slot, m.mu[row], m.k[row])?;
                        }
                    }
                    rest = tail;
                    first += take;
                }
                part_start = part_end;
            }
            Ok(())
        })
}

/// [`project_swiglu`] for several tokens: `out[row * inputs.len() + t]`.
pub fn project_swiglu_batch(
    gate: MatrixRows<'_>,
    up: MatrixRows<'_>,
    inputs: &[PreparedInput],
    out: &mut [i64],
) -> Result<(), ModernError> {
    let tokens = check_batch(inputs)?;
    for input in inputs {
        gate.check(input.len())?;
        up.check(input.len())?;
    }
    if gate.rows != up.rows || gate.rows.checked_mul(tokens) != Some(out.len()) {
        return Err(ModernError::Invalid(format!(
            "batched SwiGLU: gate {} rows, up {} rows, output {} for {tokens} inputs",
            gate.rows,
            up.rows,
            out.len()
        )));
    }
    out.par_chunks_mut(TASK_ROWS * tokens)
        .enumerate()
        .try_for_each(|(chunk_index, chunk)| {
            let row0 = chunk_index * TASK_ROWS;
            let mut up_acc = [0i64; TASK_ROWS * MAX_BATCH];
            let up_chunk = &mut up_acc[..chunk.len()];
            row_dots_batch(gate, row0, inputs, chunk)?;
            row_dots_batch(up, row0, inputs, up_chunk)?;
            for (index, (g, &u)) in chunk.iter_mut().zip(up_chunk.iter()).enumerate() {
                let row = row0 + index / tokens;
                let g_out = arith::dyadic_epilogue(*g, gate.mu[row], gate.k[row])?;
                let u_out = arith::dyadic_epilogue(u, up.mu[row], up.k[row])?;
                *g = arith::gated_silu(g_out, u_out)?;
            }
            Ok(())
        })
}

// ------------------------------------------------------------- attention --

/// A kernel whose CPU support has been checked, for the attention loops.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Isa(Kernel);

impl Isa {
    /// `kernel` if this CPU supports it, else the scalar kernel.
    pub fn new(kernel: Kernel) -> Self {
        if kernel.available() {
            Isa(kernel)
        } else {
            Isa(Kernel::Scalar)
        }
    }

    pub fn kernel(self) -> Kernel {
        self.0
    }

    /// Exact `Σ_t a_t · b_t` over the common length.
    ///
    /// Every partial sum is bounded by `Σ_t |a_t| · max|b|`; the caller keeps
    /// that below 2^63 (attention: `Σ|q_t| < 2^32` and `|k| < 2^31`).
    pub fn dot_i32(self, a: &[i32], b: &[i32]) -> i64 {
        let n = a.len().min(b.len());
        let (a, b) = (&a[..n], &b[..n]);
        match self.0 {
            #[cfg(target_arch = "x86_64")]
            Kernel::Avx2 => {
                // SAFETY: `Isa::new` checked AVX2; the slices have equal length.
                unsafe { x86::dot_i32(a, b) }
            }
            #[cfg(target_arch = "aarch64")]
            Kernel::Neon => {
                // SAFETY: `Isa::new` checked dotprod, which implies NEON.
                unsafe { arm::dot_i32(a, b) }
            }
            _ => a
                .iter()
                .zip(b)
                .map(|(&x, &y)| i64::from(x) * i64::from(y))
                .sum(),
        }
    }

    /// `acc_t += w · v_t` over the common length, in i64 like the reference
    /// attention loop.
    pub fn axpy_i32(self, acc: &mut [i64], w: i32, v: &[i32]) {
        let n = acc.len().min(v.len());
        let (acc, v) = (&mut acc[..n], &v[..n]);
        match self.0 {
            #[cfg(target_arch = "x86_64")]
            Kernel::Avx2 => {
                // SAFETY: `Isa::new` checked AVX2; the slices have equal length.
                unsafe { x86::axpy_i32(acc, w, v) }
            }
            #[cfg(target_arch = "aarch64")]
            Kernel::Neon => {
                // SAFETY: `Isa::new` checked dotprod, which implies NEON.
                unsafe { arm::axpy_i32(acc, w, v) }
            }
            _ => {
                for (a, &x) in acc.iter_mut().zip(v) {
                    *a += i64::from(w) * i64::from(x);
                }
            }
        }
    }
}

// ---------------------------------------------------------------- tiling --

/// How the SIMD kernels walk the rows of a projection. Every order computes
/// the same integers; they differ only in memory access pattern.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Tiling {
    /// The order measured best for the kernel and the kind of call: batched
    /// calls (prefill) tile four rows; single-input calls (decode) use the
    /// order [`calibrate_auto`] measured on this machine, or until then
    /// `stream` on AVX2 and `rows4` on NEON (see [`auto_order`]).
    #[default]
    Auto,
    /// Four rows per register tile: each digit vector is loaded once and
    /// used for four rows (four short row streams per thread).
    Rows4,
    /// One row at a time, several column blocks per step: each thread reads
    /// its rows as one sequential stream.
    Stream,
}

impl Tiling {
    /// Every concrete row order, in a fixed order.
    pub const ALL: [Tiling; 2] = [Tiling::Rows4, Tiling::Stream];

    pub fn name(self) -> &'static str {
        match self {
            Tiling::Auto => "auto",
            Tiling::Rows4 => "rows4",
            Tiling::Stream => "stream",
        }
    }

    pub fn parse(name: &str) -> Result<Self, ModernError> {
        match name {
            "auto" => Ok(Tiling::Auto),
            "rows4" => Ok(Tiling::Rows4),
            "stream" => Ok(Tiling::Stream),
            other => Err(ModernError::Invalid(format!(
                "unknown tiling {other} (expected auto, rows4 or stream)"
            ))),
        }
    }

    /// The concrete row order `kernel` runs with, for a single-input call
    /// (`batched == false`) or a batched one. An explicit order is kept;
    /// `auto` is [`auto_order`].
    pub fn resolve(self, kernel: Kernel, batched: bool) -> Tiling {
        match self {
            Tiling::Auto => auto_order(kernel, batched),
            order => order,
        }
    }

    fn code(self) -> u8 {
        match self {
            Tiling::Auto => 0,
            Tiling::Rows4 => 1,
            Tiling::Stream => 2,
        }
    }

    fn from_code(code: u8) -> Self {
        match code {
            1 => Tiling::Rows4,
            2 => Tiling::Stream,
            _ => Tiling::Auto,
        }
    }
}

static TILING: AtomicU8 = AtomicU8::new(0);

/// Select the row order of the SIMD kernels process-wide (a speed setting;
/// it cannot change a value). [`Tiling::Auto`] is the default.
pub fn set_tiling(tiling: Tiling) {
    TILING.store(tiling.code(), Ordering::Relaxed);
}

/// The configured tiling of the SIMD kernels (see [`set_tiling`]); the
/// order a call runs with is [`Tiling::resolve`] of it.
pub fn tiling() -> Tiling {
    Tiling::from_code(TILING.load(Ordering::Relaxed))
}

/// Per kernel, the row order `auto` uses for single-input calls: the
/// built-in default (code 0) until [`calibrate_auto`] measured this machine.
static AUTO_SINGLE: [AtomicU8; 3] = [const { AtomicU8::new(0) }; 3];

/// The row order [`Tiling::Auto`] resolves to for `kernel`.
///
/// Batched calls (prefill) tile four rows: on every CI runner that order
/// read each weight tile once per block of tokens fastest. Single-input
/// calls (decode) use the order [`calibrate_auto`] measured on this
/// machine; until a calibration ran, `stream` on AVX2 (2-13% faster on AMD
/// EPYC 9V74 and 7763 runners, 2% slower on an Intel Xeon 6973P-C runner)
/// and `rows4` on NEON (15% faster on a Neoverse-N2 runner).
pub fn auto_order(kernel: Kernel, batched: bool) -> Tiling {
    if batched || kernel == Kernel::Scalar {
        return Tiling::Rows4;
    }
    match Tiling::from_code(AUTO_SINGLE[kernel.code() as usize].load(Ordering::Relaxed)) {
        Tiling::Auto if kernel == Kernel::Avx2 => Tiling::Stream,
        Tiling::Auto => Tiling::Rows4,
        order => order,
    }
}

/// Set the row order `auto` uses for single-input calls of `kernel`
/// ([`Tiling::Auto`] restores the built-in default). A speed setting; it
/// cannot change a value.
pub fn set_auto_order(kernel: Kernel, order: Tiling) {
    AUTO_SINGLE[kernel.code() as usize].store(order.code(), Ordering::Relaxed);
}

/// How this machine ran both row orders of one kernel ([`calibrate_auto`]).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Calibration {
    pub kernel: Kernel,
    /// Best pass over the matrices with `rows4`, seconds.
    pub rows4_seconds: f64,
    /// Best pass over the matrices with `stream`, seconds.
    pub stream_seconds: f64,
    /// Weight bytes one pass reads.
    pub weight_bytes: usize,
    /// Passes per order.
    pub passes: usize,
    /// The order `auto` now uses for single-input calls of the kernel.
    pub chosen: Tiling,
}

/// Best pass over `matrices` with each order of [`Tiling::ALL`], in seconds,
/// alternating the orders for `passes` rounds. Leaves the last order
/// measured selected; the caller restores the tiling.
fn time_orders(
    matrices: &[MatrixRows<'_>],
    input: &PreparedInput,
    passes: usize,
) -> Result<[f64; 2], ModernError> {
    let widest = matrices.iter().map(|m| m.rows).max().unwrap_or(0);
    let mut out = vec![0i64; widest];
    let mut best = [f64::INFINITY; 2];
    for _ in 0..passes {
        for (slot, order) in Tiling::ALL.into_iter().enumerate() {
            set_tiling(order);
            let start = Instant::now();
            for m in matrices {
                project(*m, input, &mut out[..m.rows])?;
            }
            best[slot] = best[slot].min(start.elapsed().as_secs_f64());
        }
    }
    Ok(best)
}

/// Measure both row orders of `kernel` on `matrices` with `input` and make
/// the faster one the `auto` order for single-input calls of `kernel`.
///
/// Each pass projects every matrix once. Give matrices that together exceed
/// the caches (one matrix of every layer, say), so a pass streams weights
/// from memory the way a decoded token does. The orders alternate for
/// `passes` rounds and each order's best pass counts; `stream` must win by
/// more than 1% to replace `rows4`. The configured tiling is restored. The
/// result changes speed only: every order computes the same integers.
pub fn calibrate_auto(
    kernel: Kernel,
    matrices: &[MatrixRows<'_>],
    input: &PreparedInput,
    passes: usize,
) -> Result<Calibration, ModernError> {
    if kernel == Kernel::Scalar || matrices.is_empty() {
        return Err(ModernError::Invalid(
            "tiling calibration needs a SIMD kernel and at least one matrix".into(),
        ));
    }
    if input.kernel() != kernel {
        return Err(ModernError::Invalid(format!(
            "tiling calibration input was prepared for {}, not {}",
            input.kernel().name(),
            kernel.name()
        )));
    }
    let previous = tiling();
    let passes = passes.max(1);
    let timed = time_orders(matrices, input, passes);
    set_tiling(previous);
    let best = timed?;
    let chosen = if best[1] < best[0] * 0.99 {
        Tiling::Stream
    } else {
        Tiling::Rows4
    };
    set_auto_order(kernel, chosen);
    Ok(Calibration {
        kernel,
        rows4_seconds: best[0],
        stream_seconds: best[1],
        weight_bytes: matrices.iter().map(|m| m.rows * m.cols).sum(),
        passes,
        chosen,
    })
}

// ------------------------------------------------- reference-path switch --

static REFERENCE_KERNEL: AtomicU8 = AtomicU8::new(0);

/// Select the kernel of the reference forward pass ([`arith::project`]).
///
/// The default is [`Kernel::Scalar`]. The legacy opt-in kernel of
/// [`crate::canonical_simd`] takes precedence while it is enabled.
pub fn set_reference_kernel(kernel: Kernel) {
    REFERENCE_KERNEL.store(kernel.code(), Ordering::Relaxed);
}

/// The kernel [`arith::project`] uses (see [`set_reference_kernel`]).
pub fn reference_kernel() -> Kernel {
    Kernel::from_code(REFERENCE_KERNEL.load(Ordering::Relaxed))
}

thread_local! {
    /// Prepared input reused by [`arith::project`] with an exact SIMD kernel.
    static REFERENCE_INPUT: RefCell<PreparedInput> = const { RefCell::new(PreparedInput::new()) };
}

/// [`arith::project`] with an exact kernel of this module.
pub(crate) fn project_reference(
    m: &DyadicMatrix,
    x: &[i64],
    kernel: Kernel,
    out: &mut [i64],
) -> Result<(), ModernError> {
    REFERENCE_INPUT.with(|cell| {
        if let Ok(mut input) = cell.try_borrow_mut() {
            input.prepare(x, kernel)?;
            project(MatrixRows::of(m), &input, out)
        } else {
            // Re-entered on this thread (a rayon worker that stole another
            // projection): use a fresh buffer instead of the borrowed one.
            let mut input = PreparedInput::new();
            input.prepare(x, kernel)?;
            project(MatrixRows::of(m), &input, out)
        }
    })
}

// --------------------------------------------------------------- x86-64 --

#[cfg(target_arch = "x86_64")]
mod x86 {
    use std::arch::x86_64::*;

    use super::{MatrixRows, ModernError, PreparedInput, Tiling, combine};

    /// Columns per i32 accumulation block: 128 `vpmaddwd` steps of at most
    /// 2^23 per lane, so a lane stays below 2^30 before it is widened.
    const FLUSH_COLS: usize = 2048;
    const _: () = assert!((FLUSH_COLS / 16) as i64 * (1i64 << 23) <= 1i64 << 30);

    /// Exact accumulators of rows `row0 ..` with base-2^16 digit planes, in
    /// row order `order` (a concrete one, see [`Tiling::resolve`]).
    ///
    /// # Safety
    /// AVX2 must be available; `input` must hold `limbs` planes of
    /// `input.stride >= m.cols` digits; `m.q` must hold `m.rows * m.cols`
    /// weights and `row0 + acc.len() <= m.rows`.
    #[target_feature(enable = "avx2")]
    pub(super) unsafe fn row_dots(
        m: MatrixRows<'_>,
        row0: usize,
        input: &PreparedInput,
        limbs: usize,
        order: Tiling,
        acc: &mut [i64],
    ) -> Result<(), ModernError> {
        // SAFETY: the caller's contract is forwarded unchanged.
        unsafe {
            match limbs {
                1 => run::<1, 4>(m, row0, input, order, acc),
                2 => run::<2, 4>(m, row0, input, order, acc),
                _ => run::<3, 2>(m, row0, input, order, acc),
            }
        }
    }

    /// Rows in tiles of `R`, then single rows, with `L` digit planes.
    ///
    /// # Safety
    /// As [`row_dots`], with `L` planes.
    #[target_feature(enable = "avx2")]
    unsafe fn run<const L: usize, const R: usize>(
        m: MatrixRows<'_>,
        row0: usize,
        input: &PreparedInput,
        order: Tiling,
        acc: &mut [i64],
    ) -> Result<(), ModernError> {
        let cols = m.cols;
        let vector_cols = cols - cols % 16;
        let planes = input.planes16.as_ptr();
        let stride = input.stride;
        let base = m.q.as_ptr();
        if order == Tiling::Stream {
            for (offset, slot) in acc.iter_mut().enumerate() {
                let row = row0 + offset;
                // SAFETY: row < m.rows, so its `cols >= vector_cols` weights
                // lie in m.q; the planes hold L * stride digits with
                // vector_cols <= stride.
                let sums =
                    unsafe { stream::<L>(base.add(row * cols), planes, stride, vector_cols) };
                *slot = combine(&sums, 16, m.row(row), &input.x, vector_cols)?;
            }
            return Ok(());
        }
        let mut i = 0usize;
        while i < acc.len() {
            if acc.len() - i >= R {
                let mut rows = [std::ptr::null::<i8>(); R];
                for (r, slot) in rows.iter_mut().enumerate() {
                    // SAFETY: row0 + i + r < m.rows, so the row lies in m.q.
                    *slot = unsafe { base.add((row0 + i + r) * cols) };
                }
                // SAFETY: rows hold `cols >= vector_cols` weights; the planes
                // hold L * stride digits with vector_cols <= stride.
                let sums = unsafe { tile::<L, R>(rows, planes, stride, vector_cols) };
                for (r, sum) in sums.iter().enumerate() {
                    acc[i + r] = combine(sum, 16, m.row(row0 + i + r), &input.x, vector_cols)?;
                }
                i += R;
            } else {
                // SAFETY: as above, for the single row row0 + i.
                let rows = [unsafe { base.add((row0 + i) * cols) }];
                // SAFETY: as above.
                let sums = unsafe { tile::<L, 1>(rows, planes, stride, vector_cols) };
                acc[i] = combine(&sums[0], 16, m.row(row0 + i), &input.x, vector_cols)?;
                i += 1;
            }
        }
        Ok(())
    }

    /// `R` rows against `L` digit planes over the first `vector_cols`
    /// columns (a multiple of 16): `out[r][l] = Σ_j row_r[j] · plane_l[j]`.
    ///
    /// # Safety
    /// AVX2; each row pointer valid for `vector_cols` reads; `planes` valid
    /// for `L * stride` reads with `vector_cols <= stride`.
    #[target_feature(enable = "avx2")]
    #[allow(clippy::needless_range_loop)]
    unsafe fn tile<const L: usize, const R: usize>(
        rows: [*const i8; R],
        planes: *const i16,
        stride: usize,
        vector_cols: usize,
    ) -> [[i64; L]; R] {
        // SAFETY: every load stays inside the bounds stated above.
        unsafe {
            let mut total = [[0i64; L]; R];
            let mut c = 0usize;
            while c < vector_cols {
                let block_end = vector_cols.min(c + FLUSH_COLS);
                let mut acc = [[_mm256_setzero_si256(); L]; R];
                while c < block_end {
                    let mut digits = [_mm256_setzero_si256(); L];
                    for l in 0..L {
                        digits[l] = _mm256_loadu_si256(planes.add(l * stride + c).cast());
                    }
                    for r in 0..R {
                        let w = _mm256_cvtepi8_epi16(_mm_loadu_si128(rows[r].add(c).cast()));
                        for l in 0..L {
                            acc[r][l] =
                                _mm256_add_epi32(acc[r][l], _mm256_madd_epi16(w, digits[l]));
                        }
                    }
                    c += 16;
                }
                for r in 0..R {
                    for l in 0..L {
                        total[r][l] += hsum_epi32(acc[r][l]);
                    }
                }
            }
            total
        }
    }

    /// One row against `L` digit planes over the first `vector_cols`
    /// columns (a multiple of 16), two 16-column blocks per step (`2L`
    /// accumulators), so each thread reads its rows as one sequential stream.
    /// Each accumulator takes at most 65 `vpmaddwd` steps per 2,048-column
    /// block, so its lanes stay below 2^30 before they are widened.
    ///
    /// # Safety
    /// AVX2; `row` valid for `vector_cols` reads; `planes` valid for
    /// `L * stride` reads with `vector_cols <= stride`.
    #[target_feature(enable = "avx2")]
    #[allow(clippy::needless_range_loop)]
    unsafe fn stream<const L: usize>(
        row: *const i8,
        planes: *const i16,
        stride: usize,
        vector_cols: usize,
    ) -> [i64; L] {
        // SAFETY: every load stays inside the bounds stated above.
        unsafe {
            let mut total = [0i64; L];
            let mut c = 0usize;
            while c < vector_cols {
                let block_end = vector_cols.min(c + FLUSH_COLS);
                let mut even = [_mm256_setzero_si256(); L];
                let mut odd = [_mm256_setzero_si256(); L];
                while c + 32 <= block_end {
                    let w0 = _mm256_cvtepi8_epi16(_mm_loadu_si128(row.add(c).cast()));
                    let w1 = _mm256_cvtepi8_epi16(_mm_loadu_si128(row.add(c + 16).cast()));
                    for l in 0..L {
                        let plane = planes.add(l * stride + c);
                        let d0 = _mm256_loadu_si256(plane.cast());
                        let d1 = _mm256_loadu_si256(plane.add(16).cast());
                        even[l] = _mm256_add_epi32(even[l], _mm256_madd_epi16(w0, d0));
                        odd[l] = _mm256_add_epi32(odd[l], _mm256_madd_epi16(w1, d1));
                    }
                    c += 32;
                }
                if c < block_end {
                    // One 16-column block is left (vector_cols is a multiple
                    // of 16 and FLUSH_COLS a multiple of 32).
                    let w0 = _mm256_cvtepi8_epi16(_mm_loadu_si128(row.add(c).cast()));
                    for l in 0..L {
                        let d0 = _mm256_loadu_si256(planes.add(l * stride + c).cast());
                        even[l] = _mm256_add_epi32(even[l], _mm256_madd_epi16(w0, d0));
                    }
                    c += 16;
                }
                for l in 0..L {
                    total[l] += hsum_epi32(even[l]) + hsum_epi32(odd[l]);
                }
            }
            total
        }
    }

    /// Sum of eight i32 lanes, in i64.
    ///
    /// # Safety
    /// AVX2 must be available.
    #[target_feature(enable = "avx2")]
    unsafe fn hsum_epi32(v: __m256i) -> i64 {
        let mut lanes = [0i32; 8];
        // SAFETY: `lanes` is 32 writable bytes.
        unsafe { _mm256_storeu_si256(lanes.as_mut_ptr().cast(), v) };
        lanes.iter().map(|&lane| i64::from(lane)).sum()
    }

    /// Exact `Σ a_t · b_t` of two equal-length i32 vectors.
    ///
    /// # Safety
    /// AVX2 must be available and `a.len() == b.len()`.
    #[target_feature(enable = "avx2")]
    pub(super) unsafe fn dot_i32(a: &[i32], b: &[i32]) -> i64 {
        let n = a.len().min(b.len());
        let vector = n - n % 4;
        // SAFETY: every 16-byte load starts at t <= vector - 4 < n.
        unsafe {
            let mut acc0 = _mm256_setzero_si256();
            let mut acc1 = _mm256_setzero_si256();
            let mut t = 0usize;
            while t + 8 <= vector {
                let a0 = _mm256_cvtepi32_epi64(_mm_loadu_si128(a.as_ptr().add(t).cast()));
                let b0 = _mm256_cvtepi32_epi64(_mm_loadu_si128(b.as_ptr().add(t).cast()));
                acc0 = _mm256_add_epi64(acc0, _mm256_mul_epi32(a0, b0));
                let a1 = _mm256_cvtepi32_epi64(_mm_loadu_si128(a.as_ptr().add(t + 4).cast()));
                let b1 = _mm256_cvtepi32_epi64(_mm_loadu_si128(b.as_ptr().add(t + 4).cast()));
                acc1 = _mm256_add_epi64(acc1, _mm256_mul_epi32(a1, b1));
                t += 8;
            }
            if t < vector {
                let a0 = _mm256_cvtepi32_epi64(_mm_loadu_si128(a.as_ptr().add(t).cast()));
                let b0 = _mm256_cvtepi32_epi64(_mm_loadu_si128(b.as_ptr().add(t).cast()));
                acc0 = _mm256_add_epi64(acc0, _mm256_mul_epi32(a0, b0));
            }
            let mut lanes = [0i64; 4];
            _mm256_storeu_si256(lanes.as_mut_ptr().cast(), _mm256_add_epi64(acc0, acc1));
            let mut total: i64 = lanes.iter().sum();
            for (&x, &y) in a[vector..n].iter().zip(&b[vector..n]) {
                total += i64::from(x) * i64::from(y);
            }
            total
        }
    }

    /// `acc_t += w · v_t` with i64 lanes.
    ///
    /// # Safety
    /// AVX2 must be available and `acc.len() == v.len()`.
    #[target_feature(enable = "avx2")]
    pub(super) unsafe fn axpy_i32(acc: &mut [i64], w: i32, v: &[i32]) {
        let n = acc.len().min(v.len());
        let vector = n - n % 4;
        // SAFETY: every load and store of four lanes starts at t <= vector - 4.
        unsafe {
            let weight = _mm256_set1_epi64x(i64::from(w));
            let mut t = 0usize;
            while t < vector {
                let x = _mm256_cvtepi32_epi64(_mm_loadu_si128(v.as_ptr().add(t).cast()));
                let a = _mm256_loadu_si256(acc.as_ptr().add(t).cast());
                let sum = _mm256_add_epi64(a, _mm256_mul_epi32(weight, x));
                _mm256_storeu_si256(acc.as_mut_ptr().add(t).cast(), sum);
                t += 4;
            }
            for (slot, &x) in acc[vector..n].iter_mut().zip(&v[vector..n]) {
                *slot += i64::from(w) * i64::from(x);
            }
        }
    }
}

// ---------------------------------------------------------------- ARM64 --

#[cfg(target_arch = "aarch64")]
mod arm {
    use std::arch::aarch64::*;

    use super::{MatrixRows, ModernError, PreparedInput, Tiling, combine};

    /// `SDOT Vd.4S, Vn.16B, Vm.16B`: four exact 4-way i8 dot products added
    /// to i32 lanes. Inline assembly, because the intrinsic is still behind
    /// an unstable feature on the pinned toolchain (see `canonical_simd`).
    ///
    /// # Safety
    /// Requires the `dotprod` target feature.
    #[target_feature(enable = "neon,dotprod")]
    #[inline]
    unsafe fn sdot(acc: int32x4_t, a: int8x16_t, b: int8x16_t) -> int32x4_t {
        let mut out = acc;
        // SAFETY: register-only instruction with no memory access.
        unsafe {
            std::arch::asm!(
                "sdot {o:v}.4s, {a:v}.16b, {b:v}.16b",
                o = inout(vreg) out,
                a = in(vreg) a,
                b = in(vreg) b,
                options(pure, nomem, nostack)
            );
        }
        out
    }

    /// Exact accumulators of rows `row0 ..` with base-256 digit planes, in
    /// row order `order` (a concrete one, see [`Tiling::resolve`]).
    ///
    /// # Safety
    /// dotprod must be available; `input` must hold `limbs` planes of
    /// `input.stride >= m.cols` digits with `m.cols <= I8_MAX_COLS`; `m.q`
    /// must hold `m.rows * m.cols` weights and `row0 + acc.len() <= m.rows`.
    #[target_feature(enable = "neon,dotprod")]
    pub(super) unsafe fn row_dots(
        m: MatrixRows<'_>,
        row0: usize,
        input: &PreparedInput,
        limbs: usize,
        order: Tiling,
        acc: &mut [i64],
    ) -> Result<(), ModernError> {
        // SAFETY: the caller's contract is forwarded unchanged.
        unsafe {
            match limbs {
                1 => run::<1, 4>(m, row0, input, order, acc),
                2 => run::<2, 4>(m, row0, input, order, acc),
                3 => run::<3, 4>(m, row0, input, order, acc),
                _ => run::<4, 4>(m, row0, input, order, acc),
            }
        }
    }

    /// Rows in tiles of `R`, then single rows, with `L` digit planes.
    ///
    /// # Safety
    /// As [`row_dots`], with `L` planes.
    #[target_feature(enable = "neon,dotprod")]
    unsafe fn run<const L: usize, const R: usize>(
        m: MatrixRows<'_>,
        row0: usize,
        input: &PreparedInput,
        order: Tiling,
        acc: &mut [i64],
    ) -> Result<(), ModernError> {
        let cols = m.cols;
        let vector_cols = cols - cols % 16;
        let planes = input.planes8.as_ptr();
        let stride = input.stride;
        let base = m.q.as_ptr();
        if order == Tiling::Stream {
            for (offset, slot) in acc.iter_mut().enumerate() {
                let row = row0 + offset;
                // SAFETY: row < m.rows, so its `cols >= vector_cols` weights
                // lie in m.q; the planes hold L * stride digits with
                // vector_cols <= stride <= ... and vector_cols <= I8_MAX_COLS.
                let sums =
                    unsafe { stream::<L>(base.add(row * cols), planes, stride, vector_cols) };
                *slot = combine(&sums, 8, m.row(row), &input.x, vector_cols)?;
            }
            return Ok(());
        }
        let mut i = 0usize;
        while i < acc.len() {
            if acc.len() - i >= R {
                let mut rows = [std::ptr::null::<i8>(); R];
                for (r, slot) in rows.iter_mut().enumerate() {
                    // SAFETY: row0 + i + r < m.rows, so the row lies in m.q.
                    *slot = unsafe { base.add((row0 + i + r) * cols) };
                }
                // SAFETY: rows hold `cols >= vector_cols` weights; the planes
                // hold L * stride digits with vector_cols <= stride.
                let sums = unsafe { tile::<L, R>(rows, planes, stride, vector_cols) };
                for (r, sum) in sums.iter().enumerate() {
                    acc[i + r] = combine(sum, 8, m.row(row0 + i + r), &input.x, vector_cols)?;
                }
                i += R;
            } else {
                // SAFETY: as above, for the single row row0 + i.
                let rows = [unsafe { base.add((row0 + i) * cols) }];
                // SAFETY: as above.
                let sums = unsafe { tile::<L, 1>(rows, planes, stride, vector_cols) };
                acc[i] = combine(&sums[0], 8, m.row(row0 + i), &input.x, vector_cols)?;
                i += 1;
            }
        }
        Ok(())
    }

    /// `R` rows against `L` digit planes over the first `vector_cols`
    /// columns (a multiple of 16), `R · L` independent `sdot` chains.
    ///
    /// # Safety
    /// dotprod; each row pointer valid for `vector_cols` reads; `planes`
    /// valid for `L * stride` reads with `vector_cols <= stride <= ...` and
    /// `vector_cols <= I8_MAX_COLS` (no i32 lane can overflow).
    #[target_feature(enable = "neon,dotprod")]
    #[allow(clippy::needless_range_loop)]
    unsafe fn tile<const L: usize, const R: usize>(
        rows: [*const i8; R],
        planes: *const i8,
        stride: usize,
        vector_cols: usize,
    ) -> [[i64; L]; R] {
        // SAFETY: every load stays inside the bounds stated above.
        unsafe {
            let mut acc = [[vdupq_n_s32(0); L]; R];
            let mut c = 0usize;
            while c < vector_cols {
                let mut digits = [vdupq_n_s8(0); L];
                for l in 0..L {
                    digits[l] = vld1q_s8(planes.add(l * stride + c));
                }
                for r in 0..R {
                    let w = vld1q_s8(rows[r].add(c));
                    for l in 0..L {
                        acc[r][l] = sdot(acc[r][l], w, digits[l]);
                    }
                }
                c += 16;
            }
            let mut total = [[0i64; L]; R];
            for r in 0..R {
                for l in 0..L {
                    total[r][l] = i64::from(vaddvq_s32(acc[r][l]));
                }
            }
            total
        }
    }

    /// One row against `L` digit planes over the first `vector_cols`
    /// columns (a multiple of 16), four 16-column blocks per step (`4L`
    /// independent `sdot` chains), so each thread reads its rows as one
    /// sequential stream.
    ///
    /// # Safety
    /// dotprod; `row` valid for `vector_cols` reads; `planes` valid for
    /// `L * stride` reads with `vector_cols <= stride` and
    /// `vector_cols <= I8_MAX_COLS` (no i32 lane can overflow).
    #[target_feature(enable = "neon,dotprod")]
    #[allow(clippy::needless_range_loop)]
    unsafe fn stream<const L: usize>(
        row: *const i8,
        planes: *const i8,
        stride: usize,
        vector_cols: usize,
    ) -> [i64; L] {
        // SAFETY: every load stays inside the bounds stated above.
        unsafe {
            let mut acc = [[vdupq_n_s32(0); L]; 4];
            let mut c = 0usize;
            while c + 64 <= vector_cols {
                for b in 0..4 {
                    let w = vld1q_s8(row.add(c + 16 * b));
                    for l in 0..L {
                        let d = vld1q_s8(planes.add(l * stride + c + 16 * b));
                        acc[b][l] = sdot(acc[b][l], w, d);
                    }
                }
                c += 64;
            }
            while c < vector_cols {
                let w = vld1q_s8(row.add(c));
                for l in 0..L {
                    acc[0][l] = sdot(acc[0][l], w, vld1q_s8(planes.add(l * stride + c)));
                }
                c += 16;
            }
            let mut total = [0i64; L];
            for l in 0..L {
                for b in 0..4 {
                    total[l] += i64::from(vaddvq_s32(acc[b][l]));
                }
            }
            total
        }
    }

    /// Exact `Σ a_t · b_t` of two equal-length i32 vectors.
    ///
    /// # Safety
    /// NEON must be available and `a.len() == b.len()`.
    #[target_feature(enable = "neon")]
    pub(super) unsafe fn dot_i32(a: &[i32], b: &[i32]) -> i64 {
        let n = a.len().min(b.len());
        let vector = n - n % 8;
        // SAFETY: every 16-byte load starts at t <= vector - 4 < n.
        unsafe {
            let mut acc0 = vdupq_n_s64(0);
            let mut acc1 = vdupq_n_s64(0);
            let mut acc2 = vdupq_n_s64(0);
            let mut acc3 = vdupq_n_s64(0);
            let mut t = 0usize;
            while t < vector {
                let a0 = vld1q_s32(a.as_ptr().add(t));
                let b0 = vld1q_s32(b.as_ptr().add(t));
                let a1 = vld1q_s32(a.as_ptr().add(t + 4));
                let b1 = vld1q_s32(b.as_ptr().add(t + 4));
                acc0 = vmlal_s32(acc0, vget_low_s32(a0), vget_low_s32(b0));
                acc1 = vmlal_high_s32(acc1, a0, b0);
                acc2 = vmlal_s32(acc2, vget_low_s32(a1), vget_low_s32(b1));
                acc3 = vmlal_high_s32(acc3, a1, b1);
                t += 8;
            }
            let mut total = vaddvq_s64(vaddq_s64(vaddq_s64(acc0, acc1), vaddq_s64(acc2, acc3)));
            for (&x, &y) in a[vector..n].iter().zip(&b[vector..n]) {
                total += i64::from(x) * i64::from(y);
            }
            total
        }
    }

    /// `acc_t += w · v_t` with i64 lanes.
    ///
    /// # Safety
    /// NEON must be available and `acc.len() == v.len()`.
    #[target_feature(enable = "neon")]
    pub(super) unsafe fn axpy_i32(acc: &mut [i64], w: i32, v: &[i32]) {
        let n = acc.len().min(v.len());
        let vector = n - n % 4;
        // SAFETY: every load and store of four lanes starts at t <= vector - 4.
        unsafe {
            let mut t = 0usize;
            while t < vector {
                let x = vld1q_s32(v.as_ptr().add(t));
                let lo = vld1q_s64(acc.as_ptr().add(t));
                let hi = vld1q_s64(acc.as_ptr().add(t + 2));
                vst1q_s64(acc.as_mut_ptr().add(t), vmlal_n_s32(lo, vget_low_s32(x), w));
                vst1q_s64(acc.as_mut_ptr().add(t + 2), vmlal_high_n_s32(hi, x, w));
                t += 4;
            }
            for (slot, &x) in acc[vector..n].iter_mut().zip(&v[vector..n]) {
                *slot += i64::from(w) * i64::from(x);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// xorshift64*: deterministic, no dependency.
    struct Rng(u64);

    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
        }

        /// Uniform-ish in `[-bound, bound]`.
        fn signed(&mut self, bound: i64) -> i64 {
            let span = 2 * u128::from(bound.unsigned_abs()) + 1;
            (u128::from(self.next()) % span) as i64 - bound
        }
    }

    fn matrix(rng: &mut Rng, rows: usize, cols: usize, weight_bound: i64) -> DyadicMatrix {
        DyadicMatrix {
            rows,
            cols,
            q: (0..rows * cols)
                .map(|_| rng.signed(weight_bound) as i8)
                .collect(),
            mu: (0..rows)
                .map(|_| ((1u64 << 30) + rng.next() % (1 << 30)) as i32)
                .collect(),
            k: (0..rows).map(|_| 40 + (rng.next() % 6) as u8).collect(),
        }
    }

    /// The spec's projection, written independently of every kernel.
    fn reference(m: &DyadicMatrix, x: &[i64]) -> Vec<i64> {
        (0..m.rows)
            .map(|r| {
                let acc: i128 = m.q[r * m.cols..(r + 1) * m.cols]
                    .iter()
                    .zip(x)
                    .map(|(&w, &v)| i128::from(w) * i128::from(v))
                    .sum();
                let y = (acc * i128::from(m.mu[r])) >> m.k[r];
                i64::try_from(y).unwrap()
            })
            .collect()
    }

    /// Largest per-element magnitude that keeps `127 · Σ|x| < 2^63`.
    fn magnitude_cap(cols: usize) -> i64 {
        ((1u128 << 63) / (127 * cols as u128 + 1) - 1) as i64
    }

    #[test]
    fn digit_ranges_follow_the_formula_and_bound_the_split() {
        assert_eq!(digit_range(16, 1), (-32_768, 32_767));
        assert_eq!(digit_range(8, 1), (-128, 127));
        assert_eq!(digit_range(8, 2), (-32_896, 32_639));
        assert_eq!(digit_range(8, 3), (-8_421_504, 8_355_711));
        assert_eq!(
            digit_range(16, 3),
            (-140_739_635_871_744, 140_735_340_838_911)
        );
        for kernel in Kernel::available_kernels() {
            let (bits, most) = match kernel {
                Kernel::Avx2 => (16, I16_MAX_LIMBS),
                Kernel::Neon => (8, I8_MAX_LIMBS),
                Kernel::Scalar => continue,
            };
            for limbs in 1..=most {
                let (min, max) = digit_range(bits, limbs);
                // The range's edges need exactly `limbs` digits; one past
                // them needs one more (or is refused at the top).
                for (values, expected) in [
                    (vec![min, max, 0, -1, 1], limbs),
                    (vec![max + 1], limbs + 1),
                    (vec![min - 1], limbs + 1),
                ] {
                    let mut input = PreparedInput::new();
                    input.prepare(&values, kernel).unwrap();
                    if expected <= most {
                        assert_eq!(input.kernel(), kernel, "{values:?}");
                        assert_eq!(input.limbs(), expected, "{values:?}");
                    } else {
                        assert_eq!(input.kernel(), Kernel::Scalar, "{values:?}");
                    }
                }
            }
        }
    }

    #[test]
    fn every_kernel_matches_the_reference_projection() {
        let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
        // Widths around every vector, tile and i32-flush boundary, plus the
        // SmolLM3 widths; magnitudes that need 1 to 4 digits or the scalar
        // fallback.
        let widths = [1usize, 15, 16, 17, 33, 64, 2047, 2048, 2049, 4111, 11_008];
        let row_counts = [1usize, 3, 4, 5, 9, 66];
        let magnitudes = [
            100i64,
            30_000,
            8_000_000,
            2_000_000_000,
            2_200_000_000,
            1 << 40,
        ];
        for kernel in Kernel::available_kernels() {
            for &cols in &widths {
                for &rows in &row_counts {
                    if rows * cols > 300_000 {
                        continue;
                    }
                    let m = matrix(&mut rng, rows, cols, 127);
                    for &magnitude in &magnitudes {
                        let bound = magnitude.min(magnitude_cap(cols));
                        let x: Vec<i64> = (0..cols).map(|_| rng.signed(bound)).collect();
                        let mut input = PreparedInput::new();
                        input.prepare(&x, kernel).unwrap();
                        let mut out = vec![0i64; rows];
                        project(MatrixRows::of(&m), &input, &mut out).unwrap();
                        assert_eq!(
                            out,
                            reference(&m, &x),
                            "kernel {} ({} digits) rows {rows} cols {cols} magnitude {magnitude}",
                            input.kernel().name(),
                            input.limbs()
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn both_tilings_compute_the_same_integers() {
        let _guard = crate::canonical_simd::kernel_switch_guard();
        let mut rng = Rng(0x7111_4e55);
        for kernel in Kernel::available_kernels() {
            for &cols in &[16usize, 33, 64, 2048, 2080, 4111, 11_008] {
                let rows = 9;
                let m = matrix(&mut rng, rows, cols, 127);
                for magnitude in [1i64 << 12, 1 << 20, 3 << 29, 1 << 40] {
                    let bound = magnitude.min(magnitude_cap(cols));
                    let x: Vec<i64> = (0..cols).map(|_| rng.signed(bound)).collect();
                    let mut input = PreparedInput::new();
                    input.prepare(&x, kernel).unwrap();
                    let expected = reference(&m, &x);
                    for tiling in Tiling::ALL {
                        set_tiling(tiling);
                        let mut out = vec![0i64; rows];
                        project(MatrixRows::of(&m), &input, &mut out).unwrap();
                        assert_eq!(
                            out,
                            expected,
                            "{} {} cols {cols} magnitude {magnitude}",
                            kernel.name(),
                            tiling.name()
                        );
                    }
                }
            }
        }
        set_tiling(Tiling::default());
        assert_eq!(Tiling::default(), Tiling::Auto);
        assert_eq!(Tiling::parse("auto").unwrap(), Tiling::Auto);
        assert_eq!(Tiling::parse("stream").unwrap(), Tiling::Stream);
        assert!(Tiling::parse("rows8").is_err());
        // `auto` streams single-input AVX2 calls and tiles everything else;
        // an explicit order is kept for every kernel and kind of call.
        assert_eq!(Tiling::Auto.resolve(Kernel::Avx2, false), Tiling::Stream);
        assert_eq!(Tiling::Auto.resolve(Kernel::Avx2, true), Tiling::Rows4);
        assert_eq!(Tiling::Auto.resolve(Kernel::Neon, false), Tiling::Rows4);
        assert_eq!(Tiling::Auto.resolve(Kernel::Scalar, false), Tiling::Rows4);
        assert_eq!(Tiling::Stream.resolve(Kernel::Neon, true), Tiling::Stream);
        assert_eq!(Tiling::Rows4.resolve(Kernel::Avx2, false), Tiling::Rows4);
    }

    #[test]
    fn calibration_picks_a_concrete_order_and_auto_follows_it() {
        let _guard = crate::canonical_simd::kernel_switch_guard();
        let mut rng = Rng(0xca11_b8a7);
        let cols = 2048;
        let mats: Vec<DyadicMatrix> = (0..3).map(|_| matrix(&mut rng, 64, cols, 127)).collect();
        let views: Vec<MatrixRows<'_>> = mats.iter().map(MatrixRows::of).collect();
        let x: Vec<i64> = (0..cols).map(|_| rng.signed(8 << 16)).collect();
        let before = tiling();
        for kernel in Kernel::available_kernels() {
            let mut input = PreparedInput::new();
            input.prepare(&x, kernel).unwrap();
            if kernel == Kernel::Scalar {
                assert!(calibrate_auto(kernel, &views, &input, 1).is_err());
                assert_eq!(Tiling::Auto.resolve(kernel, false), Tiling::Rows4);
                continue;
            }
            let built_in = Tiling::Auto.resolve(kernel, false);
            let c = calibrate_auto(kernel, &views, &input, 2).unwrap();
            assert!(matches!(c.chosen, Tiling::Rows4 | Tiling::Stream), "{c:?}");
            assert_eq!(c.kernel, kernel);
            assert_eq!(c.weight_bytes, 3 * 64 * cols);
            assert_eq!(c.passes, 2);
            assert!(
                c.rows4_seconds.is_finite() && c.stream_seconds.is_finite(),
                "{c:?}"
            );
            // `auto` follows the measurement for single-input calls only; an
            // explicit order and the configured tiling are untouched.
            assert_eq!(Tiling::Auto.resolve(kernel, false), c.chosen);
            assert_eq!(Tiling::Auto.resolve(kernel, true), Tiling::Rows4);
            assert_eq!(Tiling::Stream.resolve(kernel, false), Tiling::Stream);
            assert_eq!(tiling(), before);
            // Whatever was chosen, the values are the reference's.
            let mut out = vec![0i64; 64];
            project(views[0], &input, &mut out).unwrap();
            assert_eq!(out, reference(&mats[0], &x));
            set_auto_order(kernel, Tiling::Auto);
            assert_eq!(Tiling::Auto.resolve(kernel, false), built_in);
        }
    }

    #[test]
    fn extreme_digits_and_weights_cannot_overflow_a_lane() {
        // Weight -128 (outside the profile, inside every kernel's bound) times
        // the most negative digit in every column maximises every lane; 4,111
        // columns cross two i32 flush blocks and leave a tail. Both tilings.
        let _guard = crate::canonical_simd::kernel_switch_guard();
        for (kernel, tiling) in Kernel::available_kernels()
            .into_iter()
            .flat_map(|k| Tiling::ALL.map(|t| (k, t)))
        {
            set_tiling(tiling);
            let (bits, most) = match kernel {
                Kernel::Avx2 => (16, I16_MAX_LIMBS),
                Kernel::Neon => (8, I8_MAX_LIMBS),
                Kernel::Scalar => (16, 1),
            };
            for limbs in 1..=most {
                let cols = 4111;
                let (min, max) = digit_range(bits, limbs);
                // The precondition assumes |w| <= 127; with -128 weights the
                // accumulator itself must stay in i64 (beyond it the kernels
                // refuse, which `refuses_an_accumulator_beyond_i64` checks).
                let cap = ((1u128 << 63) / (128 * cols as u128) - 1) as i64;
                let value = min.max(-cap);
                for fill in [value, max.min(cap)] {
                    let m = DyadicMatrix {
                        rows: 5,
                        cols,
                        q: vec![-128; 5 * cols],
                        mu: vec![1 << 30; 5],
                        k: vec![62; 5],
                    };
                    let x = vec![fill; cols];
                    let mut input = PreparedInput::new();
                    input.prepare(&x, kernel).unwrap();
                    let mut out = vec![0i64; m.rows];
                    project(MatrixRows::of(&m), &input, &mut out).unwrap();
                    assert_eq!(
                        out,
                        reference(&m, &x),
                        "{} {} {limbs} {fill}",
                        kernel.name(),
                        tiling.name()
                    );
                }
            }
        }
        set_tiling(Tiling::default());
    }

    #[test]
    fn refuses_an_accumulator_beyond_i64() {
        // -128 weights are outside the profile, so the 127-based precondition
        // admits inputs whose exact sum leaves i64; the SIMD kernels refuse
        // them instead of wrapping.
        let cols = 4111;
        let cap = magnitude_cap(cols);
        let m = DyadicMatrix {
            rows: 1,
            cols,
            q: vec![-128; cols],
            mu: vec![1 << 30],
            k: vec![62],
        };
        let x = vec![-cap; cols];
        let exact: i128 = 128 * i128::from(cap) * cols as i128;
        assert!(exact > i128::from(i64::MAX));
        for kernel in Kernel::available_kernels() {
            let mut input = PreparedInput::new();
            input.prepare(&x, kernel).unwrap();
            if input.kernel() == Kernel::Scalar {
                // The scalar loop is the reference, defined for profile
                // weights only; it is not asked about this input.
                continue;
            }
            let mut out = vec![0i64; 1];
            let result = project(MatrixRows::of(&m), &input, &mut out);
            assert!(
                matches!(result, Err(ModernError::Domain(_))),
                "{}: {result:?}",
                kernel.name()
            );
        }
    }

    #[test]
    fn the_precondition_refuses_like_the_reference() {
        for kernel in Kernel::available_kernels() {
            let big = 1i64 << 57;
            let mut input = PreparedInput::new();
            assert!(matches!(
                input.prepare(&[big, big], kernel),
                Err(ModernError::Domain(_))
            ));
            assert!(matches!(
                arith::check_projection_input(&[big, big]),
                Err(ModernError::Domain(_))
            ));
            // Just inside: accepted (scalar after the digit refusal).
            let ok = magnitude_cap(2);
            input.prepare(&[ok, -ok], kernel).unwrap();
            assert!(input.prepare(&[], kernel).is_err());
        }
    }

    #[test]
    fn fused_calls_match_separate_projections_across_chunk_boundaries() {
        let mut rng = Rng(0x5eed_cafe);
        let cols = 96;
        let parts: Vec<DyadicMatrix> = [30usize, 50, 70]
            .iter()
            .map(|&rows| matrix(&mut rng, rows, cols, 127))
            .collect();
        let x: Vec<i64> = (0..cols).map(|_| rng.signed(3 << 20)).collect();
        let y: Vec<i64> = (0..cols).map(|_| rng.signed(1 << 17)).collect();
        for kernel in Kernel::available_kernels() {
            let mut px = PreparedInput::new();
            px.prepare(&x, kernel).unwrap();
            let mut py = PreparedInput::new();
            py.prepare(&y, kernel).unwrap();
            // Different inputs per part, as the down projections of experts.
            let views = [
                (MatrixRows::of(&parts[0]), &px),
                (MatrixRows::of(&parts[1]), &py),
                (MatrixRows::of(&parts[2]), &px),
            ];
            let mut expected = reference(&parts[0], &x);
            expected.extend(reference(&parts[1], &y));
            expected.extend(reference(&parts[2], &x));
            let mut fused = vec![0i64; expected.len()];
            project_many(&views, &mut fused).unwrap();
            assert_eq!(fused, expected, "{}", kernel.name());
            // A row range is the same rows of the whole matrix.
            let range = MatrixRows::range(&parts[2], 40, 20).unwrap();
            let mut ranged = vec![0i64; range.rows];
            project(range, &px, &mut ranged).unwrap();
            assert_eq!(ranged, expected[120..140].to_vec());
            assert!(MatrixRows::range(&parts[2], 60, 11).is_err());
            let mut short = vec![0i64; expected.len() - 1];
            assert!(project_many(&views, &mut short).is_err());
        }
    }

    #[test]
    fn swiglu_matches_the_reference_operators() {
        let mut rng = Rng(0xfeed_beef);
        let cols = 80;
        let gates: Vec<DyadicMatrix> = (0..3).map(|_| matrix(&mut rng, 70, cols, 127)).collect();
        let ups: Vec<DyadicMatrix> = (0..3).map(|_| matrix(&mut rng, 70, cols, 127)).collect();
        let x: Vec<i64> = (0..cols).map(|_| rng.signed(1 << 18)).collect();
        let mut expected = Vec::new();
        for (gate, up) in gates.iter().zip(&ups) {
            let g = reference(gate, &x);
            let u = reference(up, &x);
            for (&g, &u) in g.iter().zip(&u) {
                expected.push(arith::gated_silu(g, u).unwrap());
            }
        }
        for kernel in Kernel::available_kernels() {
            let mut input = PreparedInput::new();
            input.prepare(&x, kernel).unwrap();
            let parts: Vec<(MatrixRows<'_>, MatrixRows<'_>)> = gates
                .iter()
                .zip(&ups)
                .map(|(g, u)| (MatrixRows::of(g), MatrixRows::of(u)))
                .collect();
            let mut out = vec![0i64; expected.len()];
            project_swiglu_many(&parts, &input, &mut out).unwrap();
            assert_eq!(out, expected, "{}", kernel.name());
        }
    }

    #[test]
    fn batched_calls_match_one_token_at_a_time() {
        let mut rng = Rng(0xba7c4);
        let cols = 70;
        let wq = matrix(&mut rng, 37, cols, 127);
        let wk = matrix(&mut rng, 29, cols, 127);
        let gate = matrix(&mut rng, 75, cols, 127);
        let up = matrix(&mut rng, 75, cols, 127);
        for kernel in Kernel::available_kernels() {
            for tokens in [1usize, 2, 5, MAX_BATCH] {
                // Mixed magnitudes, so the inputs use different digit counts
                // (and, on NEON, the scalar fallback) inside one batch, all
                // small enough that the gated SiLU stays below 2^62.
                let inputs: Vec<PreparedInput> = (0..tokens)
                    .map(|t| {
                        let bound = [1i64 << 10, 1 << 20, 1 << 33, 1 << 36][t % 4];
                        let x: Vec<i64> = (0..cols).map(|_| rng.signed(bound)).collect();
                        let mut input = PreparedInput::new();
                        input.prepare(&x, kernel).unwrap();
                        input
                    })
                    .collect();
                let rows = wq.rows + wk.rows;
                let mut batched = vec![0i64; rows * tokens];
                project_batch(
                    &[MatrixRows::of(&wq), MatrixRows::of(&wk)],
                    &inputs,
                    &mut batched,
                )
                .unwrap();
                let mut swiglu = vec![0i64; gate.rows * tokens];
                project_swiglu_batch(
                    MatrixRows::of(&gate),
                    MatrixRows::of(&up),
                    &inputs,
                    &mut swiglu,
                )
                .unwrap();
                for (t, input) in inputs.iter().enumerate() {
                    let mut single = vec![0i64; rows];
                    project_many(
                        &[(MatrixRows::of(&wq), input), (MatrixRows::of(&wk), input)],
                        &mut single,
                    )
                    .unwrap();
                    let column: Vec<i64> = (0..rows).map(|r| batched[r * tokens + t]).collect();
                    assert_eq!(column, single, "{} tokens {tokens} t {t}", kernel.name());
                    let mut silu = vec![0i64; gate.rows];
                    project_swiglu(MatrixRows::of(&gate), MatrixRows::of(&up), input, &mut silu)
                        .unwrap();
                    let column: Vec<i64> = (0..gate.rows).map(|r| swiglu[r * tokens + t]).collect();
                    assert_eq!(column, silu, "{} tokens {tokens} t {t}", kernel.name());
                }
            }
        }
        let too_many: Vec<PreparedInput> = (0..=MAX_BATCH).map(|_| PreparedInput::new()).collect();
        let mut out = vec![0i64; wq.rows * too_many.len()];
        assert!(project_batch(&[MatrixRows::of(&wq)], &too_many, &mut out).is_err());
    }

    #[test]
    fn thread_count_cannot_change_a_projection() {
        let mut rng = Rng(42);
        let m = matrix(&mut rng, 333, 517, 127);
        let x: Vec<i64> = (0..517).map(|_| rng.signed(1 << 30)).collect();
        for kernel in Kernel::available_kernels() {
            let mut input = PreparedInput::new();
            input.prepare(&x, kernel).unwrap();
            let run = |threads: usize| {
                let pool = rayon::ThreadPoolBuilder::new()
                    .num_threads(threads)
                    .build()
                    .unwrap();
                pool.install(|| {
                    let mut out = vec![0i64; m.rows];
                    project(MatrixRows::of(&m), &input, &mut out).unwrap();
                    out
                })
            };
            assert_eq!(run(1), run(3));
            assert_eq!(run(1), reference(&m, &x));
        }
    }

    #[test]
    fn attention_helpers_are_exact_for_every_kernel() {
        let mut rng = Rng(7);
        for width in [1usize, 3, 4, 7, 8, 9, 64, 128, 131] {
            // Σ|a| < 2^32 and |b| < 2^31, the attention bound.
            let bound = ((1i64 << 32) / (width as i64 + 1)).min(i64::from(i32::MAX));
            let a: Vec<i32> = (0..width).map(|_| rng.signed(bound) as i32).collect();
            let mut b: Vec<i32> = (0..width)
                .map(|_| rng.signed(i64::from(i32::MAX)) as i32)
                .collect();
            b[0] = i32::MIN;
            let exact: i64 = a
                .iter()
                .zip(&b)
                .map(|(&x, &y)| i64::from(x) * i64::from(y))
                .sum();
            let acc0: Vec<i64> = (0..width).map(|_| rng.signed(1 << 50)).collect();
            let w = 65_536;
            let mut expected = acc0.clone();
            for (slot, &y) in expected.iter_mut().zip(&b) {
                *slot += i64::from(w) * i64::from(y);
            }
            for kernel in Kernel::available_kernels() {
                let isa = Isa::new(kernel);
                assert_eq!(isa.dot_i32(&a, &b), exact, "{} {width}", kernel.name());
                let mut acc = acc0.clone();
                isa.axpy_i32(&mut acc, w, &b);
                assert_eq!(acc, expected, "{} {width}", kernel.name());
            }
        }
    }

    #[test]
    fn census_counts_attempts_and_refusals() {
        let _guard = crate::canonical_simd::kernel_switch_guard();
        set_census_enabled(true);
        reset_census();
        let best = Kernel::best();
        let mut input = PreparedInput::new();
        input.prepare(&[1, 2, 3], best).unwrap();
        input.prepare(&[1, 2, 3], Kernel::Scalar).unwrap();
        // Far outside every digit domain, inside the precondition.
        input.prepare(&[1 << 50, 1], best).unwrap();
        let c = census();
        set_census_enabled(false);
        if best != Kernel::Scalar {
            assert!(c.attempted >= 2, "{c:?}");
            assert!(c.accepted >= 1, "{c:?}");
            assert!(c.refused_activation_out_of_domain >= 1, "{c:?}");
            assert!(limb_histogram()[1] >= 1);
        }
        reset_census();
    }

    #[test]
    fn kernel_names_round_trip_and_unavailable_kernels_fall_back() {
        for kernel in Kernel::ALL {
            assert_eq!(Kernel::parse(kernel.name()).unwrap(), kernel);
            assert_eq!(Kernel::from_code(kernel.code()), kernel);
        }
        assert!(Kernel::parse("avx512").is_err());
        assert!(Kernel::best().available());
        assert!(Kernel::available_kernels().contains(&Kernel::Scalar));
        for kernel in Kernel::ALL {
            let mut input = PreparedInput::new();
            input.prepare(&[5, -7, 9], kernel).unwrap();
            if !kernel.available() {
                assert_eq!(input.kernel(), Kernel::Scalar);
            }
            assert!(Isa::new(kernel).kernel().available());
        }
    }
}
