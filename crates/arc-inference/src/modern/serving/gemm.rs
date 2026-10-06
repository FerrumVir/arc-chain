//! Exact batched projection: all the activation rows of a step share one read
//! of each weight row.
//!
//! The profile's projection is `y_i = (sum_j W_ij * x_j * mu_i) >> k_i` with an
//! exact `i64` sum (spec §5.2). An integer sum does not depend on order, vector
//! width or grouping. Computing the sums of many activation rows together (a
//! GEMM instead of one GEMV per token) therefore changes how often the weights
//! cross the memory bus, and nothing else. Two exact kernels compute the sums:
//!
//! * **scalar**: [`dot_i8_i64`] for every pair of weight row and activation
//!   row, with blocks of weight rows in parallel;
//! * **SIMD**, when the canonical fast kernel is enabled
//!   (`ARC_FAST_CANONICAL_KERNEL=1` or `--kernel simd`): each activation is
//!   split into balanced digits, base 2^16 on x86-64 AVX2 and base 2^8 on ARM
//!   `sdot`. The digit planes of every activation row are multiplied against
//!   each weight vector while that vector sits in a register. A row with a value
//!   outside the digit domain uses the scalar sum, for that row only.
//!
//! A step with one live row goes through [`arith::project`], so batch 1 runs
//! exactly today's single-token path. Domain checks and epilogues are per row,
//! so a row that leaves the profile's domain fails alone.

use rayon::prelude::*;

use crate::modern::ModernError;
use crate::modern::arith::{
    self, DyadicMatrix, check_projection_input, dot_i8_i64, dyadic_epilogue,
};

/// Weight rows per parallel task. With a column chunk of [`simd`]'s `CHUNK`
/// width, a block's weights and one group of digit planes fit in L1 together.
const ROW_BLOCK: usize = 8;

/// Largest inner dimension the digit kernels accept. With `|w|, |d| <= 128`
/// every `i32` partial sum of an ARM digit plane stays below `2^31`; the x86
/// kernel flushes to `i64` and uses the same limit for one rule everywhere.
pub const MAX_SIMD_COLS: usize = 131_071;

/// `out[r] = W x_r` (spec §5.2) for each activation row `r` of `xs` that has
/// no error yet.
///
/// `xs` holds `errors.len()` rows of `m.cols` values and `out` the same number
/// of rows of `m.rows` values. A row whose input or output leaves the profile's
/// domain gets the error [`arith::project`] would return for it alone, and
/// zero outputs; other rows are unaffected.
pub fn project_rows(
    m: &DyadicMatrix,
    xs: &[i64],
    out: &mut [i64],
    errors: &mut [Option<ModernError>],
) {
    let n = errors.len();
    debug_assert_eq!(xs.len(), n * m.cols);
    debug_assert_eq!(out.len(), n * m.rows);
    out.fill(0);
    for (r, error) in errors.iter_mut().enumerate() {
        if error.is_none()
            && let Err(e) = check_projection_input(&xs[r * m.cols..(r + 1) * m.cols])
        {
            *error = Some(e);
        }
    }
    let live: Vec<usize> = (0..n).filter(|&r| errors[r].is_none()).collect();
    match live.len() {
        0 => {}
        1 => {
            let r = live[0];
            let y = &mut out[r * m.rows..(r + 1) * m.rows];
            if let Err(e) = arith::project(m, &xs[r * m.cols..(r + 1) * m.cols], y) {
                y.fill(0);
                errors[r] = Some(e);
            }
        }
        width => {
            let sums = exact_sums(m, xs, &live);
            for (t, &r) in live.iter().enumerate() {
                let y = &mut out[r * m.rows..(r + 1) * m.rows];
                let mut failure = None;
                for (i, slot) in y.iter_mut().enumerate() {
                    match dyadic_epilogue(sums[i * width + t], m.mu[i], m.k[i]) {
                        Ok(value) => *slot = value,
                        Err(e) => {
                            failure = Some(e);
                            break;
                        }
                    }
                }
                if let Some(e) = failure {
                    y.fill(0);
                    errors[r] = Some(e);
                }
            }
        }
    }
}

