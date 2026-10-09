//! Exact INT16 GEMV on Apple GPUs, through native Metal.
//!
//! Computes the row dots `d_i = sum_j w_ij * x_j` of the MLA + MoE profile's
//! INT16 projection (`precision::project_i16` in arc-inference: little-endian
//! INT16 weights in `[-32767, 32767]`, i64 Q16 activations) and returns
//! integers byte-identical to the CPU's. It is a decode GEMV: one activation
//! vector against one resident matrix.
//!
//! The profile's epilogue `floor(d_i * mu_i / 2^k_i)` needs a 95-bit product
//! and Metal has no 128-bit integer type, so this engine stops at the exact
//! dots and the caller applies the epilogue on the CPU, with the i128
//! function the CPU projection uses (`arith::dyadic_epilogue`).
//!
//! # Exact or refuse
//!
//! Every call either computes the exact dots or returns a [`Refusal`] without
//! writing any output. The conditions are the CPU projection's own:
//!
//! 1. shape;
//! 2. the accumulator guard `sum_j |x_j| < floor(2^63 / 32767)`
//!    ([`ACCUMULATOR_GUARD`], the same u128 expression as the CPU), so every
//!    true dot satisfies `|d| <= 2^63 - 32775`;
//! 3. weights in `[-32767, 32767]`: checked once, at [`MetalExactI16::upload`],
//!    which refuses an odd byte count or any -32768, as `I16Weights::new` does.
//!
//! Every activation the guard admits fits in seven balanced base-256 digits
//! ([`split_planes`]), so no admitted input is refused for its size. The
//! kernel (`metal_exact_gemv_i16.metal`) forms INT16 x i8 digit products in
//! int, flushes them to a 64-bit total at most every 64 blocks, adds the lane
//! totals modulo 2^64 in exact 16-bit pieces, and reads the result as a long.
//! The proof that each step is exact is at the top of the kernel source. No
//! floating point, division or modulo appears in the kernels;
//! `kernel_source_is_integer_only` checks the source.
//!
//! # Per-head batches
//!
//! [`MetalExactI16::dot_heads`] projects a stack of equal per-head matrices
//! (MLA's `wk_b` and `wv_b`), each head with its own vector, in one dispatch:
//! the kernel's y grid index is the head, and every row runs exactly the
//! steps of the single-matrix kernel. [`MetalExactI16::dot_heads_two_phase`]
//! runs two such phases with a CPU step between them (MLA's attention) in one
//! command buffer: the GPU signals a shared event after phase A and waits on it
//! before phase B, while the host reads phase A, runs the step and writes
//! phase B's digits. The host releases the GPU on every path, and both waits
//! time out instead of hanging. The one-command-buffer handoff is used only
//! after it reproduced the reference on this device at start-up; otherwise
//! each phase gets its own command buffer, which computes the same integers.
//!
//! # Self-test
//!
//! [`MetalExactI16::new`] runs [`MetalExactI16::self_test`] on the device
//! before returning: every pipeline variant (each digit-plane count, row
//! tile, simdgroup count and product width) against an independent i128
//! reference on random and boundary inputs, including the exact int bound of
//! a lane's 64-block partial sum and the largest dots the guard admits. An
//! engine that disagrees with the reference on one integer is never returned.
//!
//! Timings from [`MetalExactI16::time_batch`] use the command buffers' GPU
//! timestamps. They are measurements of the host's device and never feed a
//! computed value.

use std::collections::HashMap;
use std::ops::Range;
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant};

use metal::{
    Buffer, BufferRef, CommandBufferRef, CommandQueue, CommandQueueRef, CompileOptions,
    ComputeCommandEncoderRef, ComputePipelineState, Device, MTLCommandBufferStatus, MTLGPUFamily,
    MTLResourceOptions, MTLSize, SharedEvent, SharedEventRef,
};
use objc::Message;
use objc::rc::autoreleasepool;
use objc::runtime::Sel;
use rayon::prelude::*;

use crate::metal_exact::{DeviceReport, SelfTestReport, Storage, Tile};

/// Source of every kernel this module runs.
pub const KERNEL_SOURCE: &str = include_str!("metal_exact_gemv_i16.metal");
/// Largest weight magnitude; -32768 is refused at upload.
pub const WEIGHT_MAX: i64 = 32_767;
/// A projection needs `sum_j |x_j| < ACCUMULATOR_GUARD`, `floor(2^63 / 32767)`:
/// the CPU projection's bound, so `|dot| <= 32767 * (ACCUMULATOR_GUARD - 1)`.
pub const ACCUMULATOR_GUARD: u128 = (1u128 << 63) / 32_767;
/// Balanced base-256 digit planes per activation: enough for every guarded one.
pub const MAX_PLANES: usize = 7;
/// `sum_{d<7} 127 * 256^d`: the largest activation seven digits represent.
pub const PLANE_MAX: i64 = 35_887_507_618_889_599;
/// `sum_{d<7} -128 * 256^d`: the smallest.
pub const PLANE_MIN: i64 = -36_170_086_419_038_336;
/// Loop iterations of a lane between flushes of its int partial sums (the
/// kernel's `I16_FLUSH`).
pub const FLUSH_ITERATIONS: i64 = 64;
/// Largest 16-byte blocks per row: plane offsets (`7 * blocks + block`) and
/// the kernel's loop counters stay inside u32.
pub const MAX_BLOCKS: usize = (u32::MAX / 8) as usize;
/// Rows per simdgroup that have compiled pipelines (as for the INT8 kernel).
pub const ROWS_PER_SIMDGROUP: [u32; 4] = crate::metal_exact::ROWS_PER_SIMDGROUP;
/// Simdgroups per threadgroup accepted by [`Tile`].
pub const SIMDGROUPS: [u32; 4] = crate::metal_exact::SIMDGROUPS;
/// Widest simdgroup accepted: the kernel's 16-bit reduction pieces then sum
/// to at most `1024 * 65535 < 2^27`.
pub const MAX_SIMD_WIDTH: u64 = 1024;

/// Default decomposition: two rows per simdgroup, eight simdgroups, 32-bit
/// products. On the hosted runner's first sweep (PR #177) it had the lowest
/// mean time relative to each K2.6 shape's best tile. Every tile computes the
/// same integers; this only changes speed.
pub const DEFAULT_TILE: Tile = Tile {
    rows_per_simdgroup: 2,
    simdgroups: 8,
    mul16: false,
};

/// How long the host waits for the GPU at a handoff, or for a command buffer
/// with a handoff to complete, before it refuses. Far above any real wait.
const HANDOFF_TIMEOUT: Duration = Duration::from_secs(10);

/// Weights per 16-byte block.
const BLOCK_WEIGHTS: usize = 8;
const BLOCK_BYTES: usize = 16;
const SCRATCH_POOL: usize = 8;
const SELF_TEST_SEED: u64 = 0x00A2_C0DE_5EED_0016;
/// Largest `|w * c|`, and the largest block sum of 8 products.
const PRODUCT_MAX: i64 = WEIGHT_MAX * 128;
const BLOCK_SUM_MAX: i64 = 8 * PRODUCT_MAX;

const fn plane_bound(digit: i64, planes: usize) -> i64 {
    let (mut total, mut weight, mut d) = (0i64, 1i64, 0usize);
    while d < planes {
        total += digit * weight;
        weight *= 256;
        d += 1;
    }
    total
}
// The published bounds are the digit formula, not assumed constants.
const _: () = assert!(PLANE_MAX == plane_bound(127, MAX_PLANES));
const _: () = assert!(PLANE_MIN == plane_bound(-128, MAX_PLANES));
// Every activation the guard admits has seven digits, and six would not do.
const _: () = assert!(ACCUMULATOR_GUARD - 1 <= PLANE_MAX as u128);
const _: () = assert!(ACCUMULATOR_GUARD - 1 > plane_bound(127, MAX_PLANES - 1) as u128);
// The guard keeps every true dot inside i64: 32767 * (guard - 1) = 2^63 - 32775.
const _: () = assert!(32_767 * (ACCUMULATOR_GUARD - 1) == (1u128 << 63) - 32_775);
// A block sum is below 2^25, and 64 of them fit in int, 65 might not.
const _: () = assert!(BLOCK_SUM_MAX == 33_553_408);
const _: () = assert!(BLOCK_SUM_MAX < (1 << 25));
const _: () = assert!(FLUSH_ITERATIONS * BLOCK_SUM_MAX <= i32::MAX as i64);
const _: () = assert!((FLUSH_ITERATIONS + 1) * BLOCK_SUM_MAX > i32::MAX as i64);
// The 16-bit variant's products: |h * c| <= 128 * 128 and l * c in
// [255 * -128, 255 * 127], both inside i16.
const _: () = assert!(128 * 128 <= i16::MAX as i64);
const _: () = assert!(255 * 128 <= i16::MAX as i64);
// The kernel's reduction pieces: at most MAX_SIMD_WIDTH lanes of 65535.
const _: () = assert!(MAX_SIMD_WIDTH * 65_535 < 1 << 31);
// Plane offsets 7 * blocks + block and block + 64 * MAX_SIMD_WIDTH stay in u32.
const _: () = assert!(8 * (MAX_BLOCKS as u64) <= u32::MAX as u64);
const _: () =
    assert!(MAX_BLOCKS as u64 + (FLUSH_ITERATIONS as u64 + 1) * MAX_SIMD_WIDTH <= u32::MAX as u64);

/// Why a projection was not computed on the GPU. No output is written.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Refusal {
    /// Shape, length or tile mismatch.
    Shape,
    /// `sum_j |x_j| >= floor(2^63 / 32767)`: the CPU refuses the same input.
    AccumulatorBound,
    /// An activation whose seven digits did not rebuild it. Unreachable for
    /// guarded inputs; kept so that a split error can only refuse.
    DigitSplit,
    /// The command buffer did not complete.
    Device,
}

