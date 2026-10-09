//! Bit-exact vectorised canonical INT8 projection (ARM64 NEON / x86-64 AVX2).
//!
//! This module does **not** define a new arithmetic profile. It computes the
//! same integer value as the scalar kernel for every input it accepts, and
//! refuses every input it cannot prove exact, leaving the existing scalar path
//! to handle it. Turning it on or off therefore changes no admission,
//! execution or consensus semantics, only speed.
//!
//! # Default per target
//!
//! * **x86-64 (AVX2): on.** The determinism proof (PR #136, run 37467005539)
//!   decoded Llama-2-7B, 16 prompts x 32 tokens, with the scalar and the AVX2
//!   kernel on Linux, Windows and Intel macOS. All six transcripts had the same
//!   SHA-256 (`d2c8c82b...`), and the AVX2 legs accepted 176,175 of 176,175
//!   projections. CPUs without AVX2, and x86-64 processes that Rosetta 2
//!   translates on Apple Silicon, keep the scalar kernel.
//! * **arm64 (NEON dotprod): on.** The same 16 x 32 proof passed with NEON on
//!   Apple Silicon (run 37467005539, attempt 4) and on Linux arm64 without swap
//!   (run 37882594919); see [`NEON_ON_BY_DEFAULT`], the single switch. CPUs
//!   without the dotprod extension keep the scalar kernel.
//! * **Overrides**, read once at first use: `ARC_CANONICAL_KERNEL=scalar`
//!   forces the scalar reference, `simd` forces the vectorised kernel where the
//!   CPU has one, and `auto` keeps the default. The older
//!   `ARC_FAST_CANONICAL_KERNEL` still works: `1` selects the vectorised kernel
//!   and any other value the scalar one. [`set_fast_canonical_kernel`] wins
//!   over both.
//!
//! # Scope, stated exactly
//!
//! The integration point is [`crate::cached_integer_model::matmul_i8_into`],
//! which is the **generic per-row I8 matmul**. When the kernel is on, this path
//! is therefore reachable from *every* I8 matmul caller in the crate -
//! whole-model forward, shard forward, validator routes - not only from the
//! named canonical profile. That is a wider blast radius than the profile name
//! suggests. It is safe because every accepted value is exact and every other
//! input is refused to the scalar kernel.
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
//! Each inner sum is an i8 x i8 dot product. ARM `sdot` computes four-byte
//! groups; AVX2 sign-extends to i16 and uses non-saturating i16 products with
//! i32 accumulation. Neither backend requantizes or changes the operands.
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
#[cfg(any(target_arch = "aarch64", target_arch = "x86_64"))]
use crate::integer_lut::FRAC_BITS;
#[cfg(any(target_arch = "aarch64", target_arch = "x86_64"))]
use rayon::prelude::*;
#[cfg(any(target_arch = "aarch64", target_arch = "x86_64"))]
use std::cell::RefCell;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering};

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

/// Whether the vectorised kernel is on by default on x86-64 (AVX2).
///
/// On. The determinism proof (PR #136, run 37467005539) produced the same
/// 16-prompt x 32-token Llama-2-7B transcript, SHA-256 `d2c8c82b...`, with the
/// scalar and the AVX2 kernel on Linux, Windows and Intel macOS.
pub const AVX2_ON_BY_DEFAULT: bool = true;

/// Whether the vectorised kernel is on by default on arm64 (NEON dotprod).
///
/// **On.** This constant is the single switch for NEON. It was turned on after
/// two full proofs passed. In each, the 16-prompt x 32-token Llama-2-7B
/// transcript had the SHA-256 of every other cell (`d2c8c82b...`), and NEON
/// accepted 176,175 of 176,175 projections:
/// 1. GitHub's 7 GB Apple Silicon runner (Apple M1, virtual), run
///    37467005539 attempt 4, compare job 113664110749. That runner swaps the
///    model.
/// 2. GitHub's `ubuntu-24.04-arm` runner (Neoverse-N2, 15.6 GiB, no swap
///    used), run 37882594919, compare job 113668215021. This is the
///    non-swapping arm64 proof the kernel plan asks for (note a). NEON decoded
///    at 1.5x the scalar kernel's rate there.
///
/// The kernel still runs only where the CPU reports the dotprod extension at
/// run time; elsewhere the scalar kernel runs. `ARC_CANONICAL_KERNEL=scalar`
/// forces the scalar kernel on any node. Setting this to `false` restores the
/// scalar default on every arm64 build without any other change.
pub const NEON_ON_BY_DEFAULT: bool = true;

