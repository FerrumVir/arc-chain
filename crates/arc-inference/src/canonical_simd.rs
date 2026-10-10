//! Bit-exact vectorised canonical INT8 projection (ARM64 NEON / x86-64 AVX2).
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
//! # Multi-row kernels (CPU matrix extensions)
//!
//! [`matmul_i8_batched_fast`] computes `n_tokens` rows against one weight
//! matrix. It runs one of five kernels, chosen at run time by
//! [`selected_batched_kernel`]. All of them multiply the same digit planes
//! and differ only in the instruction:
//!
//! | Kernel | Needs | Instruction | i8 products per instruction |
//! |---|---|---|---|
//! | `neon-sdot-limb` | ARM FEAT_DotProd | `SDOT` | 16 |
//! | `neon-i8mm-limb` | ARM FEAT_I8MM (Armv8.6: Neoverse N2/V1, Apple M2 and later) | `SMMLA`, 2x8 by 8x2 | 32 |
//! | `avx2-limb` | AVX2 | `vpmaddwd` on sign-extended bytes | 16 |
//! | `avx-vnni-limb` | AVX-VNNI | 256-bit `vpdpbusd` | 32 |
//! | `avx512-vnni-limb` | AVX-512F and AVX-512 VNNI | 512-bit `vpdpbusd` | 64 |
//!
//! The two new instruction families are exact by these bounds:
//!
//! * **SMMLA.** It multiplies a 2x8 block of weight bytes (two rows) by an
//!   8x2 block of digits (two tokens) and adds the 2x2 product to four i32
//!   lanes. Each lane holds one (row, token) dot over every column, so it is
//!   bounded by `16_384 * K` exactly as above: at most `2_147_467_264` for
//!   `K = 131_071`, under `i32::MAX`. Lanes are widened to i64 per plane.
//! * **VPDPBUSD.** It multiplies *unsigned* bytes by signed bytes, so the
//!   digits are offset: `c' = c + 128` lies in `[0, 255]` (it is the byte
//!   `c ^ 0x80`), and `sum_j w_j * c_j = sum_j w_j * c'_j - 128 * sum_j w_j`.
//!   The row sum `sum_j w_j` is computed once per row, in i64. Each
//!   instruction adds four products `|c' * w| <= 255 * 128` to a lane, and a
//!   lane receives one such group per 32 columns (256-bit) or 64 columns
//!   (512-bit). So `|lane| <= 4 * 255 * 128 * ceil(K / 32) = 534_773_760`
//!   for `K = 131_071`, a quarter of `i32::MAX`. Lanes are summed in i64.
//!
//! Columns past the last whole vector are summed in i64 straight from the
//! activations, which the digit split reproduces exactly. Every kernel
//! therefore returns the scalar kernel's integer dot for every input this
//! module accepts, and they share the scale epilogue, so outputs are
//! byte-identical by construction. The tests check it on every CI runner.
//!
//! The opt-in is unchanged. These kernels run only when
//! [`fast_canonical_kernel_enabled`] is true (`ARC_FAST_CANONICAL_KERNEL=1`
//! or [`set_fast_canonical_kernel`]). `ARC_CANONICAL_BATCHED_KERNEL=<label>`
//! or [`set_batched_kernel_preference`] picks among the kernels the CPU has,
//! for tests and benchmarks; a kernel the CPU lacks falls back to the best
//! one it has. [`batched_kernel_runs`] counts which kernel actually ran.
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
use std::sync::OnceLock;
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
// VNNI lanes: four products `|(c + 128) * w| <= 255 * 128` per instruction,
// and one instruction per 32 columns at 256 bits (per 64 at 512 bits).
const VNNI_GROUP_MAX: i64 = 4 * 255 * 128;
const _: () = assert!((MAX_COLS_FOR_I32.div_ceil(32) as i64) * VNNI_GROUP_MAX <= i32::MAX as i64);

static FAST_KERNEL: AtomicBool = AtomicBool::new(false);

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

#[inline]
fn record_attempt() {
    if CENSUS_ON.load(Ordering::Relaxed) {
        N_ATTEMPTED.fetch_add(1, Ordering::Relaxed);
    }
}

#[cfg(any(target_arch = "aarch64", target_arch = "x86_64"))]
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
/// The historical name is retained for callers: x86-64 requires AVX2,
/// whereas ARM64 requires NEON dotprod. The opt-in still defaults to off.
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

/// An exact multi-row limb kernel for [`matmul_i8_batched_fast`]. Every
/// kernel computes the same integers; they differ only in the instruction
/// that multiplies digits (see the module docs for the bounds).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BatchedKernel {
    /// ARM `SDOT` (FEAT_DotProd): four tokens share each weight load.
    NeonSdot,
    /// ARM `SMMLA` (FEAT_I8MM): two weight rows meet two tokens in each
    /// instruction.
    NeonI8mm,
    /// x86-64 AVX2: sign-extended bytes into `vpmaddwd`; four tokens share
    /// each weight load.
    Avx2,
    /// x86-64 AVX-VNNI: 256-bit `vpdpbusd`, digits offset by 128.
    AvxVnni,
    /// x86-64 AVX-512 VNNI: 512-bit `vpdpbusd`, digits offset by 128.
    Avx512Vnni,
}

impl BatchedKernel {
    /// Every kernel, in declaration order.
    pub const ALL: [Self; 5] = [
        Self::NeonSdot,
        Self::NeonI8mm,
        Self::Avx2,
        Self::AvxVnni,
        Self::Avx512Vnni,
    ];