impl std::fmt::Display for Refusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let text = match self {
            Refusal::Shape => "shape mismatch",
            Refusal::AccumulatorBound => "sum of |x| at or above the INT16 accumulator guard",
            Refusal::DigitSplit => "activation outside the seven-digit domain",
            Refusal::Device => "GPU command buffer did not complete",
        };
        f.write_str(text)
    }
}

/// Why a head batch was not completed on the GPU. Its outputs hold nothing
/// meaningful.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum HeadsRefusal {
    /// A phase refused, as a single projection of the same input would.
    Refused(Refusal),
    /// The CPU step between the phases declined.
    Declined,
}

impl From<Refusal> for HeadsRefusal {
    fn from(refusal: Refusal) -> Self {
        HeadsRefusal::Refused(refusal)
    }
}

impl std::fmt::Display for HeadsRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            HeadsRefusal::Refused(refusal) => write!(f, "{refusal}"),
            HeadsRefusal::Declined => f.write_str("the step between the phases declined"),
        }
    }
}

/// How a two-phase head batch is submitted. Every mode computes the same
/// integers; only the host round trips differ.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Submission {
    /// One command buffer: phase A, a shared-event handoff to the CPU step,
    /// then phase B. One commit and one completion.
    OneCommandBuffer,
    /// A command buffer per phase.
    PerPhase,
    /// A command buffer per head and phase, as the per-projection hook pays.
    PerHead,
}

impl Submission {
    /// Every mode, the default first.
    pub const ALL: [Submission; 3] = [
        Submission::OneCommandBuffer,
        Submission::PerPhase,
        Submission::PerHead,
    ];

    /// Short label for reports.
    pub fn label(self) -> &'static str {
        match self {
            Submission::OneCommandBuffer => "one command buffer per layer",
            Submission::PerPhase => "one command buffer per phase",
            Submission::PerHead => "one command buffer per head and phase",
        }
    }
}

/// One phase of a head batch: `heads` matrices of `rows` x `n_cols`, stored
/// one after another in `matrix` from row `first_row` (head h's row i is
/// matrix row `first_row + h * rows + i`), each projected with its own
/// vector. Inputs hold the heads' vectors one after another, outputs the
/// heads' rows one after another.
#[derive(Clone, Copy)]
pub struct HeadPhase<'a> {
    pub matrix: &'a ResidentI16,
    pub first_row: usize,
    pub heads: usize,
    pub rows: usize,
}

impl HeadPhase<'_> {
    /// Rows of every head together.
    pub fn total_rows(&self) -> usize {
        self.heads * self.rows
    }

    /// Head `head`'s rows of the matrix.
    pub fn head_rows(&self, head: usize) -> Range<usize> {
        let start = self.first_row + head * self.rows;
        start..start + self.rows
    }
}

/// Whether `input` passes the CPU projection's accumulator guard,
/// `sum_j |x_j| < floor(2^63 / 32767)`, computed with the same u128 sum.
pub fn guard_holds(input: &[i64]) -> bool {
    let sum: u128 = input.iter().map(|v| u128::from(v.unsigned_abs())).sum();
    sum < ACCUMULATOR_GUARD
}

/// Whether any little-endian INT16 value of `bytes`, read at even offsets, is
/// -32768. The parallel chunks have an even size, so pairs stay aligned.
pub fn holds_int16_min(bytes: &[u8]) -> bool {
    bytes
        .par_chunks(1 << 20)
        .any(|chunk| chunk.chunks_exact(2).any(|v| v == [0, 0x80]))
}

/// A row-major INT16 matrix resident on the device. Rows are stored with a
/// stride rounded up to whole 16-byte blocks; the padding is zero, so it adds
/// nothing to any sum.
pub struct ResidentI16 {
    weights: Buffer,
    n_rows: usize,
    n_cols: usize,
    /// 16-byte blocks per row.
    blocks: usize,
    storage: Storage,
}

impl ResidentI16 {
    pub fn n_rows(&self) -> usize {
        self.n_rows
    }

    pub fn n_cols(&self) -> usize {
        self.n_cols
    }

    /// INT16 weight bytes, excluding padding.
    pub fn weight_bytes(&self) -> usize {
        self.n_rows * self.n_cols * 2
    }

    pub fn storage(&self) -> Storage {
        self.storage
    }
}

/// Per-call device buffers: digit planes in, dots out.
struct Scratch {
    digits: Buffer,
    digit_capacity: usize,
    out: Buffer,
    out_capacity: usize,
}

/// Mirrors `ExactI16Params` in the kernel source.
#[repr(C)]
#[derive(Clone, Copy)]
struct Params {
    rows: u32,
    row_offset: u32,
    blocks: u32,
    reserved: u32,
}

/// Mirrors `ExactI16HeadsParams` in the kernel source.
#[repr(C)]
#[derive(Clone, Copy)]
struct HeadsParams {
    rows: u32,
    row_offset: u32,
    blocks: u32,
    heads: u32,
}

/// Sets a shared event when dropped, so a GPU waiting on it always resumes,
/// whatever path the host takes (including a panic in the CPU step).
struct Release<'a> {
    event: &'a SharedEventRef,
    value: u64,
}

impl Drop for Release<'_> {
    fn drop(&mut self) {
        self.event.set_signaled_value(self.value);
    }
}

/// Wait until `event` reaches `value`. `false` if the command buffer failed or
/// the wait timed out.
fn wait_for_event(event: &SharedEventRef, value: u64, commands: &CommandBufferRef) -> bool {
    let start = Instant::now();
    loop {
        if event.signaled_value() >= value {
            return true;
        }
        if commands.status() == MTLCommandBufferStatus::Error || start.elapsed() > HANDOFF_TIMEOUT {
            return false;
        }
        std::thread::yield_now();
    }
}

/// Wait until the command buffer completes. `false` if it failed or the wait
/// timed out.
fn wait_for_completion(commands: &CommandBufferRef) -> bool {
    let start = Instant::now();
    loop {
        match commands.status() {
            MTLCommandBufferStatus::Completed => return true,
            MTLCommandBufferStatus::Error => return false,
            _ if start.elapsed() > HANDOFF_TIMEOUT => return false,
            _ => std::thread::yield_now(),
        }
    }
}

/// Copy the first `dots.len()` values of an output buffer.
///
/// # Safety
/// No command buffer that writes `out` may be running, and `out` must hold at
/// least `dots.len()` i64 values.
unsafe fn read_out(out: &BufferRef, dots: &mut [i64]) {
    // SAFETY: guaranteed by the caller.
    let results = unsafe { std::slice::from_raw_parts(out.contents().cast::<i64>(), dots.len()) };
    dots.copy_from_slice(results);
}

/// One encoded projection.
struct Dispatch<'a> {
    pipeline: &'a ComputePipelineState,
    matrix: &'a ResidentI16,
    digits: &'a BufferRef,
    out: &'a BufferRef,
    rows: Range<usize>,
    tile: Tile,
}

/// The exact INT16 Metal GEMV engine: one device, one queue, every pipeline.
pub struct MetalExactI16 {
    device: Device,
    queue: CommandQueue,
    pipelines: HashMap<(usize, u32, bool), ComputePipelineState>,
    heads_pipelines: HashMap<(usize, u32, bool), ComputePipelineState>,
    simd_width: u64,
    scratch: Mutex<Vec<Scratch>>,
    /// Shared events for one-command-buffer head batches, each with the last
    /// value it reached. A batch owns its event until it completes.
    events: Mutex<Vec<(SharedEvent, u64)>>,
    /// Whether the one-command-buffer handoff reproduced the reference here.
    one_buffer: bool,
    /// Why it did not, if it did not.
    one_buffer_error: Option<String>,
    tile: Tile,
}

fn kernel_name(planes: usize, rows: u32, mul16: bool) -> String {
    format!("exact_gemv_i16_p{planes}_r{rows}_m{}", u8::from(mul16))
}

fn heads_kernel_name(planes: usize, rows: u32, mul16: bool) -> String {
    format!(
        "exact_gemv_i16_heads_p{planes}_r{rows}_m{}",
        u8::from(mul16)
    )
}

fn tile_is_valid(tile: Tile) -> bool {
    ROWS_PER_SIMDGROUP.contains(&tile.rows_per_simdgroup) && SIMDGROUPS.contains(&tile.simdgroups)
}

/// GPU execution time of a completed command buffer, in seconds (read as in
/// `metal_exact`: metal 0.29 has no accessor for these properties).
fn gpu_seconds(commands: &CommandBufferRef) -> f64 {
    let read = |property: &str| -> f64 {
        // SAFETY: GPUStartTime and GPUEndTime are read-only CFTimeInterval
        // (f64) properties of MTLCommandBuffer, and the buffer has completed.
        unsafe { commands.send_message::<(), f64>(Sel::register(property), ()) }
            .expect("MTLCommandBuffer GPU timestamps")
    };
    read("GPUEndTime") - read("GPUStartTime")
}

