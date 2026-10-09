//! Exact canonical per-row INT8 GEMV on Apple GPUs, through native Metal.
//!
//! Computes `y_i = ((sum_j w_ij * x_j) * s_i) >> 16` (integer profile contract
//! §3.4: INT8 weights, i64 Q16 activations, one i64 Q16 scale per row, `>>` an
//! arithmetic shift) and returns integers byte-identical to the CPU scalar
//! kernel (`dot_i8_i64` + `matmul_i8_view_into` in arc-inference). It is a
//! decode GEMV: one activation vector against one resident matrix.
//!
//! # Exact or refuse
//!
//! Every call either computes the exact value or returns a [`Refusal`] without
//! writing any output, so the caller can run its CPU kernel instead. Before
//! anything reaches the GPU the host applies the same checks, in the same
//! order, as the CPU limb kernel (`canonical_simd::matmul_i8_canonical_rows_fast_view`):
//!
//! 1. shape;
//! 2. `n_cols <= 131,071` ([`MAX_COLS`]), so i32 digit sums cannot overflow;
//! 3. for the canonical epilogue, `128 * sum_j |x_j| * |s_i|` fits in i64 for
//!    every row (one check against the largest `|s_i|`, which is equivalent);
//! 4. every activation lies in `[-2,155,905,152, 2,139,062,143]` and is written
//!    as four balanced base-256 digits with a per-element reconstruction check
//!    ([`split_planes`], the digits of `canonical_simd::split_limbs`).
//!
//! The kernel (`metal_exact_gemv.metal`) then forms i8 x i8 digit-plane sums in
//! int, recombines them in 64-bit two's complement, multiplies by the scale in
//! 64 bits and applies the floor shift with logical operations. The proof that
//! each step is exact is at the top of the kernel source. Integer addition is
//! associative, so tiles, lane counts, `simd_sum` order and threadgroup counts
//! cannot change a value. No floating point and no division appear in the
//! kernels; `kernel_source_is_integer_only` checks the source.
//!
//! # Self-test
//!
//! [`MetalExactGemv::new`] runs [`MetalExactGemv::self_test`] on the device
//! before returning: every pipeline variant against an independent i128
//! reference, on random and boundary inputs (including the exact i32 digit-sum
//! bound at 131,071 columns and the scale-overflow boundary). An engine that
//! disagrees with the CPU on one integer is never returned.
//!
//! Timings from [`MetalExactGemv::time_batch`] and
//! [`MetalExactGemv::measure_read_bandwidth`] use the command buffers' GPU
//! timestamps. They are measurements of the host's device and never feed a
//! computed value.

use std::collections::HashMap;
use std::ops::Range;
use std::sync::{Mutex, PoisonError};

use metal::{
    Buffer, BufferRef, CommandBufferRef, CommandQueue, CompileOptions, ComputeCommandEncoderRef,
    ComputePipelineState, Device, MTLCommandBufferStatus, MTLGPUFamily, MTLResourceOptions,
    MTLSize,
};
use objc::Message;
use objc::rc::autoreleasepool;
use objc::runtime::Sel;

/// Source of every kernel this module runs.
pub const KERNEL_SOURCE: &str = include_str!("metal_exact_gemv.metal");
/// Balanced base-256 digit planes per activation (`canonical_simd::LIMB_COUNT`).
pub const MAX_PLANES: usize = 4;
/// `sum_{d<4} 127 * 256^d`: the largest activation four digits represent.
pub const PLANE_MAX: i64 = 2_139_062_143;
/// `sum_{d<4} -128 * 256^d`: the smallest.
pub const PLANE_MIN: i64 = -2_155_905_152;
/// Largest row length whose i32 digit-plane sums cannot overflow.
pub const MAX_COLS: usize = 131_071;
/// Rows per simdgroup that have compiled pipelines.
pub const ROWS_PER_SIMDGROUP: [u32; 4] = [1, 2, 4, 8];
/// Simdgroups per threadgroup accepted by [`Tile`].
pub const SIMDGROUPS: [u32; 4] = [1, 2, 4, 8];

const BLOCK_BYTES: usize = 16;
const SCRATCH_POOL: usize = 8;
const SELF_TEST_SEED: u64 = 0x00A2_C0DE_5EED_0001;
const READ_PROBE_THREADS: u64 = 256 * 1024;