/// Operator override, read once at first use: `scalar`, `simd` or `auto`, in
/// any letter case. An unrecognised value selects the scalar reference kernel.
pub const KERNEL_ENV: &str = "ARC_CANONICAL_KERNEL";

/// Older override, honoured when [`KERNEL_ENV`] is unset or empty: `1` selects
/// the vectorised kernel and any other value the scalar one.
pub const LEGACY_KERNEL_ENV: &str = "ARC_FAST_CANONICAL_KERNEL";

// The process-wide kernel choice, packed as `(source << 2) | mode`. Zero means
// not resolved yet; every resolved state has a non-zero mode.
const STATE_UNRESOLVED: u8 = 0;
const MODE_SCALAR: u8 = 1;
const MODE_SIMD: u8 = 2;
const MODE_MASK: u8 = 0b11;
const SOURCE_DEFAULT: u8 = 0;
const SOURCE_ENV: u8 = 1;
const SOURCE_LEGACY_ENV: u8 = 2;
const SOURCE_EXPLICIT: u8 = 3;

static KERNEL_STATE: AtomicU8 = AtomicU8::new(STATE_UNRESOLVED);

const fn pack_state(simd: bool, source: u8) -> u8 {
    let mode = if simd { MODE_SIMD } else { MODE_SCALAR };
    (source << 2) | mode
}

/// A resolved kernel request: the kernel, where the choice came from, and
/// whether an override value was unrecognised (and so fell back to scalar).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct KernelChoice {
    simd: bool,
    source: u8,
    unrecognised: bool,
}

/// Choose the kernel from the two override variables and this target's
/// default. Pure, so the precedence is tested without touching the process
/// environment.
fn choose_kernel(
    requested: Option<&str>,
    legacy: Option<&str>,
    simd_by_default: bool,
) -> KernelChoice {
    if let Some(value) = requested.map(str::trim).filter(|value| !value.is_empty()) {
        let (simd, unrecognised) = if value.eq_ignore_ascii_case("auto") {
            (simd_by_default, false)
        } else if value.eq_ignore_ascii_case("simd") {
            (true, false)
        } else if value.eq_ignore_ascii_case("scalar") {
            (false, false)
        } else {
            // Fail safe: the scalar kernel is the reference and always correct.
            (false, true)
        };
        return KernelChoice {
            simd,
            source: SOURCE_ENV,
            unrecognised,
        };
    }
    if let Some(value) = legacy {
        return KernelChoice {
            simd: value == "1",
            source: SOURCE_LEGACY_ENV,
            unrecognised: false,
        };
    }
    KernelChoice {
        simd: simd_by_default,
        source: SOURCE_DEFAULT,
        unrecognised: false,
    }
}

/// Whether the vectorised kernel is on when nothing overrides it, for this
/// build target and process. Availability is checked separately, per call.
pub fn simd_on_by_default() -> bool {
    #[cfg(target_arch = "x86_64")]
    {
        AVX2_ON_BY_DEFAULT && !running_under_rosetta()
    }
    #[cfg(target_arch = "aarch64")]
    {
        NEON_ON_BY_DEFAULT
    }
    #[cfg(not(any(target_arch = "aarch64", target_arch = "x86_64")))]
    {
        false
    }
}

/// True when this x86-64 macOS process is translated by Rosetta 2. The proof
/// ran on Intel CPUs, not on translated AVX2, so the default stays scalar
/// there; an operator can still force the vectorised kernel.
#[cfg(target_arch = "x86_64")]
fn running_under_rosetta() -> bool {
    #[cfg(target_os = "macos")]
    {
        let mut translated: libc::c_int = 0;
        let mut size = std::mem::size_of::<libc::c_int>();
        // SAFETY: `sysctlbyname` reads the NUL-terminated name and writes at
        // most `size` bytes into `translated`, which outlives the call. A null
        // new-value pointer with length 0 means nothing is set.
        let status = unsafe {
            libc::sysctlbyname(
                c"sysctl.proc_translated".as_ptr(),
                std::ptr::addr_of_mut!(translated).cast(),
                &mut size,
                std::ptr::null_mut(),
                0,
            )
        };
        status == 0 && translated == 1
    }
    #[cfg(not(target_os = "macos"))]
    {
        false
    }
}

