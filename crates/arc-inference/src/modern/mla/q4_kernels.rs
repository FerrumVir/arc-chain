//! Exact INT4 group-32 kernels for the routed experts (spec §13), opt-in.
//!
//! An expert projection of k tokens needs, for every row r, token t and
//! 32-column group g, the group dot `sum_{j in g} q_rj x_tj` that the scalar
//! reference forms exactly in i64. The group scales and the single floor are
//! then applied by the reference's own epilogue
//! (`ops::Q4RowSummary::finish`), so the output is byte-identical by
//! construction whenever the dots are.
//!
//! The kernels form those dots exactly with 8-bit dot-product instructions,
//! using the limb scheme of `canonical_simd` (#190):
//! * Every activation in the four-digit domain `[LIMB_MIN, LIMB_MAX]` is split
//!   into balanced base-256 digits, `x = sum_d 256^d c_d` with `c_d` in
//!   `[-128, 127]` (`canonical_simd::split_limbs`, which verifies every
//!   reconstruction). An activation outside the domain refuses the fast path,
//!   and the scalar reference runs instead.
//! * Each (token, digit plane) pair is an entity. Per row and group, a kernel
//!   forms the i32 sum `S = sum_{j in g} q_j c_j` for every entity, and the
//!   dot is `sum_d 256^d S_(t,d)` in i64.
//! * INT4 values are unpacked to bytes: `q in [-8, 7]` for the signed kernels
//!   (SDOT, SMMLA), `u = q + 8 in [0, 15]` (the nibble XOR 8) for the kernels
//!   whose instruction takes unsigned bytes (AVX2 `vpmaddubsw`, `vpdpbusd`).
//!   Those form `sum u c = S + 8 sum c`, and subtract `8 sum_{j in g} c`,
//!   computed once per call for every entity and group: the offset trick of
//!   #190's VNNI kernels, with the weights as the unsigned operand.
//!
//! Bounds, so every partial sum is exact:
//! * signed: `|q c| <= 8 * 128 = 1,024`, 32 terms per group, `|S| <= 32,768`;
//! * unsigned: `|u c| <= 15 * 128 = 1,920`. A `vpmaddubsw` pair sum is at most
//!   3,840, far below its 16-bit saturation, and a lane holds at most
//!   `32 * 1,920 = 61,440` before the correction;
//! * the dot: `|sum_d 256^d S_d| <= 32,768 * (1 + 2^8 + 2^16 + 2^24) < 2^40`.
//!
//! Layouts. Every lane accumulates one (row, entity) group sum, with no
//! horizontal reduction.
//! * x86-64 puts rows in the lanes, 8 (AVX2, AVX-VNNI) or 16 (AVX-512 VNNI)
//!   per vector, and broadcasts one entity's 4-byte digit quad against them,
//!   so every lane is busy whatever the token count. Each block of rows is
//!   unpacked and transposed once per group (an 8 x 8 transpose of 4-byte
//!   quads) and then serves every entity.
//! * SDOT (by element) puts entities in its 4 lanes and broadcasts the row's
//!   weight quad; SMMLA multiplies a pair of rows (8 columns each) by a pair of
//!   entities, as #190's tiles do. Rows in SDOT's lanes would need the same
//!   transpose, which costs about what the idle lane does at one token
//!   (three entities in four lanes), so arm64 keeps entities in lanes.
//!
//! The digit layouts depend on the activations only and are built once per
//! call.
//!
//! Selection. The kernels run only behind the existing opt-in
//! (`canonical_simd::fast_canonical_kernel_enabled`, `ARC_FAST_CANONICAL_KERNEL`).
//! The default is [`best_q4_kernel`]: SMMLA, then SDOT, on arm64; AVX2 on
//! x86-64, which no VNNI kernel has measured clearly faster at these shapes.
//! `ARC_MLA_Q4_KERNEL=<label>`, `=scalar` or [`set_q4_kernel_pin`] pins one
//! for tests and benchmarks, and a pinned kernel the CPU lacks falls back to
//! the default.
//! [`q4_kernel_runs`] counts the projections each kernel, and the scalar
//! reference, computed.

use std::sync::OnceLock;
use std::sync::atomic::{AtomicU8, AtomicU64, Ordering};

use crate::canonical_simd::{LIMB_COUNT, split_limbs};
use crate::modern::ModernError;

use super::ops::{Q4_GROUP, Q4View};

/// Columns per INT4 group.
const GROUP: usize = Q4_GROUP;
/// 4-column quads per group.
const QUADS: usize = GROUP / 4;
/// Packed bytes per group of one row.
const GROUP_BYTES: usize = GROUP / 2;
/// Rows per parallel task.
const ROW_CHUNK: usize = 16;

/// Largest group sum of a signed kernel: 32 products of at most 8 * 128.
const SIGNED_GROUP_MAX: i64 = (GROUP as i64) * 8 * 128;
/// Largest lane of an unsigned-weight kernel before its correction: 32
/// products of at most 15 * 128.
const UNSIGNED_GROUP_MAX: i64 = (GROUP as i64) * 15 * 128;

const _: () = assert!(GROUP == 32);
const _: () = assert!(SIGNED_GROUP_MAX <= i32::MAX as i64);
// Pair sums below the 16-bit saturation, and a lane and its correction
// (8 times a group's digit sum, at most the signed bound) in an i32.
const _: () = assert!(2 * 15 * 128 <= i16::MAX as i64);
const _: () = assert!(UNSIGNED_GROUP_MAX + SIGNED_GROUP_MAX <= i32::MAX as i64);
// The dot of a group over four planes, in i64.
const _: () =
    assert!((SIGNED_GROUP_MAX as i128) * (1 + 256 + 65_536 + 16_777_216) < i64::MAX as i128);

/// An exact INT4 expert kernel. Every kernel computes the same integers; they
/// differ only in the instruction that multiplies the bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Q4Kernel {
    /// ARM `SDOT` (FEAT_DotProd), by element: four entities per vector.
    NeonSdot,
    /// ARM `SMMLA` (FEAT_I8MM): two rows meet two entities per instruction.
    NeonI8mm,
    /// x86-64 AVX2 `vpmaddubsw` + `vpmaddwd`: eight entities per vector.
    Avx2,
    /// x86-64 AVX-VNNI 256-bit `vpdpbusd`: eight entities per vector.
    AvxVnni,
    /// x86-64 AVX-512 VNNI 512-bit `vpdpbusd`: sixteen entities per vector.
    Avx512Vnni,
}

impl Q4Kernel {
    /// Every kernel, in declaration order.
    pub const ALL: [Self; 5] = [
        Self::NeonSdot,
        Self::NeonI8mm,
        Self::Avx2,
        Self::AvxVnni,
        Self::Avx512Vnni,
    ];