    /// The kernel's name in logs and bench reports. The two base kernels keep
    /// the names `neon-sdot-limb` and `avx2-limb`.
    pub const fn label(self) -> &'static str {
        match self {
            Self::NeonSdot => "neon-sdot-limb",
            Self::NeonI8mm => "neon-i8mm-limb",
            Self::Avx2 => "avx2-limb",
            Self::AvxVnni => "avx-vnni-limb",
            Self::Avx512Vnni => "avx512-vnni-limb",
        }
    }

    /// The kernel with this [`Self::label`].
    pub fn from_label(label: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|kernel| kernel.label() == label)
    }

    /// Whether this build and CPU can run the kernel, detected at run time.
    /// Each matrix-extension kernel also requires its architecture's base
    /// kernel, which runs a lone row and any input the extension skips.
    pub fn available(self) -> bool {
        match self {
            #[cfg(target_arch = "aarch64")]
            Self::NeonSdot => std::arch::is_aarch64_feature_detected!("dotprod"),
            #[cfg(target_arch = "aarch64")]
            Self::NeonI8mm => {
                std::arch::is_aarch64_feature_detected!("dotprod")
                    && std::arch::is_aarch64_feature_detected!("i8mm")
            }
            #[cfg(target_arch = "x86_64")]
            Self::Avx2 => std::arch::is_x86_feature_detected!("avx2"),
            #[cfg(target_arch = "x86_64")]
            Self::AvxVnni => {
                std::arch::is_x86_feature_detected!("avx2")
                    && std::arch::is_x86_feature_detected!("avxvnni")
            }
            #[cfg(target_arch = "x86_64")]
            Self::Avx512Vnni => {
                std::arch::is_x86_feature_detected!("avx2")
                    && std::arch::is_x86_feature_detected!("avx512f")
                    && std::arch::is_x86_feature_detected!("avx512vnni")
            }
            _ => false,
        }
    }
}

/// Every batched kernel this CPU can run, in declaration order.
pub fn available_batched_kernels() -> Vec<BatchedKernel> {
    BatchedKernel::ALL
        .into_iter()
        .filter(|kernel| kernel.available())
        .collect()
}

/// The kernel used when no preference is set: the widest this CPU has.
pub fn best_batched_kernel() -> Option<BatchedKernel> {
    [
        BatchedKernel::Avx512Vnni,
        BatchedKernel::AvxVnni,
        BatchedKernel::Avx2,
        BatchedKernel::NeonI8mm,
        BatchedKernel::NeonSdot,
    ]
    .into_iter()
    .find(|kernel| kernel.available())
}

/// `0` is automatic selection; otherwise the preferred kernel's index + 1.
static BATCHED_PREFERENCE: AtomicU8 = AtomicU8::new(0);

/// `ARC_CANONICAL_BATCHED_KERNEL=<label>` sets the initial preference once.
/// An explicit [`set_batched_kernel_preference`] call always wins.
fn apply_batched_kernel_env() {
    static ENV_INIT: OnceLock<()> = OnceLock::new();
    ENV_INIT.get_or_init(|| {
        let Ok(value) = std::env::var("ARC_CANONICAL_BATCHED_KERNEL") else {
            return;
        };
        let value = value.trim();
        match BatchedKernel::from_label(value) {
            Some(kernel) => BATCHED_PREFERENCE.store(kernel as u8 + 1, Ordering::Relaxed),
            None if value.is_empty() || value == "auto" => {}
            None => eprintln!(
                "ARC_CANONICAL_BATCHED_KERNEL={value:?} names no batched kernel; selecting automatically"
            ),
        }
    });
}

/// Prefer one batched kernel, or `None` for automatic selection.
///
/// This only chooses among the vector kernels. It never enables the vector
/// path, which stays behind [`set_fast_canonical_kernel`]. A preferred kernel
/// the CPU lacks falls back to [`best_batched_kernel`].
pub fn set_batched_kernel_preference(kernel: Option<BatchedKernel>) {
    apply_batched_kernel_env();
    BATCHED_PREFERENCE.store(kernel.map_or(0, |k| k as u8 + 1), Ordering::Relaxed);
}

/// The preferred batched kernel, if one is set.
pub fn batched_kernel_preference() -> Option<BatchedKernel> {
    apply_batched_kernel_env();
    match BATCHED_PREFERENCE.load(Ordering::Relaxed) {
        0 => None,
        slot => BatchedKernel::ALL.get(usize::from(slot) - 1).copied(),
    }
}

/// The kernel [`matmul_i8_batched_fast`] runs: the preference if this CPU has
/// it, otherwise the best kernel it has.
pub fn selected_batched_kernel() -> Option<BatchedKernel> {
    match batched_kernel_preference() {
        Some(kernel) if kernel.available() => Some(kernel),
        _ => best_batched_kernel(),
    }
}

static BATCHED_RUNS: [AtomicU64; 5] = [const { AtomicU64::new(0) }; 5];
static LAST_BATCHED: AtomicU8 = AtomicU8::new(0);

/// Always counted: one relaxed increment per projection call, so a log or a
/// bench can show which kernel really ran.
#[cfg(any(target_arch = "aarch64", target_arch = "x86_64"))]
fn record_batched_run(kernel: BatchedKernel) {
    BATCHED_RUNS[kernel as usize].fetch_add(1, Ordering::Relaxed);
    LAST_BATCHED.store(kernel as u8 + 1, Ordering::Relaxed);
}

/// How many batched projections `kernel` has computed in this process.
pub fn batched_kernel_runs(kernel: BatchedKernel) -> u64 {
    BATCHED_RUNS[kernel as usize].load(Ordering::Relaxed)
}

/// The kernel of the most recent accepted batched projection, if any.
pub fn last_batched_kernel() -> Option<BatchedKernel> {
    match LAST_BATCHED.load(Ordering::Relaxed) {
        0 => None,
        slot => BatchedKernel::ALL.get(usize::from(slot) - 1).copied(),
    }
}

/// `label=count` for every kernel that is available or has run.
pub fn batched_kernel_run_report() -> String {
    BatchedKernel::ALL
        .into_iter()
        .filter(|kernel| kernel.available() || batched_kernel_runs(*kernel) > 0)
        .map(|kernel| format!("{}={}", kernel.label(), batched_kernel_runs(kernel)))
        .collect::<Vec<_>>()
        .join(" ")
}

