//! Bit-exact vectorised canonical INT8 projection (ARM64 NEON `dotprod`).
//!
//! This module does **not** define a new arithmetic profile. It computes the
//! same integer value as the scalar kernel for every input it accepts, and
//! refuses every input it cannot prove exact, leaving the existing scalar path
//! to handle it. It is opt-in and defaults to OFF, so no admission, execution
//! or consensus semantics change unless a caller explicitly enables it.
//!
//! # Scope of the opt-in, stated exactly
//!
//! The integration point is [`crate::cached_integer_model::matmul_i8_into`],
//! which is the **generic per-row I8 matmul**. When the flag is on this path is
//! therefore reachable from *every* I8 matmul caller in the crate - whole-model
//! forward, shard forward, validator routes - not only from the named canonical
//! profile. That is deliberate but it is a wider blast radius than the profile
//! name suggests, and it is why the flag is default-off and experiment-only.
//!
//! # Arithmetic-safety precondition, proved HERE and not inherited
//!
//! `matmul_i8_canonical_rows` performs a checked input-magnitude and post-scale
//! overflow guard before it runs a kernel, but **the ordinary forward path does
//! not go through that wrapper** - it reaches `matmul_i8_into` through
//! `matmul_fast_preq`, and `matmul_i8_canonical_rows` is called only from
//! `tensor_parallel`. A Stage B claim that this module sat behind that guard
//! was wrong. It therefore establishes the equivalent bound itself, at its own
//! entry, before any output byte is written:
//!
//! * **Dot accumulation.** Inside the accepted domain the dot cannot overflow
//!   i64 by construction: `|acc| <= 128 * 2_155_905_152 * 131_071 ~= 3.62e16`,
//!   which is under `i64::MAX ~= 9.22e18`. No runtime check is needed for this.
//! * **Post-dot scale.** `acc * scale` **can** overflow for an adversarial or
//!   unusual scale vector, so it is checked: with
//!   `dot_bound = 128 * sum|x_j| >= |acc|`, the multiply is representable
//!   whenever `dot_bound * |scale_i|` is, and that is verified for every row
//!   before the parallel loop starts.
//!
//! Refusal falls back to the pre-existing scalar kernel, whose behaviour on
//! such inputs is exactly what it is today. **This module does not, and does
//! not claim to, fix unchecked scalar callers elsewhere in the crate.**
//!
//! # Why this is exact, not an approximation
//!
//! Integer addition and multiplication are exact and associative, so changing
//! the reduction order, the vector width or the thread count cannot change an
//! integer dot product. The only thing that can change a value is changing the
//! *operands*. The existing disabled NEON path in `cached_integer_model.rs`
//! requantised the i64 activation down to i8, which changes operands and is
//! therefore lossy. This module instead decomposes each activation into
//! balanced base-256 digits, which is an exact identity:
//!
//! ```text
//!   x = sum_{i<4} c_i * 256^i ,  c_i in [-128, 127]
//!   sum_j w_j * x_j = sum_{i<4} 256^i * (sum_j w_j * c_i[j])
//! ```
//!
//! Each inner sum is an i8 x i8 dot product, which `sdot` computes four lanes
//! at a time with exact i32 accumulation.
//!
//! # Derived domain (not assumed)
//!
//! * Four balanced base-256 digits represent exactly
//!   `[-2_155_905_152, +2_139_062_143]` — an **asymmetric** range that does
//!   *not* include `i32::MAX`. Activations outside it are refused.
//! * Weight bytes are untrusted and may be `-128`, so `|w| <= 128` and
//!   `|c| <= 128` give `|w*c| <= 16_384`. Accumulating `K` such products in
//!   i32 requires `16_384 * K <= i32::MAX`, i.e. **`K <= 131_071`**. Note that
//!   [`crate::tensor_parallel::MAX_ROW_INPUT_ELEMENTS`] is `131_072`, exactly
//!   one over this bound, so `K` is checked rather than assumed.
//! * Observed Llama-2-7B Q16 activations are about `2^22.6`, roughly 180x
//!   inside the digit bound.
//!
//! Conformance evidence, including sanitiser runs and the boundary suite, is in
//! `Desktop/Arc Chain V2/claude-reviews/stage-b-kernel-conformance.c`.

use crate::cached_integer_model::{I8Weights, I8WeightsView};
#[cfg(target_arch = "aarch64")]
use crate::integer_lut::FRAC_BITS;
#[cfg(target_arch = "aarch64")]
use rayon::prelude::*;
#[cfg(target_arch = "aarch64")]
use std::cell::RefCell;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

/// Balanced base-256 digits used by the vectorised path.
pub const LIMB_COUNT: usize = 4;
/// `sum_{i<4} 127 * 256^i`.
pub const LIMB_MAX: i64 = 2_139_062_143;
/// `sum_{i<4} -128 * 256^i`.
pub const LIMB_MIN: i64 = -2_155_905_152;
/// Largest inner dimension whose i32 partial sums cannot overflow.
pub const MAX_COLS_FOR_I32: usize = 131_071;
/// Bound on a batched call's token count, so the digit scratch stays bounded.
/// `n_tokens * LIMB_COUNT * in_size` bytes at most, i.e. 4 MiB per 1024 tokens
/// at `in_size = 1024`. Larger batches must be chunked by the caller.
pub const MAX_BATCH_TOKENS: usize = 1024;