    /// The kernel's name in logs, pins and bench reports.
    pub const fn label(self) -> &'static str {
        match self {
            Self::NeonSdot => "neon-sdot-q4",
            Self::NeonI8mm => "neon-i8mm-q4",
            Self::Avx2 => "avx2-q4",
            Self::AvxVnni => "avx-vnni-q4",
            Self::Avx512Vnni => "avx512-vnni-q4",
        }
    }

    /// The kernel with this [`Self::label`].
    pub fn from_label(label: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|kernel| kernel.label() == label)
    }

    /// Whether this build and CPU can run the kernel, detected at run time.
    pub fn available(self) -> bool {
        match self {
            #[cfg(target_arch = "aarch64")]
            Self::NeonSdot => std::arch::is_aarch64_feature_detected!("dotprod"),
            #[cfg(target_arch = "aarch64")]
            Self::NeonI8mm => std::arch::is_aarch64_feature_detected!("i8mm"),
            #[cfg(target_arch = "x86_64")]
            Self::Avx2 => std::arch::is_x86_feature_detected!("avx2"),
            #[cfg(target_arch = "x86_64")]
            Self::AvxVnni => {
                std::arch::is_x86_feature_detected!("avx2")
                    && std::arch::is_x86_feature_detected!("avxvnni")
            }
            #[cfg(target_arch = "x86_64")]
            Self::Avx512Vnni => {
                std::arch::is_x86_feature_detected!("avx512f")
                    && std::arch::is_x86_feature_detected!("avx512vnni")
            }
            _ => false,
        }
    }
}

/// Every INT4 kernel this CPU can run, in declaration order.
pub fn available_q4_kernels() -> Vec<Q4Kernel> {
    Q4Kernel::ALL
        .into_iter()
        .filter(|kernel| kernel.available())
        .collect()
}

/// The kernel used when nothing is pinned: on arm64, SMMLA, then SDOT; on
/// x86-64, AVX2, then the VNNI kernels. One K2.6-shaped MoE layer, in CI:
/// * With entities in the lanes, a one-row AVX-512 VNNI pass took 24% longer
///   than AVX2 on an EPYC 9V45 (run 38057646072) and 23% longer on a Xeon
///   Platinum 8370C (run 38058847095). AVX-VNNI took 3% longer on the 9V45.
/// * With rows in the lanes, AVX-512 VNNI and AVX2 are within 2% on an
///   EPYC 9V74 (run 38060375415): 44.2 and 43.6 ms one-row, 39.1 and 39.5 ms
///   per row at 16 rows. AVX-VNNI was not measured.
///
/// No VNNI kernel has measured clearly faster, so AVX2 stays first.
pub fn best_q4_kernel() -> Option<Q4Kernel> {
    [
        Q4Kernel::Avx2,
        Q4Kernel::AvxVnni,
        Q4Kernel::Avx512Vnni,
        Q4Kernel::NeonI8mm,
        Q4Kernel::NeonSdot,
    ]
    .into_iter()
    .find(|kernel| kernel.available())
}

/// What computes routed-expert projections while the opt-in is on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Q4Pin {
    /// The best kernel this CPU has.
    Auto,
    /// The scalar reference.
    Scalar,
    /// This kernel if the CPU has it, otherwise the best it has.
    Kernel(Q4Kernel),
}

/// 0: automatic; 1: scalar; otherwise the kernel's index + 2.
static PIN: AtomicU8 = AtomicU8::new(0);

/// `ARC_MLA_Q4_KERNEL=<label>|scalar|auto` sets the initial pin once. An
/// explicit [`set_q4_kernel_pin`] call always wins.
fn apply_env() {
    static ENV: OnceLock<()> = OnceLock::new();
    ENV.get_or_init(|| {
        let Ok(value) = std::env::var("ARC_MLA_Q4_KERNEL") else {
            return;
        };
        let value = value.trim();
        match (value, Q4Kernel::from_label(value)) {
            (_, Some(kernel)) => PIN.store(kernel as u8 + 2, Ordering::Relaxed),
            ("scalar", None) => PIN.store(1, Ordering::Relaxed),
            ("" | "auto", None) => {}
            (_, None) => eprintln!(
                "ARC_MLA_Q4_KERNEL={value:?} names no INT4 kernel; selecting automatically"
            ),
        }
    });
}

/// Pin what computes routed-expert projections. This never enables the fast
/// path, which stays behind the existing opt-in.
pub fn set_q4_kernel_pin(pin: Q4Pin) {
    apply_env();
    let slot = match pin {
        Q4Pin::Auto => 0,
        Q4Pin::Scalar => 1,
        Q4Pin::Kernel(kernel) => kernel as u8 + 2,
    };
    PIN.store(slot, Ordering::Relaxed);
}

/// The current pin.
pub fn q4_kernel_pin() -> Q4Pin {
    apply_env();
    match PIN.load(Ordering::Relaxed) {
        0 => Q4Pin::Auto,
        1 => Q4Pin::Scalar,
        slot => Q4Kernel::ALL
            .get(usize::from(slot) - 2)
            .map_or(Q4Pin::Auto, |&kernel| Q4Pin::Kernel(kernel)),
    }
}

/// The kernel the next projection uses, or `None` for the scalar reference:
/// scalar unless the opt-in is on; then the pin, as [`Q4Pin`] describes.
pub fn selected_q4_kernel() -> Option<Q4Kernel> {
    if !crate::canonical_simd::fast_canonical_kernel_enabled() {
        return None;
    }
    match q4_kernel_pin() {
        Q4Pin::Scalar => None,
        Q4Pin::Kernel(kernel) if kernel.available() => Some(kernel),
        _ => best_q4_kernel(),
    }
}

/// Projections computed per kernel, the scalar reference last.
static RUNS: [AtomicU64; 6] = [const { AtomicU64::new(0) }; 6];
/// Projections a kernel declined (an activation outside the digit domain).
static REFUSED: AtomicU64 = AtomicU64::new(0);

fn run_slot(kernel: Option<Q4Kernel>) -> usize {
    kernel.map_or(5, |kernel| kernel as usize)
}

/// Count one projection computed by `kernel` (`None`: the scalar reference).
pub(crate) fn record_run(kernel: Option<Q4Kernel>) {
    RUNS[run_slot(kernel)].fetch_add(1, Ordering::Relaxed);
}

/// Count one projection a kernel declined.
pub(crate) fn record_refusal() {
    REFUSED.fetch_add(1, Ordering::Relaxed);
}

/// Projections `kernel` (`None`: the scalar reference) has computed in this
/// process, one per call whatever its token count.
pub fn q4_kernel_runs(kernel: Option<Q4Kernel>) -> u64 {
    RUNS[run_slot(kernel)].load(Ordering::Relaxed)
}

/// Projections a kernel declined, running the scalar reference instead.
pub fn q4_kernel_refusals() -> u64 {
    REFUSED.load(Ordering::Relaxed)
}

/// `label=count` for the scalar reference and every kernel that is available
/// or has run, then the refusals.
pub fn q4_kernel_run_report() -> String {
    let mut parts = vec![format!("scalar-q4={}", q4_kernel_runs(None))];
    for kernel in Q4Kernel::ALL {
        if kernel.available() || q4_kernel_runs(Some(kernel)) > 0 {
            parts.push(format!(
                "{}={}",
                kernel.label(),
                q4_kernel_runs(Some(kernel))
            ));
        }
    }
    parts.push(format!("refused={}", q4_kernel_refusals()));
    parts.join(" ")
}