/// The process-wide kernel state, resolved from the overrides and this
/// target's default on first use.
fn kernel_state() -> u8 {
    match KERNEL_STATE.load(Ordering::Relaxed) {
        STATE_UNRESOLVED => resolve_kernel_state(),
        state => state,
    }
}

#[cold]
fn resolve_kernel_state() -> u8 {
    let requested = std::env::var(KERNEL_ENV).ok();
    let legacy = std::env::var(LEGACY_KERNEL_ENV).ok();
    let choice = choose_kernel(
        requested.as_deref(),
        legacy.as_deref(),
        simd_on_by_default(),
    );
    let state = pack_state(choice.simd, choice.source);
    // Installed only while still unresolved, so an explicit
    // `set_fast_canonical_kernel` always wins over the environment and default.
    match KERNEL_STATE.compare_exchange(
        STATE_UNRESOLVED,
        state,
        Ordering::Relaxed,
        Ordering::Relaxed,
    ) {
        Ok(_) => {
            if choice.unrecognised {
                tracing::warn!(
                    variable = KERNEL_ENV,
                    value = requested.as_deref().unwrap_or_default(),
                    "unrecognised kernel override (expected scalar, simd or auto); \
                     using the scalar reference kernel"
                );
            }
            state
        }
        Err(current) => current,
    }
}

/// Why a projection declined the vectorised path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    /// No supported exact vector backend on this build/CPU.
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
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize)]
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

#[cfg(test)]
thread_local! {
    /// This thread's `[attempted, accepted, refused]` projections. The global
    /// census is shared by every test running in the process; this one lets a
    /// test attribute projections to the calls made on its own thread.
    static THREAD_CENSUS: std::cell::Cell<[u64; 3]> = const { std::cell::Cell::new([0; 3]) };
}

#[cfg(test)]
fn count_on_this_thread(slot: usize) {
    THREAD_CENSUS.with(|cell| {
        let mut counts = cell.get();
        counts[slot] += 1;
        cell.set(counts);
    });
}

/// Return this thread's `[attempted, accepted, refused]` projection counts and
/// reset them. Counted whether or not the global census is on.
#[cfg(test)]
pub(crate) fn take_thread_census() -> [u64; 3] {
    THREAD_CENSUS.with(|cell| cell.replace([0; 3]))
}

#[inline]
fn record_attempt() {
    #[cfg(test)]
    count_on_this_thread(0);
    if CENSUS_ON.load(Ordering::Relaxed) {
        N_ATTEMPTED.fetch_add(1, Ordering::Relaxed);
    }
}

#[cfg(any(target_arch = "aarch64", target_arch = "x86_64"))]
#[inline]
fn record_accept() {
    #[cfg(test)]
    count_on_this_thread(1);
    if CENSUS_ON.load(Ordering::Relaxed) {
        N_ACCEPTED.fetch_add(1, Ordering::Relaxed);
    }
}

#[inline]
fn record_refusal(reason: Refusal) -> bool {
    #[cfg(test)]
    count_on_this_thread(2);
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

#[cfg(any(target_arch = "aarch64", target_arch = "x86_64"))]
thread_local! {
    /// Reused digit scratch. The digits depend only on the activation, so they
    /// are computed once per projection and read by every row. Allocating this
    /// per call was measurable: it is 44 KB per gate/up projection, 225 times
    /// per forward pass.
    static LIMB_SCRATCH: RefCell<Vec<i8>> = const { RefCell::new(Vec::new()) };
}

/// Enable or disable the vectorised canonical kernel process-wide.
///
/// An explicit call wins over the environment overrides and the per-target
/// default, whether it is made before or after first use. Both kernels compute
/// the same values; see the module docs.
pub fn set_fast_canonical_kernel(enabled: bool) {
    KERNEL_STATE.store(pack_state(enabled, SOURCE_EXPLICIT), Ordering::Relaxed);
}

/// Whether the vectorised canonical kernel is enabled *and* available here.
///
/// On first use this resolves the per-target default and the
/// `ARC_CANONICAL_KERNEL` and `ARC_FAST_CANONICAL_KERNEL` overrides (see the
/// module docs), unless [`set_fast_canonical_kernel`] has already chosen.
pub fn fast_canonical_kernel_enabled() -> bool {
    (kernel_state() & MODE_MASK) == MODE_SIMD && dotprod_available()
}

/// The projection kernel that I8 matmuls use right now, named as the
/// determinism proof names them: `avx2-limb`, `neon-sdot-limb` or
/// `scalar-i8xi64`.
pub fn effective_kernel_name() -> &'static str {
    if !fast_canonical_kernel_enabled() {
        "scalar-i8xi64"
    } else if cfg!(target_arch = "x86_64") {
        "avx2-limb"
    } else {
        "neon-sdot-limb"
    }
}