const fn derive_limb_bound(digit: i64) -> i64 {
    let (mut acc, mut p, mut i) = (0i64, 1i64, 0usize);
    while i < LIMB_COUNT {
        acc += digit * p;
        p *= 256;
        i += 1;
    }
    acc
}
// Compile-time assertions: the published bounds ARE the digit formula, and the
// i32 partial-sum bound is the largest K for which 128*128*K fits in i32.
const _: () = assert!(LIMB_MAX == derive_limb_bound(127));
const _: () = assert!(LIMB_MIN == derive_limb_bound(-128));
const _: () = assert!(16_384i64 * (MAX_COLS_FOR_I32 as i64) <= i32::MAX as i64);
const _: () = assert!(16_384i64 * (MAX_COLS_FOR_I32 as i64 + 1) > i32::MAX as i64);
// The dot itself cannot overflow i64 anywhere inside the accepted domain.
const _: () = assert!((MAX_COLS_FOR_I32 as i128) * 128 * (-LIMB_MIN as i128) < i64::MAX as i128);

static FAST_KERNEL: AtomicBool = AtomicBool::new(false);

/// Why a projection declined the vectorised path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    /// No `dotprod` on this build/CPU.
    Unavailable,
    /// Shape, length or scale-vector mismatch.
    Shape,
    /// Inner dimension above the derived i32 partial-sum bound.
    InnerDimAboveI32Bound,
    /// An activation outside the exact four-digit domain.
    ActivationOutOfDomain,
    /// `dot_bound * |scale|` is not representable in i64.
    ScaleMultiplyWouldOverflow,
}

static CENSUS_ON: AtomicBool = AtomicBool::new(false);
static N_ATTEMPTED: AtomicU64 = AtomicU64::new(0);
static N_ACCEPTED: AtomicU64 = AtomicU64::new(0);
static N_UNAVAILABLE: AtomicU64 = AtomicU64::new(0);
static N_SHAPE: AtomicU64 = AtomicU64::new(0);
static N_K_BOUND: AtomicU64 = AtomicU64::new(0);
static N_DOMAIN: AtomicU64 = AtomicU64::new(0);
static N_SCALE: AtomicU64 = AtomicU64::new(0);

/// Counts of attempted / accepted / refused vectorised projections.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ProjectionCensus {
    pub attempted: u64,
    pub accepted: u64,
    pub refused_unavailable: u64,
    pub refused_shape: u64,
    pub refused_inner_dim_above_i32_bound: u64,
    pub refused_activation_out_of_domain: u64,
    pub refused_scale_multiply_would_overflow: u64,
}

impl ProjectionCensus {
    pub fn refused_total(&self) -> u64 {
        self.refused_unavailable
            + self.refused_shape
            + self.refused_inner_dim_above_i32_bound
            + self.refused_activation_out_of_domain
            + self.refused_scale_multiply_would_overflow
    }
}

/// Enable or disable diagnostic counting. Default OFF, so timed runs carry no
/// counting work at all unless a caller asks for it.
pub fn set_projection_census_enabled(on: bool) {
    CENSUS_ON.store(on, Ordering::Relaxed);
}

pub fn reset_projection_census() {
    for c in [
        &N_ATTEMPTED,
        &N_ACCEPTED,
        &N_UNAVAILABLE,
        &N_SHAPE,
        &N_K_BOUND,
        &N_DOMAIN,
        &N_SCALE,
    ] {
        c.store(0, Ordering::Relaxed);
    }
}

pub fn projection_census() -> ProjectionCensus {
    ProjectionCensus {
        attempted: N_ATTEMPTED.load(Ordering::Relaxed),
        accepted: N_ACCEPTED.load(Ordering::Relaxed),
        refused_unavailable: N_UNAVAILABLE.load(Ordering::Relaxed),
        refused_shape: N_SHAPE.load(Ordering::Relaxed),
        refused_inner_dim_above_i32_bound: N_K_BOUND.load(Ordering::Relaxed),
        refused_activation_out_of_domain: N_DOMAIN.load(Ordering::Relaxed),
        refused_scale_multiply_would_overflow: N_SCALE.load(Ordering::Relaxed),
    }
}

#[inline]
fn record_attempt() {
    if CENSUS_ON.load(Ordering::Relaxed) {
        N_ATTEMPTED.fetch_add(1, Ordering::Relaxed);
    }
}

#[cfg(target_arch = "aarch64")]
#[inline]
fn record_accept() {
    if CENSUS_ON.load(Ordering::Relaxed) {
        N_ACCEPTED.fetch_add(1, Ordering::Relaxed);
    }
}

#[inline]
fn record_refusal(reason: Refusal) -> bool {
    if CENSUS_ON.load(Ordering::Relaxed) {
        match reason {
            Refusal::Unavailable => &N_UNAVAILABLE,
            Refusal::Shape => &N_SHAPE,
            Refusal::InnerDimAboveI32Bound => &N_K_BOUND,
            Refusal::ActivationOutOfDomain => &N_DOMAIN,
            Refusal::ScaleMultiplyWouldOverflow => &N_SCALE,
        }
        .fetch_add(1, Ordering::Relaxed);
    }
    false
}