/// The balanced base-256 digits of k activation vectors.
pub(crate) struct Digits {
    tokens: usize,
    /// Digit planes in use: the most any token needs.
    planes: usize,
    cols: usize,
    /// `[token][plane][column]`, `LIMB_COUNT` planes per token.
    raw: Vec<i8>,
}

impl Digits {
    /// The digits of `xs` (every vector the same length), or `None` if an
    /// activation is outside the four-digit domain.
    pub(crate) fn split(xs: &[&[i64]]) -> Option<Self> {
        let cols = xs.first()?.len();
        if cols == 0 || xs.iter().any(|x| x.len() != cols) {
            return None;
        }
        let mut raw = vec![0i8; xs.len() * LIMB_COUNT * cols];
        let mut planes = 1;
        for (x, limbs) in xs.iter().zip(raw.chunks_exact_mut(LIMB_COUNT * cols)) {
            planes = planes.max(split_limbs(x, limbs)?);
        }
        Some(Self {
            tokens: xs.len(),
            planes,
            cols,
            raw,
        })
    }

    /// Entities: (token, plane) pairs, entity `t * planes + d`.
    fn entities(&self) -> usize {
        self.tokens * self.planes
    }

    /// Entity `e`'s digits, one per column.
    fn entity(&self, e: usize) -> &[i8] {
        let (t, d) = (e / self.planes, e % self.planes);
        &self.raw[(t * LIMB_COUNT + d) * self.cols..(t * LIMB_COUNT + d + 1) * self.cols]
    }

    /// Entities in lanes, `width` per block: for group g, quad q and block b,
    /// `width * 4` bytes at `((g * QUADS + q) * blocks + b) * width * 4`, lane
    /// l holding entity `b * width + l`'s digits of columns `32g + 4q ..
    /// 32g + 4q + 4` (zero past the last entity). Returns the bytes and the
    /// block count.
    #[cfg(target_arch = "aarch64")]
    fn lanes(&self, width: usize) -> (Vec<i8>, usize) {
        let groups = self.cols / GROUP;
        let blocks = self.entities().div_ceil(width);
        let mut out = vec![0i8; groups * QUADS * blocks * width * 4];
        for e in 0..self.entities() {
            let (b, l) = (e / width, e % width);
            let digits = self.entity(e);
            for g in 0..groups {
                for q in 0..QUADS {
                    let at = (((g * QUADS + q) * blocks + b) * width + l) * 4;
                    let from = g * GROUP + 4 * q;
                    out[at..at + 4].copy_from_slice(&digits[from..from + 4]);
                }
            }
        }
        (out, blocks)
    }

    /// The x86 kernels' operands: each entity's digits as one little-endian
    /// i32 per 4-column quad, at `(g * E + e) * QUADS + q`, and the offset
    /// corrections `8 * sum_{j in g} c_ej` at `g * E + e` (E entities).
    #[cfg(target_arch = "x86_64")]
    fn quad_words(&self) -> (Vec<i32>, Vec<i32>) {
        let (groups, entities) = (self.cols / GROUP, self.entities());
        let mut quads = vec![0i32; groups * entities * QUADS];
        let mut offsets = vec![0i32; groups * entities];
        for e in 0..entities {
            for (g, group) in self.entity(e).chunks_exact(GROUP).enumerate() {
                offsets[g * entities + e] = 8 * group.iter().map(|&c| i32::from(c)).sum::<i32>();
                for (q, quad) in group.chunks_exact(4).enumerate() {
                    quads[(g * entities + e) * QUADS + q] = i32::from_le_bytes([
                        quad[0] as u8,
                        quad[1] as u8,
                        quad[2] as u8,
                        quad[3] as u8,
                    ]);
                }
            }
        }
        (quads, offsets)
    }

    /// Entity pairs for SMMLA: for pair p, group g and 8-column block k, 16
    /// bytes at `((p * groups + g) * 4 + k) * 16`, entity 2p's eight digits
    /// then entity 2p + 1's (zero past the last entity). Returns the bytes and
    /// the pair count.
    #[cfg(target_arch = "aarch64")]
    fn pairs(&self) -> (Vec<i8>, usize) {
        let groups = self.cols / GROUP;
        let pairs = self.entities().div_ceil(2);
        let mut out = vec![0i8; pairs * groups * 4 * 16];
        for e in 0..self.entities() {
            let (p, h) = (e / 2, e % 2);
            let digits = self.entity(e);
            for g in 0..groups {
                for k in 0..4 {
                    let at = ((p * groups + g) * 4 + k) * 16 + h * 8;
                    let from = g * GROUP + 8 * k;
                    out[at..at + 8].copy_from_slice(&digits[from..from + 8]);
                }
            }
        }
        (out, pairs)
    }
}

/// `dots[t * groups + g] = sum_d 256^d s[g * stride + t * planes + d]`.
#[cfg(target_arch = "aarch64")]
fn fold_planes(s: &[i32], stride: usize, digits: &Digits, groups: usize, dots: &mut [i64]) {
    for t in 0..digits.tokens {
        for g in 0..groups {
            let base = g * stride + t * digits.planes;
            let mut dot = 0i64;
            for d in 0..digits.planes {
                // Multiplication, not `<<`, as in canonical_simd.
                dot += i64::from(s[base + d]) * (1i64 << (8 * d));
            }
            dots[t * groups + g] = dot;
        }
    }
}

/// One row's group sums for every entity block: `(packed row, lane digits,
/// blocks, sums)`.
#[cfg(target_arch = "aarch64")]
type LaneKernel = fn(&[u8], &[i8], usize, &mut [i32]);

/// Every row of `view` against the k tokens of `digits` with `kernel`: for
/// each row r, `finish(r, dots, out)` receives the group dots
/// (`dots[t * groups + g]`) and writes the row's k outputs
/// (`res[r * k .. r * k + k]`). Errors are `finish`'s; the scalar reference
/// returns the same ones.
///
/// The caller selected `kernel` with [`selected_q4_kernel`], so the CPU has
/// it; an unavailable kernel is refused without writing.
pub(crate) fn project_tokens<F>(
    kernel: Q4Kernel,
    view: &Q4View<'_>,
    digits: &Digits,
    res: &mut [i64],
    finish: F,
) -> Result<(), ModernError>
where
    F: Fn(usize, &[i64], &mut [i64]) -> Result<(), ModernError> + Sync,
{
    if !kernel.available() || view.cols != digits.cols || res.len() != view.rows * digits.tokens {
        return Err(ModernError::Invalid(format!(
            "INT4 kernel {} cannot run this projection",
            kernel.label()
        )));
    }
    match kernel {
        #[cfg(target_arch = "aarch64")]
        Q4Kernel::NeonSdot => lanes(view, digits, res, &finish, 4, neon::row_sdot),
        #[cfg(target_arch = "aarch64")]
        Q4Kernel::NeonI8mm => neon::pairs_i8mm(view, digits, res, &finish),
        #[cfg(target_arch = "x86_64")]
        Q4Kernel::Avx2 => x86::row_blocks(view, digits, res, &finish, 8, x86::block_avx2),
        #[cfg(target_arch = "x86_64")]
        Q4Kernel::AvxVnni => x86::row_blocks(view, digits, res, &finish, 8, x86::block_vnni256),
        #[cfg(target_arch = "x86_64")]
        Q4Kernel::Avx512Vnni => x86::row_blocks(view, digits, res, &finish, 16, x86::block_vnni512),
        #[allow(unreachable_patterns)]
        _ => Err(ModernError::Invalid(format!(
            "INT4 kernel {} is not built for this architecture",
            kernel.label()
        ))),
    }
}