/// Exact sums `acc[i * live.len() + t] = sum_c W[i][c] * x_{live[t]}[c]`.
///
/// Every row of `xs` named in `live` has passed [`check_projection_input`], so
/// no `i64` sum can overflow. Work is tiled: a parallel task owns a block of
/// [`ROW_BLOCK`] weight rows, and each group of activation data is reused
/// across that whole block while it is in cache.
pub fn exact_sums(m: &DyadicMatrix, xs: &[i64], live: &[usize]) -> Vec<i64> {
    let width = live.len();
    let mut acc = vec![0i64; m.rows * width];
    if width == 0 {
        return acc;
    }
    let cols = m.cols;
    #[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
    {
        if digit_kernel_enabled() && cols <= MAX_SIMD_COLS {
            let planes = Planes::split(xs, cols, live);
            acc.par_chunks_mut(ROW_BLOCK * width)
                .enumerate()
                .for_each(|(block, chunk)| {
                    let rows = block_rows(m, block, chunk.len() / width);
                    planes.accumulate(&rows, chunk, width);
                    for &t in &planes.scalar {
                        let x = &xs[live[t] * cols..(live[t] + 1) * cols];
                        for (row, out) in rows.iter().zip(chunk.chunks_mut(width)) {
                            out[t] = dot_i8_i64(row, x);
                        }
                    }
                });
            return acc;
        }
    }
    acc.par_chunks_mut(ROW_BLOCK * width)
        .enumerate()
        .for_each(|(block, chunk)| {
            let rows = block_rows(m, block, chunk.len() / width);
            for (t, &r) in live.iter().enumerate() {
                let x = &xs[r * cols..(r + 1) * cols];
                for (row, out) in rows.iter().zip(chunk.chunks_mut(width)) {
                    out[t] = dot_i8_i64(row, x);
                }
            }
        });
    acc
}

/// The weight rows of parallel block `block` (`count` of them).
fn block_rows(m: &DyadicMatrix, block: usize, count: usize) -> Vec<&[i8]> {
    let first = block * ROW_BLOCK;
    (first..first + count)
        .map(|i| &m.q[i * m.cols..(i + 1) * m.cols])
        .collect()
}

/// Whether batched projections use the digit kernel on this CPU.
pub fn digit_kernel_enabled() -> bool {
    #[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
    {
        crate::canonical_simd::fast_canonical_kernel_enabled() && simd::available()
    }
    #[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
    {
        false
    }
}

/// Balanced digits of `x` in base `2^DIGIT_BITS`, least significant first, or
/// `None` when `x` needs more than `MAX_DIGITS` of them.
///
/// Each step takes `d` in `[-base/2, base/2)` with `rest = d (mod base)` and
/// replaces `rest` by `(rest - d) / base`, an exact division, so
/// `x = sum_i d_i * base^i` holds whenever the final rest is zero.
#[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
fn split_digits(x: i64) -> Option<[i64; simd::MAX_DIGITS]> {
    let base = 1i64 << simd::DIGIT_BITS;
    let half = base / 2;
    let mut digits = [0i64; simd::MAX_DIGITS];
    let mut rest = x;
    for digit in &mut digits {
        let d = (rest + half).rem_euclid(base) - half;
        *digit = d;
        rest = (rest - d) >> simd::DIGIT_BITS;
    }
    (rest == 0).then_some(digits)
}

/// The digit planes of a step's live activation rows.
#[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
struct Planes {
    cols: usize,
    /// Digit planes of `cols` digits each, concatenated.
    digits: Vec<simd::Digit>,
    /// Per plane: the live row it belongs to and the weight of its digit as a
    /// power of two.
    owners: Vec<(usize, u32)>,
    /// Live rows with a value outside the digit domain (scalar sums).
    scalar: Vec<usize>,
}

#[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
impl Planes {
    fn split(xs: &[i64], cols: usize, live: &[usize]) -> Self {
        let rows: Vec<_> = live
            .par_iter()
            .map(|&r| split_row(&xs[r * cols..(r + 1) * cols]))
            .collect();
        let mut planes = Planes {
            cols,
            digits: Vec::new(),
            owners: Vec::new(),
            scalar: Vec::new(),
        };
        for (t, row) in rows.into_iter().enumerate() {
            match row {
                Some((used, digits)) => {
                    planes
                        .owners
                        .extend((0..used).map(|level| (t, simd::DIGIT_BITS * level as u32)));
                    planes.digits.extend_from_slice(&digits);
                }
                None => planes.scalar.push(t),
            }
        }
        planes
    }