#[cfg(target_arch = "aarch64")]
thread_local! {
    /// Reused digit scratch. The digits depend only on the activation, so they
    /// are computed once per projection and read by every row. Allocating this
    /// per call was measurable: it is 44 KB per gate/up projection, 225 times
    /// per forward pass.
    static LIMB_SCRATCH: RefCell<Vec<i8>> = const { RefCell::new(Vec::new()) };
}

/// Enable or disable the vectorised canonical kernel process-wide.
///
/// Default is OFF. When OFF, every call site keeps the exact scalar code path
/// it had before this module existed.
pub fn set_fast_canonical_kernel(enabled: bool) {
    FAST_KERNEL.store(enabled, Ordering::Relaxed);
}

/// Whether the vectorised canonical kernel is enabled *and* available here.
///
/// `ARC_FAST_CANONICAL_KERNEL=1` sets the initial state once, so an existing
/// test binary can be run against the vectorised path without editing it. An
/// explicit [`set_fast_canonical_kernel`] call afterwards still wins.
pub fn fast_canonical_kernel_enabled() -> bool {
    static ENV_INIT: OnceLock<()> = OnceLock::new();
    ENV_INIT.get_or_init(|| {
        if std::env::var("ARC_FAST_CANONICAL_KERNEL").as_deref() == Ok("1") {
            FAST_KERNEL.store(true, Ordering::Relaxed);
        }
    });
    FAST_KERNEL.load(Ordering::Relaxed) && dotprod_available()
}

/// Whether this build and CPU can run the vectorised path at all.
pub fn dotprod_available() -> bool {
    #[cfg(target_arch = "aarch64")]
    {
        std::arch::is_aarch64_feature_detected!("dotprod")
    }
    #[cfg(not(target_arch = "aarch64"))]
    {
        false
    }
}

/// Verify the compile-time digit bounds against the digit formula.
pub fn limb_bounds_match_formula() -> bool {
    let (mut hi, mut lo, mut p) = (0i64, 0i64, 1i64);
    for _ in 0..LIMB_COUNT {
        hi += 127 * p;
        lo += -128 * p;
        p *= 256;
    }
    hi == LIMB_MAX && lo == LIMB_MIN
}

/// Decompose `input` into balanced base-256 digits, returning the number of
/// digits actually needed, or `None` if any element is outside the exact domain.
///
/// `limbs` is written as `LIMB_COUNT` contiguous planes of `input.len()` bytes.
/// Every element's reconstruction is verified, so a decomposition error can
/// never silently produce a different value.
pub fn split_limbs(input: &[i64], limbs: &mut [i8]) -> Option<usize> {
    let len = input.len();
    if len == 0 || limbs.len() < LIMB_COUNT * len {
        return None;
    }
    let mut used = 1usize;
    for (j, &x) in input.iter().enumerate() {
        if !(LIMB_MIN..=LIMB_MAX).contains(&x) {
            return None;
        }
        // Unsigned two's-complement arithmetic: defined for every input, and
        // only the low 8 bits are read each round.
        let mut u = x as u64;
        for i in 0..LIMB_COUNT {
            let low = (u & 255) as i32;
            let c = if low > 127 { low - 256 } else { low };
            limbs[i * len + j] = c as i8;
            u = u.wrapping_sub(c as i64 as u64) >> 8;
            if c != 0 && i + 1 > used {
                used = i + 1;
            }
        }
        // The reconstruction check is the actual guarantee.
        let mut r = 0i64;
        for i in (0..LIMB_COUNT).rev() {
            r = r * 256 + limbs[i * len + j] as i64;
        }
        if r != x {
            return None;
        }
    }
    Some(used)
}

/// One `SDOT Vd.4S, Vn.16B, Vm.16B`: four independent 4-way i8 dot products
/// accumulated exactly into i32 lanes.
///
/// `core::arch::aarch64::vdotq_s32` sits behind the unstable
/// `stdarch_neon_dotprod` feature (rust-lang/rust#117224) on the toolchain this
/// repo pins (`rust-toolchain.toml` = `nightly-2026-03-16`, i.e. rustc
/// 1.96.0-nightly). Using the intrinsic would mean adding a crate-level
/// `#![feature(stdarch_neon_dotprod)]`, which would bind this module to nightly
/// forever. Inline assembly needs no feature gate on either channel - it was
/// verified to assemble and produce the correct result on stable 1.95.0 as well
/// - so the instruction is emitted that way instead.
///
/// # Safety
/// Requires the `dotprod` target feature, which the caller checks at runtime.
#[cfg(target_arch = "aarch64")]
#[target_feature(enable = "neon,dotprod")]
#[inline]
unsafe fn sdot(
    acc: std::arch::aarch64::int32x4_t,
    a: std::arch::aarch64::int8x16_t,
    b: std::arch::aarch64::int8x16_t,
) -> std::arch::aarch64::int32x4_t {
    // SAFETY: wrapped for `unsafe_op_in_unsafe_fn`. The instruction reads no
    // memory and has no side effects; operands are register values.
    unsafe {
        let mut out = acc;
        std::arch::asm!(
            "sdot {o:v}.4s, {a:v}.16b, {b:v}.16b",
            o = inout(vreg) out,
            a = in(vreg) a,
            b = in(vreg) b,
            options(pure, nomem, nostack)
        );
        out
    }
}