/// The arm64 lane-kernel driver: rows in parallel chunks, entities in
/// blocks of `width` lanes, the row kernel `row` on each row.
#[cfg(target_arch = "aarch64")]
fn lanes<F>(
    view: &Q4View<'_>,
    digits: &Digits,
    res: &mut [i64],
    finish: &F,
    width: usize,
    row: LaneKernel,
) -> Result<(), ModernError>
where
    F: Fn(usize, &[i64], &mut [i64]) -> Result<(), ModernError> + Sync,
{
    use rayon::prelude::*;
    let (k, groups, half) = (digits.tokens, view.cols / GROUP, view.cols / 2);
    let (layout, blocks) = digits.lanes(width);
    let stride = blocks * width;
    res.par_chunks_mut(ROW_CHUNK * k)
        .enumerate()
        .try_for_each(|(chunk, out)| {
            let mut s = vec![0i32; groups * stride];
            let mut dots = vec![0i64; k * groups];
            for (i, row_out) in out.chunks_mut(k).enumerate() {
                let r = chunk * ROW_CHUNK + i;
                row(&view.q4[r * half..(r + 1) * half], &layout, blocks, &mut s);
                fold_planes(&s, stride, digits, groups, &mut dots);
                finish(r, &dots, row_out)?;
            }
            Ok(())
        })
}

#[cfg(target_arch = "aarch64")]
mod neon {
    use std::arch::aarch64::*;

    use super::{Digits, GROUP, GROUP_BYTES, QUADS, ROW_CHUNK, fold_planes};
    use crate::modern::ModernError;
    use crate::modern::mla::ops::Q4View;

    /// The 32 signed INT4 values of one packed group: columns 0-15 and 16-31.
    ///
    /// # Safety
    /// NEON is available; `packed` is valid for 16 reads.
    #[inline]
    #[target_feature(enable = "neon")]
    unsafe fn unpack_signed(packed: *const u8) -> (int8x16_t, int8x16_t) {
        // SAFETY: the caller's contract; the rest are register operations.
        unsafe {
            let p = vld1q_u8(packed);
            let low = vandq_u8(p, vdupq_n_u8(0x0F));
            let high = vshrq_n_u8::<4>(p);
            // Column 2i is byte i's low nibble, column 2i + 1 its high one.
            let first = vzip1q_u8(low, high);
            let second = vzip2q_u8(low, high);
            // Sign: (v ^ 8) - 8 maps a nibble v to v - 16 when v >= 8.
            let eight = vdupq_n_u8(8);
            (
                vreinterpretq_s8_u8(vsubq_u8(veorq_u8(first, eight), eight)),
                vreinterpretq_s8_u8(vsubq_u8(veorq_u8(second, eight), eight)),
            )
        }
    }

    macro_rules! sdot_lane {
        ($name:ident, $index:literal) => {
            /// `SDOT Vd.4S, Vn.16B, Vm.4B[index]` (FEAT_DotProd): each 32-bit
            /// lane of `acc` gains the dot of its four bytes of `digits` with
            /// the four bytes `index` of `weights`. Emitted with inline
            /// assembly, as `canonical_simd::sdot` is (the intrinsics are
            /// unstable on the pinned toolchain).
            ///
            /// # Safety
            /// The `dotprod` target feature is available.
            #[inline]
            #[target_feature(enable = "neon,dotprod")]
            unsafe fn $name(acc: int32x4_t, digits: int8x16_t, weights: int8x16_t) -> int32x4_t {
                // SAFETY: register operands only; no memory, no side effects.
                unsafe {
                    let mut out = acc;
                    std::arch::asm!(
                        concat!("sdot {o:v}.4s, {a:v}.16b, {b:v}.4b[", $index, "]"),
                        o = inout(vreg) out,
                        a = in(vreg) digits,
                        b = in(vreg) weights,
                        options(pure, nomem, nostack)
                    );
                    out
                }
            }
        };
    }

    sdot_lane!(sdot_lane0, "0");
    sdot_lane!(sdot_lane1, "1");
    sdot_lane!(sdot_lane2, "2");
    sdot_lane!(sdot_lane3, "3");

    /// One row's group sums, four entities per block, with by-element SDOT:
    /// `s[g * stride + b * 4 + l]` for entity `4b + l`, `stride = 4 * blocks`.
    /// No offset: the weights are signed.
    pub(super) fn row_sdot(packed: &[u8], digits: &[i8], blocks: usize, s: &mut [i32]) {
        let groups = packed.len() / GROUP_BYTES;
        let stride = blocks * 4;
        assert!(
            digits.len() >= groups * QUADS * stride * 4 && s.len() >= groups * stride,
            "SDOT row kernel: short digit or sum buffer"
        );
        // SAFETY: `project_tokens` runs this kernel only when the CPU has
        // dotprod. Packed loads read 16 bytes at `16g < packed.len()`; digit
        // loads read 16 bytes at `((g * 8 + q) * blocks + b) * 16`, inside
        // `groups * 8 * blocks * 16` bytes; stores write 4 lanes at
        // `g * stride + 4b`, inside `groups * stride` (all checked above).
        unsafe { row_sdot_inner(packed, digits, blocks, s, groups, stride) }
    }

    #[target_feature(enable = "neon,dotprod")]
    unsafe fn row_sdot_inner(
        packed: &[u8],
        digits: &[i8],
        blocks: usize,
        s: &mut [i32],
        groups: usize,
        stride: usize,
    ) {
        // SAFETY: see `row_sdot`.
        unsafe {
            let step = blocks * 16;
            for g in 0..groups {
                let (w0, w1) = unpack_signed(packed.as_ptr().add(g * GROUP_BYTES));
                for b in 0..blocks {
                    let base = digits.as_ptr().add((g * QUADS * blocks + b) * 16);
                    let mut acc = vdupq_n_s32(0);
                    acc = sdot_lane0(acc, vld1q_s8(base), w0);
                    acc = sdot_lane1(acc, vld1q_s8(base.add(step)), w0);
                    acc = sdot_lane2(acc, vld1q_s8(base.add(2 * step)), w0);
                    acc = sdot_lane3(acc, vld1q_s8(base.add(3 * step)), w0);
                    acc = sdot_lane0(acc, vld1q_s8(base.add(4 * step)), w1);
                    acc = sdot_lane1(acc, vld1q_s8(base.add(5 * step)), w1);
                    acc = sdot_lane2(acc, vld1q_s8(base.add(6 * step)), w1);
                    acc = sdot_lane3(acc, vld1q_s8(base.add(7 * step)), w1);
                    vst1q_s32(s.as_mut_ptr().add(g * stride + b * 4), acc);
                }
            }
        }
    }