/// Write `input` as seven balanced base-256 digit planes of `stride` bytes
/// each (plane `d` at `planes[d * stride..]`, zero padded), returning the
/// number of planes in use (the highest non-zero digit, at least 1), or
/// `None` if any element is outside `[PLANE_MIN, PLANE_MAX]`.
///
/// The digits are those of `canonical_simd::split_limbs`, extended from four
/// to seven, and the reconstruction of every element is checked, so a
/// decomposition error can only refuse, never change a value.
pub fn split_planes(input: &[i64], planes: &mut [i8], stride: usize) -> Option<usize> {
    if input.is_empty() || stride < input.len() || planes.len() < MAX_PLANES * stride {
        return None;
    }
    let mut used = 1usize;
    for (j, &x) in input.iter().enumerate() {
        if !(PLANE_MIN..=PLANE_MAX).contains(&x) {
            return None;
        }
        // Unsigned two's-complement arithmetic, defined for every input; only
        // the low byte is read in each round.
        let mut u = x as u64;
        let mut digits = [0i8; MAX_PLANES];
        for (d, digit) in digits.iter_mut().enumerate() {
            let low = (u & 255) as i32;
            let c = if low > 127 { low - 256 } else { low };
            *digit = c as i8;
            u = u.wrapping_sub(c as i64 as u64) >> 8;
            if c != 0 {
                used = used.max(d + 1);
            }
        }
        let rebuilt = digits
            .iter()
            .rev()
            .fold(0i64, |acc, &c| acc * 256 + i64::from(c));
        if rebuilt != x {
            return None;
        }
        for (d, &c) in digits.iter().enumerate() {
            planes[d * stride + j] = c;
        }
    }
    for plane in planes.chunks_exact_mut(stride).take(MAX_PLANES) {
        plane[input.len()..].fill(0);
    }
    Some(used)
}

/// Independent reference: the exact dot of every row in i128.
pub fn reference_dots(weights: &[u8], n_cols: usize, input: &[i64]) -> Vec<i128> {
    weights
        .chunks_exact(2 * n_cols)
        .map(|row| {
            row.chunks_exact(2)
                .zip(input)
                .map(|(w, &x)| i128::from(i16::from_le_bytes([w[0], w[1]])) * i128::from(x))
                .sum::<i128>()
        })
        .collect()
}

/// SplitMix64, for the self-test inputs.
struct SplitMix64(u64);

impl SplitMix64 {
    fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// Uniform in `[-limit, limit]`.
    fn symmetric(&mut self, limit: i64) -> i64 {
        let span = 2 * limit as u64 + 1;
        (self.next_u64() % span) as i64 - limit
    }
}

/// Little-endian bytes of `values`.
fn le_bytes(values: &[i16]) -> Vec<u8> {
    values.iter().flat_map(|v| v.to_le_bytes()).collect()
}

/// An input that needs exactly `planes` digit planes (its first element, of
/// either sign) and passes the guard.
fn guarded_input(rng: &mut SplitMix64, cols: usize, planes: usize, negative: bool) -> Vec<i64> {
    let guard_max = (ACCUMULATOR_GUARD - 1) as i64;
    // Both signs of a magnitude in (128 * s_{p-1}, 127 * s_p] need exactly p
    // digits, where s_q = (256^q - 1) / 255.
    let low = plane_bound(128, planes - 1);
    let high = plane_bound(127, planes).min(guard_max);
    let top = low + 1 + (rng.next_u64() % (high - low) as u64) as i64;
    let limit = ((guard_max - top) / cols as i64).min(plane_bound(127, planes));
    let mut input: Vec<i64> = (0..cols).map(|_| rng.symmetric(limit)).collect();
    input[0] = if negative { -top } else { top };
    input
}

impl MetalExactI16 {
    /// Open the system default Metal device, compile every pipeline and run
    /// the self-test. Returns an error rather than an engine that disagrees
    /// with the reference.
    pub fn new() -> Result<Self, String> {
        let mut engine = autoreleasepool(Self::build)?;
        engine.self_test(1)?;
        // The handoff is used only if it reproduces the reference here, on a
        // queue of its own so that a device without working shared events
        // cannot stall the engine's queue.
        match engine.self_test_one_buffer() {
            Ok(_) => engine.one_buffer = true,
            Err(error) => engine.one_buffer_error = Some(error),
        }
        Ok(engine)
    }

    fn build() -> Result<Self, String> {
        let device = Device::system_default().ok_or_else(|| {
            "no Metal device: MTLCreateSystemDefaultDevice returned nil".to_string()
        })?;
        let options = CompileOptions::new();
        options.set_fast_math_enabled(false);
        let library = device
            .new_library_with_source(KERNEL_SOURCE, &options)
            .map_err(|e| format!("exact INT16 GEMV kernels failed to compile: {e}"))?;
        let mut pipelines = HashMap::new();
        let mut heads_pipelines = HashMap::new();
        let mut simd_width = u64::MAX;
        for planes in 1..=MAX_PLANES {
            for rows in ROWS_PER_SIMDGROUP {
                for mul16 in [false, true] {
                    let name = kernel_name(planes, rows, mul16);
                    let function = library
                        .get_function(&name, None)
                        .map_err(|e| format!("{name}: {e}"))?;
                    let pipeline = device
                        .new_compute_pipeline_state_with_function(&function)
                        .map_err(|e| format!("{name}: {e}"))?;
                    simd_width = simd_width.min(pipeline.thread_execution_width());
                    pipelines.insert((planes, rows, mul16), pipeline);
                    let name = heads_kernel_name(planes, rows, mul16);
                    let function = library
                        .get_function(&name, None)
                        .map_err(|e| format!("{name}: {e}"))?;
                    let pipeline = device
                        .new_compute_pipeline_state_with_function(&function)
                        .map_err(|e| format!("{name}: {e}"))?;
                    simd_width = simd_width.min(pipeline.thread_execution_width());
                    heads_pipelines.insert((planes, rows, mul16), pipeline);
                }
            }
        }
        // The kernel hands row r of a simdgroup to lane r, and its reduction
        // pieces are exact for at most MAX_SIMD_WIDTH lanes.
        let widest = ROWS_PER_SIMDGROUP.iter().copied().max().unwrap_or(1);
        if simd_width < u64::from(widest) || simd_width > MAX_SIMD_WIDTH {
            return Err(format!(
                "unsupported simdgroup width {simd_width} (need {widest} to {MAX_SIMD_WIDTH})"
            ));
        }
        let queue = device.new_command_queue();
        Ok(Self {
            device,
            queue,
            pipelines,
            heads_pipelines,
            simd_width,
            scratch: Mutex::new(Vec::new()),
            events: Mutex::new(Vec::new()),
            one_buffer: false,
            one_buffer_error: None,
            tile: DEFAULT_TILE,
        })
    }

    /// Device name, limits and GPU families.
    pub fn report(&self) -> DeviceReport {
        let families = [
            (MTLGPUFamily::Apple7, "apple7"),
            (MTLGPUFamily::Apple8, "apple8"),
            (MTLGPUFamily::Apple9, "apple9"),
            (MTLGPUFamily::Mac2, "mac2"),
            (MTLGPUFamily::Metal3, "metal3"),
        ]
        .into_iter()
        .filter(|(family, _)| self.device.supports_family(*family))
        .map(|(_, name)| name.to_string())
        .collect();
        DeviceReport {
            name: self.device.name().to_string(),
            max_buffer_length: self.device.max_buffer_length(),
            recommended_max_working_set_size: self.device.recommended_max_working_set_size(),
            has_unified_memory: self.device.has_unified_memory(),
            thread_execution_width: self.simd_width,
            families,
        }
    }

    /// The tile used by [`Self::dot_rows`].
    pub fn tile(&self) -> Tile {
        self.tile
    }

    /// Choose the tile used by [`Self::dot_rows`]. Every tile computes the
    /// same integers; this only changes speed.
    pub fn set_tile(&mut self, tile: Tile) -> Result<(), Refusal> {
        if !tile_is_valid(tile) {
            return Err(Refusal::Shape);
        }
        self.tile = tile;
        Ok(())
    }

    /// Copy a row-major little-endian INT16 matrix to the device, after the
    /// profile's one-time weight check: an odd or inconsistent byte count, or
    /// any -32768, is refused.
    pub fn upload(
        &self,
        weights: &[u8],
        n_rows: usize,
        n_cols: usize,
        storage: Storage,
    ) -> Result<ResidentI16, String> {
        let blocks = n_cols.div_ceil(BLOCK_WEIGHTS);
        if n_rows == 0
            || n_cols == 0
            || n_rows > i32::MAX as usize
            || blocks > MAX_BLOCKS
            || n_rows.checked_mul(n_cols).and_then(|n| n.checked_mul(2)) != Some(weights.len())
        {
            return Err(format!(
                "invalid INT16 matrix: {n_rows}x{n_cols}, {} bytes",
                weights.len()
            ));
        }
        if holds_int16_min(weights) {
            return Err("INT16 weights hold -32768, which is not a profile value".to_string());
        }
        let stride = blocks * BLOCK_BYTES;
        let row_bytes = 2 * n_cols;
        let bytes = n_rows
            .checked_mul(stride)
            .filter(|&b| b as u64 <= self.device.max_buffer_length())
            .ok_or_else(|| format!("{n_rows}x{n_cols} exceeds the device's maximum buffer"))?;
        autoreleasepool(|| {
            let staging = self
                .device
                .new_buffer(bytes as u64, MTLResourceOptions::StorageModeShared);
            // SAFETY: a new shared buffer of exactly `bytes` bytes that no
            // command buffer references yet.
            let dst =
                unsafe { std::slice::from_raw_parts_mut(staging.contents().cast::<u8>(), bytes) };
            if stride == row_bytes {
                dst.copy_from_slice(weights);
            } else {
                for (row, src) in dst
                    .chunks_exact_mut(stride)
                    .zip(weights.chunks_exact(row_bytes))
                {
                    let (head, tail) = row.split_at_mut(row_bytes);
                    head.copy_from_slice(src);
                    tail.fill(0);
                }
            }
            let weights = match storage {
                Storage::Shared => staging,
                Storage::Private => {
                    let private = self
                        .device
                        .new_buffer(bytes as u64, MTLResourceOptions::StorageModePrivate);
                    let commands = self.queue.new_command_buffer();
                    let blit = commands.new_blit_command_encoder();
                    blit.copy_from_buffer(&staging, 0, &private, 0, bytes as u64);
                    blit.end_encoding();
                    commands.commit();
                    commands.wait_until_completed();
                    if commands.status() != MTLCommandBufferStatus::Completed {
                        return Err("upload blit did not complete".to_string());
                    }
                    private
                }
            };
            Ok(ResidentI16 {
                weights,
                n_rows,
                n_cols,
                blocks,
                storage,
            })
        })
    }