/// Exact i8 x i64 dot product over pre-split digits, using `sdot`.
///
/// # Safety
/// `row` must be valid for `len` reads; `limbs` must hold `LIMB_COUNT * len`
/// bytes; `used <= LIMB_COUNT`; `len <= MAX_COLS_FOR_I32`.
#[cfg(target_arch = "aarch64")]
#[target_feature(enable = "neon,dotprod")]
unsafe fn dot_limbs_dotprod(row: *const i8, limbs: &[i8], len: usize, used: usize) -> i64 {
    // SAFETY: wrapped for `unsafe_op_in_unsafe_fn`; the caller's contract is
    // unchanged. Bounds are checked by the caller before this is reached.
    unsafe {
        use std::arch::aarch64::*;
        let mut total: i64 = 0;
        for i in 0..used {
            let plane = limbs.as_ptr().add(i * len);
            let mut acc0 = vdupq_n_s32(0);
            let mut acc1 = vdupq_n_s32(0);
            let mut j = 0usize;
            while j + 32 <= len {
                acc0 = sdot(acc0, vld1q_s8(row.add(j)), vld1q_s8(plane.add(j)));
                acc1 = sdot(acc1, vld1q_s8(row.add(j + 16)), vld1q_s8(plane.add(j + 16)));
                j += 32;
            }
            let mut a = vaddvq_s32(vaddq_s32(acc0, acc1)) as i64;
            while j < len {
                a += (*row.add(j) as i64) * (*plane.add(j) as i64);
                j += 1;
            }
            // Multiplication, not `<<`: shifting a negative value left is
            // undefined behaviour in C and this kernel is mirrored by a C
            // prototype where UBSan flagged exactly that construct.
            total += a * (1i64 << (8 * i));
        }
        total
    }
}

/// The post-dot overflow precondition, established at this entry point.
///
/// Returns `Some(())` when `acc * scale_i` is representable in i64 for every
/// row, using `dot_bound = 128 * sum|x_j| >= |acc|` as the accumulator bound.
/// This mirrors the guard in `matmul_i8_canonical_rows`, which the ordinary
/// forward path does **not** pass through.
#[cfg(target_arch = "aarch64")]
fn post_scale_bound_holds(input: &[i64], scales: &[i64]) -> bool {
    let Some(input_abs_sum) = input
        .iter()
        .try_fold(0i64, |sum, v| sum.checked_add(v.checked_abs()?))
    else {
        return false;
    };
    let Some(dot_bound) = input_abs_sum.checked_mul(128) else {
        return false;
    };
    scales.iter().all(|s| {
        s.checked_abs()
            .and_then(|a| dot_bound.checked_mul(a))
            .is_some()
    })
}

/// Raw output pointer shared across rayon tasks that own disjoint row ranges.
///
/// Needed because the batched output is token-major (`out[t * n_rows + i]`), so
/// one task's elements are strided and cannot be expressed as a `&mut` slice
/// chunk. Disjointness is guaranteed by the row-block decomposition.
#[cfg(target_arch = "aarch64")]
#[derive(Clone, Copy)]
struct SendPtr(*mut i64);
#[cfg(target_arch = "aarch64")]
unsafe impl Send for SendPtr {}
#[cfg(target_arch = "aarch64")]
unsafe impl Sync for SendPtr {}
#[cfg(target_arch = "aarch64")]
impl SendPtr {
    /// Accessor rather than a public field: edition-2021 closures capture
    /// individual fields, so `p.0` inside a rayon closure would capture the
    /// bare `*mut i64` (not `Send`) instead of this wrapper.
    #[inline]
    fn get(self) -> *mut i64 {
        self.0
    }
}

/// Four tokens against one weight row, sharing every weight load.
///
/// This is the batching win in one function: the row is loaded once per 16-byte
/// vector and feeds four independent SDOT accumulator chains. Each returned
/// value is bit-identical to `dot_limbs_dotprod` for that token.
///
/// # Safety
/// `row` valid for `len` reads; each `planes[q]` valid for `LIMB_COUNT * len`
/// bytes; `used <= LIMB_COUNT`; `len <= MAX_COLS_FOR_I32`; `dotprod` available.
#[cfg(target_arch = "aarch64")]
#[target_feature(enable = "neon,dotprod")]
unsafe fn dot_limbs_x4(
    row: *const i8,
    planes: &[*const i8; 4],
    len: usize,
    used: usize,
) -> [i64; 4] {
    // SAFETY: wrapped for `unsafe_op_in_unsafe_fn`; caller's contract unchanged.
    unsafe {
        use std::arch::aarch64::*;
        let mut total = [0i64; 4];
        for l in 0..used {
            let off = l * len;
            let mut a0 = [vdupq_n_s32(0); 4];
            let mut a1 = [vdupq_n_s32(0); 4];
            let mut j = 0usize;
            while j + 32 <= len {
                let w0 = vld1q_s8(row.add(j));
                let w1 = vld1q_s8(row.add(j + 16));
                for q in 0..4 {
                    let c = planes[q].add(off + j);
                    a0[q] = sdot(a0[q], w0, vld1q_s8(c));
                    a1[q] = sdot(a1[q], w1, vld1q_s8(c.add(16)));
                }
                j += 32;
            }
            for q in 0..4 {
                let mut acc = vaddvq_s32(vaddq_s32(a0[q], a1[q])) as i64;
                let c = planes[q].add(off);
                let mut jj = j;
                while jj < len {
                    acc += (*row.add(jj) as i64) * (*c.add(jj) as i64);
                    jj += 1;
                }
                total[q] += acc * (1i64 << (8 * l));
            }
        }
        total
    }
}