    /// One `SMMLA Vd.4S, Vn.16B, Vm.16B` (FEAT_I8MM): `Vn` holds two rows of
    /// eight signed bytes, `Vm` two entities of eight, and the 2x2 product is
    /// added to the lanes `[r0.e0, r0.e1, r1.e0, r1.e1]`. Inline assembly, as
    /// in `canonical_simd`.
    ///
    /// # Safety
    /// The `i8mm` target feature is available.
    #[inline]
    #[target_feature(enable = "neon,i8mm")]
    unsafe fn smmla(acc: int32x4_t, rows: int8x16_t, entities: int8x16_t) -> int32x4_t {
        // SAFETY: register operands only; no memory, no side effects.
        unsafe {
            let mut out = acc;
            std::arch::asm!(
                "smmla {o:v}.4s, {a:v}.16b, {b:v}.16b",
                o = inout(vreg) out,
                a = in(vreg) rows,
                b = in(vreg) entities,
                options(pure, nomem, nostack)
            );
            out
        }
    }

    /// Two rows' group sums for every entity pair: `sa`/`sb[g * stride + e]`,
    /// `stride = 2 * pairs`.
    ///
    /// # Safety
    /// The `i8mm` target feature is available; `pa` and `pb` hold `16 *
    /// groups` bytes, `digits` `pairs * groups * 64` and each sum buffer
    /// `groups * stride` values.
    #[target_feature(enable = "neon,i8mm")]
    unsafe fn rows_i8mm(
        pa: &[u8],
        pb: &[u8],
        digits: &[i8],
        pairs: usize,
        groups: usize,
        sa: &mut [i32],
        sb: &mut [i32],
    ) {
        // SAFETY: the caller's contract bounds every load and store below.
        unsafe {
            let stride = 2 * pairs;
            let zip = |x: int8x16_t, y: int8x16_t, second: bool| {
                let (x, y) = (vreinterpretq_s64_s8(x), vreinterpretq_s64_s8(y));
                vreinterpretq_s8_s64(if second {
                    vzip2q_s64(x, y)
                } else {
                    vzip1q_s64(x, y)
                })
            };
            for g in 0..groups {
                let (a0, a1) = unpack_signed(pa.as_ptr().add(g * GROUP_BYTES));
                let (b0, b1) = unpack_signed(pb.as_ptr().add(g * GROUP_BYTES));
                // 8-column blocks of the row pair: columns 0-7, 8-15, 16-23, 24-31.
                let blocks = [
                    zip(a0, b0, false),
                    zip(a0, b0, true),
                    zip(a1, b1, false),
                    zip(a1, b1, true),
                ];
                for p in 0..pairs {
                    let base = digits.as_ptr().add((p * groups + g) * 64);
                    let mut acc = vdupq_n_s32(0);
                    acc = smmla(acc, blocks[0], vld1q_s8(base));
                    acc = smmla(acc, blocks[1], vld1q_s8(base.add(16)));
                    acc = smmla(acc, blocks[2], vld1q_s8(base.add(32)));
                    acc = smmla(acc, blocks[3], vld1q_s8(base.add(48)));
                    let mut lanes = [0i32; 4];
                    vst1q_s32(lanes.as_mut_ptr(), acc);
                    let at = g * stride + 2 * p;
                    sa[at] = lanes[0];
                    sa[at + 1] = lanes[1];
                    sb[at] = lanes[2];
                    sb[at + 1] = lanes[3];
                }
            }
        }
    }

    /// The SMMLA driver: row pairs in parallel chunks (a chunk's odd last row
    /// pairs with itself), entity pairs per instruction.
    pub(super) fn pairs_i8mm<F>(
        view: &Q4View<'_>,
        digits: &Digits,
        res: &mut [i64],
        finish: &F,
    ) -> Result<(), ModernError>
    where
        F: Fn(usize, &[i64], &mut [i64]) -> Result<(), ModernError> + Sync,
    {
        use rayon::prelude::*;
        let (k, groups, half) = (digits.tokens, view.cols / GROUP, view.cols / 2);
        let (layout, pairs) = digits.pairs();
        let stride = 2 * pairs;
        res.par_chunks_mut(ROW_CHUNK * k)
            .enumerate()
            .try_for_each(|(chunk, out)| {
                let mut sa = vec![0i32; groups * stride];
                let mut sb = vec![0i32; groups * stride];
                let mut dots = vec![0i64; k * groups];
                let rows = out.len() / k;
                let first = chunk * ROW_CHUNK;
                for i in (0..rows).step_by(2) {
                    let (ra, rb) = (first + i, first + (i + 1).min(rows - 1));
                    // SAFETY: `project_tokens` runs this kernel only when the
                    // CPU has i8mm; each packed row is `half = 16 * groups`
                    // bytes, the layout `pairs * groups * 64` and each sum
                    // buffer `groups * stride` (built above).
                    unsafe {
                        rows_i8mm(
                            &view.q4[ra * half..(ra + 1) * half],
                            &view.q4[rb * half..(rb + 1) * half],
                            &layout,
                            pairs,
                            groups,
                            &mut sa,
                            &mut sb,
                        );
                    }
                    fold_planes(&sa, stride, digits, groups, &mut dots);
                    finish(ra, &dots, &mut out[i * k..(i + 1) * k])?;
                    if rb != ra {
                        fold_planes(&sb, stride, digits, groups, &mut dots);
                        finish(rb, &dots, &mut out[(i + 1) * k..(i + 2) * k])?;
                    }
                }
                Ok(())
            })
    }
}

#[cfg(target_arch = "x86_64")]
mod x86 {
    use std::arch::x86_64::*;

    use rayon::prelude::*;

    use super::{Digits, GROUP, GROUP_BYTES, QUADS, ROW_CHUNK};
    use crate::modern::ModernError;
    use crate::modern::mla::ops::Q4View;

    /// One block of rows in lanes: `(packed rows, digit quads, offsets,
    /// entities, sums)`, the sums at `s[(g * E + e) * width + i]` for row i.
    pub(super) type BlockKernel = fn(&[&[u8]], &[i32], &[i32], usize, &mut [i32]);