    /// Add each weight row's exact dot product with every plane into the
    /// plane owner's slot of that row's output (`width` slots per row).
    ///
    /// Loop order: column chunks, then groups of planes, then weight rows. A
    /// chunk of one plane group and of the block's weight rows stay in L1
    /// while every pair is multiplied. Partial sums of chunks add up exactly.
    fn accumulate(&self, rows: &[&[i8]], out: &mut [i64], width: usize) {
        let count = self.owners.len();
        let cols = self.cols;
        let mut c0 = 0usize;
        while c0 < cols {
            let c1 = (c0 + simd::CHUNK).min(cols);
            let mut start = 0usize;
            while start < count {
                let take = (count - start).min(simd::GROUP);
                let group: [&[simd::Digit]; simd::GROUP] = std::array::from_fn(|q| {
                    let p = start + q.min(take - 1);
                    &self.digits[p * cols + c0..p * cols + c1]
                });
                for (row, slots) in rows.iter().zip(out.chunks_mut(width)) {
                    // SAFETY: `digit_kernel_enabled` checked the CPU feature,
                    // every plane chunk holds `c1 - c0` digits like the row
                    // chunk, and `c1 - c0 <= CHUNK <= MAX_SIMD_COLS`.
                    let sums = unsafe { simd::dot_planes(&row[c0..c1], &group) };
                    for (q, &sum) in sums.iter().enumerate().take(take) {
                        let (t, shift) = self.owners[start + q];
                        slots[t] += sum * (1i64 << shift);
                    }
                }
                start += take;
            }
            c0 = c1;
        }
    }
}

/// One activation row's digit planes, least significant first (`used` planes
/// of `x.len()` digits each), or `None` when a value is outside the digit
/// domain.
#[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
fn split_row(x: &[i64]) -> Option<(usize, Vec<simd::Digit>)> {
    let mut split = Vec::with_capacity(x.len());
    let mut used = 0usize;
    for &value in x {
        let digits = split_digits(value)?;
        if let Some(top) = digits.iter().rposition(|&d| d != 0) {
            used = used.max(top + 1);
        }
        split.push(digits);
    }
    let mut planes = Vec::with_capacity(used * x.len());
    for level in 0..used {
        planes.extend(split.iter().map(|digits| digits[level] as simd::Digit));
    }
    Some((used, planes))
}

#[cfg(target_arch = "x86_64")]
mod simd {
    //! AVX2: balanced base-2^16 digits multiplied by sign-extended weights with
    //! `vpmaddwd`, which is exact for these operands.
    //!
    //! With `|w| <= 128` and `|d| <= 2^15`, one `vpmaddwd` lane gains at most
    //! `2 * 128 * 2^15 = 2^23` per 16 columns. Flushing the `i32` lanes to
    //! `i64` every [`FLUSH`] iterations keeps each lane below `2^30`. The eight
    //! accumulators are named variables so they stay in registers: each weight
    //! vector is loaded and widened once and feeds eight `vpmaddwd`.

    use std::arch::x86_64::{
        _mm_loadu_si128, _mm256_add_epi32, _mm256_cvtepi8_epi16, _mm256_loadu_si256,
        _mm256_madd_epi16, _mm256_setzero_si256, _mm256_storeu_si256,
    };

    /// Digit type of a plane.
    pub(super) type Digit = i16;
    /// Bits per digit.
    pub(super) const DIGIT_BITS: u32 = 16;
    /// Digits that cover any accepted activation.
    pub(super) const MAX_DIGITS: usize = 2;
    /// Planes multiplied per pass over a weight row.
    pub(super) const GROUP: usize = 8;
    /// Columns per call: eight 2 KiB plane chunks and eight 1 KiB weight
    /// chunks (24 KiB) fit in a 32 KiB L1.
    pub(super) const CHUNK: usize = 1024;
    /// 16-column iterations between flushes: `128 * 2^23 = 2^30`.
    const FLUSH: usize = 128;