/// Vectorised canonical row projection.
///
/// Returns `true` if it computed `output` exactly, `false` if it refused; on
/// `false` the caller must use the existing scalar kernel. **`output` is not
/// written at all when this returns `false`**, so a refusal can never leave a
/// partially updated buffer behind.
pub fn matmul_i8_canonical_rows_fast(
    weights: &I8Weights,
    input: &[i64],
    in_size: usize,
    output: &mut [i64],
) -> bool {
    matmul_i8_canonical_rows_fast_view(I8WeightsView::from(weights), input, in_size, output)
}

pub(crate) fn matmul_i8_canonical_rows_fast_view(
    weights: I8WeightsView<'_>,
    input: &[i64],
    in_size: usize,
    output: &mut [i64],
) -> bool {
    #[cfg(not(target_arch = "aarch64"))]
    {
        let _ = (weights, input, in_size, output);
        record_attempt();
        record_refusal(Refusal::Unavailable)
    }
    #[cfg(target_arch = "aarch64")]
    {
        record_attempt();
        if !dotprod_available() {
            return record_refusal(Refusal::Unavailable);
        }
        if in_size == 0
            || input.len() != in_size
            || weights.n_cols != in_size
            || weights.n_rows == 0
            || weights.data.len() != weights.n_rows.saturating_mul(in_size)
            || weights.scales.len() != weights.n_rows
            || output.len() != weights.n_rows
        {
            return record_refusal(Refusal::Shape);
        }
        if in_size > MAX_COLS_FOR_I32 {
            return record_refusal(Refusal::InnerDimAboveI32Bound);
        }
        // Post-dot scale safety. Checked BEFORE any output byte is written,
        // and not inherited from `matmul_i8_canonical_rows`, which this call
        // path does not go through. The dot accumulation itself is bounded by
        // construction inside the accepted domain (see the module docs and the
        // compile-time assertion above), so only the scale multiply is checked.
        if !post_scale_bound_holds(input, &weights.scales) {
            return record_refusal(Refusal::ScaleMultiplyWouldOverflow);
        }
        LIMB_SCRATCH.with(|cell| {
            let mut scratch = cell.borrow_mut();
            if scratch.len() < LIMB_COUNT * in_size {
                scratch.resize(LIMB_COUNT * in_size, 0);
            }
            let Some(used) = split_limbs(input, &mut scratch[..LIMB_COUNT * in_size]) else {
                return record_refusal(Refusal::ActivationOutOfDomain);
            };
            let data = &weights.data;
            let scales = &weights.scales;
            let limbs = &scratch[..LIMB_COUNT * in_size];
            // Same 256-row rayon chunking as `matmul_i8_into`; integer
            // reduction order is irrelevant to the result, so this cannot
            // change a value.
            output
                .par_chunks_mut(256)
                .enumerate()
                .for_each(|(chunk_idx, chunk)| {
                    let start = chunk_idx * 256;
                    for (local_i, out) in chunk.iter_mut().enumerate() {
                        let i = start + local_i;
                        // SAFETY: `data` holds n_rows * in_size bytes (checked
                        // above) and `i < n_rows`, so `i * in_size` is in
                        // bounds for `in_size` reads. `dotprod` availability
                        // is checked.
                        let acc = unsafe {
                            dot_limbs_dotprod(data.as_ptr().add(i * in_size), limbs, in_size, used)
                        };
                        // Safe: `post_scale_bound_holds` proved that
                        // `dot_bound * |scales[i]|` is representable, and
                        // `|acc| <= dot_bound`.
                        *out = (acc * scales[i]) >> FRAC_BITS;
                    }
                });
            record_accept();
            true
        })
    }
}