const fn plane_bound(digit: i64) -> i64 {
    let (mut total, mut weight, mut d) = (0i64, 1i64, 0usize);
    while d < MAX_PLANES {
        total += digit * weight;
        weight *= 256;
        d += 1;
    }
    total
}
// The published bounds are the digit formula, not assumed constants.
const _: () = assert!(PLANE_MAX == plane_bound(127));
const _: () = assert!(PLANE_MIN == plane_bound(-128));
// i32 digit sums: 16,384 * K fits exactly up to K = 131,071 and not beyond.
const _: () = assert!(16_384i64 * (MAX_COLS as i64) <= i32::MAX as i64);
const _: () = assert!(16_384i64 * (MAX_COLS as i64 + 1) > i32::MAX as i64);
/// The largest `|acc|` four i32 digit-plane sums can recombine to:
/// `2^31 * (1 + 2^8 + 2^16 + 2^24)`.
const fn recombined_max() -> i128 {
    let (mut total, mut d) = (0i128, 0usize);
    while d < MAX_PLANES {
        total += (1i128 << 31) << (8 * d);
        d += 1;
    }
    total
}
// Recombination stays far inside i64.
const _: () = assert!(recombined_max() < (1i128 << 56));

/// Why a projection was not computed on the GPU. No output is written.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Refusal {
    /// Shape, length or tile mismatch.
    Shape,
    /// Row length above [`MAX_COLS`].
    InnerDimAboveI32Bound,
    /// An activation outside the four-digit domain.
    ActivationOutOfDomain,
    /// `128 * sum|x| * |s_i|` does not fit in i64 for some row.
    ScaleMultiplyWouldOverflow,
    /// The command buffer did not complete.
    Device,
}

impl std::fmt::Display for Refusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let text = match self {
            Refusal::Shape => "shape mismatch",
            Refusal::InnerDimAboveI32Bound => "row length above the i32 digit-sum bound",
            Refusal::ActivationOutOfDomain => "activation outside the four-digit domain",
            Refusal::ScaleMultiplyWouldOverflow => "scale multiply would overflow i64",
            Refusal::Device => "GPU command buffer did not complete",
        };
        f.write_str(text)
    }
}

/// What the kernel writes for each row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Epilogue {
    /// `(acc_i * s_i) >> 16`: the canonical per-row profile.
    CanonicalQ16,
    /// The exact dot `acc_i` itself, for a caller that owns its epilogue (for
    /// example the dyadic `(acc * mu) >> k` profile). Always in range:
    /// `|acc| < 2^56` inside the accepted domain.
    RawDot,
}

/// Work decomposition. Changes speed, never a value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Tile {
    /// Output rows per simdgroup (1, 2, 4 or 8).
    pub rows_per_simdgroup: u32,
    /// Simdgroups per threadgroup (1, 2, 4 or 8; reduced if the pipeline
    /// cannot run that many threads).
    pub simdgroups: u32,
    /// 16-bit products instead of 32-bit ones (both exact).
    pub mul16: bool,
}

impl Tile {
    /// Default decomposition: four rows per simdgroup, two simdgroups,
    /// 16-bit products. On the hosted runner it was within about 1% of the
    /// fastest of the 32 tiles for every Llama-2-7B shape (run 37882303401).
    pub const DEFAULT: Tile = Tile {
        rows_per_simdgroup: 4,
        simdgroups: 2,
        mul16: true,
    };

    /// Every tile with compiled pipelines.
    pub fn all() -> Vec<Tile> {
        let mut tiles = Vec::new();
        for rows_per_simdgroup in ROWS_PER_SIMDGROUP {
            for simdgroups in SIMDGROUPS {
                for mul16 in [false, true] {
                    tiles.push(Tile {
                        rows_per_simdgroup,
                        simdgroups,
                        mul16,
                    });
                }
            }
        }
        tiles
    }

    /// Short label, for example `r4s2m32`.
    pub fn label(&self) -> String {
        format!(
            "r{}s{}{}",
            self.rows_per_simdgroup,
            self.simdgroups,
            if self.mul16 { "m16" } else { "m32" }
        )
    }

    fn is_valid(&self) -> bool {
        ROWS_PER_SIMDGROUP.contains(&self.rows_per_simdgroup)
            && SIMDGROUPS.contains(&self.simdgroups)
    }
}

/// Where a resident matrix's weight bytes live.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Storage {
    /// CPU-visible (`MTLStorageModeShared`).
    Shared,
    /// GPU-only (`MTLStorageModePrivate`), filled by a blit at upload.
    Private,
}

/// What the device reports about itself.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct DeviceReport {
    pub name: String,
    pub max_buffer_length: u64,
    pub recommended_max_working_set_size: u64,
    pub has_unified_memory: bool,
    pub thread_execution_width: u64,
    pub families: Vec<String>,
}