    pub(super) fn available() -> bool {
        std::arch::is_x86_feature_detected!("avx2")
    }

    /// `sum_c row[c] * planes[q][c]` for every `q`, exactly.
    ///
    /// # Safety
    /// AVX2 must be available ([`available`]) and every plane must hold at
    /// least `row.len()` digits.
    #[target_feature(enable = "avx2")]
    pub(super) unsafe fn dot_planes(row: &[i8], planes: &[&[Digit]; GROUP]) -> [i64; GROUP] {
        let len = row.len();
        let full = len - len % 16;
        let w = row.as_ptr();
        let [p0, p1, p2, p3, p4, p5, p6, p7] = planes.map(|plane| plane.as_ptr());
        let mut total = [0i64; GROUP];
        let mut start = 0usize;
        while start < full {
            let stop = (start + FLUSH * 16).min(full);
            // SAFETY: AVX2 is available (caller contract). Every load starts at
            // a column `j` with `j + 16 <= full <= len`: 16 weight bytes from
            // `row` and 16 digits (32 bytes) from each plane, which holds at
            // least `len` digits.
            let lanes = unsafe {
                let zero = _mm256_setzero_si256();
                let (mut a0, mut a1, mut a2, mut a3) = (zero, zero, zero, zero);
                let (mut a4, mut a5, mut a6, mut a7) = (zero, zero, zero, zero);
                let mut j = start;
                while j < stop {
                    let x = _mm256_cvtepi8_epi16(_mm_loadu_si128(w.add(j).cast()));
                    let d0 = _mm256_loadu_si256(p0.add(j).cast());
                    let d1 = _mm256_loadu_si256(p1.add(j).cast());
                    let d2 = _mm256_loadu_si256(p2.add(j).cast());
                    let d3 = _mm256_loadu_si256(p3.add(j).cast());
                    a0 = _mm256_add_epi32(a0, _mm256_madd_epi16(x, d0));
                    a1 = _mm256_add_epi32(a1, _mm256_madd_epi16(x, d1));
                    a2 = _mm256_add_epi32(a2, _mm256_madd_epi16(x, d2));
                    a3 = _mm256_add_epi32(a3, _mm256_madd_epi16(x, d3));
                    let d4 = _mm256_loadu_si256(p4.add(j).cast());
                    let d5 = _mm256_loadu_si256(p5.add(j).cast());
                    let d6 = _mm256_loadu_si256(p6.add(j).cast());
                    let d7 = _mm256_loadu_si256(p7.add(j).cast());
                    a4 = _mm256_add_epi32(a4, _mm256_madd_epi16(x, d4));
                    a5 = _mm256_add_epi32(a5, _mm256_madd_epi16(x, d5));
                    a6 = _mm256_add_epi32(a6, _mm256_madd_epi16(x, d6));
                    a7 = _mm256_add_epi32(a7, _mm256_madd_epi16(x, d7));
                    j += 16;
                }
                let mut lanes = [[0i32; 8]; GROUP];
                for (out, value) in lanes.iter_mut().zip([a0, a1, a2, a3, a4, a5, a6, a7]) {
                    _mm256_storeu_si256(out.as_mut_ptr().cast(), value);
                }
                lanes
            };
            for (sum, lane) in total.iter_mut().zip(&lanes) {
                *sum += lane.iter().map(|&v| i64::from(v)).sum::<i64>();
            }
            start = stop;
        }
        for (c, &weight) in row.iter().enumerate().skip(full) {
            let weight = i64::from(weight);
            for (sum, plane) in total.iter_mut().zip(planes) {
                *sum += weight * i64::from(plane[c]);
            }
        }
        total
    }
}

#[cfg(target_arch = "aarch64")]
mod simd {
    //! ARM: balanced base-2^8 digits multiplied with `sdot`, four exact `i8`
    //! products into each `i32` lane.
    //!
    //! With `|w|, |d| <= 128` every product is at most `2^14`, so a plane's
    //! whole sum (the four lanes added) is at most `cols * 2^14 < 2^31` for
    //! `cols <= 131_071`. The eight accumulators are named variables so they
    //! stay in registers: each weight vector is loaded once and feeds eight
    //! independent `sdot` chains.