/// Batched form of [`matmul_i8_canonical_rows_fast`]: `n_tokens` activations
/// against the same weight matrix.
///
/// Exactness is unchanged and is trivial to see: output `[t][i]` is
/// `(dot(row_i, x_t) * scale_i) >> FRAC_BITS`, the same expression the
/// single-token path computes, with no cross-token coupling anywhere. Nothing
/// is shared between tokens except the weights, which are read-only. Batching
/// therefore cannot change a value - only how many times the weights are read.
///
/// `inputs` is `n_tokens * in_size`, `output` is `n_tokens * n_rows`, both
/// token-major. Returns `false` and writes nothing if it refuses.
pub fn matmul_i8_batched_fast(
    weights: &I8Weights,
    inputs: &[i64],
    n_tokens: usize,
    in_size: usize,
    output: &mut [i64],
) -> bool {
    #[cfg(not(target_arch = "aarch64"))]
    {
        let _ = (weights, inputs, n_tokens, in_size, output);
        record_attempt();
        record_refusal(Refusal::Unavailable)
    }
    #[cfg(target_arch = "aarch64")]
    {
        record_attempt();
        if !dotprod_available() {
            return record_refusal(Refusal::Unavailable);
        }
        if in_size == 0
            || n_tokens == 0
            || n_tokens > MAX_BATCH_TOKENS
            || inputs.len() != n_tokens.saturating_mul(in_size)
            || weights.n_cols != in_size
            || weights.n_rows == 0
            || weights.data.len() != weights.n_rows.saturating_mul(in_size)
            || weights.scales.len() != weights.n_rows
            || output.len() != n_tokens.saturating_mul(weights.n_rows)
        {
            return record_refusal(Refusal::Shape);
        }
        if in_size > MAX_COLS_FOR_I32 {
            return record_refusal(Refusal::InnerDimAboveI32Bound);
        }
        // The post-dot bound must hold for EVERY token, so it is checked
        // against every token's own activation before anything is written.
        for t in 0..n_tokens {
            if !post_scale_bound_holds(&inputs[t * in_size..(t + 1) * in_size], &weights.scales) {
                return record_refusal(Refusal::ScaleMultiplyWouldOverflow);
            }
        }
        let mut limbs = vec![0i8; n_tokens * LIMB_COUNT * in_size];
        let mut used = vec![0usize; n_tokens];
        for t in 0..n_tokens {
            let lo = t * LIMB_COUNT * in_size;
            match split_limbs(
                &inputs[t * in_size..(t + 1) * in_size],
                &mut limbs[lo..lo + LIMB_COUNT * in_size],
            ) {
                Some(u) => used[t] = u,
                None => return record_refusal(Refusal::ActivationOutOfDomain),
            }
        }
        let data = &weights.data;
        let scales = &weights.scales;
        let n_rows = weights.n_rows;
        let limbs = &limbs[..];
        let used_max = used.iter().copied().max().unwrap_or(1);
        // Using more digit planes than a token needs is still exact - the extra
        // planes are zero - so one `used` for the quad keeps the inner loop
        // branch-free.
        //
        // Tiling. `row_block` keeps a block of weight rows inside L1
        // (~128 KiB), and tokens are processed four at a time so ONE weight
        // vector load feeds four SDOT chains. DRAM weight traffic therefore
        // falls from once-per-token to once-per-matmul, which is the entire
        // point of batching; the per-(row, token) arithmetic is untouched.
        let row_block = (131_072 / in_size).clamp(1, n_rows.max(1));
        let n_blocks = n_rows.div_ceil(row_block);
        let out_ptr = SendPtr(output.as_mut_ptr());
        (0..n_blocks).into_par_iter().for_each(|b| {
            let r0 = b * row_block;
            let r1 = (r0 + row_block).min(n_rows);
            let mut t = 0usize;
            while t < n_tokens {
                let quad = (n_tokens - t).min(4);
                // Short final quad repeats the first token's planes so the
                // inner kernel stays branch-free; those lanes are discarded.
                let plane_of = |q: usize| {
                    let tok = t + q.min(quad - 1);
                    // SAFETY: `tok < n_tokens`, and `limbs` holds
                    // `n_tokens * LIMB_COUNT * in_size` bytes.
                    unsafe { limbs.as_ptr().add(tok * LIMB_COUNT * in_size) }
                };
                let planes = [plane_of(0), plane_of(1), plane_of(2), plane_of(3)];
                for i in r0..r1 {
                    // SAFETY: `i < n_rows` and `data` holds `n_rows * in_size`
                    // bytes. Each rayon task owns a disjoint row range, and for
                    // a given row it writes only `out[(t+q) * n_rows + i]`, so
                    // no two tasks ever touch the same output element.
                    let acc = unsafe {
                        dot_limbs_x4(data.as_ptr().add(i * in_size), &planes, in_size, used_max)
                    };
                    let sc = scales[i];
                    for (q, a) in acc.iter().enumerate().take(quad) {
                        unsafe {
                            *out_ptr.get().add((t + q) * n_rows + i) = (*a * sc) >> FRAC_BITS
                        };
                    }
                }
                t += quad;
            }
        });
        record_accept();
        true
    }
}