    /// The x86-64 driver. Rows sit in the lanes, `width` per block, and each
    /// entity's 4-byte digit quad is broadcast against them, so every lane
    /// does useful work whatever the token count. Blocks run in parallel
    /// chunks; a block past a chunk's last row repeats that row and its extra
    /// lanes are dropped.
    pub(super) fn row_blocks<F>(
        view: &Q4View<'_>,
        digits: &Digits,
        res: &mut [i64],
        finish: &F,
        width: usize,
        kernel: BlockKernel,
    ) -> Result<(), ModernError>
    where
        F: Fn(usize, &[i64], &mut [i64]) -> Result<(), ModernError> + Sync,
    {
        let (k, groups, half) = (digits.tokens, view.cols / GROUP, view.cols / 2);
        let entities = digits.entities();
        let (quads, offsets) = digits.quad_words();
        // The sum buffer grows with the entities (688 KiB at 16 tokens and
        // 16 rows per block), so each worker allocates its buffers once.
        res.par_chunks_mut(ROW_CHUNK * k)
            .enumerate()
            .try_for_each_init(
                || {
                    (
                        vec![0i32; groups * entities * width],
                        vec![0i64; k * groups],
                    )
                },
                |(s, dots), (chunk, out)| {
                    let count = out.len() / k;
                    let first = chunk * ROW_CHUNK;
                    let mut block: [&[u8]; 16] = [&[]; 16];
                    for start in (0..count).step_by(width) {
                        for (i, slot) in block.iter_mut().take(width).enumerate() {
                            let r = first + (start + i).min(count - 1);
                            *slot = &view.q4[r * half..(r + 1) * half];
                        }
                        kernel(&block[..width], &quads, &offsets, entities, s);
                        for i in 0..width.min(count - start) {
                            fold_block(s, width, i, digits, groups, dots);
                            let row = start + i;
                            finish(first + row, dots, &mut out[row * k..(row + 1) * k])?;
                        }
                    }
                    Ok(())
                },
            )
    }

    /// Row i of a block: `dots[t * groups + g] = sum_d 256^d s[(g * E + t *
    /// planes + d) * width + i]`.
    fn fold_block(
        s: &[i32],
        width: usize,
        i: usize,
        digits: &Digits,
        groups: usize,
        dots: &mut [i64],
    ) {
        let entities = digits.entities();
        for t in 0..digits.tokens {
            for g in 0..groups {
                let base = g * entities + t * digits.planes;
                let mut dot = 0i64;
                for d in 0..digits.planes {
                    // Multiplication, not `<<`, as in canonical_simd.
                    dot += i64::from(s[(base + d) * width + i]) * (1i64 << (8 * d));
                }
                dots[t * groups + g] = dot;
            }
        }
    }

    /// One packed group of a row as 32 unsigned bytes `q + 8` (the nibble
    /// XOR 8), byte j holding column j: columns 0-15 in the low 128 bits,
    /// so 32-bit lane q holds quad q.
    ///
    /// # Safety
    /// AVX2 is available; `packed` is valid for 16 reads.
    #[inline]
    #[target_feature(enable = "avx2")]
    unsafe fn unpack_row(packed: *const u8) -> __m256i {
        // SAFETY: the caller's contract; the rest are register operations.
        unsafe {
            // Byte i of the group in 16-bit lane i: its low nibble stays in
            // the lane's low byte (column 2i), its high nibble moves to the
            // high byte (column 2i + 1).
            let wide = _mm256_cvtepu8_epi16(_mm_loadu_si128(packed.cast()));
            let low = _mm256_and_si256(wide, _mm256_set1_epi16(0x0F));
            let high = _mm256_slli_epi16::<4>(_mm256_and_si256(wide, _mm256_set1_epi16(0xF0)));
            _mm256_xor_si256(_mm256_or_si256(low, high), _mm256_set1_epi8(8))
        }
    }

    /// Eight rows of eight 4-byte quads to eight quads of eight rows: lane i
    /// of output q is row i's quad q.
    #[inline]
    #[target_feature(enable = "avx2")]
    #[allow(unused_unsafe)]
    unsafe fn transpose8(r: [__m256i; 8]) -> [__m256i; 8] {
        // SAFETY: register operations only.
        unsafe {
            // Per 128-bit half: quads 0-3 in the low half, 4-7 in the high.
            let t0 = _mm256_unpacklo_epi32(r[0], r[1]);
            let t1 = _mm256_unpackhi_epi32(r[0], r[1]);
            let t2 = _mm256_unpacklo_epi32(r[2], r[3]);
            let t3 = _mm256_unpackhi_epi32(r[2], r[3]);
            let t4 = _mm256_unpacklo_epi32(r[4], r[5]);
            let t5 = _mm256_unpackhi_epi32(r[4], r[5]);
            let t6 = _mm256_unpacklo_epi32(r[6], r[7]);
            let t7 = _mm256_unpackhi_epi32(r[6], r[7]);
            // Rows 0-3 (u0..u3) and 4-7 (u4..u7) of quads q | q + 4.
            let u0 = _mm256_unpacklo_epi64(t0, t2);
            let u1 = _mm256_unpackhi_epi64(t0, t2);
            let u2 = _mm256_unpacklo_epi64(t1, t3);
            let u3 = _mm256_unpackhi_epi64(t1, t3);
            let u4 = _mm256_unpacklo_epi64(t4, t6);
            let u5 = _mm256_unpackhi_epi64(t4, t6);
            let u6 = _mm256_unpacklo_epi64(t5, t7);
            let u7 = _mm256_unpackhi_epi64(t5, t7);
            [
                _mm256_permute2x128_si256::<0x20>(u0, u4),
                _mm256_permute2x128_si256::<0x20>(u1, u5),
                _mm256_permute2x128_si256::<0x20>(u2, u6),
                _mm256_permute2x128_si256::<0x20>(u3, u7),
                _mm256_permute2x128_si256::<0x31>(u0, u4),
                _mm256_permute2x128_si256::<0x31>(u1, u5),
                _mm256_permute2x128_si256::<0x31>(u2, u6),
                _mm256_permute2x128_si256::<0x31>(u3, u7),
            ]
        }
    }

    /// The quads of group `g` of eight rows, rows in lanes.
    ///
    /// # Safety
    /// AVX2 is available; every row holds group `g` (16 bytes at `16g`).
    #[inline]
    #[target_feature(enable = "avx2")]
    unsafe fn group_of_eight(rows: &[&[u8]], g: usize) -> [__m256i; 8] {
        // SAFETY: the caller's contract.
        unsafe {
            let mut unpacked = [_mm256_setzero_si256(); 8];
            for (slot, row) in unpacked.iter_mut().zip(rows) {
                *slot = unpack_row(row.as_ptr().add(g * GROUP_BYTES));
            }
            transpose8(unpacked)
        }
    }

    /// Checks shared by the block kernels: `width` rows of the same length,
    /// and buffers for every group and entity.
    fn check(
        rows: &[&[u8]],
        quads: &[i32],
        offsets: &[i32],
        entities: usize,
        s: &[i32],
        width: usize,
    ) -> usize {
        let groups = rows.first().map_or(0, |row| row.len() / GROUP_BYTES);
        assert!(
            rows.len() == width
                && rows.iter().all(|row| row.len() == groups * GROUP_BYTES)
                && quads.len() >= groups * entities * QUADS
                && offsets.len() >= groups * entities
                && s.len() >= groups * entities * width,
            "x86 INT4 block kernel: bad block or short buffer"
        );
        groups
    }