/// The CPU features the vector kernels depend on, as this process detects
/// them, for logs.
pub fn detected_cpu_features() -> Vec<(&'static str, bool)> {
    #[cfg(target_arch = "aarch64")]
    {
        vec![
            (
                "dotprod",
                std::arch::is_aarch64_feature_detected!("dotprod"),
            ),
            ("i8mm", std::arch::is_aarch64_feature_detected!("i8mm")),
        ]
    }
    #[cfg(target_arch = "x86_64")]
    {
        vec![
            ("avx2", std::arch::is_x86_feature_detected!("avx2")),
            ("avx512f", std::arch::is_x86_feature_detected!("avx512f")),
            (
                "avx512vnni",
                std::arch::is_x86_feature_detected!("avx512vnni"),
            ),
            ("avxvnni", std::arch::is_x86_feature_detected!("avxvnni")),
        ]
    }
    #[cfg(not(any(target_arch = "aarch64", target_arch = "x86_64")))]
    {
        Vec::new()
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

/// One `SMMLA Vd.4S, Vn.16B, Vm.16B` (FEAT_I8MM): `Vn` holds two rows of
/// eight signed bytes, `Vm` two columns of eight, and the 2x2 product is
/// added exactly to the i32 lanes `[r0.c0, r0.c1, r1.c0, r1.c1]`.
///
/// `vmmlaq_s32` is unstable (`stdarch_neon_i8mm`), so the instruction is
/// emitted with inline assembly, as [`sdot`] is.
///
/// # Safety
/// Requires the `i8mm` target feature, which the caller checks at run time.
#[cfg(target_arch = "aarch64")]
#[target_feature(enable = "neon,i8mm")]
#[inline]
unsafe fn smmla(
    acc: std::arch::aarch64::int32x4_t,
    a: std::arch::aarch64::int8x16_t,
    b: std::arch::aarch64::int8x16_t,
) -> std::arch::aarch64::int32x4_t {
    // SAFETY: wrapped for `unsafe_op_in_unsafe_fn`. The instruction reads no
    // memory and has no side effects; operands are register values.
    unsafe {
        let mut out = acc;
        std::arch::asm!(
            "smmla {o:v}.4s, {a:v}.16b, {b:v}.16b",
            o = inout(vreg) out,
            a = in(vreg) a,
            b = in(vreg) b,
            options(pure, nomem, nostack)
        );
        out
    }
}

/// Four weight rows (two row pairs) against `2 * TP` tokens (`TP` token
/// pairs) over the first `len` columns of `used` digit planes, with `SMMLA`.
///
/// Returns, per row pair `[a, b]` and token pair `[t0, t1]`, the exact sums
/// `[a.t0, a.t1, b.t0, b.t1]` of `256^l * sum_j w_j * c_l[j]` over planes.
///
/// Each 16 columns of a row pair are zipped into two `Vn` operands (columns
/// 0-7 of both rows, then 8-15). Token pairs come pre-interleaved by
/// `run_i8mm`: per plane, each 8-column block holds the first token's eight
/// digits and then the second's, which is the `Vm` layout. Every i32 lane is
/// one (row, token) dot, bounded by `16_384 * len` (module docs).
///
/// # Safety
/// `neon` and `i8mm` available. Each row pointer is valid for `len` reads.
/// Each pair pointer is valid for `used * 2 * len` reads in that layout.
/// `len` is a multiple of 8 and at most `MAX_COLS_FOR_I32`; `used <=
/// LIMB_COUNT`.
#[cfg(target_arch = "aarch64")]
#[target_feature(enable = "neon,i8mm")]
unsafe fn i8mm_tile<const TP: usize>(
    rows: &[[*const i8; 2]; 2],
    pairs: &[*const i8; TP],
    len: usize,
    used: usize,
) -> [[[i64; 4]; TP]; 2] {
    // SAFETY: wrapped for `unsafe_op_in_unsafe_fn`. Row loads read columns
    // `j..j + 16` (or `j..j + 8`) with `j + 16 <= len` (or `j + 8 <= len`);
    // pair loads read `2 * j..2 * j + 32` (or `+ 16`) inside plane `l <
    // used`, each plane being `2 * len` bytes.
    unsafe {
        use std::arch::aarch64::*;
        let mut total = [[[0i64; 4]; TP]; 2];
        for l in 0..used {
            let plane = l * 2 * len;
            let mut acc = [[vdupq_n_s32(0); TP]; 2];
            let mut j = 0usize;
            while j + 16 <= len {
                let mut low = [vdupq_n_s8(0); 2];
                let mut high = [vdupq_n_s8(0); 2];
                for ((low, high), [a, b]) in low.iter_mut().zip(high.iter_mut()).zip(rows) {
                    let a = vreinterpretq_s64_s8(vld1q_s8(a.add(j)));
                    let b = vreinterpretq_s64_s8(vld1q_s8(b.add(j)));
                    *low = vreinterpretq_s8_s64(vzip1q_s64(a, b));
                    *high = vreinterpretq_s8_s64(vzip2q_s64(a, b));
                }
                for (tp, pair) in pairs.iter().enumerate() {
                    let digits = pair.add(plane + 2 * j);
                    let first = vld1q_s8(digits);
                    let second = vld1q_s8(digits.add(16));
                    for rp in 0..2 {
                        acc[rp][tp] = smmla(acc[rp][tp], low[rp], first);
                        acc[rp][tp] = smmla(acc[rp][tp], high[rp], second);
                    }
                }
                j += 16;
            }
            if j < len {
                // `len` is a multiple of 8: one block of 8 columns is left.
                let mut both = [vdupq_n_s8(0); 2];
                for (both, [a, b]) in both.iter_mut().zip(rows) {
                    *both = vcombine_s8(vld1_s8(a.add(j)), vld1_s8(b.add(j)));
                }
                for (tp, pair) in pairs.iter().enumerate() {
                    let digits = vld1q_s8(pair.add(plane + 2 * j));
                    for rp in 0..2 {
                        acc[rp][tp] = smmla(acc[rp][tp], both[rp], digits);
                    }
                }
            }
            let place = 1i64 << (8 * l);
            for (total, acc) in total.iter_mut().zip(&acc) {
                for (total, acc) in total.iter_mut().zip(acc) {
                    let mut lanes = [0i32; 4];
                    vst1q_s32(lanes.as_mut_ptr(), *acc);
                    for (total, lane) in total.iter_mut().zip(lanes) {
                        // Multiplication, as in `dot_limbs_dotprod`.
                        *total += i64::from(lane) * place;
                    }
                }
            }
        }
        total
    }
}

/// Sum of the sixteen i32 lanes, in i64: the lanes' total can exceed i32.
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx512f")]
#[inline]
fn lanes_sum_512(v: std::arch::x86_64::__m512i) -> i64 {
    use std::arch::x86_64::*;
    _mm512_reduce_add_epi64(_mm512_add_epi64(
        _mm512_cvtepi32_epi64(_mm512_castsi512_si256(v)),
        _mm512_cvtepi32_epi64(_mm512_extracti64x4_epi64::<1>(v)),
    ))
}

/// Sum of the eight i32 lanes, in i64.
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
#[inline]
fn lanes_sum_256(v: std::arch::x86_64::__m256i) -> i64 {
    use std::arch::x86_64::*;
    let wide = _mm256_add_epi64(
        _mm256_cvtepi32_epi64(_mm256_castsi256_si128(v)),
        _mm256_cvtepi32_epi64(_mm256_extracti128_si256::<1>(v)),
    );
    let pair = _mm_add_epi64(
        _mm256_castsi256_si128(wide),
        _mm256_extracti128_si256::<1>(wide),
    );
    _mm_extract_epi64::<0>(pair) + _mm_extract_epi64::<1>(pair)
}

/// Four weight rows against `T` tokens over the first `len` columns of
/// `used` offset digit planes, with 512-bit `vpdpbusd`.
///
/// Returns per row and token the exact `sum_l 256^l * sum_j w_j * c_l[j]`.
/// The planes hold `c + 128` as unsigned bytes, so each plane's sum has
/// `128 * sums[r]` subtracted, which undoes the offset (module docs).
///
/// # Safety
/// AVX-512F and AVX-512 VNNI available. Each row pointer is valid for `len`
/// reads; each token pointer for `(used - 1) * in_size + len` reads of offset
/// digits, plane `l` starting at `l * in_size`. `len` is a multiple of 64 and
/// at most `MAX_COLS_FOR_I32`, and `sums[r]` is the sum of row `r`'s first
/// `len` weights.
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx512f,avx512vnni")]
unsafe fn vnni512_tile<const T: usize>(
    rows: &[*const i8; 4],
    tokens: &[*const i8; T],
    in_size: usize,
    len: usize,
    used: usize,
    sums: &[i64; 4],
) -> [[i64; T]; 4] {
    // SAFETY: wrapped for `unsafe_op_in_unsafe_fn`. Every load reads 64
    // bytes at column `j` with `j + 64 <= len`, inside the ranges above.
    unsafe {
        use std::arch::x86_64::*;
        let mut total = [[0i64; T]; 4];
        for l in 0..used {
            let mut acc = [[_mm512_setzero_si512(); T]; 4];
            let mut j = 0usize;
            while j + 64 <= len {
                let mut weights = [_mm512_setzero_si512(); 4];
                for (weights, row) in weights.iter_mut().zip(rows) {
                    *weights = _mm512_loadu_si512(row.add(j).cast());
                }
                for (t, token) in tokens.iter().enumerate() {
                    let digits = _mm512_loadu_si512(token.add(l * in_size + j).cast());
                    for r in 0..4 {
                        acc[r][t] = _mm512_dpbusd_epi32(acc[r][t], digits, weights[r]);
                    }
                }
                j += 64;
            }
            let place = 1i64 << (8 * l);
            for ((total, acc), &sum) in total.iter_mut().zip(&acc).zip(sums) {
                for (total, &acc) in total.iter_mut().zip(acc) {
                    *total += (lanes_sum_512(acc) - 128 * sum) * place;
                }
            }
        }
        total
    }
}

/// Two weight rows against `T` tokens, as [`vnni512_tile`] but with the
/// 256-bit VEX `vpdpbusd` of AVX-VNNI and `len` a multiple of 32.
///
/// # Safety
/// AVX2 and AVX-VNNI available; otherwise as [`vnni512_tile`].
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2,avxvnni")]
unsafe fn vnni256_tile<const T: usize>(
    rows: &[*const i8; 2],
    tokens: &[*const i8; T],
    in_size: usize,
    len: usize,
    used: usize,
    sums: &[i64; 2],
) -> [[i64; T]; 2] {
    // SAFETY: wrapped for `unsafe_op_in_unsafe_fn`. Every load reads 32
    // bytes at column `j` with `j + 32 <= len`, inside the ranges above.
    unsafe {
        use std::arch::x86_64::*;
        let mut total = [[0i64; T]; 2];
        for l in 0..used {
            let mut acc = [[_mm256_setzero_si256(); T]; 2];
            let mut j = 0usize;
            while j + 32 <= len {
                let mut weights = [_mm256_setzero_si256(); 2];
                for (weights, row) in weights.iter_mut().zip(rows) {
                    *weights = _mm256_loadu_si256(row.add(j).cast());
                }
                for (t, token) in tokens.iter().enumerate() {
                    let digits = _mm256_loadu_si256(token.add(l * in_size + j).cast());
                    for r in 0..2 {
                        acc[r][t] = _mm256_dpbusd_avx_epi32(acc[r][t], digits, weights[r]);
                    }
                }
                j += 32;
            }
            let place = 1i64 << (8 * l);
            for ((total, acc), &sum) in total.iter_mut().zip(&acc).zip(sums) {
                for (total, &acc) in total.iter_mut().zip(acc) {
                    *total += (lanes_sum_256(acc) - 128 * sum) * place;
                }
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
/// against the same weight matrix, on the kernel [`selected_batched_kernel`]
/// picks.
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
    let Some(kernel) = selected_batched_kernel() else {
        record_attempt();
        return record_refusal(Refusal::Unavailable);
    };
    matmul_i8_batched_with(kernel, weights, inputs, n_tokens, in_size, output)
}

/// [`matmul_i8_batched_fast`] on a named kernel. Refuses with
/// [`Refusal::Unavailable`], writing nothing, when this CPU lacks it.
///
/// Every check and the digit split are shared by all kernels and run before
/// any output is written. `NeonI8mm` pairs tokens, so a single token runs on
/// `NeonSdot`; [`batched_kernel_runs`] records the kernel that ran.
pub fn matmul_i8_batched_with(
    kernel: BatchedKernel,
    weights: &I8Weights,
    inputs: &[i64],
    n_tokens: usize,
    in_size: usize,
    output: &mut [i64],
) -> bool {
    #[cfg(not(any(target_arch = "aarch64", target_arch = "x86_64")))]
    {
        let _ = (kernel, weights, inputs, n_tokens, in_size, output);
        record_attempt();
        record_refusal(Refusal::Unavailable)
    }
    #[cfg(any(target_arch = "aarch64", target_arch = "x86_64"))]
    {
        record_attempt();
        if !kernel.available() {
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
        let scales = &weights.scales[..];
        // The post-dot bound must hold for EVERY token, so it is checked
        // against every token's own activation before anything is written.
        // Tokens are independent, so the checks run in parallel; whether any
        // token fails does not depend on the order.
        if !inputs
            .par_chunks(in_size)
            .all(|input| post_scale_bound_holds(input, scales))
        {
            return record_refusal(Refusal::ScaleMultiplyWouldOverflow);
        }
        // Each token's digits fill its own `LIMB_COUNT * in_size` planes, so
        // the split also runs in parallel. Order cannot change a digit.
        let mut limbs = vec![0i8; n_tokens * LIMB_COUNT * in_size];
        let used: Option<Vec<usize>> = limbs
            .par_chunks_mut(LIMB_COUNT * in_size)
            .zip(inputs.par_chunks(in_size))
            .map(|(planes, input)| split_limbs(input, planes))
            .collect();
        let Some(used) = used else {
            return record_refusal(Refusal::ActivationOutOfDomain);
        };
        // Using more digit planes than a token needs is still exact - the
        // extra planes are zero - so one `used` for the whole call keeps the
        // inner loops branch-free.
        let job = BatchJob {
            data: &weights.data[..],
            scales,
            inputs,
            n_rows: weights.n_rows,
            n_tokens,
            in_size,
            used: used.iter().copied().max().unwrap_or(1),
        };
        let ran = run_batched(kernel, job, &mut limbs, SendPtr(output.as_mut_ptr()));
        record_batched_run(ran);
        record_accept();
        true
    }
}

/// One validated batched projection, shared by the kernel drivers.
#[cfg(any(target_arch = "aarch64", target_arch = "x86_64"))]
#[derive(Clone, Copy)]
struct BatchJob<'a> {
    /// `n_rows * in_size` weight bytes, row-major.
    data: &'a [i8],
    scales: &'a [i64],
    /// `n_tokens * in_size` activations, token-major.
    inputs: &'a [i64],
    n_rows: usize,
    n_tokens: usize,
    in_size: usize,
    /// Digit planes in use: the most any token needs.
    used: usize,
}

/// The exact i64 dot of the columns a vector loop did not cover.
#[cfg(any(target_arch = "aarch64", target_arch = "x86_64"))]
fn tail_dot(weights: &[i8], inputs: &[i64]) -> i64 {
    weights
        .iter()
        .zip(inputs)
        .map(|(&w, &x)| i64::from(w) * x)
        .sum()
}

/// Copies a tile of `T <= 4` results into a fixed width of four.
#[cfg(any(target_arch = "aarch64", target_arch = "x86_64"))]
fn widen<E: Copy + Default, const R: usize, const T: usize>(tile: [[E; T]; R]) -> [[E; 4]; R] {
    let mut wide = [[E::default(); 4]; R];
    for (wide, tile) in wide.iter_mut().zip(tile) {
        wide[..T].copy_from_slice(&tile);
    }
    wide
}

/// Runs `kernel` and returns the kernel that ran.
#[cfg(target_arch = "aarch64")]
fn run_batched(
    kernel: BatchedKernel,
    job: BatchJob<'_>,
    limbs: &mut [i8],
    out: SendPtr,
) -> BatchedKernel {
    // SMMLA pairs tokens: one token alone would leave half of every
    // instruction idle, so a single row runs on SDOT.
    if kernel == BatchedKernel::NeonI8mm && job.n_tokens > 1 {
        run_i8mm(job, limbs, out);
        return BatchedKernel::NeonI8mm;
    }
    run_quads(job, limbs, out);
    BatchedKernel::NeonSdot
}

/// Runs `kernel` and returns the kernel that ran.
#[cfg(target_arch = "x86_64")]
fn run_batched(
    kernel: BatchedKernel,
    job: BatchJob<'_>,
    limbs: &mut [i8],
    out: SendPtr,
) -> BatchedKernel {
    let (in_size, used) = (job.in_size, job.used);
    match kernel {
        BatchedKernel::Avx512Vnni => {
            offset_digits(job, limbs);
            run_vnni::<4, _>(job, limbs, 64, out, |rows, tokens, len, sums| {
                // SAFETY: AVX-512F and AVX-512 VNNI were detected by
                // `kernel.available()`. `run_vnni` passes row pointers valid
                // for `in_size >= len` bytes and token pointers valid for
                // `LIMB_COUNT * in_size` offset digits; `len` is a multiple
                // of 64 and at most `MAX_COLS_FOR_I32`.
                unsafe {
                    match *tokens {
                        [a] => widen(vnni512_tile(rows, &[a], in_size, len, used, sums)),
                        [a, b] => widen(vnni512_tile(rows, &[a, b], in_size, len, used, sums)),
                        [a, b, c] => {
                            widen(vnni512_tile(rows, &[a, b, c], in_size, len, used, sums))
                        }
                        [a, b, c, d] => {
                            widen(vnni512_tile(rows, &[a, b, c, d], in_size, len, used, sums))
                        }
                        _ => unreachable!("a token tile holds one to four tokens"),
                    }
                }
            });
            kernel
        }
        BatchedKernel::AvxVnni => {
            offset_digits(job, limbs);
            run_vnni::<2, _>(job, limbs, 32, out, |rows, tokens, len, sums| {
                // SAFETY: AVX2 and AVX-VNNI were detected by
                // `kernel.available()`; pointers and `len` as above, with
                // `len` a multiple of 32.
                unsafe {
                    match *tokens {
                        [a] => widen(vnni256_tile(rows, &[a], in_size, len, used, sums)),
                        [a, b] => widen(vnni256_tile(rows, &[a, b], in_size, len, used, sums)),
                        [a, b, c] => {
                            widen(vnni256_tile(rows, &[a, b, c], in_size, len, used, sums))
                        }
                        [a, b, c, d] => {
                            widen(vnni256_tile(rows, &[a, b, c, d], in_size, len, used, sums))
                        }
                        _ => unreachable!("a token tile holds one to four tokens"),
                    }
                }
            });
            kernel
        }
        _ => {
            run_quads(job, limbs, out);
            BatchedKernel::Avx2
        }
    }
}

/// The base kernels (`SDOT`, AVX2): four tokens per weight load.
///
/// Tiling. `row_block` keeps a block of weight rows inside L1 (~128 KiB),
/// and tokens are processed four at a time so ONE weight vector load feeds
/// four accumulator chains. DRAM weight traffic therefore falls from
/// once-per-token to once-per-matmul, which is the entire point of batching;
/// the per-(row, token) arithmetic is untouched.
#[cfg(any(target_arch = "aarch64", target_arch = "x86_64"))]
fn run_quads(job: BatchJob<'_>, limbs: &[i8], out: SendPtr) {
    let BatchJob {
        data,
        scales,
        n_rows,
        n_tokens,
        in_size,
        used,
        ..
    } = job;
    let row_block = (131_072 / in_size).clamp(1, n_rows.max(1));
    let n_blocks = n_rows.div_ceil(row_block);
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
                // no two tasks ever touch the same output element. The
                // kernel's CPU feature was checked by `kernel.available()`.
                let acc =
                    unsafe { dot_limbs_x4(data.as_ptr().add(i * in_size), &planes, in_size, used) };
                for (q, a) in acc.iter().enumerate().take(quad) {
                    unsafe { *out.get().add((t + q) * n_rows + i) = (*a * sc) >> FRAC_BITS };
                }
            }
            t += quad;
        }
    });
}

/// `SMMLA` driver: tiles of four rows (two row pairs) by up to four token
/// pairs.
///
/// The digit planes are first re-laid as token pairs (see [`i8mm_tile`]); an
/// odd final token is paired with zero digits, whose lanes are never written.
/// A short final row tile repeats its last row, likewise discarded. Columns
/// past the last multiple of 8 are summed in i64 by [`tail_dot`].
#[cfg(target_arch = "aarch64")]
fn run_i8mm(job: BatchJob<'_>, limbs: &[i8], out: SendPtr) {
    let BatchJob {
        data,
        scales,
        inputs,
        n_rows,
        n_tokens,
        in_size,
        used,
    } = job;
    let len = in_size / 8 * 8;
    let n_pairs = n_tokens.div_ceil(2);
    let pair_len = used * 2 * len;
    let mut pairs = vec![0i8; n_pairs * pair_len];
    if pair_len > 0 {
        pairs
            .par_chunks_mut(pair_len)
            .enumerate()
            .for_each(|(pair, interleaved)| {
                for (half, token) in [2 * pair, 2 * pair + 1].into_iter().enumerate() {
                    if token >= n_tokens {
                        continue;
                    }
                    for (l, plane) in interleaved.chunks_exact_mut(2 * len).enumerate() {
                        let digits = &limbs[(token * LIMB_COUNT + l) * in_size..][..len];
                        for (block, eight) in plane.chunks_exact_mut(16).zip(digits.chunks_exact(8))
                        {
                            block[8 * half..8 * half + 8].copy_from_slice(eight);
                        }
                    }
                }
            });
    }
    let pairs = &pairs[..];
    let row_block = (131_072 / in_size).max(1).next_multiple_of(4);
    let n_blocks = n_rows.div_ceil(row_block);
    (0..n_blocks).into_par_iter().for_each(|block| {
        let first = block * row_block;
        let end = (first + row_block).min(n_rows);
        let mut r = first;
        while r < end {
            let real = (end - r).min(4);
            let row = |k: usize| data[(r + k.min(real - 1)) * in_size..].as_ptr();
            let rows = [[row(0), row(1)], [row(2), row(3)]];
            let mut p = 0usize;
            while p < n_pairs {
                let count = (n_pairs - p).min(4);
                let pair = |q: usize| pairs[(p + q.min(count - 1)) * pair_len..].as_ptr();
                // SAFETY: FEAT_I8MM was detected by `kernel.available()`.
                // Each row pointer starts a whole row (`in_size >= len`
                // bytes), and each pair pointer starts a whole pair of
                // `used * 2 * len` interleaved digits; `len` is a multiple
                // of 8 and at most `MAX_COLS_FOR_I32`.
                let totals = unsafe {
                    match count {
                        1 => widen(i8mm_tile(&rows, &[pair(0)], len, used)),
                        2 => widen(i8mm_tile(&rows, &[pair(0), pair(1)], len, used)),
                        3 => widen(i8mm_tile(&rows, &[pair(0), pair(1), pair(2)], len, used)),
                        _ => widen(i8mm_tile(
                            &rows,
                            &[pair(0), pair(1), pair(2), pair(3)],
                            len,
                            used,
                        )),
                    }
                };
                for (rp, by_pair) in totals.iter().enumerate() {
                    for (q, lanes) in by_pair.iter().enumerate().take(count) {
                        for (lane, &dot) in lanes.iter().enumerate() {
                            let i = r + 2 * rp + lane / 2;
                            let token = 2 * (p + q) + lane % 2;
                            if i >= r + real || token >= n_tokens {
                                continue;
                            }
                            let acc = dot
                                + tail_dot(
                                    &data[i * in_size + len..(i + 1) * in_size],
                                    &inputs[token * in_size + len..(token + 1) * in_size],
                                );
                            // SAFETY: `token < n_tokens` and `i < n_rows`, so
                            // the element is inside `output`. Each rayon task
                            // owns a disjoint row range, so no two tasks write
                            // the same element.
                            unsafe {
                                *out.get().add(token * n_rows + i) = (acc * scales[i]) >> FRAC_BITS
                            };
                        }
                    }
                }
                p += count;
            }
            r += real;
        }
    });
}

/// Rewrites the used digit planes in place as the unsigned bytes `c + 128`
/// that `vpdpbusd` multiplies: `c ^ 0x80` is `c + 128` for every `c` in
/// `[-128, 127]`. The tile functions subtract `128 * sum_j w_j` to undo it.
#[cfg(target_arch = "x86_64")]
fn offset_digits(job: BatchJob<'_>, limbs: &mut [i8]) {
    let span = job.used * job.in_size;
    limbs
        .par_chunks_mut(LIMB_COUNT * job.in_size)
        .for_each(|planes| {
            for digit in &mut planes[..span] {
                // `i8::MIN` is the byte 0x80.
                *digit ^= i8::MIN;
            }
        });
}

/// VNNI driver: tiles of `R` rows by up to four tokens. `step` is the
/// columns per instruction (64 or 32); columns past the last whole step are
/// summed in i64 by [`tail_dot`]. `tile` returns, per row and token, the
/// exact plane-weighted dot of the covered columns. A short final row tile
/// repeats its last row, whose lanes are never written.
#[cfg(target_arch = "x86_64")]
fn run_vnni<const R: usize, F>(job: BatchJob<'_>, planes: &[i8], step: usize, out: SendPtr, tile: F)
where
    F: Fn(&[*const i8; R], &[*const i8], usize, &[i64; R]) -> [[i64; 4]; R] + Sync,
{
    let BatchJob {
        data,
        scales,
        inputs,
        n_rows,
        n_tokens,
        in_size,
        ..
    } = job;
    let len = in_size / step * step;
    let row_block = (131_072 / in_size).max(1).next_multiple_of(R);
    let n_blocks = n_rows.div_ceil(row_block);
    (0..n_blocks).into_par_iter().for_each(|block| {
        let first = block * row_block;
        let end = (first + row_block).min(n_rows);
        let mut r = first;
        while r < end {
            let real = (end - r).min(R);
            let row_of = |k: usize| r + k.min(real - 1);
            let rows: [*const i8; R] =
                std::array::from_fn(|k| data[row_of(k) * in_size..].as_ptr());
            // The offset correction needs each row's sum over the covered
            // columns, once per row, in i64.
            let sums: [i64; R] = std::array::from_fn(|k| {
                data[row_of(k) * in_size..][..len]
                    .iter()
                    .map(|&w| i64::from(w))
                    .sum()
            });
            let mut t = 0usize;
            while t < n_tokens {
                let count = (n_tokens - t).min(4);
                let tokens: [*const i8; 4] = std::array::from_fn(|q| {
                    planes[(t + q.min(count - 1)) * LIMB_COUNT * in_size..].as_ptr()
                });
                let totals = tile(&rows, &tokens[..count], len, &sums);
                for (k, by_token) in totals.iter().enumerate().take(real) {
                    let i = r + k;
                    for (q, &dot) in by_token.iter().enumerate().take(count) {
                        let token = t + q;
                        let acc = dot
                            + tail_dot(
                                &data[i * in_size + len..(i + 1) * in_size],
                                &inputs[token * in_size + len..(token + 1) * in_size],
                            );
                        // SAFETY: `token < n_tokens` and `i < n_rows`, so the
                        // element is inside `output`. Each rayon task owns a
                        // disjoint row range, so no two tasks write the same
                        // element.
                        unsafe {
                            *out.get().add(token * n_rows + i) = (acc * scales[i]) >> FRAC_BITS
                        };
                    }
                }
                t += count;
            }
            r += real;
        }
    });
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

    /// The scalar reference for one token-major batched projection.
    fn scalar_batched(w: &I8Weights, inputs: &[i64], tokens: usize, cols: usize) -> Vec<i64> {
        let mut out = vec![0i64; tokens * w.n_rows];
        for t in 0..tokens {
            for r in 0..w.n_rows {
                let dot = scalar_dot(
                    &w.data[r * cols..(r + 1) * cols],
                    &inputs[t * cols..(t + 1) * cols],
                );
                out[t * w.n_rows + r] = (dot * w.scales[r]) >> FRAC_BITS;
            }
        }
        out
    }

    /// Printed under --nocapture, so CI logs show which kernels each runner
    /// really has. `ARC_BATCHED_KERNELS_OUT` receives the available labels,
    /// one per line, for the workflow's per-kernel runs. When the workflow
    /// derives `ARC_EXPECT_BATCHED_KERNELS` from the OS's CPU flags, every
    /// kernel those flags allow must also be detected here.
    #[test]
    fn batched_kernel_report() {
        let available: Vec<&str> = available_batched_kernels()
            .into_iter()
            .map(BatchedKernel::label)
            .collect();
        println!("cpu features detected: {:?}", detected_cpu_features());
        println!("batched kernels available: {available:?}");
        println!(
            "batched kernel selected automatically: {:?}",
            best_batched_kernel().map(BatchedKernel::label)
        );
        if let Ok(path) = std::env::var("ARC_BATCHED_KERNELS_OUT") {
            std::fs::write(&path, available.join("\n")).expect("write the kernel list");
        }
        if let Ok(expected) = std::env::var("ARC_EXPECT_BATCHED_KERNELS") {
            for label in expected
                .split(|c: char| c == ',' || c.is_whitespace())
                .filter(|label| !label.is_empty())
            {
                let kernel = BatchedKernel::from_label(label)
                    .unwrap_or_else(|| panic!("unknown kernel {label}"));
                assert!(
                    kernel.available(),
                    "the OS reports the CPU flags for {label}, but it was not detected"
                );
            }
        }
    }

    #[test]
    fn every_batched_kernel_matches_scalar_on_every_tile_shape() {
        let kernels = available_batched_kernels();
        if kernels.is_empty() {
            eprintln!("no batched kernel on this target; nothing to compare");
            return;
        }
        let mut seed = 0x2545_F491_4F6C_DD1Du64;
        let mut next = move || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        // Columns around every vector width (8, 16, 32 and 64) and two real
        // widths; rows around the 4-row tiles; tokens around the 2-token
        // pairs and 4-token tiles, up to 64.
        for cols in [
            1usize, 7, 8, 9, 15, 16, 17, 31, 32, 33, 63, 64, 65, 127, 128, 129, 4096, 4097,
        ] {
            for rows in [1usize, 2, 3, 4, 5, 9] {
                let mut w = weights_from(rows, cols, |_, _| (next() % 256) as u8 as i8);
                w.data[0] = -128;
                *w.data.last_mut().unwrap() = 127;
                for tokens in [1usize, 2, 3, 5, 8, 9, 17, 64] {
                    let inputs: Vec<i64> = (0..tokens * cols)
                        .map(|i| match (next() % 7, i % 3) {
                            (0, _) => LIMB_MIN,
                            (1, _) => LIMB_MAX,
                            (2, _) => 0,
                            (3, 0) => -129,
                            (3, _) => 65_536,
                            (4, _) => -(((next() % 8_388_608) as i64) + 1),
                            _ => ((next() % 8_388_608) as i64) + 1,
                        })
                        .collect();
                    let want = scalar_batched(&w, &inputs, tokens, cols);
                    for &kernel in &kernels {
                        let mut got = vec![SENTINEL; tokens * rows];
                        assert!(
                            matmul_i8_batched_with(kernel, &w, &inputs, tokens, cols, &mut got),
                            "{} refused cols={cols} rows={rows} tokens={tokens}",
                            kernel.label()
                        );
                        assert_eq!(
                            got,
                            want,
                            "{} cols={cols} rows={rows} tokens={tokens}",
                            kernel.label()
                        );
                    }
                }
            }
        }
    }

    /// The i32 lane bounds of the module docs, at the largest accepted inner
    /// dimension. Token 0 is all `LIMB_MIN` (every digit -128): against rows
    /// of -128 each SDOT/SMMLA lane reaches `+16_384 * K`, the bound itself.
    /// Token 1 is all `LIMB_MAX` (every digit 127, offset byte 255): against
    /// -128 it gives the largest negative VNNI products.
    #[test]
    fn every_batched_kernel_is_exact_at_the_i32_lane_bound() {
        let kernels = available_batched_kernels();
        if kernels.is_empty() {
            return;
        }
        let cols = MAX_COLS_FOR_I32;
        let rows = 5;
        let w = weights_from(rows, cols, |r, c| match r {
            0 | 1 => -128,
            2 => 127,
            _ if c % 2 == 0 => -128,
            _ => 127,
        });
        let tokens = 3;
        let inputs: Vec<i64> = (0..tokens * cols)
            .map(|i| match (i / cols, i % 2) {
                (0, _) => LIMB_MIN,
                (1, _) => LIMB_MAX,
                (_, 0) => LIMB_MAX,
                _ => LIMB_MIN,
            })
            .collect();
        let want = scalar_batched(&w, &inputs, tokens, cols);
        for kernel in kernels {
            let mut got = vec![SENTINEL; tokens * rows];
            assert!(
                matmul_i8_batched_with(kernel, &w, &inputs, tokens, cols, &mut got),
                "{} refused K = {cols}",
                kernel.label()
            );
            assert_eq!(got, want, "{} at K = {cols}", kernel.label());
        }
    }

    #[test]
    fn batched_kernel_selection_falls_back_to_the_best_available() {
        let _guard = kernel_switch_guard();
        let previous = batched_kernel_preference();
        let mut labels: Vec<&str> = BatchedKernel::ALL.map(BatchedKernel::label).to_vec();
        labels.sort_unstable();
        labels.dedup();
        assert_eq!(labels.len(), BatchedKernel::ALL.len(), "labels are unique");
        for kernel in BatchedKernel::ALL {
            assert_eq!(BatchedKernel::from_label(kernel.label()), Some(kernel));
        }
        assert_eq!(BatchedKernel::from_label("auto"), None);
        let best = best_batched_kernel();
        assert_eq!(best.is_some(), dotprod_available());
        for kernel in BatchedKernel::ALL {
            set_batched_kernel_preference(Some(kernel));
            let want = if kernel.available() {
                Some(kernel)
            } else {
                best
            };
            assert_eq!(selected_batched_kernel(), want, "{}", kernel.label());
        }
        set_batched_kernel_preference(None);
        assert_eq!(selected_batched_kernel(), best);
        set_batched_kernel_preference(previous);
    }

    #[test]
    fn a_kernel_this_cpu_lacks_is_refused_without_writing() {
        let Some(missing) = BatchedKernel::ALL.into_iter().find(|k| !k.available()) else {
            return;
        };
        let cols = 64usize;
        let w = weights_with_scale(4, cols, 1);
        let inputs = vec![7i64; 2 * cols];
        let mut out = vec![SENTINEL; 2 * 4];
        assert!(!matmul_i8_batched_with(
            missing, &w, &inputs, 2, cols, &mut out
        ));
        assert_untouched(&out, missing.label());
    }

    #[test]
    fn the_kernel_that_ran_is_recorded() {
        let cols = 96usize;
        let w = weights_from(8, cols, |r, c| (r * 31 + c * 7) as u8 as i8);
        let inputs: Vec<i64> = (0..3 * cols as i64).map(|i| i * 1_000 - 50_000).collect();
        for kernel in available_batched_kernels() {
            let before = batched_kernel_runs(kernel);
            let mut out = vec![0i64; 3 * 8];
            assert!(matmul_i8_batched_with(
                kernel, &w, &inputs, 3, cols, &mut out
            ));
            assert!(
                batched_kernel_runs(kernel) > before,
                "{} ran but was not recorded",
                kernel.label()
            );
        }
        if BatchedKernel::NeonI8mm.available() {
            // SMMLA pairs tokens, so one token runs on SDOT and says so.
            let before = batched_kernel_runs(BatchedKernel::NeonSdot);
            let mut out = vec![0i64; 8];
            assert!(matmul_i8_batched_with(
                BatchedKernel::NeonI8mm,
                &w,
                &inputs[..cols],
                1,
                cols,
                &mut out
            ));
            assert!(batched_kernel_runs(BatchedKernel::NeonSdot) > before);
        }
    }
}