    /// Exact dots of rows `rows` (output index 0 is row `rows.start`) with
    /// the engine's tile. Returns the digit planes used.
    pub fn dot_rows(
        &self,
        matrix: &ResidentI16,
        rows: Range<usize>,
        input: &[i64],
        dots: &mut [i64],
    ) -> Result<usize, Refusal> {
        self.dot_rows_with(matrix, rows, input, dots, self.tile)
    }

    /// The general entry point: any row range and tile. Writes `dots` only on
    /// success.
    pub fn dot_rows_with(
        &self,
        matrix: &ResidentI16,
        rows: Range<usize>,
        input: &[i64],
        dots: &mut [i64],
        tile: Tile,
    ) -> Result<usize, Refusal> {
        self.check(matrix, &rows, input, dots.len(), tile)?;
        autoreleasepool(|| {
            let scratch = self.take_scratch(matrix.blocks, rows.len());
            let outcome = self.split(&scratch, matrix, input).and_then(|planes| {
                let pipeline = self.pipeline(planes, tile)?;
                let commands = self.queue.new_command_buffer();
                let encoder = commands.new_compute_command_encoder();
                self.encode(
                    encoder,
                    &Dispatch {
                        pipeline,
                        matrix,
                        digits: &scratch.digits,
                        out: &scratch.out,
                        rows: rows.clone(),
                        tile,
                    },
                );
                encoder.end_encoding();
                commands.commit();
                commands.wait_until_completed();
                if commands.status() != MTLCommandBufferStatus::Completed {
                    return Err(Refusal::Device);
                }
                // SAFETY: the command buffer has completed, the output buffer
                // holds at least `rows.len()` i64 values and the GPU no longer
                // uses it.
                let results = unsafe {
                    std::slice::from_raw_parts(scratch.out.contents().cast::<i64>(), rows.len())
                };
                dots.copy_from_slice(results);
                Ok(planes)
            });
            self.return_scratch(scratch);
            outcome
        })
    }

    /// The CPU projection's conditions, in its order: shape, then the guard.
    fn check(
        &self,
        matrix: &ResidentI16,
        rows: &Range<usize>,
        input: &[i64],
        out_len: usize,
        tile: Tile,
    ) -> Result<(), Refusal> {
        if rows.start >= rows.end
            || rows.end > matrix.n_rows
            || input.len() != matrix.n_cols
            || out_len != rows.len()
            || !tile_is_valid(tile)
        {
            return Err(Refusal::Shape);
        }
        if !guard_holds(input) {
            return Err(Refusal::AccumulatorBound);
        }
        Ok(())
    }

    fn pipeline(&self, planes: usize, tile: Tile) -> Result<&ComputePipelineState, Refusal> {
        self.pipelines
            .get(&(planes, tile.rows_per_simdgroup, tile.mul16))
            .ok_or(Refusal::Shape)
    }

    fn split(
        &self,
        scratch: &Scratch,
        matrix: &ResidentI16,
        input: &[i64],
    ) -> Result<usize, Refusal> {
        let stride = matrix.blocks * BLOCK_WEIGHTS;
        let len = MAX_PLANES * stride;
        // SAFETY: this call owns `scratch` (taken from the pool), its digit
        // buffer holds at least `len` bytes, and no command buffer that uses
        // it is in flight.
        let planes =
            unsafe { std::slice::from_raw_parts_mut(scratch.digits.contents().cast::<i8>(), len) };
        split_planes(input, planes, stride).ok_or(Refusal::DigitSplit)
    }

    fn encode(&self, encoder: &ComputeCommandEncoderRef, job: &Dispatch<'_>) {
        let fit = (job.pipeline.max_total_threads_per_threadgroup() / self.simd_width).max(1);
        let simdgroups = u64::from(job.tile.simdgroups).min(fit);
        let rows_per_group = simdgroups * u64::from(job.tile.rows_per_simdgroup);
        let groups = (job.rows.len() as u64).div_ceil(rows_per_group);
        let params = Params {
            rows: job.rows.len() as u32,
            row_offset: job.rows.start as u32,
            blocks: job.matrix.blocks as u32,
            reserved: 0,
        };
        encoder.set_compute_pipeline_state(job.pipeline);
        encoder.set_buffer(0, Some(&job.matrix.weights), 0);
        encoder.set_buffer(1, Some(job.digits), 0);
        encoder.set_buffer(2, Some(job.out), 0);
        encoder.set_bytes(
            3,
            std::mem::size_of::<Params>() as u64,
            std::ptr::from_ref(&params).cast(),
        );
        encoder.dispatch_thread_groups(
            MTLSize::new(groups, 1, 1),
            MTLSize::new(simdgroups * self.simd_width, 1, 1),
        );
    }

    fn new_scratch(&self, blocks: usize, rows: usize) -> Scratch {
        let digit_capacity = MAX_PLANES * blocks * BLOCK_WEIGHTS;
        Scratch {
            digits: self
                .device
                .new_buffer(digit_capacity as u64, MTLResourceOptions::StorageModeShared),
            digit_capacity,
            out: self.device.new_buffer(
                (rows * std::mem::size_of::<i64>()) as u64,
                MTLResourceOptions::StorageModeShared,
            ),
            out_capacity: rows,
        }
    }

    fn take_scratch(&self, blocks: usize, rows: usize) -> Scratch {
        let needed = MAX_PLANES * blocks * BLOCK_WEIGHTS;
        let reused = {
            let mut pool = self.scratch.lock().unwrap_or_else(PoisonError::into_inner);
            pool.iter()
                .position(|s| s.digit_capacity >= needed && s.out_capacity >= rows)
                .map(|index| pool.swap_remove(index))
        };
        reused.unwrap_or_else(|| self.new_scratch(blocks, rows))
    }

    fn return_scratch(&self, scratch: Scratch) {
        let mut pool = self.scratch.lock().unwrap_or_else(PoisonError::into_inner);
        if pool.len() >= SCRATCH_POOL {
            // Keep the larger buffers: each one also serves every smaller call.
            let size = |s: &Scratch| (s.digit_capacity, s.out_capacity);
            let smallest = pool
                .iter()
                .enumerate()
                .min_by_key(|(_, s)| size(s))
                .map(|(index, s)| (index, size(s)));
            match smallest {
                Some((index, kept)) if kept < size(&scratch) => {
                    pool.swap_remove(index);
                }
                _ => return,
            }
        }
        pool.push(scratch);
    }

    /// GPU seconds for one pass over `items`, each its own projection of
    /// every row, averaged over `repeats` passes encoded in one command
    /// buffer. The work is identical to [`Self::dot_rows`]; only the host
    /// round trip per projection is removed.
    pub fn time_batch(
        &self,
        items: &[(&ResidentI16, &[i64])],
        tile: Tile,
        repeats: usize,
    ) -> Result<f64, Refusal> {
        if items.is_empty() || repeats == 0 {
            return Err(Refusal::Shape);
        }
        autoreleasepool(|| {
            let mut prepared = Vec::with_capacity(items.len());
            for &(matrix, input) in items {
                let rows = 0..matrix.n_rows;
                self.check(matrix, &rows, input, matrix.n_rows, tile)?;
                let scratch = self.new_scratch(matrix.blocks, matrix.n_rows);
                let planes = self.split(&scratch, matrix, input)?;
                let pipeline = self.pipeline(planes, tile)?;
                prepared.push((matrix, scratch, pipeline));
            }
            let commands = self.queue.new_command_buffer();
            let encoder = commands.new_compute_command_encoder();
            for _ in 0..repeats {
                for (matrix, scratch, pipeline) in &prepared {
                    self.encode(
                        encoder,
                        &Dispatch {
                            pipeline,
                            matrix,
                            digits: &scratch.digits,
                            out: &scratch.out,
                            rows: 0..matrix.n_rows,
                            tile,
                        },
                    );
                }
            }
            encoder.end_encoding();
            commands.commit();
            commands.wait_until_completed();
            if commands.status() != MTLCommandBufferStatus::Completed {
                return Err(Refusal::Device);
            }
            Ok(gpu_seconds(commands) / repeats as f64)
        })
    }

    /// Whether two-phase head batches run in one command buffer here (the
    /// handoff reproduced the reference at start-up). Otherwise
    /// [`Submission::OneCommandBuffer`] runs as [`Submission::PerPhase`].
    pub fn one_command_buffer(&self) -> bool {
        self.one_buffer
    }

    /// Why the one-command-buffer handoff failed its start-up test, if it did.
    pub fn one_command_buffer_error(&self) -> Option<&str> {
        self.one_buffer_error.as_deref()
    }

    fn heads_pipeline(&self, planes: usize, tile: Tile) -> Result<&ComputePipelineState, Refusal> {
        self.heads_pipelines
            .get(&(planes, tile.rows_per_simdgroup, tile.mul16))
            .ok_or(Refusal::Shape)
    }