    /// Eight rows per block, AVX2: `vpmaddubsw` (unsigned weights by signed
    /// digits, exact pair sums), `vpmaddwd` by ones, then the offset sums
    /// subtracted.
    pub(super) fn block_avx2(
        rows: &[&[u8]],
        quads: &[i32],
        offsets: &[i32],
        entities: usize,
        s: &mut [i32],
    ) {
        let groups = check(rows, quads, offsets, entities, s, 8);
        // SAFETY: `project_tokens` runs this kernel only when the CPU has
        // AVX2; `check` bounded every access.
        unsafe { block_avx2_inner(rows, quads, offsets, entities, s, groups) }
    }

    #[target_feature(enable = "avx2")]
    unsafe fn block_avx2_inner(
        rows: &[&[u8]],
        quads: &[i32],
        offsets: &[i32],
        entities: usize,
        s: &mut [i32],
        groups: usize,
    ) {
        // SAFETY: see `block_avx2`.
        unsafe {
            let ones = _mm256_set1_epi16(1);
            for g in 0..groups {
                let w = group_of_eight(rows, g);
                for e in 0..entities {
                    let base = (g * entities + e) * QUADS;
                    let mut acc = _mm256_setzero_si256();
                    for (q, &wq) in w.iter().enumerate() {
                        let c = _mm256_set1_epi32(quads[base + q]);
                        acc = _mm256_add_epi32(
                            acc,
                            _mm256_madd_epi16(_mm256_maddubs_epi16(wq, c), ones),
                        );
                    }
                    let acc = _mm256_sub_epi32(acc, _mm256_set1_epi32(offsets[g * entities + e]));
                    _mm256_storeu_si256(s.as_mut_ptr().add((g * entities + e) * 8).cast(), acc);
                }
            }
        }
    }

    /// As [`block_avx2`] with the 256-bit `vpdpbusd` of AVX-VNNI.
    pub(super) fn block_vnni256(
        rows: &[&[u8]],
        quads: &[i32],
        offsets: &[i32],
        entities: usize,
        s: &mut [i32],
    ) {
        let groups = check(rows, quads, offsets, entities, s, 8);
        // SAFETY: `project_tokens` runs this kernel only when the CPU has
        // AVX2 and AVX-VNNI; `check` bounded every access.
        unsafe { block_vnni256_inner(rows, quads, offsets, entities, s, groups) }
    }

    #[target_feature(enable = "avx2,avxvnni")]
    unsafe fn block_vnni256_inner(
        rows: &[&[u8]],
        quads: &[i32],
        offsets: &[i32],
        entities: usize,
        s: &mut [i32],
        groups: usize,
    ) {
        // SAFETY: see `block_vnni256`.
        unsafe {
            for g in 0..groups {
                let w = group_of_eight(rows, g);
                for e in 0..entities {
                    let base = (g * entities + e) * QUADS;
                    let mut acc = _mm256_setzero_si256();
                    for (q, &wq) in w.iter().enumerate() {
                        acc = _mm256_dpbusd_avx_epi32(acc, wq, _mm256_set1_epi32(quads[base + q]));
                    }
                    let acc = _mm256_sub_epi32(acc, _mm256_set1_epi32(offsets[g * entities + e]));
                    _mm256_storeu_si256(s.as_mut_ptr().add((g * entities + e) * 8).cast(), acc);
                }
            }
        }
    }

    /// Sixteen rows per block, with the 512-bit `vpdpbusd` of AVX-512 VNNI.
    pub(super) fn block_vnni512(
        rows: &[&[u8]],
        quads: &[i32],
        offsets: &[i32],
        entities: usize,
        s: &mut [i32],
    ) {
        let groups = check(rows, quads, offsets, entities, s, 16);
        // SAFETY: `project_tokens` runs this kernel only when the CPU has
        // AVX-512F and AVX-512 VNNI; `check` bounded every access.
        unsafe { block_vnni512_inner(rows, quads, offsets, entities, s, groups) }
    }