/// Where the current kernel choice came from: `default`, the name of the
/// environment variable that made it, or `explicit`.
pub fn kernel_choice_source() -> &'static str {
    match kernel_state() >> 2 {
        SOURCE_ENV => KERNEL_ENV,
        SOURCE_LEGACY_ENV => LEGACY_KERNEL_ENV,
        SOURCE_EXPLICIT => "explicit",
        _ => "default",
    }
}

/// Whether this build and CPU can run the vectorised path at all.
/// The historical name is retained for callers: x86-64 requires AVX2,
/// whereas ARM64 requires NEON dotprod. Whether it runs by default is decided
/// per target; see the module docs.
pub fn dotprod_available() -> bool {
    #[cfg(target_arch = "aarch64")]
    {
        std::arch::is_aarch64_feature_detected!("dotprod")
    }
    #[cfg(target_arch = "x86_64")]
    {
        std::arch::is_x86_feature_detected!("avx2")
    }
    #[cfg(not(any(target_arch = "aarch64", target_arch = "x86_64")))]
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
#[cfg(any(target_arch = "aarch64", target_arch = "x86_64"))]
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
#[cfg(any(target_arch = "aarch64", target_arch = "x86_64"))]
#[derive(Clone, Copy)]
struct SendPtr(*mut i64);
#[cfg(any(target_arch = "aarch64", target_arch = "x86_64"))]
unsafe impl Send for SendPtr {}
#[cfg(any(target_arch = "aarch64", target_arch = "x86_64"))]
unsafe impl Sync for SendPtr {}
#[cfg(any(target_arch = "aarch64", target_arch = "x86_64"))]
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

/// Exact signed-byte products on AVX2. Sign extension avoids the saturating
/// unsigned-byte multiply/add instruction, which is not exact for this domain.
///
/// # Safety
/// AVX2 must be available. `row` holds `len` bytes, each plane pointer holds
/// `LIMB_COUNT * len` bytes, `used <= LIMB_COUNT`, and
/// `len <= MAX_COLS_FOR_I32`. Every full load is bounded by `j + 16 <= len`.
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
unsafe fn dot_limbs_avx2<const N: usize>(
    row: *const i8,
    planes: &[*const i8; N],
    len: usize,
    used: usize,
) -> [i64; N] {
    // SAFETY: all memory accesses follow the caller's bounds above. Products
    // are exact i16*i16->i32, and the same proven i32 sum bound as ARM applies.
    unsafe {
        use std::arch::x86_64::*;
        let mut total = [0i64; N];
        for limb in 0..used {
            let mut accumulators = [_mm256_setzero_si256(); N];
            let mut j = 0usize;
            while j + 16 <= len {
                let weights = _mm256_cvtepi8_epi16(_mm_loadu_si128(row.add(j).cast()));
                for q in 0..N {
                    let digits =
                        _mm256_cvtepi8_epi16(_mm_loadu_si128(planes[q].add(limb * len + j).cast()));
                    accumulators[q] =
                        _mm256_add_epi32(accumulators[q], _mm256_madd_epi16(weights, digits));
                }
                j += 16;
            }
            for q in 0..N {
                let mut lanes = [0i32; 8];
                _mm256_storeu_si256(lanes.as_mut_ptr().cast(), accumulators[q]);
                let mut partial: i64 = lanes.iter().map(|&lane| i64::from(lane)).sum();
                for tail in j..len {
                    partial +=
                        i64::from(*row.add(tail)) * i64::from(*planes[q].add(limb * len + tail));
                }
                total[q] += partial * (1i64 << (8 * limb));
            }
        }
        total
    }
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
unsafe fn dot_limbs_dotprod(row: *const i8, limbs: &[i8], len: usize, used: usize) -> i64 {
    // SAFETY: this wrapper has the same bounds as dot_limbs_avx2; the caller
    // has validated the row and the complete digit scratch before dispatch.
    unsafe { dot_limbs_avx2(row, &[limbs.as_ptr()], len, used)[0] }
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
unsafe fn dot_limbs_x4(
    row: *const i8,
    planes: &[*const i8; 4],
    len: usize,
    used: usize,
) -> [i64; 4] {
    // SAFETY: the batched caller validates every plane and owns distinct
    // output rows. Repeated plane pointers for a short final batch are reads.
    unsafe { dot_limbs_avx2(row, planes, len, used) }
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
    #[cfg(not(any(target_arch = "aarch64", target_arch = "x86_64")))]
    {
        let _ = (weights, input, in_size, output);
        record_attempt();
        record_refusal(Refusal::Unavailable)
    }
    #[cfg(any(target_arch = "aarch64", target_arch = "x86_64"))]
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
        if !post_scale_bound_holds(input, weights.scales) {
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

/// Exact unscaled row dot products with the limb kernel.
///
/// Writes `acc[i] = sum_j data[i * n_cols + j] * input[j]` for every row of a
/// row-major INT8 matrix. No scale is applied: the caller owns the epilogue
/// (the dyadic profile in `crate::modern` applies `(acc * mu) >> k`). Returns
/// `false` without writing `acc` when it refuses, exactly like
/// [`matmul_i8_canonical_rows_fast`]; the caller then computes the same values
/// with its scalar kernel. The dot itself is exact inside the accepted domain
/// (see the module docs), so no scale-overflow check is needed here.
pub(crate) fn exact_row_dots_fast(
    data: &[i8],
    n_rows: usize,
    n_cols: usize,
    input: &[i64],
    acc: &mut [i64],
) -> bool {
    #[cfg(not(any(target_arch = "aarch64", target_arch = "x86_64")))]
    {
        let _ = (data, n_rows, n_cols, input, acc);
        record_attempt();
        record_refusal(Refusal::Unavailable)
    }
    #[cfg(any(target_arch = "aarch64", target_arch = "x86_64"))]
    {
        record_attempt();
        if !dotprod_available() {
            return record_refusal(Refusal::Unavailable);
        }
        if n_cols == 0
            || n_rows == 0
            || input.len() != n_cols
            || data.len() != n_rows.saturating_mul(n_cols)
            || acc.len() != n_rows
        {
            return record_refusal(Refusal::Shape);
        }
        if n_cols > MAX_COLS_FOR_I32 {
            return record_refusal(Refusal::InnerDimAboveI32Bound);
        }
        LIMB_SCRATCH.with(|cell| {
            let mut scratch = cell.borrow_mut();
            if scratch.len() < LIMB_COUNT * n_cols {
                scratch.resize(LIMB_COUNT * n_cols, 0);
            }
            let Some(used) = split_limbs(input, &mut scratch[..LIMB_COUNT * n_cols]) else {
                return record_refusal(Refusal::ActivationOutOfDomain);
            };
            let limbs = &scratch[..LIMB_COUNT * n_cols];
            // Small chunks: the dyadic models have 512-row K/V projections,
            // which 256-row chunks would split across only two workers.
            acc.par_chunks_mut(64)
                .enumerate()
                .for_each(|(chunk_idx, chunk)| {
                    let start = chunk_idx * 64;
                    for (local_i, out) in chunk.iter_mut().enumerate() {
                        let i = start + local_i;
                        // SAFETY: `data` holds n_rows * n_cols bytes (checked
                        // above) and `i < n_rows`, so `i * n_cols` is in bounds
                        // for `n_cols` reads; `limbs` holds LIMB_COUNT * n_cols
                        // digits; `n_cols <= MAX_COLS_FOR_I32`; the vector
                        // feature was checked by `dotprod_available`.
                        *out = unsafe {
                            dot_limbs_dotprod(data.as_ptr().add(i * n_cols), limbs, n_cols, used)
                        };
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
    #[cfg(not(any(target_arch = "aarch64", target_arch = "x86_64")))]
    {
        let _ = (weights, inputs, n_tokens, in_size, output);
        record_attempt();
        record_refusal(Refusal::Unavailable)
    }
    #[cfg(any(target_arch = "aarch64", target_arch = "x86_64"))]
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
                for (offset, &sc) in scales[r0..r1].iter().enumerate() {
                    let i = r0 + offset;
                    // SAFETY: `i < n_rows` and `data` holds `n_rows * in_size`
                    // bytes. Each rayon task owns a disjoint row range, and for
                    // a given row it writes only `out[(t+q) * n_rows + i]`, so
                    // no two tasks ever touch the same output element.
                    let acc = unsafe {
                        dot_limbs_x4(data.as_ptr().add(i * in_size), &planes, in_size, used_max)
                    };
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
/// switches: `KERNEL_STATE`, `CENSUS_ON`, the projection counters and the
/// prefill counters in [`crate::canonical_prefill`].
///
/// These switches are deliberately process-wide — that is what makes them
/// usable as an operator control — but the test harness runs tests in parallel
/// threads of one process, so a test that asserts a switch's state will observe
/// another test's change unless both serialise. That is exactly how the former
/// default-off test (`disabled_unless_explicitly_requested`) failed the first
/// time the batched prefill conformance tests were ever executed:
/// `run_batched_conformance` enables the kernel, restores it correctly
/// afterwards, and the default-off assertion still ran inside that window.
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
                for (i, got) in fast.iter().enumerate() {
                    let acc = scalar_dot(&w.data[i * cols..(i + 1) * cols], &input);
                    let want = (acc * w.scales[i]) >> FRAC_BITS;
                    assert_eq!(*got, want, "cols={cols} rows={rows} row={i}");
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

    #[test]
    fn exact_vector_backend_handles_the_maximum_inner_dimension() {
        let _guard = kernel_switch_guard();
        if !dotprod_available() {
            return;
        }
        let cols = MAX_COLS_FOR_I32;
        let w = weights_from(1, cols, |_, _| -128);
        let input = vec![LIMB_MIN; cols];
        let mut output = vec![0; 1];
        assert!(matmul_i8_canonical_rows_fast(&w, &input, cols, &mut output));
        assert_eq!(output[0], scalar_dot(&w.data, &input) >> FRAC_BITS);
    }

    #[test]
    fn exact_batched_backend_matches_scalar_across_short_quads_and_tails() {
        let _guard = kernel_switch_guard();
        if !dotprod_available() {
            return;
        }
        for cols in [1, 15, 16, 17, 33, 4096] {
            let rows = 5;
            let w = weights_from(rows, cols, |r, c| (r * 73 + c * 19) as u8 as i8);
            for tokens in [1, 2, 3, 4, 5, 7] {
                let inputs: Vec<i64> = (0..tokens * cols)
                    .map(|i| match (i / cols + i % cols) % 5 {
                        0 => LIMB_MIN,
                        1 => LIMB_MAX,
                        2 => -129,
                        3 => 1,
                        _ => 65536,
                    })
                    .collect();
                let mut output = vec![0; tokens * rows];
                assert!(matmul_i8_batched_fast(
                    &w,
                    &inputs,
                    tokens,
                    cols,
                    &mut output
                ));
                for token in 0..tokens {
                    for row in 0..rows {
                        let dot = scalar_dot(
                            &w.data[row * cols..(row + 1) * cols],
                            &inputs[token * cols..(token + 1) * cols],
                        );
                        assert_eq!(
                            output[token * rows + row],
                            (dot * w.scales[row]) >> FRAC_BITS,
                            "cols={cols}, tokens={tokens}, token={token}, row={row}"
                        );
                    }
                }
            }
        }
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

    fn choice(requested: Option<&str>, legacy: Option<&str>, default: bool) -> (bool, u8, bool) {
        let resolved = choose_kernel(requested, legacy, default);
        (resolved.simd, resolved.source, resolved.unrecognised)
    }

    #[test]
    fn kernel_overrides_take_precedence_and_fail_safe_to_scalar() {
        // No override: this target's default.
        assert_eq!(choice(None, None, true), (true, SOURCE_DEFAULT, false));
        assert_eq!(choice(None, None, false), (false, SOURCE_DEFAULT, false));
        // An operator can force the scalar reference whatever the default.
        for value in ["scalar", "SCALAR", " Scalar "] {
            let forced = choice(Some(value), None, true);
            assert_eq!(forced, (false, SOURCE_ENV, false), "{value:?}");
        }
        // ...force the vectorised kernel (the NEON opt-in), or keep the default.
        assert_eq!(choice(Some("simd"), None, false), (true, SOURCE_ENV, false));
        assert_eq!(choice(Some("auto"), None, true), (true, SOURCE_ENV, false));
        assert_eq!(
            choice(Some("auto"), None, false),
            (false, SOURCE_ENV, false)
        );
        // The new variable wins over the old one, in both directions.
        let new_wins = choice(Some("scalar"), Some("1"), true);
        assert_eq!(new_wins, (false, SOURCE_ENV, false));
        assert_eq!(
            choice(Some("simd"), Some("0"), false),
            (true, SOURCE_ENV, false)
        );
        // A value nobody recognises fails safe to scalar and is reported.
        let typo = choice(Some("scaler"), None, true);
        assert_eq!(typo, (false, SOURCE_ENV, true));
        // Empty means unset.
        let empty = choice(Some(" "), None, true);
        assert_eq!(empty, (true, SOURCE_DEFAULT, false));
        // The old variable keeps its meaning: `1` on, any other value off.
        let legacy_on = choice(None, Some("1"), false);
        assert_eq!(legacy_on, (true, SOURCE_LEGACY_ENV, false));
        for value in ["0", "", "off", "true"] {
            let legacy_off = choice(None, Some(value), true);
            assert_eq!(legacy_off, (false, SOURCE_LEGACY_ENV, false), "{value:?}");
        }
    }

    #[test]
    fn the_default_is_avx2_on_x86_and_follows_the_neon_switch_on_arm64() {
        // x86-64: the AVX2 kernel, except under Rosetta 2 translation.
        #[cfg(target_arch = "x86_64")]
        assert_eq!(simd_on_by_default(), !running_under_rosetta());
        // arm64: `NEON_ON_BY_DEFAULT` alone decides (on, now that both NEON
        // proofs have passed), so changing it stays a one-line edit.
        #[cfg(target_arch = "aarch64")]
        assert_eq!(simd_on_by_default(), NEON_ON_BY_DEFAULT);
    }

    #[test]
    fn an_explicit_choice_wins_and_is_reported() {
        let _guard = kernel_switch_guard();
        let previous = fast_canonical_kernel_enabled();

        set_fast_canonical_kernel(false);
        assert!(!fast_canonical_kernel_enabled());
        assert_eq!(effective_kernel_name(), "scalar-i8xi64");
        assert_eq!(kernel_choice_source(), "explicit");

        set_fast_canonical_kernel(true);
        assert_eq!(fast_canonical_kernel_enabled(), dotprod_available());
        let expected = if !dotprod_available() {
            "scalar-i8xi64"
        } else if cfg!(target_arch = "x86_64") {
            "avx2-limb"
        } else {
            "neon-sdot-limb"
        };
        assert_eq!(effective_kernel_name(), expected);
        assert_eq!(kernel_choice_source(), "explicit");

        set_fast_canonical_kernel(previous);
    }

    #[test]
    fn the_thread_census_counts_only_this_threads_projections() {
        let _ = take_thread_census();
        let cols = 64usize;
        let w = weights_with_scale(4, cols, 1);
        let good = vec![7i64; cols];
        let mut out = vec![0i64; 4];
        let accepted = matmul_i8_canonical_rows_fast(&w, &good, cols, &mut out);
        let mut bad = good.clone();
        bad[0] = LIMB_MAX + 1;
        assert!(!matmul_i8_canonical_rows_fast(&w, &bad, cols, &mut out));
        let [attempted, accepted_count, refused] = take_thread_census();
        assert_eq!(attempted, 2);
        assert_eq!(accepted_count, u64::from(accepted));
        assert_eq!(refused, 2 - u64::from(accepted));
        assert_eq!(accepted, dotprod_available());
        assert_eq!(take_thread_census(), [0; 3], "taking the census resets it");
    }
}