    /// A head phase's shape against `out_len` outputs: the stack fits the
    /// matrix, the digit buffer fits the device, and the tile is valid.
    fn check_heads_shape(
        &self,
        phase: &HeadPhase<'_>,
        out_len: usize,
        tile: Tile,
    ) -> Result<(), Refusal> {
        let total = phase.heads.checked_mul(phase.rows);
        let end = total.and_then(|rows| phase.first_row.checked_add(rows));
        let digit_bytes = phase
            .heads
            .checked_mul(MAX_PLANES * BLOCK_WEIGHTS)
            .and_then(|bytes| bytes.checked_mul(phase.matrix.blocks));
        if phase.heads == 0
            || phase.rows == 0
            || phase.heads > u32::MAX as usize
            || end.is_none_or(|end| end > phase.matrix.n_rows)
            || total != Some(out_len)
            || digit_bytes.is_none_or(|bytes| bytes as u64 > self.device.max_buffer_length())
            || !tile_is_valid(tile)
        {
            return Err(Refusal::Shape);
        }
        Ok(())
    }

    /// The conditions of one projection per head, in its order: shape, then
    /// every head's guard.
    fn check_heads(
        &self,
        phase: &HeadPhase<'_>,
        inputs: &[i64],
        out_len: usize,
        tile: Tile,
    ) -> Result<(), Refusal> {
        self.check_heads_shape(phase, out_len, tile)?;
        if phase.heads.checked_mul(phase.matrix.n_cols) != Some(inputs.len()) {
            return Err(Refusal::Shape);
        }
        if !inputs.chunks_exact(phase.matrix.n_cols).all(guard_holds) {
            return Err(Refusal::AccumulatorBound);
        }
        Ok(())
    }

    /// Write every head's vector as seven digit planes (head h's planes start
    /// at `h * 7 * stride` bytes) and return the most planes any head uses.
    fn split_heads(
        &self,
        scratch: &Scratch,
        phase: &HeadPhase<'_>,
        inputs: &[i64],
    ) -> Result<usize, Refusal> {
        let stride = phase.matrix.blocks * BLOCK_WEIGHTS;
        let per_head = MAX_PLANES * stride;
        // SAFETY: this call owns `scratch` (taken from the pool), its digit
        // buffer holds at least `heads * per_head` bytes (`take_scratch` with
        // `heads * blocks`), and no command buffer reads it now: none uses it
        // yet, or the one that does waits at an event until the host is done.
        let planes = unsafe {
            std::slice::from_raw_parts_mut(
                scratch.digits.contents().cast::<i8>(),
                phase.heads * per_head,
            )
        };
        let mut used = 1;
        for (input, head_planes) in inputs
            .chunks_exact(phase.matrix.n_cols)
            .zip(planes.chunks_exact_mut(per_head))
        {
            used = used.max(split_planes(input, head_planes, stride).ok_or(Refusal::DigitSplit)?);
        }
        Ok(used)
    }

    fn encode_heads(
        &self,
        encoder: &ComputeCommandEncoderRef,
        pipeline: &ComputePipelineState,
        phase: &HeadPhase<'_>,
        scratch: &Scratch,
        tile: Tile,
    ) {
        let fit = (pipeline.max_total_threads_per_threadgroup() / self.simd_width).max(1);
        let simdgroups = u64::from(tile.simdgroups).min(fit);
        let rows_per_group = simdgroups * u64::from(tile.rows_per_simdgroup);
        let groups = (phase.rows as u64).div_ceil(rows_per_group);
        let params = HeadsParams {
            rows: phase.rows as u32,
            row_offset: phase.first_row as u32,
            blocks: phase.matrix.blocks as u32,
            heads: phase.heads as u32,
        };
        encoder.set_compute_pipeline_state(pipeline);
        encoder.set_buffer(0, Some(&phase.matrix.weights), 0);
        encoder.set_buffer(1, Some(&scratch.digits), 0);
        encoder.set_buffer(2, Some(&scratch.out), 0);
        encoder.set_bytes(
            3,
            std::mem::size_of::<HeadsParams>() as u64,
            std::ptr::from_ref(&params).cast(),
        );
        encoder.dispatch_thread_groups(
            MTLSize::new(groups, phase.heads as u64, 1),
            MTLSize::new(simdgroups * self.simd_width, 1, 1),
        );
    }

    fn take_heads_scratch(&self, phase: &HeadPhase<'_>) -> Scratch {
        self.take_scratch(phase.heads * phase.matrix.blocks, phase.total_rows())
    }

    /// Every head's dots of one phase: one dispatch, one command buffer.
    /// Returns the most digit planes any head used. Writes `dots` only on
    /// success.
    pub fn dot_heads(
        &self,
        phase: &HeadPhase<'_>,
        inputs: &[i64],
        dots: &mut [i64],
        tile: Tile,
    ) -> Result<usize, Refusal> {
        self.check_heads(phase, inputs, dots.len(), tile)?;
        autoreleasepool(|| {
            let scratch = self.take_heads_scratch(phase);
            let outcome = self
                .split_heads(&scratch, phase, inputs)
                .and_then(|planes| {
                    let pipeline = self.heads_pipeline(planes, tile)?;
                    let commands = self.queue.new_command_buffer();
                    let encoder = commands.new_compute_command_encoder();
                    self.encode_heads(encoder, pipeline, phase, &scratch, tile);
                    encoder.end_encoding();
                    commands.commit();
                    commands.wait_until_completed();
                    if commands.status() != MTLCommandBufferStatus::Completed {
                        return Err(Refusal::Device);
                    }
                    // SAFETY: the command buffer has completed and the output
                    // buffer holds `total_rows` values.
                    unsafe { read_out(&scratch.out, dots) };
                    Ok(planes)
                });
            self.return_scratch(scratch);
            outcome
        })
    }

    /// Two head phases with a CPU step between them: every head's phase-A
    /// dots, then `between` (on this thread) turns them into the heads'
    /// phase-B vectors (or declines with `None`), then every head's phase-B
    /// dots. Returns the submission that ran: [`Submission::OneCommandBuffer`]
    /// runs as [`Submission::PerPhase`] where the handoff did not pass its
    /// start-up test. On an error `dots_a` and `dots_b` hold nothing
    /// meaningful.
    #[allow(clippy::too_many_arguments)]
    pub fn dot_heads_two_phase<F>(
        &self,
        a: &HeadPhase<'_>,
        inputs_a: &[i64],
        b: &HeadPhase<'_>,
        between: F,
        dots_a: &mut [i64],
        dots_b: &mut [i64],
        tile: Tile,
        submission: Submission,
    ) -> Result<Submission, HeadsRefusal>
    where
        F: FnOnce(&[i64]) -> Option<Vec<i64>>,
    {
        self.check_heads(a, inputs_a, dots_a.len(), tile)?;
        self.check_heads_shape(b, dots_b.len(), tile)?;
        match submission {
            Submission::OneCommandBuffer if self.one_buffer => self
                .two_phase_one_buffer(&self.queue, a, inputs_a, b, between, dots_a, dots_b, tile)
                .map(|()| Submission::OneCommandBuffer),
            Submission::OneCommandBuffer | Submission::PerPhase => {
                self.dot_heads(a, inputs_a, dots_a, tile)?;
                let inputs_b = between(dots_a).ok_or(HeadsRefusal::Declined)?;
                self.dot_heads(b, &inputs_b, dots_b, tile)?;
                Ok(Submission::PerPhase)
            }
            Submission::PerHead => {
                let cols_a = a.matrix.n_cols;
                for (head, (input, dots)) in inputs_a
                    .chunks_exact(cols_a)
                    .zip(dots_a.chunks_exact_mut(a.rows))
                    .enumerate()
                {
                    self.dot_rows_with(a.matrix, a.head_rows(head), input, dots, tile)?;
                }
                let inputs_b = between(dots_a).ok_or(HeadsRefusal::Declined)?;
                let cols_b = b.matrix.n_cols;
                if b.heads.checked_mul(cols_b) != Some(inputs_b.len()) {
                    return Err(Refusal::Shape.into());
                }
                for (head, (input, dots)) in inputs_b
                    .chunks_exact(cols_b)
                    .zip(dots_b.chunks_exact_mut(b.rows))
                    .enumerate()
                {
                    self.dot_rows_with(b.matrix, b.head_rows(head), input, dots, tile)?;
                }
                Ok(Submission::PerHead)
            }
        }
    }