/// Self-test outcome: every case matched (a mismatch is an error instead).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize)]
pub struct SelfTestReport {
    /// Projections compared element by element with the reference.
    pub compared: usize,
    /// Cases refused, as required, by both the engine and the reference rules.
    pub refused: usize,
}

/// A weight matrix and its per-row scales, resident on the device. Rows are
/// stored with a stride rounded up to 16 bytes; the padding is zero, so it
/// adds nothing to any sum.
pub struct ResidentMatrix {
    weights: Buffer,
    scales: Buffer,
    /// The same scales on the host, for the scale bound of a row range.
    host_scales: Vec<i64>,
    n_rows: usize,
    n_cols: usize,
    stride: usize,
    /// `max_i |s_i|` over every row, or `None` if some scale is `i64::MIN`.
    max_abs_scale: Option<i64>,
    storage: Storage,
}

impl ResidentMatrix {
    /// `max |s_i|` over `rows`, or `None` if one of them is `i64::MIN`: the
    /// scales the CPU limb kernel checks when it projects the same rows.
    fn max_abs_scale_of(&self, rows: &Range<usize>) -> Option<i64> {
        if rows.start == 0 && rows.end == self.n_rows {
            self.max_abs_scale
        } else {
            max_abs(&self.host_scales[rows.clone()])
        }
    }

    pub fn n_rows(&self) -> usize {
        self.n_rows
    }

    pub fn n_cols(&self) -> usize {
        self.n_cols
    }

    /// INT8 weight bytes, excluding padding and scales.
    pub fn weight_bytes(&self) -> usize {
        self.n_rows * self.n_cols
    }

    pub fn storage(&self) -> Storage {
        self.storage
    }
}

/// Per-call device buffers: digit planes in, results out.
struct Scratch {
    digits: Buffer,
    digit_capacity: usize,
    out: Buffer,
    out_capacity: usize,
}

/// Mirrors `ExactGemvParams` in the kernel source.
#[repr(C)]
#[derive(Clone, Copy)]
struct Params {
    rows: u32,
    row_offset: u32,
    blocks: u32,
    mode: u32,
}

/// One encoded projection.
struct Dispatch<'a> {
    pipeline: &'a ComputePipelineState,
    matrix: &'a ResidentMatrix,
    digits: &'a BufferRef,
    out: &'a BufferRef,
    rows: Range<usize>,
    tile: Tile,
    epilogue: Epilogue,
}

/// The exact Metal GEMV engine: one device, one queue, every pipeline.
pub struct MetalExactGemv {
    device: Device,
    queue: CommandQueue,
    pipelines: HashMap<(usize, u32, bool), ComputePipelineState>,
    read_pipeline: ComputePipelineState,
    simd_width: u64,
    scratch: Mutex<Vec<Scratch>>,
    tile: Tile,
}

fn kernel_name(planes: usize, rows: u32, mul16: bool) -> String {
    format!("exact_gemv_p{planes}_r{rows}_m{}", u8::from(mul16))
}

/// GPU execution time of a completed command buffer, in seconds.
///
/// metal 0.29 has no accessor for these properties, so they are read with
/// objc's typed message send (not its `msg_send!` macro, which expands an
/// undeclared `cargo-clippy` cfg).
fn gpu_seconds(commands: &CommandBufferRef) -> f64 {
    let read = |property: &str| -> f64 {
        // SAFETY: GPUStartTime and GPUEndTime are read-only CFTimeInterval
        // (f64) properties of MTLCommandBuffer, and the buffer has completed.
        unsafe { commands.send_message::<(), f64>(Sel::register(property), ()) }
            .expect("MTLCommandBuffer GPU timestamps")
    };
    read("GPUEndTime") - read("GPUStartTime")
}

/// `max_i |s_i|`, or `None` if some scale is `i64::MIN` (no absolute value).
fn max_abs(scales: &[i64]) -> Option<i64> {
    scales
        .iter()
        .try_fold(0i64, |max, scale| scale.checked_abs().map(|a| max.max(a)))
}

/// `128 * sum_j |x_j| * max_i |s_i| <= i64::MAX`, with every step checked,
/// where `max_abs_scale` is taken over the rows being projected.
///
/// Multiplication by a non-negative bound is monotonic, so this holds exactly
/// when `canonical_simd::post_scale_bound_holds` holds for every one of those
/// rows: for a whole matrix and for a row range alike. A scale of `i64::MIN`
/// has no absolute value and is refused, as on the CPU.
pub fn scale_bound_holds(input: &[i64], max_abs_scale: Option<i64>) -> bool {
    let Some(scale) = max_abs_scale else {
        return false;
    };
    input
        .iter()
        .try_fold(0i64, |sum, value| sum.checked_add(value.checked_abs()?))
        .and_then(|sum| sum.checked_mul(128))
        .and_then(|bound| bound.checked_mul(scale))
        .is_some()
}