    #[target_feature(enable = "avx512f,avx512vnni")]
    unsafe fn block_vnni512_inner(
        rows: &[&[u8]],
        quads: &[i32],
        offsets: &[i32],
        entities: usize,
        s: &mut [i32],
        groups: usize,
    ) {
        // SAFETY: see `block_vnni512`.
        unsafe {
            for g in 0..groups {
                let low = group_of_eight(&rows[..8], g);
                let high = group_of_eight(&rows[8..16], g);
                let mut w = [_mm512_setzero_si512(); 8];
                for ((slot, &l), &h) in w.iter_mut().zip(&low).zip(&high) {
                    *slot = _mm512_inserti64x4::<1>(_mm512_castsi256_si512(l), h);
                }
                for e in 0..entities {
                    let base = (g * entities + e) * QUADS;
                    let mut acc = _mm512_setzero_si512();
                    for (q, &wq) in w.iter().enumerate() {
                        acc = _mm512_dpbusd_epi32(acc, wq, _mm512_set1_epi32(quads[base + q]));
                    }
                    let acc = _mm512_sub_epi32(acc, _mm512_set1_epi32(offsets[g * entities + e]));
                    _mm512_storeu_si512(s.as_mut_ptr().add((g * entities + e) * 16).cast(), acc);
                }
            }
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::canonical_simd::{
        LIMB_MAX, LIMB_MIN, kernel_switch_guard, set_fast_canonical_kernel,
    };

    /// Turns the opt-in off and unpins the INT4 kernel when dropped, also on
    /// a panic, so no other test inherits a pinned kernel.
    pub(crate) struct KernelRestore;

    impl Drop for KernelRestore {
        fn drop(&mut self) {
            set_fast_canonical_kernel(false);
            set_q4_kernel_pin(Q4Pin::Auto);
        }
    }

    /// The scalar reference, then every kernel this CPU has, as pins.
    pub(crate) fn pins() -> Vec<Q4Pin> {
        let mut pins = vec![Q4Pin::Scalar];
        pins.extend(available_q4_kernels().into_iter().map(Q4Pin::Kernel));
        pins
    }

    /// A pin's name in test output.
    pub(crate) fn pin_label(pin: Q4Pin) -> &'static str {
        match pin {
            Q4Pin::Kernel(kernel) => kernel.label(),
            Q4Pin::Scalar => "scalar-q4",
            Q4Pin::Auto => "auto",
        }
    }

    /// Select `pin` with the opt-in on, and return the counter it advances.
    pub(crate) fn use_pin(pin: Q4Pin) -> Option<Q4Kernel> {
        set_q4_kernel_pin(pin);
        set_fast_canonical_kernel(true);
        match pin {
            Q4Pin::Kernel(kernel) => Some(kernel),
            _ => None,
        }
    }

    /// SplitMix64.
    pub(crate) struct Rng(pub(crate) u64);

    impl Rng {
        pub(crate) fn next(&mut self) -> u64 {
            self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
            let mut z = self.0;
            z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
            z ^ (z >> 31)
        }

        /// A value of up to `bits` bits, either sign.
        fn signed(&mut self, bits: u32) -> i64 {
            let magnitude = (self.next() >> (64 - bits)) as i64;
            if self.next() & 1 == 0 {
                magnitude
            } else {
                -magnitude
            }
        }
    }

    /// Random packed INT4 rows and BF16 group scales over mixed binades:
    /// zero scales, subnormal scales and scales far below a row's largest
    /// (outside the 40-binade span) included.
    fn matrix(rng: &mut Rng, rows: usize, cols: usize) -> (Vec<u8>, Vec<u16>) {
        let packed = (0..rows * cols / 2).map(|_| rng.next() as u8).collect();
        let scales = (0..rows * cols / GROUP)
            .map(|_| match rng.next() % 16 {
                0 => 0,
                1 => (rng.next() % 128) as u16,
                2 => (((60 + rng.next() % 4) << 7) | (rng.next() % 128)) as u16,
                _ => (((112 + rng.next() % 12) << 7) | (rng.next() % 128)) as u16,
            })
            .collect();
        (packed, scales)
    }

    /// `k` activation vectors: each of up to `bits` bits (at most 30, inside
    /// the digit domain), with the domain's edges and zeros sprinkled in.
    fn activations(rng: &mut Rng, cols: usize, k: usize, bits: u32) -> Vec<Vec<i64>> {
        (0..k)
            .map(|_| {
                (0..cols)
                    .map(|_| match rng.next() % 32 {
                        0 => LIMB_MAX,
                        1 => LIMB_MIN,
                        2 => 0,
                        3 => -1,
                        _ => rng.signed(bits),
                    })
                    .collect()
            })
            .collect()
    }

    fn project(view: &Q4View<'_>, xs: &[Vec<i64>]) -> Result<Vec<i64>, String> {
        let refs: Vec<&[i64]> = xs.iter().map(Vec::as_slice).collect();
        let mut res = vec![0i64; view.rows * xs.len()];
        view.project_into(&refs, &mut res)
            .map(|()| res)
            .map_err(|e| e.to_string())
    }

    /// Every kernel this CPU has gives the scalar reference's bytes (and its
    /// errors) for every shape, token count and input class below, and the
    /// pinned kernel is the one that ran.
    #[test]
    fn every_q4_kernel_matches_the_scalar_reference() {
        let _guard = kernel_switch_guard();
        let _restore = KernelRestore;
        let mut rng = Rng(0x0004_C0DE_0000_0001);
        let mut compared = 0usize;
        for (rows, cols) in [
            (1, 32),
            (3, 32),
            (2, 64),
            (17, 64),
            (33, 96),
            (5, 128),
            (24, 2048),
        ] {
            let (packed, scales) = matrix(&mut rng, rows, cols);
            let view = Q4View::new(rows, cols, &packed, &scales);
            for k in [1, 2, 3, 5, 8, 16, 17] {
                for bits in [8, 20, 30] {
                    let xs = activations(&mut rng, cols, k, bits);
                    use_pin(Q4Pin::Scalar);
                    let want = project(&view, &xs);
                    for kernel in available_q4_kernels() {
                        use_pin(Q4Pin::Kernel(kernel));
                        let before = q4_kernel_runs(Some(kernel));
                        let got = project(&view, &xs);
                        assert_eq!(
                            got,
                            want,
                            "{}: {rows}x{cols}, {k} tokens, {bits}-bit inputs",
                            kernel.label()
                        );
                        assert!(q4_kernel_runs(Some(kernel)) > before, "{}", kernel.label());
                        compared += 1;
                    }
                }
            }
        }
        println!(
            "INT4 kernels compared with the scalar reference: {:?} ({compared} projections)",
            available_q4_kernels()
                .into_iter()
                .map(Q4Kernel::label)
                .collect::<Vec<_>>()
        );
    }

    /// An activation outside the four-digit domain: the kernel declines and
    /// the scalar reference computes the projection, with the same bytes.
    #[test]
    fn q4_kernels_decline_inputs_outside_the_digit_domain() {
        let _guard = kernel_switch_guard();
        let _restore = KernelRestore;
        let mut rng = Rng(0x0004_C0DE_0000_0002);
        let (packed, scales) = matrix(&mut rng, 9, 64);
        let view = Q4View::new(9, 64, &packed, &scales);
        let mut xs = activations(&mut rng, 64, 3, 20);
        xs[1][17] = LIMB_MAX + 1;
        use_pin(Q4Pin::Scalar);
        let want = project(&view, &xs);
        for kernel in available_q4_kernels() {
            use_pin(Q4Pin::Kernel(kernel));
            let (refused, scalar) = (q4_kernel_refusals(), q4_kernel_runs(None));
            assert_eq!(project(&view, &xs), want, "{}", kernel.label());
            assert!(q4_kernel_refusals() > refused, "{}", kernel.label());
            assert!(q4_kernel_runs(None) > scalar, "{}", kernel.label());
        }
    }

    /// The kernels this CPU can run, for the workflow's log: printed, and
    /// written one label per line to `ARC_Q4_KERNELS_OUT` when it is set.
    #[test]
    fn q4_kernel_report() {
        let labels: Vec<&str> = available_q4_kernels()
            .into_iter()
            .map(Q4Kernel::label)
            .collect();
        println!(
            "INT4 kernels on this CPU: {labels:?}; default {:?}",
            best_q4_kernel().map(Q4Kernel::label)
        );
        println!("{}", q4_kernel_run_report());
        if let Ok(path) = std::env::var("ARC_Q4_KERNELS_OUT") {
            std::fs::write(path, labels.join("\n")).unwrap();
        }
    }

    /// The pin follows its setter; a kernel the CPU lacks falls back to the
    /// best one; scalar unless the opt-in is on.
    #[test]
    fn q4_kernel_selection_follows_the_pin_and_the_opt_in() {
        let _guard = kernel_switch_guard();
        let _restore = KernelRestore;
        set_fast_canonical_kernel(false);
        set_q4_kernel_pin(Q4Pin::Auto);
        assert_eq!(selected_q4_kernel(), None);
        set_fast_canonical_kernel(true);
        assert_eq!(selected_q4_kernel(), best_q4_kernel());
        set_q4_kernel_pin(Q4Pin::Scalar);
        assert_eq!(selected_q4_kernel(), None);
        for kernel in Q4Kernel::ALL {
            set_q4_kernel_pin(Q4Pin::Kernel(kernel));
            assert_eq!(q4_kernel_pin(), Q4Pin::Kernel(kernel));
            let expected = if kernel.available() {
                Some(kernel)
            } else {
                best_q4_kernel()
            };
            assert_eq!(selected_q4_kernel(), expected, "{}", kernel.label());
            assert_eq!(Q4Kernel::from_label(kernel.label()), Some(kernel));
        }
    }
}