    /// Both phases in one command buffer on `queue`, with a shared-event
    /// handoff between them. Phase B is encoded with all seven digit planes,
    /// because its vectors are not known when it is encoded; the planes a
    /// head does not use are zero.
    #[allow(clippy::too_many_arguments)]
    fn two_phase_one_buffer<F>(
        &self,
        queue: &CommandQueueRef,
        a: &HeadPhase<'_>,
        inputs_a: &[i64],
        b: &HeadPhase<'_>,
        between: F,
        dots_a: &mut [i64],
        dots_b: &mut [i64],
        tile: Tile,
    ) -> Result<(), HeadsRefusal>
    where
        F: FnOnce(&[i64]) -> Option<Vec<i64>>,
    {
        autoreleasepool(|| {
            let scratch_a = self.take_heads_scratch(a);
            let scratch_b = self.take_heads_scratch(b);
            let (event, value) = self.take_event();
            let mut completed = true;
            let outcome = (|| -> Result<(), HeadsRefusal> {
                let planes_a = self.split_heads(&scratch_a, a, inputs_a)?;
                let pipeline_a = self.heads_pipeline(planes_a, tile)?;
                let pipeline_b = self.heads_pipeline(MAX_PLANES, tile)?;
                let commands = queue.new_command_buffer();
                let encoder = commands.new_compute_command_encoder();
                self.encode_heads(encoder, pipeline_a, a, &scratch_a, tile);
                encoder.end_encoding();
                commands.encode_signal_event(&event, value + 1);
                commands.encode_wait_for_event(&event, value + 2);
                let encoder = commands.new_compute_command_encoder();
                self.encode_heads(encoder, pipeline_b, b, &scratch_b, tile);
                encoder.end_encoding();
                commands.commit();
                // From here the GPU is released on every path; phase B then
                // runs over whatever the digit buffer holds, and its dots are
                // read only if the step succeeded.
                let release = Release {
                    event: &event,
                    value: value + 2,
                };
                if !wait_for_event(&event, value + 1, commands) {
                    drop(release);
                    completed = wait_for_completion(commands);
                    return Err(Refusal::Device.into());
                }
                // SAFETY: the GPU signalled after phase A's dispatch, which
                // wrote `total_rows` values, and phase B does not touch it.
                unsafe { read_out(&scratch_a.out, dots_a) };
                let prepared = between(dots_a)
                    .ok_or(HeadsRefusal::Declined)
                    .and_then(|inputs_b| {
                        self.check_heads(b, &inputs_b, dots_b.len(), tile)?;
                        self.split_heads(&scratch_b, b, &inputs_b)?;
                        Ok(())
                    });
                drop(release);
                completed = wait_for_completion(commands);
                if !completed {
                    return Err(Refusal::Device.into());
                }
                prepared?;
                // SAFETY: the command buffer has completed and the output
                // buffer holds phase B's `total_rows` values.
                unsafe { read_out(&scratch_b.out, dots_b) };
                Ok(())
            })();
            // A command buffer that did not complete may still use the scratch
            // buffers and the event: they are dropped, never reused.
            if completed {
                self.return_scratch(scratch_a);
                self.return_scratch(scratch_b);
                self.return_event(event, value + 2);
            }
            outcome
        })
    }

    fn take_event(&self) -> (SharedEvent, u64) {
        let reused = self
            .events
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .pop();
        reused.unwrap_or_else(|| (self.device.new_shared_event(), 0))
    }

    fn return_event(&self, event: SharedEvent, value: u64) {
        let mut pool = self.events.lock().unwrap_or_else(PoisonError::into_inner);
        if pool.len() < SCRATCH_POOL {
            pool.push((event, value));
        }
    }