    use std::arch::aarch64::{int8x16_t, int32x4_t, vaddvq_s32, vdupq_n_s32, vld1q_s8};

    /// Digit type of a plane.
    pub(super) type Digit = i8;
    /// Bits per digit.
    pub(super) const DIGIT_BITS: u32 = 8;
    /// Digits that cover any accepted activation.
    pub(super) const MAX_DIGITS: usize = 4;
    /// Planes multiplied per pass over a weight row.
    pub(super) const GROUP: usize = 8;
    /// Columns per call: eight 2 KiB plane chunks and eight 2 KiB weight
    /// chunks (32 KiB) stay in a 64 KiB L1.
    pub(super) const CHUNK: usize = 2048;

    pub(super) fn available() -> bool {
        std::arch::is_aarch64_feature_detected!("dotprod")
    }

    /// One `SDOT Vd.4S, Vn.16B, Vm.16B`. The intrinsic is unstable on the
    /// pinned toolchain, so the instruction is emitted as inline assembly,
    /// exactly as `canonical_simd` does.
    ///
    /// # Safety
    /// Requires the `dotprod` target feature.
    #[target_feature(enable = "neon,dotprod")]
    #[inline]
    unsafe fn sdot(acc: int32x4_t, a: int8x16_t, b: int8x16_t) -> int32x4_t {
        let mut out = acc;
        // SAFETY: the instruction reads no memory and has no side effects.
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

    /// `sum_c row[c] * planes[q][c]` for every `q`, exactly.
    ///
    /// # Safety
    /// `dotprod` must be available ([`available`]), every plane must hold at
    /// least `row.len()` digits, and `row.len() <= 131_071`.
    #[target_feature(enable = "neon,dotprod")]
    pub(super) unsafe fn dot_planes(row: &[i8], planes: &[&[Digit]; GROUP]) -> [i64; GROUP] {
        let len = row.len();
        let full = len - len % 16;
        let w = row.as_ptr();
        let [p0, p1, p2, p3, p4, p5, p6, p7] = planes.map(|plane| plane.as_ptr());
        // SAFETY: `dotprod` is available (caller contract). Every load starts
        // at a column `j` with `j + 16 <= full <= len`, inside `row` and inside
        // every plane, which holds at least `len` digits.
        let mut total = unsafe {
            let zero = vdupq_n_s32(0);
            let (mut a0, mut a1, mut a2, mut a3) = (zero, zero, zero, zero);
            let (mut a4, mut a5, mut a6, mut a7) = (zero, zero, zero, zero);
            let mut j = 0usize;
            while j < full {
                let x = vld1q_s8(w.add(j));
                a0 = sdot(a0, x, vld1q_s8(p0.add(j)));
                a1 = sdot(a1, x, vld1q_s8(p1.add(j)));
                a2 = sdot(a2, x, vld1q_s8(p2.add(j)));
                a3 = sdot(a3, x, vld1q_s8(p3.add(j)));
                a4 = sdot(a4, x, vld1q_s8(p4.add(j)));
                a5 = sdot(a5, x, vld1q_s8(p5.add(j)));
                a6 = sdot(a6, x, vld1q_s8(p6.add(j)));
                a7 = sdot(a7, x, vld1q_s8(p7.add(j)));
                j += 16;
            }
            [
                i64::from(vaddvq_s32(a0)),
                i64::from(vaddvq_s32(a1)),
                i64::from(vaddvq_s32(a2)),
                i64::from(vaddvq_s32(a3)),
                i64::from(vaddvq_s32(a4)),
                i64::from(vaddvq_s32(a5)),
                i64::from(vaddvq_s32(a6)),
                i64::from(vaddvq_s32(a7)),
            ]
        };
        for (c, &weight) in row.iter().enumerate().skip(full) {
            let weight = i64::from(weight);
            for (sum, plane) in total.iter_mut().zip(planes) {
                *sum += weight * i64::from(plane[c]);
            }
        }
        total
    }
}