/// Serialises every test that observes or mutates the process-global kernel
/// switches: `FAST_CANONICAL`, `CENSUS_ON`, the projection counters and the
/// prefill counters in [`crate::canonical_prefill`].
///
/// These switches are deliberately process-wide — that is what makes them
/// usable as an operator control — but the test harness runs tests in parallel
/// threads of one process, so a test that asserts "off by default" will observe
/// another test's opt-in unless both serialise. That is exactly how
/// `disabled_unless_explicitly_requested` failed the first time the batched
/// prefill conformance tests were ever executed: `run_batched_conformance`
/// enables the kernel, restores it correctly afterwards, and the default-off
/// assertion still ran inside that window.
///
/// The lock lives outside `mod tests` because the prefill conformance tests sit
/// in `cached_integer_model`, a different module, and must take the same one.
#[cfg(test)]
pub(crate) static KERNEL_SWITCH_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Guard for [`KERNEL_SWITCH_TEST_LOCK`]. Poison is ignored deliberately: a
/// panicking test must not cascade into unrelated failures in every other test
/// that touches a switch.
#[cfg(test)]
pub(crate) fn kernel_switch_guard() -> std::sync::MutexGuard<'static, ()> {
    KERNEL_SWITCH_TEST_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::integer_lut::FRAC_BITS;

    fn census_guard() -> std::sync::MutexGuard<'static, ()> {
        kernel_switch_guard()
    }

    fn scalar_dot(row: &[i8], input: &[i64]) -> i64 {
        row.iter()
            .zip(input)
            .fold(0i64, |a, (w, x)| a + (*w as i64) * *x)
    }

    fn weights_from(rows: usize, cols: usize, mut f: impl FnMut(usize, usize) -> i8) -> I8Weights {
        let mut data = Vec::with_capacity(rows * cols);
        for r in 0..rows {
            for c in 0..cols {
                data.push(f(r, c));
            }
        }
        I8Weights {
            data,
            scales: (0..rows).map(|i| (i as i64 % 97) + 1).collect(),
            n_rows: rows,
            n_cols: cols,
        }
    }

    #[test]
    fn limb_bounds_are_derived_not_assumed() {
        assert!(limb_bounds_match_formula());
        // The asymmetry matters: i32::MAX is NOT representable in four digits.
        assert!(LIMB_MAX < i32::MAX as i64);
        assert!(LIMB_MIN < -(i32::MAX as i64));
    }

    #[test]
    fn split_is_exact_or_refuses() {
        let edges: Vec<i64> = vec![
            0,
            1,
            -1,
            127,
            128,
            -128,
            -129,
            255,
            256,
            -256,
            32767,
            32768,
            -32768,
            65535,
            65536,
            16_777_215,
            16_777_216,
            LIMB_MAX,
            LIMB_MIN,
            LIMB_MAX - 1,
            LIMB_MIN + 1,
        ];
        let mut limbs = vec![0i8; LIMB_COUNT * edges.len()];
        let used = split_limbs(&edges, &mut limbs).expect("edge values are in domain");
        assert!(used <= LIMB_COUNT);
        for (j, &x) in edges.iter().enumerate() {
            let mut r = 0i64;
            for i in (0..LIMB_COUNT).rev() {
                r = r * 256 + limbs[i * edges.len() + j] as i64;
            }
            assert_eq!(r, x, "digit reconstruction must be exact for {x}");
        }
        for bad in [
            LIMB_MAX + 1,
            LIMB_MIN - 1,
            i32::MAX as i64,
            i64::MAX,
            i64::MIN,
        ] {
            let v = vec![1i64, bad, 3];
            let mut l = vec![0i8; LIMB_COUNT * v.len()];
            assert!(split_limbs(&v, &mut l).is_none(), "{bad} must be refused");
        }
    }

    #[test]
    fn matches_scalar_on_model_shapes_and_tails() {
        if !dotprod_available() {
            eprintln!("dotprod unavailable on this target; vectorised path not exercised");
            return;
        }
        let mut seed = 0x9E37_79B9_7F4A_7C15u64;
        let mut next = move || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        // Real projection widths plus every SIMD tail around 16/32.
        for &cols in &[
            1usize, 7, 15, 16, 17, 31, 32, 33, 63, 65, 255, 4096, 4097, 11008,
        ] {
            for rows in [1usize, 3, 256, 257] {
                let w = weights_from(rows, cols, |_, _| (next() % 256) as u8 as i8);
                // Include the untrusted raw -128 explicitly.
                let mut w = w;
                if !w.data.is_empty() {
                    w.data[0] = -128;
                    *w.data.last_mut().unwrap() = -128;
                }
                let input: Vec<i64> = (0..cols)
                    .map(|j| match j % 5 {
                        0 => LIMB_MAX,
                        1 => LIMB_MIN,
                        2 => 0,
                        3 => -(((next() % 8_388_608) as i64) + 1),
                        _ => ((next() % 8_388_608) as i64) + 1,
                    })
                    .collect();
                let mut fast = vec![0i64; rows];
                assert!(
                    matmul_i8_canonical_rows_fast(&w, &input, cols, &mut fast),
                    "must accept cols={cols} rows={rows}"
                );
                for i in 0..rows {
                    let acc = scalar_dot(&w.data[i * cols..(i + 1) * cols], &input);
                    let want = (acc * w.scales[i]) >> FRAC_BITS;
                    assert_eq!(fast[i], want, "cols={cols} rows={rows} row={i}");
                }
            }
        }
    }

    #[test]
    fn refuses_inner_dimension_above_the_i32_bound() {
        let cols = MAX_COLS_FOR_I32 + 1; // == tensor_parallel::MAX_ROW_INPUT_ELEMENTS
        assert_eq!(cols, crate::tensor_parallel::MAX_ROW_INPUT_ELEMENTS);
        let w = weights_from(1, cols, |_, _| -128);
        let input = vec![LIMB_MAX; cols];
        let mut out = vec![0i64; 1];
        assert!(
            !matmul_i8_canonical_rows_fast(&w, &input, cols, &mut out),
            "K one over the derived i32 bound must be refused"
        );
        assert_eq!(out[0], 0, "a refused call must not write output");
    }

    /// Weights whose scale vector is chosen relative to a known `dot_bound`.
    fn weights_with_scale(rows: usize, cols: usize, scale: i64) -> I8Weights {
        I8Weights {
            data: vec![-128i8; rows * cols],
            scales: vec![scale; rows],
            n_rows: rows,
            n_cols: cols,
        }
    }

    const SENTINEL: i64 = 0x7E57_7E57_7E57_7E57;

    fn assert_untouched(out: &[i64], case: &str) {
        assert!(
            out.iter().all(|v| *v == SENTINEL),
            "refusal must not write output ({case})"
        );
    }

    #[test]
    fn post_scale_bound_is_checked_at_this_entry_point() {
        if !dotprod_available() {
            return;
        }
        // dot_bound = 128 * sum|x| = 128 * 32 = 4096 for this input.
        let cols = 32usize;
        let input = vec![1i64; cols];
        let dot_bound = 128i64 * cols as i64;
        assert_eq!(dot_bound, 4096);

        // Exactly at the boundary: dot_bound * scale still fits i64 -> accept.
        let ok_scale = i64::MAX / dot_bound;
        assert!(dot_bound.checked_mul(ok_scale).is_some());
        let w = weights_with_scale(4, cols, ok_scale);
        let mut out = vec![SENTINEL; 4];
        assert!(
            matmul_i8_canonical_rows_fast(&w, &input, cols, &mut out),
            "the scale boundary case must be accepted, not refused"
        );
        let acc = scalar_dot(&w.data[..cols], &input);
        assert_eq!(out[0], (acc * ok_scale) >> FRAC_BITS);

        // One step over the boundary -> refuse, and write nothing.
        let bad_scale = ok_scale + 1;
        assert!(dot_bound.checked_mul(bad_scale).is_none());
        let w = weights_with_scale(4, cols, bad_scale);
        let mut out = vec![SENTINEL; 4];
        assert!(!matmul_i8_canonical_rows_fast(&w, &input, cols, &mut out));
        assert_untouched(&out, "scale one over the bound");

        // i64::MIN has no representable absolute value -> refuse.
        let w = weights_with_scale(4, cols, i64::MIN);
        let mut out = vec![SENTINEL; 4];
        assert!(!matmul_i8_canonical_rows_fast(&w, &input, cols, &mut out));
        assert_untouched(&out, "i64::MIN scale");
    }

    #[test]
    fn every_refusal_leaves_output_untouched() {
        let cols = 64usize;
        let good = vec![7i64; cols];

        // 1. shape mismatch (scales shorter than rows)
        let mut w = weights_with_scale(4, cols, 1);
        w.scales.pop();
        let mut out = vec![SENTINEL; 4];
        assert!(!matmul_i8_canonical_rows_fast(&w, &good, cols, &mut out));
        assert_untouched(&out, "shape");

        // 2. activation outside the four-digit domain
        let w = weights_with_scale(4, cols, 1);
        let mut bad_input = good.clone();
        bad_input[cols / 2] = LIMB_MAX + 1;
        let mut out = vec![SENTINEL; 4];
        assert!(!matmul_i8_canonical_rows_fast(
            &w, &bad_input, cols, &mut out
        ));
        assert_untouched(&out, "activation out of domain");

        // 3. inner dimension above the derived i32 bound
        let big = MAX_COLS_FOR_I32 + 1;
        let w = weights_with_scale(1, big, 1);
        let input = vec![1i64; big];
        let mut out = vec![SENTINEL; 1];
        assert!(!matmul_i8_canonical_rows_fast(&w, &input, big, &mut out));
        assert_untouched(&out, "inner dim above i32 bound");

        // 4. post-scale overflow
        let w = weights_with_scale(4, cols, i64::MAX);
        let mut out = vec![SENTINEL; 4];
        assert!(!matmul_i8_canonical_rows_fast(&w, &good, cols, &mut out));
        assert_untouched(&out, "post-scale overflow");
    }

    #[test]
    fn census_records_attempts_accepts_and_refusal_reasons() {
        if !dotprod_available() {
            return;
        }
        let _guard = census_guard();
        let cols = 64usize;
        let good = vec![7i64; cols];
        reset_projection_census();
        set_projection_census_enabled(true);

        let w = weights_with_scale(4, cols, 1);
        let mut out = vec![0i64; 4];
        assert!(matmul_i8_canonical_rows_fast(&w, &good, cols, &mut out));

        let mut bad_input = good.clone();
        bad_input[0] = LIMB_MIN - 1;
        let mut out = vec![SENTINEL; 4];
        assert!(!matmul_i8_canonical_rows_fast(
            &w, &bad_input, cols, &mut out
        ));

        let w_bad = weights_with_scale(4, cols, i64::MAX);
        let mut out = vec![SENTINEL; 4];
        assert!(!matmul_i8_canonical_rows_fast(
            &w_bad, &good, cols, &mut out
        ));

        let c = projection_census();
        set_projection_census_enabled(false);
        // The counters are process-global and the test harness runs tests in
        // parallel, so other tests calling the kernel can inflate these while
        // counting is on. Assert the lower bounds this test itself guarantees.
        assert!(c.attempted >= 3, "{c:?}");
        assert!(c.accepted >= 1, "{c:?}");
        assert!(c.refused_activation_out_of_domain >= 1, "{c:?}");
        assert!(c.refused_scale_multiply_would_overflow >= 1, "{c:?}");
        reset_projection_census();
        assert!(
            !CENSUS_ON.load(Ordering::Relaxed),
            "the census test must leave counting disabled"
        );
    }

    #[test]
    fn census_is_off_by_default_and_costs_nothing_when_off() {
        // Held so the only other test that touches the flag cannot be mid-run.
        let _guard = census_guard();
        assert!(!CENSUS_ON.load(Ordering::Relaxed));
    }

    #[test]
    fn disabled_unless_explicitly_requested() {
        // Off unless the operator opted in; never on by default.
        let _guard = kernel_switch_guard();
        if std::env::var("ARC_FAST_CANONICAL_KERNEL").as_deref() == Ok("1") {
            assert!(fast_canonical_kernel_enabled() || !dotprod_available());
        } else {
            assert!(!fast_canonical_kernel_enabled());
        }
    }
}