    /// GPU seconds for one pass over `phases`, each every head of one phase
    /// in one dispatch, averaged over `repeats` passes encoded in one command
    /// buffer (no handoff: every vector is given).
    pub fn time_heads(
        &self,
        phases: &[(&HeadPhase<'_>, &[i64])],
        tile: Tile,
        repeats: usize,
    ) -> Result<f64, Refusal> {
        if phases.is_empty() || repeats == 0 {
            return Err(Refusal::Shape);
        }
        autoreleasepool(|| {
            let mut prepared = Vec::with_capacity(phases.len());
            for &(phase, inputs) in phases {
                self.check_heads(phase, inputs, phase.total_rows(), tile)?;
                let scratch = self.take_heads_scratch(phase);
                let planes = self.split_heads(&scratch, phase, inputs)?;
                let pipeline = self.heads_pipeline(planes, tile)?;
                prepared.push((phase, scratch, pipeline));
            }
            let commands = self.queue.new_command_buffer();
            let encoder = commands.new_compute_command_encoder();
            for _ in 0..repeats {
                for (phase, scratch, pipeline) in &prepared {
                    self.encode_heads(encoder, pipeline, phase, scratch, tile);
                }
            }
            encoder.end_encoding();
            commands.commit();
            commands.wait_until_completed();
            let status = commands.status();
            for (_, scratch, _) in prepared {
                self.return_scratch(scratch);
            }
            if status != MTLCommandBufferStatus::Completed {
                return Err(Refusal::Device);
            }
            Ok(gpu_seconds(commands) / repeats as f64)
        })
    }

    /// The one-command-buffer handoff on a queue of its own against the
    /// reference: two head phases whose step derives phase B's vectors from
    /// phase A's dots, every tile class, and a declined step.
    pub fn self_test_one_buffer(&self) -> Result<SelfTestReport, String> {
        let queue = self.device.new_command_queue();
        let mut rng = SplitMix64(SELF_TEST_SEED ^ 0xB0FF);
        let mut report = SelfTestReport::default();
        // Phase A: 3 heads of 5 x 17; phase B: 3 heads of 4 x 5 (B's columns
        // are A's rows, as for MLA's key and value stacks).
        let (heads, rows_a, cols_a, rows_b) = (3usize, 5usize, 17usize, 4usize);
        let a_values: Vec<i16> = (0..(heads * rows_a + 1) * cols_a)
            .map(|_| rng.symmetric(WEIGHT_MAX) as i16)
            .collect();
        let b_values: Vec<i16> = (0..heads * rows_b * rows_a)
            .map(|_| rng.symmetric(WEIGHT_MAX) as i16)
            .collect();
        let (a_bytes, b_bytes) = (le_bytes(&a_values), le_bytes(&b_values));
        let a_matrix = self.upload(&a_bytes, heads * rows_a + 1, cols_a, Storage::Shared)?;
        let b_matrix = self.upload(&b_bytes, heads * rows_b, rows_a, Storage::Shared)?;
        let a = HeadPhase {
            matrix: &a_matrix,
            first_row: 1,
            heads,
            rows: rows_a,
        };
        let b = HeadPhase {
            matrix: &b_matrix,
            first_row: 0,
            heads,
            rows: rows_b,
        };
        let inputs_a: Vec<i64> = (0..heads)
            .flat_map(|head| guarded_input(&mut rng, cols_a, 1 + head * 3, head == 1))
            .collect();
        // The step: phase B's vector of a head is its phase-A dots shifted
        // down by 24 bits (floor), which keeps every vector inside the guard.
        let step = |dots: &[i64]| -> Vec<i64> { dots.iter().map(|&dot| dot >> 24).collect() };
        let head_ref = |bytes: &[u8], first: usize, rows: usize, cols: usize, input: &[i64]| {
            reference_dots(
                &bytes[first * cols * 2..(first + rows) * cols * 2],
                cols,
                input,
            )
        };
        let want_a: Vec<i128> = (0..heads)
            .flat_map(|h| {
                let input = &inputs_a[h * cols_a..(h + 1) * cols_a];
                head_ref(&a_bytes, 1 + h * rows_a, rows_a, cols_a, input)
            })
            .collect();
        let dots_a_ref: Vec<i64> = want_a.iter().map(|&dot| dot as i64).collect();
        let inputs_b = step(&dots_a_ref);
        let want_b: Vec<i128> = (0..heads)
            .flat_map(|h| {
                let input = &inputs_b[h * rows_a..(h + 1) * rows_a];
                head_ref(&b_bytes, h * rows_b, rows_b, rows_a, input)
            })
            .collect();
        for tile in [
            DEFAULT_TILE,
            Tile {
                rows_per_simdgroup: 1,
                simdgroups: 1,
                mul16: true,
            },
            Tile {
                rows_per_simdgroup: 8,
                simdgroups: 4,
                mul16: false,
            },
        ] {
            let mut dots_a = vec![0i64; heads * rows_a];
            let mut dots_b = vec![0i64; heads * rows_b];
            self.two_phase_one_buffer(
                &queue,
                &a,
                &inputs_a,
                &b,
                |dots| Some(step(dots)),
                &mut dots_a,
                &mut dots_b,
                tile,
            )
            .map_err(|refusal| format!("one-buffer self-test {}: {refusal}", tile.label()))?;
            let same = |got: &[i64], want: &[i128]| {
                got.iter().zip(want).all(|(&g, &w)| i128::from(g) == w)
            };
            if !same(&dots_a, &want_a) || !same(&dots_b, &want_b) {
                return Err(format!(
                    "one-buffer self-test {}: the GPU differs from the reference",
                    tile.label()
                ));
            }
            report.compared += 2;
        }
        // A declined step releases the GPU and refuses cleanly.
        let mut dots_a = vec![0i64; heads * rows_a];
        let mut dots_b = vec![0i64; heads * rows_b];
        let declined = self.two_phase_one_buffer(
            &queue,
            &a,
            &inputs_a,
            &b,
            |_| None,
            &mut dots_a,
            &mut dots_b,
            DEFAULT_TILE,
        );
        if declined != Err(HeadsRefusal::Declined) {
            return Err(format!(
                "one-buffer self-test: a declined step gave {declined:?}"
            ));
        }
        report.refused += 1;
        Ok(report)
    }

    /// Run every pipeline variant against [`reference_dots`] on random and
    /// boundary inputs. Any mismatch, or any refusal where the domain rules
    /// accept, is an error naming the case.
    pub fn self_test(&self, rounds: usize) -> Result<SelfTestReport, String> {
        let mut rng = SplitMix64(SELF_TEST_SEED);
        let mut report = SelfTestReport::default();
        let tiles = Tile::all();
        for round in 0..rounds {
            for &(n_rows, n_cols) in &[(1usize, 1usize), (7, 17), (9, 64), (33, 100)] {
                let mut values: Vec<i16> = (0..n_rows * n_cols)
                    .map(|_| rng.symmetric(WEIGHT_MAX) as i16)
                    .collect();
                values[0] = 32_767;
                if let Some(last) = values.last_mut() {
                    *last = -32_767;
                }
                let weights = le_bytes(&values);
                let matrix = self.upload(&weights, n_rows, n_cols, Storage::Shared)?;
                for planes in 1..=MAX_PLANES {
                    let negative = (round + planes).is_multiple_of(2);
                    let input = guarded_input(&mut rng, n_cols, planes, negative);
                    for &tile in &tiles {
                        let used = self.compare(&matrix, &weights, &input, tile, &mut report)?;
                        if used != planes {
                            return Err(format!(
                                "self-test {n_rows}x{n_cols}: {used} digit planes used for an \
                                 input that needs {planes}"
                            ));
                        }
                    }
                }
            }
        }
        self.heads_cases(&mut rng, &mut report)?;
        self.boundary_cases(&mut report)?;
        Ok(report)
    }

    /// The per-head kernel against the reference: stacks that start past row
    /// 0, heads that need different numbers of digit planes in one dispatch,
    /// every plane count and every tile.
    fn heads_cases(&self, rng: &mut SplitMix64, report: &mut SelfTestReport) -> Result<(), String> {
        let tiles = Tile::all();
        for &(heads, rows, cols) in &[
            (1usize, 1usize, 1usize),
            (3, 5, 17),
            (4, 16, 8),
            (2, 33, 100),
        ] {
            let first_row = 1;
            let n_rows = first_row + heads * rows + 1;
            let mut values: Vec<i16> = (0..n_rows * cols)
                .map(|_| rng.symmetric(WEIGHT_MAX) as i16)
                .collect();
            values[first_row * cols] = 32_767;
            values[(first_row + heads * rows) * cols - 1] = -32_767;
            let weights = le_bytes(&values);
            let matrix = self.upload(&weights, n_rows, cols, Storage::Shared)?;
            let phase = HeadPhase {
                matrix: &matrix,
                first_row,
                heads,
                rows,
            };
            for planes in 1..=MAX_PLANES {
                // Head 0 needs exactly `planes` planes, the others at most.
                let inputs: Vec<i64> = (0..heads)
                    .flat_map(|head| {
                        let needed = if head == 0 { planes } else { 1 + head % planes };
                        guarded_input(rng, cols, needed, (head + planes).is_multiple_of(2))
                    })
                    .collect();
                let want: Vec<i128> = (0..heads)
                    .flat_map(|head| {
                        let first = first_row + head * rows;
                        reference_dots(
                            &weights[first * cols * 2..(first + rows) * cols * 2],
                            cols,
                            &inputs[head * cols..(head + 1) * cols],
                        )
                    })
                    .collect();
                for &tile in &tiles {
                    let case = format!("heads {heads}x{rows}x{cols} tile {}", tile.label());
                    let mut got = vec![0i64; heads * rows];
                    let used =
                        self.dot_heads(&phase, &inputs, &mut got, tile)
                            .map_err(|refusal| {
                                format!("self-test {case}: refused an in-domain case ({refusal})")
                            })?;
                    if used != planes {
                        return Err(format!(
                            "self-test {case}: {used} digit planes used for heads that need {planes}"
                        ));
                    }
                    if let Some(row) = got
                        .iter()
                        .zip(&want)
                        .position(|(&g, &w)| i128::from(g) != w)
                    {
                        return Err(format!(
                            "self-test {case}: row {row} is {} on the GPU and {} on the CPU",
                            got[row], want[row]
                        ));
                    }
                    report.compared += 1;
                }
            }
        }
        Ok(())
    }

    /// One projection on the GPU against the reference; returns the planes.
    fn compare(
        &self,
        matrix: &ResidentI16,
        weights: &[u8],
        input: &[i64],
        tile: Tile,
        report: &mut SelfTestReport,
    ) -> Result<usize, String> {
        let mut got = vec![0i64; matrix.n_rows];
        let case = format!("{}x{} tile {}", matrix.n_rows, matrix.n_cols, tile.label());
        let used = self
            .dot_rows_with(matrix, 0..matrix.n_rows, input, &mut got, tile)
            .map_err(|refusal| {
                format!("self-test {case}: refused an in-domain case ({refusal})")
            })?;
        let want = reference_dots(weights, matrix.n_cols, input);
        if let Some(row) = got
            .iter()
            .zip(&want)
            .position(|(&g, &w)| i128::from(g) != w)
        {
            return Err(format!(
                "self-test {case}: row {row} is {} on the GPU and {} on the CPU",
                got[row], want[row]
            ));
        }
        report.compared += 1;
        Ok(used)
    }

    /// Exact edges: the int bound of a lane's 64-block partial sum, a second
    /// flush, the largest dots the guard admits (seven planes), and refusals.
    fn boundary_cases(&self, report: &mut SelfTestReport) -> Result<(), String> {
        let edge_tiles = [
            DEFAULT_TILE,
            Tile {
                rows_per_simdgroup: 1,
                simdgroups: 1,
                mul16: false,
            },
            Tile {
                rows_per_simdgroup: 8,
                simdgroups: 8,
                mul16: true,
            },
        ];
        // 2,049 blocks: with 32 lanes, lane 0 adds 64 blocks of products
        // (-32767) * (-128) = 4,194,176 before its first flush, i.e.
        // 64 * 33,553,408 = 2,147,418,112, the largest int partial sum the
        // bound allows, and then flushes a second time.
        let cols = 2_049 * BLOCK_WEIGHTS;
        let values: Vec<i16> = (0..2 * cols)
            .map(|i| if i < cols { -32_767 } else { 32_767 })
            .collect();
        let weights = le_bytes(&values);
        let input = vec![-128i64; cols];
        let matrix = self.upload(&weights, 2, cols, Storage::Shared)?;
        for tile in edge_tiles {
            self.compare(&matrix, &weights, &input, tile, report)?;
        }

        // The heaviest accepted input with every product of row 0 positive:
        // dots +-(2^63 - 32,775), with an activation that needs seven planes.
        let guard_max = (ACCUMULATOR_GUARD - 1) as i64;
        let weights = le_bytes(&[32_767, -32_767, 32_767, -32_767, 32_767, -32_767]);
        let heaviest = [guard_max - 2, -1, 1];
        let matrix = self.upload(&weights, 2, 3, Storage::Shared)?;
        for tile in edge_tiles {
            if self.compare(&matrix, &weights, &heaviest, tile, report)? != MAX_PLANES {
                return Err("self-test: the heaviest input did not use seven planes".to_string());
            }
        }
        let mut dots = [0i64; 2];
        self.dot_rows(&matrix, 0..2, &heaviest, &mut dots)
            .map_err(|refusal| format!("self-test: heaviest input refused ({refusal})"))?;
        if dots != [i64::MAX - 32_774, -(i64::MAX - 32_774)] {
            return Err(format!("self-test: heaviest dots are {dots:?}"));
        }
        // One more unit of mass is refused, the output untouched.
        for over in [[guard_max - 1, -1, 1], [i64::MIN, 0, 0], [i64::MAX, 0, 0]] {
            let mut untouched = [7i64; 2];
            if self.dot_rows(&matrix, 0..2, &over, &mut untouched) != Err(Refusal::AccumulatorBound)
                || untouched != [7; 2]
            {
                return Err(format!("self-test: {over:?} was not refused cleanly"));
            }
            report.refused += 1;
        }
        // Shapes are refused, the output untouched.
        let mut untouched = [7i64; 2];
        for (rows, input) in [
            (0..2, &[1i64, 2][..]),
            (1..1, &[1, 2, 3][..]),
            (1..3, &[1, 2, 3][..]),
        ] {
            if self.dot_rows(&matrix, rows.clone(), input, &mut untouched[..rows.len()])
                != Err(Refusal::Shape)
                || untouched != [7; 2]
            {
                return Err(format!("self-test: shape {rows:?} was not refused cleanly"));
            }
            report.refused += 1;
        }
        // -32768 and odd byte counts are refused at upload.
        for (bytes, rows, cols) in [
            (le_bytes(&[5, i16::MIN, 7]), 1, 3),
            (le_bytes(&[i16::MIN]), 1, 1),
            (vec![1, 0, 0], 1, 1),
        ] {
            if self.upload(&bytes, rows, cols, Storage::Shared).is_ok() {
                return Err(format!("self-test: upload of {bytes:?} was not refused"));
            }
            report.refused += 1;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Remove `//` and `/* */` comments, keeping everything else.
    fn strip_comments(source: &str) -> String {
        let mut out = String::with_capacity(source.len());
        let mut rest = source;
        while !rest.is_empty() {
            if let Some(after) = rest.strip_prefix("//") {
                rest = after.find('\n').map_or("", |end| &after[end..]);
            } else if let Some(after) = rest.strip_prefix("/*") {
                rest = after.find("*/").map_or("", |end| &after[end + 2..]);
            } else {
                let mut chars = rest.chars();
                if let Some(c) = chars.next() {
                    out.push(c);
                }
                rest = chars.as_str();
            }
        }
        out
    }

    #[test]
    fn kernel_source_is_integer_only() {
        let code = strip_comments(KERNEL_SOURCE);
        let words: Vec<&str> = code
            .split(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
            .filter(|w| !w.is_empty())
            .collect();
        for banned in ["float", "half", "double", "bfloat", "fast", "precise"] {
            assert!(
                !words.iter().any(|w| w.starts_with(banned)),
                "the exact kernels must not use {banned}"
            );
        }
        // No native integer division or modulo (Apple GPUs lower it in
        // software; #150 found a miscompiled one on an M2 Ultra).
        assert!(!code.contains('/'), "the exact kernels must not divide");
        assert!(!code.contains('%'), "the exact kernels must not use modulo");
        // The flush interval the bounds above are proved for.
        assert!(code.contains(&format!("constant uint I16_FLUSH = {FLUSH_ITERATIONS};")));
    }

    #[test]
    fn split_planes_reconstructs_edges_and_refuses_outside() {
        let guard_max = (ACCUMULATOR_GUARD - 1) as i64;
        let mut edges = vec![
            0,
            1,
            -1,
            127,
            128,
            -128,
            -129,
            32_767,
            -32_768,
            2_139_062_143,
            -2_155_905_152,
            2_139_062_144,
            -2_155_905_153,
            guard_max,
            -guard_max,
            PLANE_MAX,
            PLANE_MIN,
            PLANE_MAX - 1,
            PLANE_MIN + 1,
        ];
        for planes in 1..MAX_PLANES {
            edges.push(plane_bound(127, planes));
            edges.push(plane_bound(127, planes) + 1);
            edges.push(plane_bound(-128, planes));
            edges.push(plane_bound(-128, planes) - 1);
        }
        let stride = edges.len().div_ceil(16) * 16;
        let mut planes = vec![0x55i8; MAX_PLANES * stride];
        let used = split_planes(&edges, &mut planes, stride).expect("edges are in the domain");
        assert_eq!(used, MAX_PLANES);
        for (j, &x) in edges.iter().enumerate() {
            let rebuilt = (0..MAX_PLANES)
                .rev()
                .fold(0i64, |acc, d| acc * 256 + i64::from(planes[d * stride + j]));
            assert_eq!(rebuilt, x);
        }
        for plane in planes.chunks_exact(stride) {
            assert!(
                plane[edges.len()..].iter().all(|&c| c == 0),
                "padding is zero"
            );
        }
        // The planes in use are exactly the digit count of the largest value.
        for planes_needed in 1..=MAX_PLANES {
            let top = plane_bound(127, planes_needed).min(guard_max);
            assert_eq!(
                split_planes(&[3, -top, 5], &mut planes, stride),
                Some(planes_needed)
            );
        }
        for bad in [PLANE_MAX + 1, PLANE_MIN - 1, i64::MAX, i64::MIN] {
            assert_eq!(split_planes(&[1, bad, 3], &mut planes, stride), None);
        }
    }

    #[test]
    fn the_guard_matches_the_cpu_rule() {
        let guard_max = (ACCUMULATOR_GUARD - 1) as i64;
        assert!(guard_holds(&[guard_max]));
        assert!(guard_holds(&[guard_max - 5, -5]));
        assert!(!guard_holds(&[guard_max + 1]));
        assert!(!guard_holds(&[guard_max - 4, -5]));
        assert!(!guard_holds(&[i64::MIN]));
        assert!(!guard_holds(&[i64::MAX, i64::MAX, i64::MAX]));
        assert!(guard_holds(&[]));
        assert_eq!(ACCUMULATOR_GUARD, 281_483_566_907_400);
    }

    #[test]
    fn int16_minimum_is_found_at_even_offsets_only() {
        let clean = vec![0u8; (1 << 20) + 6];
        assert!(!holds_int16_min(&clean));
        for at in [0, 2, (1 << 20) - 2, 1 << 20, (1 << 20) + 4] {
            let mut bad = clean.clone();
            bad[at..at + 2].copy_from_slice(&i16::MIN.to_le_bytes());
            assert!(holds_int16_min(&bad), "{at}");
        }
        assert!(!holds_int16_min(&[0x01, 0x00, 0x80, 0x00]));
    }

    #[test]
    fn self_test_passes_on_this_device() {
        let engine = MetalExactI16::new().expect("a Metal device that matches the reference");
        let report = engine.self_test(3).expect("self-test");
        assert!(report.compared > 0 && report.refused > 0, "{report:?}");
        eprintln!("device: {:?}", engine.report());
        eprintln!("INT16 self-test (3 rounds): {report:?}");
    }

    #[test]
    fn two_phase_head_batches_are_exact_and_refuse_cleanly_in_every_submission() {
        let engine = MetalExactI16::new().expect("Metal device");
        eprintln!("one command buffer here: {}", engine.one_command_buffer());
        let mut rng = SplitMix64(0x2F4A_5E16);
        let (heads, rows_a, cols_a, rows_b) = (2usize, 3usize, 9usize, 4usize);
        let a_values: Vec<i16> = (0..heads * rows_a * cols_a)
            .map(|_| rng.symmetric(WEIGHT_MAX) as i16)
            .collect();
        let b_values: Vec<i16> = (0..heads * rows_b * rows_a)
            .map(|_| rng.symmetric(WEIGHT_MAX) as i16)
            .collect();
        let (a_bytes, b_bytes) = (le_bytes(&a_values), le_bytes(&b_values));
        let a_matrix = engine
            .upload(&a_bytes, heads * rows_a, cols_a, Storage::Shared)
            .expect("upload");
        let b_matrix = engine
            .upload(&b_bytes, heads * rows_b, rows_a, Storage::Shared)
            .expect("upload");
        let a = HeadPhase {
            matrix: &a_matrix,
            first_row: 0,
            heads,
            rows: rows_a,
        };
        let b = HeadPhase {
            matrix: &b_matrix,
            first_row: 0,
            heads,
            rows: rows_b,
        };
        let inputs_a: Vec<i64> = (0..heads * cols_a)
            .map(|_| rng.symmetric(1 << 20))
            .collect();
        let step = |dots: &[i64]| -> Vec<i64> { dots.iter().map(|&dot| dot >> 8).collect() };
        let want_a: Vec<i128> = (0..heads)
            .flat_map(|h| {
                reference_dots(
                    &a_bytes[h * rows_a * cols_a * 2..(h + 1) * rows_a * cols_a * 2],
                    cols_a,
                    &inputs_a[h * cols_a..(h + 1) * cols_a],
                )
            })
            .collect();
        let inputs_b = step(&want_a.iter().map(|&dot| dot as i64).collect::<Vec<_>>());
        let want_b: Vec<i128> = (0..heads)
            .flat_map(|h| {
                reference_dots(
                    &b_bytes[h * rows_b * rows_a * 2..(h + 1) * rows_b * rows_a * 2],
                    rows_a,
                    &inputs_b[h * rows_a..(h + 1) * rows_a],
                )
            })
            .collect();
        let guard = ACCUMULATOR_GUARD as i64;
        let mut bad_a = inputs_a.clone();
        bad_a[cols_a] = i64::MIN;
        for submission in Submission::ALL {
            let mut dots_a = vec![0i64; heads * rows_a];
            let mut dots_b = vec![0i64; heads * rows_b];
            let ran = engine
                .dot_heads_two_phase(
                    &a,
                    &inputs_a,
                    &b,
                    |dots| Some(step(dots)),
                    &mut dots_a,
                    &mut dots_b,
                    DEFAULT_TILE,
                    submission,
                )
                .expect("in domain");
            let expected = match submission {
                Submission::OneCommandBuffer if !engine.one_command_buffer() => {
                    Submission::PerPhase
                }
                other => other,
            };
            assert_eq!(ran, expected);
            let wide = |dots: &[i64]| dots.iter().map(|&dot| i128::from(dot)).collect::<Vec<_>>();
            assert_eq!(wide(&dots_a), want_a, "{submission:?}");
            assert_eq!(wide(&dots_b), want_b, "{submission:?}");
            // A declined step, a phase-B vector past the guard, a phase-A
            // vector past the guard: clean refusals in every submission.
            let declined = engine.dot_heads_two_phase(
                &a,
                &inputs_a,
                &b,
                |_| None,
                &mut dots_a,
                &mut dots_b,
                DEFAULT_TILE,
                submission,
            );
            assert_eq!(declined, Err(HeadsRefusal::Declined), "{submission:?}");
            let past_b = engine.dot_heads_two_phase(
                &a,
                &inputs_a,
                &b,
                |dots| {
                    let mut vectors = vec![1i64; dots.len()];
                    vectors[rows_a] = guard;
                    Some(vectors)
                },
                &mut dots_a,
                &mut dots_b,
                DEFAULT_TILE,
                submission,
            );
            assert_eq!(
                past_b,
                Err(HeadsRefusal::Refused(Refusal::AccumulatorBound)),
                "{submission:?}"
            );
            let past_a = engine.dot_heads_two_phase(
                &a,
                &bad_a,
                &b,
                |dots| Some(step(dots)),
                &mut dots_a,
                &mut dots_b,
                DEFAULT_TILE,
                submission,
            );
            assert_eq!(
                past_a,
                Err(HeadsRefusal::Refused(Refusal::AccumulatorBound)),
                "{submission:?}"
            );
        }
        // After every refusal the engine still computes exactly.
        let mut dots_a = vec![0i64; heads * rows_a];
        engine
            .dot_heads(&a, &inputs_a, &mut dots_a, DEFAULT_TILE)
            .expect("in domain");
        assert_eq!(
            dots_a
                .iter()
                .map(|&dot| i128::from(dot))
                .collect::<Vec<_>>(),
            want_a
        );
    }

    #[test]
    fn private_storage_computes_the_same_integers() {
        let engine = MetalExactI16::new().expect("Metal device");
        let mut rng = SplitMix64(16);
        let (rows, cols) = (37, 300);
        let values: Vec<i16> = (0..rows * cols)
            .map(|_| rng.symmetric(WEIGHT_MAX) as i16)
            .collect();
        let weights = le_bytes(&values);
        let input = guarded_input(&mut rng, cols, 5, true);
        let want = reference_dots(&weights, cols, &input);
        for storage in [Storage::Shared, Storage::Private] {
            let matrix = engine
                .upload(&weights, rows, cols, storage)
                .expect("upload");
            assert_eq!(matrix.storage(), storage);
            assert_eq!(matrix.weight_bytes(), rows * cols * 2);
            let mut got = vec![0i64; rows];
            engine
                .dot_rows(&matrix, 0..rows, &input, &mut got)
                .expect("in domain");
            let got: Vec<i128> = got.into_iter().map(i128::from).collect();
            assert_eq!(got, want, "{storage:?}");
        }
    }
}