/// Write `input` as four balanced base-256 digit planes of `stride` bytes each
/// (plane `d` at `planes[d * stride..]`, zero padded), returning the number of
/// planes in use (the highest non-zero digit, at least 1), or `None` if any
/// element is outside `[PLANE_MIN, PLANE_MAX]`.
///
/// The digits are those of `canonical_simd::split_limbs`, and the
/// reconstruction of every element is checked, so a decomposition error can
/// only refuse, never change a value.
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

/// Independent reference: i128 sums, then the requested epilogue. Equal to the
/// CPU's i64 arithmetic for every input the engine accepts.
pub fn reference_rows(
    data: &[i8],
    scales: &[i64],
    n_cols: usize,
    input: &[i64],
    epilogue: Epilogue,
) -> Vec<i64> {
    data.chunks_exact(n_cols)
        .zip(scales)
        .map(|(row, &scale)| {
            let acc: i128 = row
                .iter()
                .zip(input)
                .map(|(&w, &x)| i128::from(w) * i128::from(x))
                .sum();
            match epilogue {
                Epilogue::CanonicalQ16 => ((acc * i128::from(scale)) >> 16) as i64,
                Epilogue::RawDot => acc as i64,
            }
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

/// Largest magnitude whose balanced digits fit in `planes` planes.
fn plane_limit(planes: usize) -> i64 {
    (0..planes).fold(0i64, |total, d| total + 127 * (1i64 << (8 * d)))
}

impl MetalExactGemv {
    /// Open the system default Metal device, compile every pipeline and run
    /// the self-test. Returns an error rather than an engine that disagrees
    /// with the CPU.
    pub fn new() -> Result<Self, String> {
        let engine = autoreleasepool(Self::build)?;
        engine.self_test(1)?;
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
            .map_err(|e| format!("exact GEMV kernels failed to compile: {e}"))?;
        let mut pipelines = HashMap::new();
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
                }
            }
        }
        let read_function = library
            .get_function("read_bandwidth", None)
            .map_err(|e| format!("read_bandwidth: {e}"))?;
        let read_pipeline = device
            .new_compute_pipeline_state_with_function(&read_function)
            .map_err(|e| format!("read_bandwidth: {e}"))?;
        // The epilogue hands row r of a simdgroup to lane r.
        let widest = ROWS_PER_SIMDGROUP.iter().copied().max().unwrap_or(1);
        if simd_width == u64::MAX || simd_width < u64::from(widest) {
            return Err(format!(
                "unsupported simdgroup width {simd_width} (need at least {widest})"
            ));
        }
        let queue = device.new_command_queue();
        Ok(Self {
            device,
            queue,
            pipelines,
            read_pipeline,
            simd_width,
            scratch: Mutex::new(Vec::new()),
            tile: Tile::DEFAULT,
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

    /// The tile used by [`Self::project_rows`].
    pub fn tile(&self) -> Tile {
        self.tile
    }

    /// Choose the tile used by [`Self::project_rows`]. Every tile computes the
    /// same integers; this only changes speed.
    pub fn set_tile(&mut self, tile: Tile) -> Result<(), Refusal> {
        if !tile.is_valid() {
            return Err(Refusal::Shape);
        }
        self.tile = tile;
        Ok(())
    }

    /// Copy a row-major INT8 matrix and its per-row scales to the device.
    pub fn upload(
        &self,
        data: &[i8],
        scales: &[i64],
        n_rows: usize,
        n_cols: usize,
        storage: Storage,
    ) -> Result<ResidentMatrix, String> {
        if n_rows == 0
            || n_cols == 0
            || n_rows > i32::MAX as usize
            || n_rows.checked_mul(n_cols) != Some(data.len())
            || scales.len() != n_rows
        {
            return Err(format!(
                "invalid matrix: {n_rows}x{n_cols}, {} weights, {} scales",
                data.len(),
                scales.len()
            ));
        }
        let stride = n_cols.div_ceil(BLOCK_BYTES) * BLOCK_BYTES;
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
                unsafe { std::slice::from_raw_parts_mut(staging.contents().cast::<i8>(), bytes) };
            if stride == n_cols {
                dst.copy_from_slice(data);
            } else {
                for (row, src) in dst.chunks_exact_mut(stride).zip(data.chunks_exact(n_cols)) {
                    let (head, tail) = row.split_at_mut(n_cols);
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
            let scales_buffer = self.device.new_buffer_with_data(
                scales.as_ptr().cast(),
                (n_rows * std::mem::size_of::<i64>()) as u64,
                MTLResourceOptions::StorageModeShared,
            );
            Ok(ResidentMatrix {
                weights,
                scales: scales_buffer,
                host_scales: scales.to_vec(),
                max_abs_scale: max_abs(scales),
                n_rows,
                n_cols,
                stride,
                storage,
            })
        })
    }

    /// Canonical projection of every row with the engine's tile.
    pub fn project(
        &self,
        matrix: &ResidentMatrix,
        input: &[i64],
        output: &mut [i64],
    ) -> Result<usize, Refusal> {
        self.project_rows(matrix, 0..matrix.n_rows, input, output)
    }

    /// Canonical projection of rows `rows` (output index 0 is row
    /// `rows.start`). Returns the digit planes used.
    pub fn project_rows(
        &self,
        matrix: &ResidentMatrix,
        rows: Range<usize>,
        input: &[i64],
        output: &mut [i64],
    ) -> Result<usize, Refusal> {
        self.project_rows_with(
            matrix,
            rows,
            input,
            output,
            self.tile,
            Epilogue::CanonicalQ16,
        )
    }

    /// The general entry point: any row range, tile and epilogue. Writes
    /// `output` only on success.
    pub fn project_rows_with(
        &self,
        matrix: &ResidentMatrix,
        rows: Range<usize>,
        input: &[i64],
        output: &mut [i64],
        tile: Tile,
        epilogue: Epilogue,
    ) -> Result<usize, Refusal> {
        self.check(matrix, &rows, input, output.len(), tile, epilogue)?;
        autoreleasepool(|| {
            let scratch = self.take_scratch(matrix.stride, rows.len());
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
                        epilogue,
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
                output.copy_from_slice(results);
                Ok(planes)
            });
            self.return_scratch(scratch);
            outcome
        })
    }

    /// The same checks, in the same order, as the CPU limb kernel, over the
    /// rows being projected: a row range is checked against its own scales.
    fn check(
        &self,
        matrix: &ResidentMatrix,
        rows: &Range<usize>,
        input: &[i64],
        output_len: usize,
        tile: Tile,
        epilogue: Epilogue,
    ) -> Result<(), Refusal> {
        if rows.start >= rows.end
            || rows.end > matrix.n_rows
            || input.len() != matrix.n_cols
            || output_len != rows.len()
            || !tile.is_valid()
        {
            return Err(Refusal::Shape);
        }
        if matrix.n_cols > MAX_COLS {
            return Err(Refusal::InnerDimAboveI32Bound);
        }
        if epilogue == Epilogue::CanonicalQ16
            && !scale_bound_holds(input, matrix.max_abs_scale_of(rows))
        {
            return Err(Refusal::ScaleMultiplyWouldOverflow);
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
        matrix: &ResidentMatrix,
        input: &[i64],
    ) -> Result<usize, Refusal> {
        let len = MAX_PLANES * matrix.stride;
        // SAFETY: this call owns `scratch` (taken from the pool), its digit
        // buffer holds at least `len` bytes, and no command buffer that uses
        // it is in flight.
        let planes =
            unsafe { std::slice::from_raw_parts_mut(scratch.digits.contents().cast::<i8>(), len) };
        split_planes(input, planes, matrix.stride).ok_or(Refusal::ActivationOutOfDomain)
    }

    fn encode(&self, encoder: &ComputeCommandEncoderRef, job: &Dispatch<'_>) {
        let fit = (job.pipeline.max_total_threads_per_threadgroup() / self.simd_width).max(1);
        let simdgroups = u64::from(job.tile.simdgroups).min(fit);
        let rows_per_group = simdgroups * u64::from(job.tile.rows_per_simdgroup);
        let groups = (job.rows.len() as u64).div_ceil(rows_per_group);
        let params = Params {
            rows: job.rows.len() as u32,
            row_offset: job.rows.start as u32,
            blocks: (job.matrix.stride / BLOCK_BYTES) as u32,
            mode: match job.epilogue {
                Epilogue::CanonicalQ16 => 0,
                Epilogue::RawDot => 1,
            },
        };
        encoder.set_compute_pipeline_state(job.pipeline);
        encoder.set_buffer(0, Some(&job.matrix.weights), 0);
        encoder.set_buffer(1, Some(job.digits), 0);
        encoder.set_buffer(2, Some(&job.matrix.scales), 0);
        encoder.set_buffer(3, Some(job.out), 0);
        encoder.set_bytes(
            4,
            std::mem::size_of::<Params>() as u64,
            std::ptr::from_ref(&params).cast(),
        );
        encoder.dispatch_thread_groups(
            MTLSize::new(groups, 1, 1),
            MTLSize::new(simdgroups * self.simd_width, 1, 1),
        );
    }

    fn new_scratch(&self, stride: usize, rows: usize) -> Scratch {
        let digit_capacity = MAX_PLANES * stride;
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

    fn take_scratch(&self, stride: usize, rows: usize) -> Scratch {
        let reused = {
            let mut pool = self.scratch.lock().unwrap_or_else(PoisonError::into_inner);
            pool.iter()
                .position(|s| s.digit_capacity >= MAX_PLANES * stride && s.out_capacity >= rows)
                .map(|index| pool.swap_remove(index))
        };
        reused.unwrap_or_else(|| self.new_scratch(stride, rows))
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

    /// GPU seconds for one pass over `items`, each its own canonical
    /// projection of every row, averaged over `repeats` passes encoded in one
    /// command buffer. The work is identical to [`Self::project`]; only the
    /// host round trip per projection is removed.
    pub fn time_batch(
        &self,
        items: &[(&ResidentMatrix, &[i64])],
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
                self.check(
                    matrix,
                    &rows,
                    input,
                    matrix.n_rows,
                    tile,
                    Epilogue::CanonicalQ16,
                )?;
                let scratch = self.new_scratch(matrix.stride, matrix.n_rows);
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
                            epilogue: Epilogue::CanonicalQ16,
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

    /// Read bandwidth of the device: GPU seconds to read `bytes` once, for
    /// each of `repeats` runs after one warm-up, sorted ascending.
    pub fn measure_read_bandwidth(
        &self,
        bytes: usize,
        repeats: usize,
        storage: Storage,
    ) -> Result<Vec<f64>, String> {
        let words = bytes / 16;
        if words == 0 || words > u32::MAX as usize || repeats == 0 {
            return Err(format!("unsupported probe size {bytes}"));
        }
        let pattern: Vec<i8> = (0..words * 16).map(|i| (i % 251) as i8).collect();
        let source = self.upload(&pattern, &[1], 1, pattern.len(), storage)?;
        autoreleasepool(|| {
            let sink = self.device.new_buffer(
                READ_PROBE_THREADS * std::mem::size_of::<u32>() as u64,
                MTLResourceOptions::StorageModeShared,
            );
            let count = words as u32;
            let mut times = Vec::with_capacity(repeats + 1);
            for _ in 0..=repeats {
                let commands = self.queue.new_command_buffer();
                let encoder = commands.new_compute_command_encoder();
                encoder.set_compute_pipeline_state(&self.read_pipeline);
                encoder.set_buffer(0, Some(&source.weights), 0);
                encoder.set_buffer(1, Some(&sink), 0);
                encoder.set_bytes(
                    2,
                    std::mem::size_of::<u32>() as u64,
                    std::ptr::from_ref(&count).cast(),
                );
                encoder.dispatch_thread_groups(
                    MTLSize::new(READ_PROBE_THREADS / 256, 1, 1),
                    MTLSize::new(256, 1, 1),
                );
                encoder.end_encoding();
                commands.commit();
                commands.wait_until_completed();
                if commands.status() != MTLCommandBufferStatus::Completed {
                    return Err("read-bandwidth probe did not complete".to_string());
                }
                times.push(gpu_seconds(commands));
            }
            times.remove(0);
            times.sort_by(f64::total_cmp);
            Ok(times)
        })
    }

    /// Run every pipeline variant against [`reference_rows`] on random and
    /// boundary inputs. Any mismatch, or any refusal where the domain rules
    /// accept, is an error naming the case.
    pub fn self_test(&self, rounds: usize) -> Result<SelfTestReport, String> {
        let mut rng = SplitMix64(SELF_TEST_SEED);
        let mut report = SelfTestReport::default();
        let tiles = Tile::all();
        for round in 0..rounds {
            for &(n_rows, n_cols) in &[(1usize, 1usize), (7, 17), (9, 64), (33, 100)] {
                let data: Vec<i8> = (0..n_rows * n_cols).map(|_| rng.next_u64() as i8).collect();
                // Signed scales up to 2^12 exercise the floor shift of negative
                // products; the canonical profile itself has scale >= 1.
                let scales: Vec<i64> = (0..n_rows).map(|_| rng.symmetric(4096)).collect();
                let matrix = self.upload(&data, &scales, n_rows, n_cols, Storage::Shared)?;
                for planes in 1..=MAX_PLANES {
                    let limit = plane_limit(planes);
                    let mut input: Vec<i64> = (0..n_cols).map(|_| rng.symmetric(limit)).collect();
                    // Pin the top digit plane so this variant really runs.
                    input[0] = if round.is_multiple_of(2) {
                        limit
                    } else {
                        -limit
                    };
                    for &tile in &tiles {
                        for epilogue in [Epilogue::CanonicalQ16, Epilogue::RawDot] {
                            self.compare(
                                &matrix,
                                &data,
                                &scales,
                                &input,
                                tile,
                                epilogue,
                                &mut report,
                            )?;
                        }
                    }
                }
            }
        }
        self.boundary_cases(&mut report)?;
        Ok(report)
    }

    #[allow(clippy::too_many_arguments)]
    fn compare(
        &self,
        matrix: &ResidentMatrix,
        data: &[i8],
        scales: &[i64],
        input: &[i64],
        tile: Tile,
        epilogue: Epilogue,
        report: &mut SelfTestReport,
    ) -> Result<(), String> {
        let mut got = vec![0i64; matrix.n_rows];
        let case = format!(
            "{}x{} tile {} {epilogue:?}",
            matrix.n_rows,
            matrix.n_cols,
            tile.label()
        );
        self.project_rows_with(matrix, 0..matrix.n_rows, input, &mut got, tile, epilogue)
            .map_err(|refusal| {
                format!("self-test {case}: refused an in-domain case ({refusal})")
            })?;
        let want = reference_rows(data, scales, matrix.n_cols, input, epilogue);
        if let Some(row) = got.iter().zip(&want).position(|(g, w)| g != w) {
            return Err(format!(
                "self-test {case}: row {row} is {} on the GPU and {} on the CPU",
                got[row], want[row]
            ));
        }
        report.compared += 1;
        Ok(())
    }

    /// Exact edges: the i32 digit-sum bound, the scale-overflow boundary,
    /// negative products near the floor shift, and refusals.
    fn boundary_cases(&self, report: &mut SelfTestReport) -> Result<(), String> {
        // Every digit of PLANE_MIN is -128, so with weights of -128 each plane
        // sum is 16,384 * 131,071 = 2,147,467,264: the largest the bound allows.
        let cols = MAX_COLS;
        let data = vec![-128i8; 2 * cols];
        let input = vec![PLANE_MIN; cols];
        let dot_bound = 128 * (cols as i64) * PLANE_MIN.abs();
        let largest = i64::MAX / dot_bound;
        let scales = [largest, -largest];
        let matrix = self.upload(&data, &scales, 2, cols, Storage::Shared)?;
        for tile in [
            Tile::DEFAULT,
            Tile {
                rows_per_simdgroup: 1,
                simdgroups: 1,
                mul16: false,
            },
        ] {
            for epilogue in [Epilogue::CanonicalQ16, Epilogue::RawDot] {
                self.compare(&matrix, &data, &scales, &input, tile, epilogue, report)?;
            }
        }
        // One step past the scale bound must be refused, output untouched.
        let over = [largest + 1, 1];
        let refused = self.upload(&data, &over, 2, cols, Storage::Shared)?;
        let mut untouched = [i64::MIN; 2];
        if self.project(&refused, &input, &mut untouched)
            != Err(Refusal::ScaleMultiplyWouldOverflow)
            || untouched != [i64::MIN; 2]
        {
            return Err("self-test: the scale bound + 1 was not refused cleanly".to_string());
        }
        report.refused += 1;

        // Floor shift of small negative products: -1 >> 16 = -1, and products
        // straddling multiples of 2^16.
        let data = [1i8, 1, 1];
        let scales = [1i64, 65_536, -65_537];
        let matrix = self.upload(&data, &scales, 3, 1, Storage::Shared)?;
        for value in [-1i64, 1, -65_536, 65_535, -65_537, 0] {
            self.compare(
                &matrix,
                &data,
                &scales,
                &[value],
                Tile::DEFAULT,
                Epilogue::CanonicalQ16,
                report,
            )?;
        }

        // Activations one outside the digit domain are refused.
        for bad in [PLANE_MAX + 1, PLANE_MIN - 1, i64::MAX, i64::MIN] {
            let mut out = [7i64; 3];
            if self.project(&matrix, &[bad], &mut out).is_ok() || out != [7i64; 3] {
                return Err(format!(
                    "self-test: activation {bad} was not refused cleanly"
                ));
            }
            report.refused += 1;
        }
        // A scale of i64::MIN has no absolute value: refused.
        let matrix = self.upload(&[1], &[i64::MIN], 1, 1, Storage::Shared)?;
        let mut out = [7i64];
        if self.project(&matrix, &[1], &mut out) != Err(Refusal::ScaleMultiplyWouldOverflow) {
            return Err("self-test: an i64::MIN scale was not refused".to_string());
        }
        report.refused += 1;
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
    }

    #[test]
    fn split_planes_reconstructs_edges_and_refuses_outside() {
        let edges = [
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
            32_767,
            32_768,
            -32_768,
            16_777_216,
            PLANE_MAX,
            PLANE_MIN,
            PLANE_MAX - 1,
            PLANE_MIN + 1,
        ];
        let stride = 32;
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
        for bad in [PLANE_MAX + 1, PLANE_MIN - 1, i64::MAX, i64::MIN] {
            assert_eq!(split_planes(&[1, bad, 3], &mut planes, stride), None);
        }
        assert_eq!(split_planes(&[5, -7], &mut planes, stride), Some(1));
    }

    #[test]
    fn scale_bound_matches_the_cpu_rule() {
        let input = vec![1i64; 32];
        let dot_bound = 128 * 32;
        assert!(scale_bound_holds(&input, Some(i64::MAX / dot_bound)));
        assert!(!scale_bound_holds(&input, Some(i64::MAX / dot_bound + 1)));
        assert!(!scale_bound_holds(&input, None));
        assert!(!scale_bound_holds(&[i64::MIN], Some(1)));
    }

    #[test]
    fn self_test_passes_on_this_device() {
        let engine = MetalExactGemv::new().expect("a Metal device that matches the CPU");
        let report = engine.self_test(3).expect("self-test");
        assert!(report.compared > 0 && report.refused > 0, "{report:?}");
        eprintln!("device: {:?}", engine.report());
        eprintln!("self-test (3 rounds): {report:?}");
    }

    #[test]
    fn refusals_leave_the_output_untouched() {
        let engine = MetalExactGemv::new().expect("Metal device");
        let matrix = engine
            .upload(&[1, 2, 3, 4], &[1, 1], 2, 2, Storage::Shared)
            .expect("upload");
        let mut out = [9i64; 2];
        assert_eq!(engine.project(&matrix, &[1], &mut out), Err(Refusal::Shape));
        assert_eq!(
            engine.project_rows(&matrix, 1..1, &[1, 1], &mut out[..0]),
            Err(Refusal::Shape)
        );
        assert_eq!(
            engine.project(&matrix, &[PLANE_MAX + 1, 0], &mut out),
            Err(Refusal::ActivationOutOfDomain)
        );
        assert_eq!(
            engine.project(&matrix, &[i64::MAX, 0], &mut out),
            Err(Refusal::ScaleMultiplyWouldOverflow)
        );
        assert_eq!(out, [9, 9]);
        let wide = engine
            .upload(
                &vec![1i8; MAX_COLS + 1],
                &[1],
                1,
                MAX_COLS + 1,
                Storage::Shared,
            )
            .expect("upload");
        let mut one = [9i64];
        assert_eq!(
            engine.project(&wide, &vec![1i64; MAX_COLS + 1], &mut one),
            Err(Refusal::InnerDimAboveI32Bound)
        );
        assert_eq!(one, [9]);
    }

    #[test]
    fn private_storage_computes_the_same_integers() {
        let engine = MetalExactGemv::new().expect("Metal device");
        let mut rng = SplitMix64(7);
        let (rows, cols) = (37, 300);
        let data: Vec<i8> = (0..rows * cols).map(|_| rng.next_u64() as i8).collect();
        let scales: Vec<i64> = (0..rows).map(|_| 1 + rng.symmetric(2000).abs()).collect();
        let input: Vec<i64> = (0..cols).map(|_| rng.symmetric(PLANE_MAX)).collect();
        let want = reference_rows(&data, &scales, cols, &input, Epilogue::CanonicalQ16);
        for storage in [Storage::Shared, Storage::Private] {
            let matrix = engine
                .upload(&data, &scales, rows, cols, storage)
                .expect("upload");
            let mut got = vec![0i64; rows];
            engine
                .project(&matrix, &input, &mut got)
                .expect("in domain");
            assert_eq!(got, want, "{storage:?}");
        }
    }
}
