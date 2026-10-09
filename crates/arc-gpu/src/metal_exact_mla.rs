//! Exact MLA attention on Apple GPUs, through native Metal: every head of an
//! INT16 layer in one command buffer.
//!
//! [`MetalExactMla::layer`] computes what the per-head loop of the MLA + MoE
//! profile's `layer_forward` (arc-inference, `modern/mla/model.rs`) computes
//! for every head of one layer, between the query projection and the output
//! projection:
//!
//! 1. every head's `wk_b` projection of its non-RoPE query: the exact INT16
//!    head GEMV of [`crate::metal_exact_i16`], then the dyadic epilogue;
//! 2. absorbed attention over the latent cache (`ops::mla_attend`): scores,
//!    the two-pass softmax with the profile's exp table, the weighted latent
//!    sum and its truncating division;
//! 3. every head's `wv_b` projection of the attention output, then its
//!    epilogue.
//!
//! Every head of the layer, attention included, is encoded in one command
//! buffer: one commit and one wait, with no host step between the projections.
//! The rest of the layer (its query, KV-A and output projections, the cache
//! row, the FFN) is not part of it. The caller supplies each head's
//! rotated RoPE query (the CPU's own `rope_interleaved`, computed before the
//! layer is encoded) and keeps the device's copy of the latent cache, a
//! [`DeviceLatentCache`], in step with its own.
//!
//! # Exact or refuse
//!
//! Every value is byte-identical to the CPU's. The kernels
//! (`metal_exact_mla.metal`) reproduce the CPU's i64 and i128 arithmetic with
//! 64-bit words, and the proof of every bound they rely on is at the top of
//! that file. Before encoding, the host checks the conditions those proofs
//! assume: shapes, the row scales' domain, every head's accumulator guard,
//! `0 < lambda < 2^31`, the rank and RoPE widths, and the cached rows.
//! Wherever the CPU would return an error (a projection output beyond 2^62,
//! an attention score whose product overflows i128 or whose floor is beyond
//! 2^62), a kernel sets a status bit and the layer is refused as a whole:
//! nothing is written to the output, and the caller runs the CPU loop, which
//! returns the CPU's own error. No floating point, division or modulo
//! appears in the kernels (`mla_source_is_integer_only` checks).
//!
//! # Device memory
//!
//! The cache copies and the scratch buffers of one engine draw on a single
//! device-memory budget ([`MetalExactMla::set_device_budget`]; by default a
//! quarter of the device's recommended working set). A reservation that would
//! exceed it is refused ([`MlaRefusal::Memory`]) and the caller computes on the
//! CPU, so a long context declines instead of exhausting memory. Every buffer
//! is checked to have been allocated before it is written. The scratch that
//! grows with the context is sized to the next power of two of the positions,
//! so a decode reuses it from token to token.
//!
//! # The cache copy and the host cache
//!
//! [`MetalExactMla::sync`] copies only the rows a [`DeviceLatentCache`] does not
//! hold yet: it relies on the host cache only appending. It also checks that
//! assumption every time: if the host cache holds fewer rows than the copy, or
//! the copy's last row no longer equals the host's, the copy starts over from
//! the host cache, and the sync reports it.
//!
//! # Start-up self-test
//!
//! [`MetalExactMla::new`] compiles the kernels, then runs
//! [`MetalExactMla::self_test`] against [`reference_layer`], an independent
//! i128 implementation in this module: narrow and wide queries, the exp
//! table's edges, a padded rank, more positions than a score stripe, cached
//! values at the i32 extremes, and each of the three refusals. An engine that
//! differs from the reference on one integer is never returned.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use metal::{
    Buffer, BufferRef, CompileOptions, ComputeCommandEncoderRef, ComputePipelineState, DeviceRef,
    MTLCommandBufferStatus, MTLResourceOptions, MTLSize,
};
use objc::rc::autoreleasepool;

use crate::metal_exact::{SelfTestReport, Tile};
use crate::metal_exact_i16::{
    ACCUMULATOR_GUARD, BLOCK_WEIGHTS, HeadPhase, MAX_PLANES, MetalExactI16, Refusal, ResidentI16,
    SplitMix64, gpu_seconds, le_bytes, reference_dots, split_heads_into,
};

/// Source of the attention kernels.
pub const MLA_SOURCE: &str = include_str!("metal_exact_mla.metal");

/// Status bit: a `wk_b` projection output beyond 2^62.
pub const STATUS_KEY: u32 = 1;
/// Status bit: an attention score whose product overflows i128 or whose
/// floor is beyond 2^62.
pub const STATUS_SCORE: u32 = 2;
/// Status bit: an attention output beyond 2^31 (unreachable; see bound 8).
pub const STATUS_OUTPUT: u32 = 4;
/// Status bit: a `wv_b` projection output beyond 2^62.
pub const STATUS_VALUE: u32 = 8;

/// Entries of the profile's exp table (`tables::EXP_TABLE`).
pub const EXP_TABLE_LEN: usize = 4097;
/// Widest latent rank the bounds are proved for (the profile's own limit).
pub const MAX_RANK: usize = 1 << 16;
/// Widest RoPE part the bounds are proved for (the profile's own limit).
pub const MAX_ROPE_DIM: usize = 1 << 12;
/// `lambda` must lie in `1..LAMBDA_LIMIT`.
pub const LAMBDA_LIMIT: i64 = 1 << 31;
/// Most positions one layer attends.
pub const MAX_POSITIONS: usize = (1 << 31) - 1;
/// Positions a long adds before it flushes into the 128-bit sum (the
/// kernel's MLA_FLUSH).
pub const FLUSH_POSITIONS: u64 = 1 << 15;

const ACTIVATION_LIMIT: u128 = 1 << 62;
/// Positions per simdgroup in the score kernel.
const SCORE_STRIPE: u32 = 16;
/// Simdgroups (heads) per threadgroup in the score and output kernels.
const HEADS_PER_GROUP: u64 = 4;
/// Threads per threadgroup of the softmax kernel (a power of two).
const SOFTMAX_THREADS: u64 = 256;
const EPILOGUE_THREADS: u64 = 256;
const SCRATCH_POOL: usize = 4;
/// Rows a new device cache reserves at least.
const MIN_CACHE_ROWS: usize = 16;
const SELF_TEST_SEED: u64 = 0x00A2_C0DE_5EED_0A77;

// The bounds of metal_exact_mla.metal, for the limits above.
// 2: a lane's mass sum stays below 2^49.
const _: () = assert!((((MAX_RANK + MAX_ROPE_DIM) as u128) << 32) < 1u128 << 49);
// 4: |q * c| <= 2^93 over at most MAX_RANK + MAX_ROPE_DIM terms.
const _: () = assert!((((MAX_RANK + MAX_ROPE_DIM) as u128) << 93) < 1u128 << 110);
// 6: total <= positions * 2^16 < 2^47.
const _: () = assert!(((MAX_POSITIONS as u64) << 16) < 1 << 47);
// 7: a long adds at most FLUSH_POSITIONS products of magnitude <= 2^47.
const _: () = assert!(FLUSH_POSITIONS << 47 <= 1 << 62);
// 8: sum |u| <= MAX_RANK * 2^31 is below the INT16 accumulator guard.
const _: () = assert!(((MAX_RANK as u128) << 31) < ACCUMULATOR_GUARD);

/// Why a layer was not computed on the GPU. Nothing was written to the
/// output.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MlaRefusal {
    /// A shape, row scale, `lambda` or width outside what the kernels are
    /// proved for.
    Shape,
    /// A head's query at or above the INT16 accumulator guard: the CPU
    /// projection refuses it too.
    AccumulatorBound,
    /// A query outside the seven-digit domain. Unreachable for guarded
    /// queries; kept so that a split error can only refuse.
    DigitSplit,
    /// The cache copy does not hold the attended positions, or the cache
    /// does not fit the device.
    Cache,
    /// The kernels set these status bits: the CPU refuses the same layer.
    Status(u32),
    /// The command buffer did not complete.
    Device,
    /// The device-memory budget would be exceeded, or a buffer could not be
    /// allocated.
    Memory,
}

impl From<Refusal> for MlaRefusal {
    fn from(refusal: Refusal) -> Self {
        match refusal {
            Refusal::Shape => MlaRefusal::Shape,
            Refusal::AccumulatorBound => MlaRefusal::AccumulatorBound,
            Refusal::DigitSplit => MlaRefusal::DigitSplit,
            Refusal::Device => MlaRefusal::Device,
        }
    }
}

impl std::fmt::Display for MlaRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            MlaRefusal::Shape => f.write_str("shape, scale or parameter mismatch"),
            MlaRefusal::AccumulatorBound => {
                f.write_str("a query at or above the INT16 accumulator guard")
            }
            MlaRefusal::DigitSplit => f.write_str("a query outside the seven-digit domain"),
            MlaRefusal::Cache => f.write_str("the device cache does not hold the positions"),
            MlaRefusal::Status(bits) => write!(f, "the kernels refused (status {bits:#x})"),
            MlaRefusal::Device => f.write_str("GPU command buffer did not complete"),
            MlaRefusal::Memory => {
                f.write_str("the attention path's device-memory budget would be exceeded")
            }
        }
    }
}

/// One MLA layer's heads at one position: `key` (`wk_b`, `heads` matrices of
/// `rank` x `nope`) and `value` (`wv_b`, `heads` matrices of `v_dim` x
/// `rank`) with their row scales, every head's queries, and the attention
/// parameters. Head h's rows are rows `h * rows..(h + 1) * rows` of the
/// scales, as of the phases.
#[derive(Clone, Copy)]
pub struct MlaLayer<'a> {
    pub key: HeadPhase<'a>,
    pub key_mu: &'a [i32],
    pub key_k: &'a [u8],
    pub value: HeadPhase<'a>,
    pub value_mu: &'a [i32],
    pub value_k: &'a [u8],
    /// `heads x nope`: every head's non-RoPE query.
    pub queries: &'a [i64],
    /// `heads x rope_dim`: every head's RoPE query, rotated.
    pub rope_queries: &'a [i64],
    /// The attention scale, in `1..LAMBDA_LIMIT`.
    pub lambda: i64,
    /// Positions attended: rows `0..positions` of the cache.
    pub positions: usize,
}

/// The attention path's device memory: what its cache copies and scratch
/// buffers hold, against a limit.
#[derive(Debug)]
struct DeviceBudget {
    limit: AtomicU64,
    used: AtomicU64,
}

impl DeviceBudget {
    /// Hold `bytes` until the returned reservation drops, or `None` if that
    /// would exceed the limit.
    fn reserve(self: &Arc<Self>, bytes: u64) -> Option<Reservation> {
        let mut used = self.used.load(Ordering::Acquire);
        loop {
            let next = used.checked_add(bytes)?;
            if next > self.limit.load(Ordering::Acquire) {
                return None;
            }
            match self
                .used
                .compare_exchange_weak(used, next, Ordering::AcqRel, Ordering::Acquire)
            {
                Ok(_) => {
                    return Some(Reservation {
                        budget: Arc::clone(self),
                        bytes,
                    });
                }
                Err(actual) => used = actual,
            }
        }
    }
}

/// Bytes held against the budget until dropped.
#[derive(Debug)]
struct Reservation {
    budget: Arc<DeviceBudget>,
    bytes: u64,
}

impl Drop for Reservation {
    fn drop(&mut self) {
        self.budget.used.fetch_sub(self.bytes, Ordering::AcqRel);
    }
}

/// A shared-storage buffer of at least `bytes` bytes (16 at least), checked to
/// have been allocated: Metal returns nil when it cannot.
fn shared_buffer(device: &DeviceRef, bytes: usize) -> Result<Buffer, MlaRefusal> {
    let buffer = device.new_buffer(bytes.max(16) as u64, MTLResourceOptions::StorageModeShared);
    if buffer.contents().is_null() {
        return Err(MlaRefusal::Memory);
    }
    Ok(buffer)
}

/// What one [`MetalExactMla::sync`] did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SyncReport {
    /// Rows copied from the host cache.
    pub copied: usize,
    /// Whether the copy started over because the host cache had shrunk or its
    /// last copied row had changed.
    pub reset: bool,
}

/// The device's copy of one layer's latent cache: the first [`Self::rows`]
/// positions of the caller's cache, `rank` latent values and `rope_dim` RoPE
/// key values (i32) each. [`MetalExactMla::sync`] appends rows; rows already
/// on the device are never compared again, so the caller's cache must only
/// ever append.
pub struct DeviceLatentCache {
    latent: Buffer,
    rope_keys: Buffer,
    rank: usize,
    rope_dim: usize,
    capacity: usize,
    rows: usize,
    /// The buffers' bytes, held against the engine's budget.
    _reservation: Option<Reservation>,
}

impl DeviceLatentCache {
    pub fn rank(&self) -> usize {
        self.rank
    }

    pub fn rope_dim(&self) -> usize {
        self.rope_dim
    }

    /// Positions held.
    pub fn rows(&self) -> usize {
        self.rows
    }

    /// Positions held without growing.
    pub fn capacity(&self) -> usize {
        self.capacity
    }

    /// The rows held: latents, then RoPE keys, row-major.
    pub fn read(&self) -> (Vec<i32>, Vec<i32>) {
        // SAFETY: shared buffers of `capacity` rows, and `rows <= capacity`.
        // `MetalExactMla::layer` waits until its command buffer has finished
        // before it returns, and `sync`, the only writer, needs `&mut self`,
        // so nothing writes these buffers while `&self` is borrowed.
        unsafe {
            (
                read(&self.latent, self.rows * self.rank),
                read(&self.rope_keys, self.rows * self.rope_dim),
            )
        }
    }
}

/// Intermediate values of one layer, for checks: the boundaries between the
/// kernels.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MlaProbe {
    /// `heads x rank`: every head's absorbed query (the `wk_b` output).
    pub qa: Vec<i64>,
    /// `heads x positions`: every head's attention scores.
    pub scores: Vec<i64>,
    /// `heads x rank`: every head's attention output (the `wv_b` input).
    pub u: Vec<i64>,
}

/// Mirrors `EpilogueParams` in the kernel source.
#[repr(C)]
#[derive(Clone, Copy)]
struct EpilogueParams {
    count: u32,
    status_bit: u32,
    reserved0: u32,
    reserved1: u32,
}

/// Mirrors `ScoreParams` in the kernel source.
#[repr(C)]
#[derive(Clone, Copy)]
struct ScoreParams {
    heads: u32,
    rank: u32,
    rope_dim: u32,
    positions: u32,
    stripe: u32,
    reserved: u32,
    lambda: i64,
}

/// Mirrors `SoftmaxParams` in the kernel source.
#[repr(C)]
#[derive(Clone, Copy)]
struct SoftmaxParams {
    heads: u32,
    positions: u32,
    reserved0: u32,
    reserved1: u32,
}

/// Mirrors `WeightedParams` in the kernel source.
#[repr(C)]
#[derive(Clone, Copy)]
struct WeightedParams {
    heads: u32,
    rank: u32,
    positions: u32,
    padded: u32,
}

const _: () = assert!(std::mem::size_of::<ScoreParams>() == 32);
const _: () = assert!(std::mem::size_of::<EpilogueParams>() == 16);

/// Element counts of one layer's device buffers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Sizes {
    /// Bytes of the `wk_b` digit planes.
    digits_a: usize,
    /// `heads * rank`.
    key_rows: usize,
    /// `heads * rope_dim`.
    rope: usize,
    /// `heads * positions`.
    scores: usize,
    heads: usize,
    /// Bytes of the `wv_b` digit planes.
    digits_b: usize,
    /// `heads * v_dim`.
    value_rows: usize,
}

impl Sizes {
    /// The same sizes with room for the positions up to the next power of two,
    /// so that a decode reuses its scratch from token to token.
    fn with_slack(self) -> Sizes {
        let positions = (self.scores / self.heads.max(1)).max(1);
        Sizes {
            scores: self.heads.max(1) * positions.next_power_of_two(),
            ..self
        }
    }

    /// Bytes of a scratch set of these sizes.
    fn bytes(&self) -> usize {
        self.digits_a
            + 24 * self.key_rows
            + 5 * self.key_rows
            + 8 * self.rope
            + 8 * self.scores * 2
            + 8 * self.heads
            + self.digits_b
            + 16 * self.value_rows
            + 5 * self.value_rows
            + 4
    }

    fn covers(&self, other: &Sizes) -> bool {
        self.digits_a >= other.digits_a
            && self.key_rows >= other.key_rows
            && self.rope >= other.rope
            && self.scores >= other.scores
            && self.heads >= other.heads
            && self.digits_b >= other.digits_b
            && self.value_rows >= other.value_rows
    }
}

/// One layer's checked geometry.
#[derive(Debug, Clone, Copy)]
struct Dims {
    heads: usize,
    rank: usize,
    rope_dim: usize,
    v_dim: usize,
    positions: usize,
    /// Digit columns per plane of the `wv_b` input: `rank` rounded up to a
    /// whole 8-value block.
    padded: usize,
    sizes: Sizes,
}

/// Per-call device buffers (shared storage).
struct MlaScratch {
    sizes: Sizes,
    digits_a: Buffer,
    dots_a: Buffer,
    key_mu: Buffer,
    key_k: Buffer,
    qa: Buffer,
    qp: Buffer,
    scores: Buffer,
    weights: Buffer,
    totals: Buffer,
    u: Buffer,
    digits_b: Buffer,
    dots_b: Buffer,
    value_mu: Buffer,
    value_k: Buffer,
    out: Buffer,
    status: Buffer,
    /// The buffers' bytes, held against the engine's budget.
    _reservation: Reservation,
}

/// Copy `values` to the start of a shared buffer.
///
/// # Safety
/// `buffer` must be a shared buffer of at least `values.len()` `T`s that no
/// command buffer is reading or writing.
unsafe fn write<T: Copy>(buffer: &BufferRef, values: &[T]) {
    // SAFETY: guaranteed by the caller.
    let dst =
        unsafe { std::slice::from_raw_parts_mut(buffer.contents().cast::<T>(), values.len()) };
    dst.copy_from_slice(values);
}

/// The first `len` `T`s of a shared buffer.
///
/// # Safety
/// `buffer` must be a shared buffer of at least `len` `T`s that no command
/// buffer is writing.
unsafe fn read<T: Copy>(buffer: &BufferRef, len: usize) -> Vec<T> {
    // SAFETY: guaranteed by the caller.
    unsafe { std::slice::from_raw_parts(buffer.contents().cast::<T>(), len) }.to_vec()
}

/// The CPU projection's scale domain on every row: `mu = 0` with `k = 16`,
/// or `mu >= 2^30` with `16 <= k <= 62` (`precision::project_i16`).
fn scales_admissible(mu: &[i32], k: &[u8]) -> bool {
    mu.len() == k.len()
        && mu
            .iter()
            .zip(k)
            .all(|(&m, &s)| (m == 0 && s == 16) || (m >= 1 << 30 && (16..=62).contains(&s)))
}

/// The exact MLA attention engine: the INT16 engine it shares a device and
/// queue with, the attention pipelines and the exp table.
pub struct MetalExactMla {
    engine: Arc<MetalExactI16>,
    epilogue: ComputePipelineState,
    scores: ComputePipelineState,
    softmax: ComputePipelineState,
    weighted: ComputePipelineState,
    table: Vec<i64>,
    exp_table: Buffer,
    scratch: Mutex<Vec<MlaScratch>>,
    budget: Arc<DeviceBudget>,
    report: SelfTestReport,
}

impl MetalExactMla {
    /// Compile the attention kernels on `engine`'s device, upload the exp
    /// table (`tables::EXP_TABLE`, [`EXP_TABLE_LEN`] entries) and run the
    /// self-test. Returns an error rather than an engine that disagrees with
    /// the reference.
    pub fn new(engine: Arc<MetalExactI16>, exp_table: &[i64]) -> Result<Self, String> {
        if exp_table.len() != EXP_TABLE_LEN {
            return Err(format!(
                "the exp table has {} entries, not {EXP_TABLE_LEN}",
                exp_table.len()
            ));
        }
        let mut mla = autoreleasepool(|| -> Result<Self, String> {
            let device = engine.device();
            let options = CompileOptions::new();
            options.set_fast_math_enabled(false);
            let library = device
                .new_library_with_source(MLA_SOURCE, &options)
                .map_err(|e| format!("exact MLA attention kernels failed to compile: {e}"))?;
            let pipeline = |name: &str| -> Result<ComputePipelineState, String> {
                let function = library
                    .get_function(name, None)
                    .map_err(|e| format!("{name}: {e}"))?;
                device
                    .new_compute_pipeline_state_with_function(&function)
                    .map_err(|e| format!("{name}: {e}"))
            };
            let epilogue = pipeline("mla_epilogue")?;
            let scores = pipeline("mla_scores")?;
            let softmax = pipeline("mla_softmax")?;
            let weighted = pipeline("mla_weighted")?;
            let table_buffer = shared_buffer(device, std::mem::size_of_val(exp_table))
                .map_err(|refusal| format!("the exp table: {refusal}"))?;
            // SAFETY: a new shared buffer of exactly the table's size that no
            // command buffer references yet.
            unsafe { write(&table_buffer, exp_table) };
            Ok(Self {
                engine: Arc::clone(&engine),
                epilogue,
                scores,
                softmax,
                weighted,
                table: exp_table.to_vec(),
                exp_table: table_buffer,
                scratch: Mutex::new(Vec::new()),
                budget: Arc::new(DeviceBudget {
                    limit: AtomicU64::new(device.recommended_max_working_set_size() / 4),
                    used: AtomicU64::new(0),
                }),
                report: SelfTestReport::default(),
            })
        })?;
        mla.report = mla.self_test()?;
        Ok(mla)
    }

    /// The INT16 engine the projections run on.
    pub fn engine(&self) -> &Arc<MetalExactI16> {
        &self.engine
    }

    /// What the start-up self-test compared and refused.
    pub fn self_test_report(&self) -> SelfTestReport {
        self.report
    }

    /// The device-memory budget of the cache copies and scratch buffers, in
    /// bytes (by default a quarter of the device's recommended working set).
    pub fn device_budget(&self) -> u64 {
        self.budget.limit.load(Ordering::Acquire)
    }

    /// Set the device-memory budget. Memory already held stays held; new
    /// reservations beyond the budget are refused.
    pub fn set_device_budget(&self, bytes: u64) {
        self.budget.limit.store(bytes, Ordering::Release);
    }

    /// Device bytes the cache copies and scratch buffers hold now.
    pub fn device_bytes_in_use(&self) -> u64 {
        self.budget.used.load(Ordering::Acquire)
    }

    /// An empty device cache for one layer.
    pub fn new_cache(&self, rank: usize, rope_dim: usize) -> DeviceLatentCache {
        let device = self.engine.device();
        DeviceLatentCache {
            latent: device.new_buffer(16, MTLResourceOptions::StorageModeShared),
            rope_keys: device.new_buffer(16, MTLResourceOptions::StorageModeShared),
            rank,
            rope_dim,
            capacity: 0,
            rows: 0,
            _reservation: None,
        }
    }

    /// Bring `cache` to `positions` rows of `latent` and `rope_keys`
    /// (row-major, `rank` and `rope_dim` values a row). Rows the device already
    /// holds are not copied again, because the host cache only appends; that
    /// is checked first: if the host holds fewer rows than the copy, or the
    /// copy's last row differs from the host's, the copy starts over. A copy
    /// grows to at least twice its capacity, within the device-memory budget,
    /// moving its rows on the device side.
    pub fn sync(
        &self,
        cache: &mut DeviceLatentCache,
        latent: &[i32],
        rope_keys: &[i32],
        positions: usize,
    ) -> Result<SyncReport, MlaRefusal> {
        let (rank, rope_dim) = (cache.rank, cache.rope_dim);
        if rank == 0
            || positions > MAX_POSITIONS
            || positions.checked_mul(rank).is_none_or(|n| latent.len() < n)
            || positions
                .checked_mul(rope_dim)
                .is_none_or(|n| rope_keys.len() < n)
        {
            return Err(MlaRefusal::Cache);
        }
        let mut report = SyncReport::default();
        // The append-only check (see the module documentation).
        if cache.rows > 0 {
            // rank >= 1 (checked above); a zero RoPE width bounds nothing.
            let latent_rows = latent.len() / rank;
            let host_rows = rope_keys
                .len()
                .checked_div(rope_dim)
                .map_or(latent_rows, |rows| rows.min(latent_rows));
            let last = cache.rows - 1;
            let changed = host_rows < cache.rows || {
                // SAFETY: shared buffers of `capacity >= rows` rows; `&mut
                // cache` excludes every other use, and `layer` waits for its
                // command buffer before returning, so no GPU work writes them.
                let (device_latent, device_keys) = unsafe {
                    (
                        std::slice::from_raw_parts(
                            cache.latent.contents().cast::<i32>().add(last * rank),
                            rank,
                        ),
                        std::slice::from_raw_parts(
                            cache
                                .rope_keys
                                .contents()
                                .cast::<i32>()
                                .add(last * rope_dim),
                            rope_dim,
                        ),
                    )
                };
                *device_latent != latent[last * rank..(last + 1) * rank]
                    || *device_keys != rope_keys[last * rope_dim..(last + 1) * rope_dim]
            };
            if changed {
                cache.rows = 0;
                report.reset = true;
            }
        }
        if positions <= cache.rows {
            return Ok(report);
        }
        if positions > cache.capacity {
            // positions <= MAX_POSITIONS (checked above) and MIN_CACHE_ROWS is
            // far below it, so the capacity stays within MAX_POSITIONS.
            let doubled = cache.capacity.saturating_mul(2).min(MAX_POSITIONS);
            let capacity = positions.max(doubled).max(MIN_CACHE_ROWS);
            let max = self.engine.device().max_buffer_length();
            let bytes = |width: usize| {
                capacity
                    .checked_mul(width)
                    .and_then(|n| n.checked_mul(4))
                    .filter(|&b| b as u64 <= max)
            };
            let (Some(latent_bytes), Some(rope_bytes)) = (bytes(rank), bytes(rope_dim)) else {
                return Err(MlaRefusal::Cache);
            };
            // The new buffers are reserved before they exist and while the old
            // ones are still held, which is the peak of a growth.
            let reservation = self
                .budget
                .reserve((latent_bytes.max(16) + rope_bytes.max(16)) as u64)
                .ok_or(MlaRefusal::Memory)?;
            let device = self.engine.device();
            let grown_latent = shared_buffer(device, latent_bytes)?;
            let grown_keys = shared_buffer(device, rope_bytes)?;
            // SAFETY: the old buffers hold `rows` rows and the new ones at
            // least as many; they are distinct allocations, and no GPU work
            // touches either (as above).
            unsafe {
                std::ptr::copy_nonoverlapping(
                    cache.latent.contents().cast::<i32>(),
                    grown_latent.contents().cast::<i32>(),
                    cache.rows * rank,
                );
                std::ptr::copy_nonoverlapping(
                    cache.rope_keys.contents().cast::<i32>(),
                    grown_keys.contents().cast::<i32>(),
                    cache.rows * rope_dim,
                );
            }
            cache.latent = grown_latent;
            cache.rope_keys = grown_keys;
            cache.capacity = capacity;
            cache._reservation = Some(reservation);
        }
        let first = cache.rows;
        // SAFETY: shared buffers of `capacity >= positions` rows, unused by
        // any GPU work (as above).
        unsafe {
            let dst = std::slice::from_raw_parts_mut(
                cache.latent.contents().cast::<i32>(),
                positions * rank,
            );
            dst[first * rank..].copy_from_slice(&latent[first * rank..positions * rank]);
            let dst = std::slice::from_raw_parts_mut(
                cache.rope_keys.contents().cast::<i32>(),
                positions * rope_dim,
            );
            dst[first * rope_dim..]
                .copy_from_slice(&rope_keys[first * rope_dim..positions * rope_dim]);
        }
        cache.rows = positions;
        report.copied = positions - first;
        Ok(report)
    }

    /// The conditions every bound of the kernels assumes, and the CPU
    /// projection's own (shape, scales, every head's guard), in its order.
    fn check(
        &self,
        job: &MlaLayer<'_>,
        cache: &DeviceLatentCache,
        out_len: usize,
        tile: Tile,
    ) -> Result<Dims, MlaRefusal> {
        let heads = job.key.heads;
        let rank = job.key.rows;
        let v_dim = job.value.rows;
        let rope_dim = cache.rope_dim;
        let nope = job.key.matrix.n_cols();
        let positions = job.positions;
        let key_rows = heads.checked_mul(rank);
        let value_rows = heads.checked_mul(v_dim);
        let fits_u32 = |n: Option<usize>| n.is_some_and(|n| n <= u32::MAX as usize);
        if heads == 0
            || rank == 0
            || v_dim == 0
            || job.value.heads != heads
            || job.value.matrix.n_cols() != rank
            || cache.rank != rank
            || rank > MAX_RANK
            || rope_dim > MAX_ROPE_DIM
            || positions == 0
            || positions > MAX_POSITIONS
            || !fits_u32(key_rows)
            || !fits_u32(value_rows)
            || key_rows != Some(job.key_mu.len())
            || value_rows != Some(job.value_mu.len())
            || value_rows != Some(out_len)
            || heads.checked_mul(nope) != Some(job.queries.len())
            || heads.checked_mul(rope_dim) != Some(job.rope_queries.len())
            || !(1..LAMBDA_LIMIT).contains(&job.lambda)
            || job
                .rope_queries
                .iter()
                .any(|v| u128::from(v.unsigned_abs()) > ACTIVATION_LIMIT)
            || !scales_admissible(job.key_mu, job.key_k)
            || !scales_admissible(job.value_mu, job.value_k)
        {
            return Err(MlaRefusal::Shape);
        }
        if positions > cache.rows {
            return Err(MlaRefusal::Cache);
        }
        let (key_rows, value_rows) = (heads * rank, heads * v_dim);
        self.engine.check_heads_shape(&job.key, key_rows, tile)?;
        self.engine
            .check_heads_shape(&job.value, value_rows, tile)?;
        // Every head's query against the guard, as the CPU projection checks.
        self.engine
            .check_heads(&job.key, job.queries, key_rows, tile)?;
        let padded = job.value.matrix.blocks() * BLOCK_WEIGHTS;
        let max = self.engine.device().max_buffer_length();
        let scores = heads.checked_mul(positions);
        let digits_b = heads
            .checked_mul(MAX_PLANES)
            .and_then(|n| n.checked_mul(padded));
        let in_device = |n: Option<usize>, size: usize| {
            n.and_then(|n| n.checked_mul(size))
                .is_some_and(|bytes| bytes as u64 <= max)
        };
        if padded < rank
            || !in_device(scores, 8)
            || !in_device(digits_b, 1)
            || !fits_u32(Some(padded))
        {
            return Err(MlaRefusal::Shape);
        }
        let digits_a = heads * MAX_PLANES * job.key.matrix.blocks() * BLOCK_WEIGHTS;
        Ok(Dims {
            heads,
            rank,
            rope_dim,
            v_dim,
            positions,
            padded,
            sizes: Sizes {
                digits_a,
                key_rows,
                rope: heads * rope_dim,
                scores: heads * positions,
                heads,
                digits_b: heads * MAX_PLANES * padded,
                value_rows,
            },
        })
    }

    /// A scratch set of `sizes`, its bytes reserved against the budget first.
    fn new_scratch(&self, sizes: Sizes) -> Result<MlaScratch, MlaRefusal> {
        let reservation = self
            .budget
            .reserve(sizes.bytes() as u64 + 16 * 16)
            .ok_or(MlaRefusal::Memory)?;
        let device = self.engine.device();
        let buffer = |bytes: usize| shared_buffer(device, bytes);
        Ok(MlaScratch {
            sizes,
            digits_a: buffer(sizes.digits_a)?,
            dots_a: buffer(8 * sizes.key_rows)?,
            key_mu: buffer(4 * sizes.key_rows)?,
            key_k: buffer(sizes.key_rows)?,
            qa: buffer(8 * sizes.key_rows)?,
            qp: buffer(8 * sizes.rope)?,
            scores: buffer(8 * sizes.scores)?,
            weights: buffer(8 * sizes.scores)?,
            totals: buffer(8 * sizes.heads)?,
            u: buffer(8 * sizes.key_rows)?,
            digits_b: buffer(sizes.digits_b)?,
            dots_b: buffer(8 * sizes.value_rows)?,
            value_mu: buffer(4 * sizes.value_rows)?,
            value_k: buffer(sizes.value_rows)?,
            out: buffer(8 * sizes.value_rows)?,
            status: buffer(4)?,
            _reservation: reservation,
        })
    }

    /// Pooled scratch that covers `sizes`, or a new set with slack for the
    /// positions. If the budget refuses a new set, the pool is emptied (its
    /// sets are released) and the set tried once more.
    fn take_scratch(&self, sizes: Sizes) -> Result<MlaScratch, MlaRefusal> {
        let reused = {
            let mut pool = self.scratch.lock().unwrap_or_else(PoisonError::into_inner);
            pool.iter()
                .position(|s| s.sizes.covers(&sizes))
                .map(|index| pool.swap_remove(index))
        };
        if let Some(scratch) = reused {
            return Ok(scratch);
        }
        match self.new_scratch(sizes.with_slack()) {
            Err(MlaRefusal::Memory) => {
                self.scratch
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .clear();
                self.new_scratch(sizes.with_slack())
            }
            other => other,
        }
    }

    fn return_scratch(&self, scratch: MlaScratch) {
        let mut pool = self.scratch.lock().unwrap_or_else(PoisonError::into_inner);
        if pool.len() >= SCRATCH_POOL {
            pool.remove(0);
        }
        pool.push(scratch);
    }

    /// Write the layer's host inputs: the `wk_b` digit planes (returning the
    /// planes in use), the RoPE queries, both stacks' row scales and a clear
    /// status word.
    fn prepare(&self, s: &MlaScratch, job: &MlaLayer<'_>) -> Result<usize, MlaRefusal> {
        // SAFETY: this call owns `s` (taken from the pool), every buffer holds
        // at least the sizes `check` derived from this job, and no command
        // buffer uses them yet.
        unsafe {
            let planes = split_heads_into(&s.digits_a, &job.key, job.queries)?;
            write(&s.qp, job.rope_queries);
            write(&s.key_mu, job.key_mu);
            write(&s.key_k, job.key_k);
            write(&s.value_mu, job.value_mu);
            write(&s.value_k, job.value_k);
            write(&s.status, &[0u32]);
            Ok(planes)
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn encode_epilogue(
        &self,
        encoder: &ComputeCommandEncoderRef,
        dots: &BufferRef,
        mu: &BufferRef,
        k: &BufferRef,
        out: &BufferRef,
        status: &BufferRef,
        count: usize,
        status_bit: u32,
    ) {
        let params = EpilogueParams {
            count: count as u32,
            status_bit,
            reserved0: 0,
            reserved1: 0,
        };
        let threads = EPILOGUE_THREADS
            .min(self.epilogue.max_total_threads_per_threadgroup())
            .max(1);
        encoder.set_compute_pipeline_state(&self.epilogue);
        encoder.set_buffer(0, Some(dots), 0);
        encoder.set_buffer(1, Some(mu), 0);
        encoder.set_buffer(2, Some(k), 0);
        encoder.set_buffer(3, Some(out), 0);
        encoder.set_buffer(4, Some(status), 0);
        encoder.set_bytes(
            5,
            std::mem::size_of::<EpilogueParams>() as u64,
            std::ptr::from_ref(&params).cast(),
        );
        encoder.dispatch_thread_groups(
            MTLSize::new((count as u64).div_ceil(threads), 1, 1),
            MTLSize::new(threads, 1, 1),
        );
    }

    /// Simdgroups per threadgroup for a one-head-per-simdgroup kernel.
    fn heads_per_group(&self, pipeline: &ComputePipelineState) -> u64 {
        let width = self.engine.simd_width();
        HEADS_PER_GROUP
            .min(pipeline.max_total_threads_per_threadgroup() / width)
            .max(1)
    }

    /// Every head's work, in order, on one serial compute encoder: each
    /// dispatch completes before the next starts and sees its writes.
    #[allow(clippy::too_many_arguments)]
    fn encode(
        &self,
        encoder: &ComputeCommandEncoderRef,
        job: &MlaLayer<'_>,
        cache: &DeviceLatentCache,
        s: &MlaScratch,
        d: &Dims,
        pipeline_a: &ComputePipelineState,
        pipeline_b: &ComputePipelineState,
        tile: Tile,
    ) {
        let width = self.engine.simd_width();
        // 1. Every head's wk_b dots, then qa.
        self.engine
            .encode_heads_with(encoder, pipeline_a, &job.key, &s.digits_a, &s.dots_a, tile);
        self.encode_epilogue(
            encoder,
            &s.dots_a,
            &s.key_mu,
            &s.key_k,
            &s.qa,
            &s.status,
            d.heads * d.rank,
            STATUS_KEY,
        );
        // 2. Scores.
        let groups = self.heads_per_group(&self.scores);
        let params = ScoreParams {
            heads: d.heads as u32,
            rank: d.rank as u32,
            rope_dim: d.rope_dim as u32,
            positions: d.positions as u32,
            stripe: SCORE_STRIPE,
            reserved: 0,
            lambda: job.lambda,
        };
        encoder.set_compute_pipeline_state(&self.scores);
        encoder.set_buffer(0, Some(&s.qa), 0);
        encoder.set_buffer(1, Some(&s.qp), 0);
        encoder.set_buffer(2, Some(&cache.latent), 0);
        encoder.set_buffer(3, Some(&cache.rope_keys), 0);
        encoder.set_buffer(4, Some(&s.scores), 0);
        encoder.set_buffer(5, Some(&s.status), 0);
        encoder.set_bytes(
            6,
            std::mem::size_of::<ScoreParams>() as u64,
            std::ptr::from_ref(&params).cast(),
        );
        encoder.dispatch_thread_groups(
            MTLSize::new(
                (d.positions as u64).div_ceil(u64::from(SCORE_STRIPE)),
                (d.heads as u64).div_ceil(groups),
                1,
            ),
            MTLSize::new(groups * width, 1, 1),
        );
        // 3. Weights and their totals: one threadgroup per head.
        let max_threads = self.softmax.max_total_threads_per_threadgroup();
        let mut threads = SOFTMAX_THREADS;
        while threads > max_threads.max(1) {
            threads >>= 1;
        }
        let params = SoftmaxParams {
            heads: d.heads as u32,
            positions: d.positions as u32,
            reserved0: 0,
            reserved1: 0,
        };
        encoder.set_compute_pipeline_state(&self.softmax);
        encoder.set_buffer(0, Some(&s.scores), 0);
        encoder.set_buffer(1, Some(&s.weights), 0);
        encoder.set_buffer(2, Some(&s.totals), 0);
        encoder.set_buffer(3, Some(&self.exp_table), 0);
        encoder.set_bytes(
            4,
            std::mem::size_of::<SoftmaxParams>() as u64,
            std::ptr::from_ref(&params).cast(),
        );
        encoder.dispatch_thread_groups(
            MTLSize::new(d.heads as u64, 1, 1),
            MTLSize::new(threads, 1, 1),
        );
        // 4. u and its digit planes.
        let groups = self.heads_per_group(&self.weighted);
        let params = WeightedParams {
            heads: d.heads as u32,
            rank: d.rank as u32,
            positions: d.positions as u32,
            padded: d.padded as u32,
        };
        encoder.set_compute_pipeline_state(&self.weighted);
        encoder.set_buffer(0, Some(&s.weights), 0);
        encoder.set_buffer(1, Some(&s.totals), 0);
        encoder.set_buffer(2, Some(&cache.latent), 0);
        encoder.set_buffer(3, Some(&s.u), 0);
        encoder.set_buffer(4, Some(&s.digits_b), 0);
        encoder.set_buffer(5, Some(&s.status), 0);
        encoder.set_bytes(
            6,
            std::mem::size_of::<WeightedParams>() as u64,
            std::ptr::from_ref(&params).cast(),
        );
        encoder.dispatch_thread_groups(
            MTLSize::new(
                (d.padded as u64).div_ceil(width),
                (d.heads as u64).div_ceil(groups),
                1,
            ),
            MTLSize::new(groups * width, 1, 1),
        );
        // 5. Every head's wv_b dots, then the layer's output.
        self.engine.encode_heads_with(
            encoder,
            pipeline_b,
            &job.value,
            &s.digits_b,
            &s.dots_b,
            tile,
        );
        self.encode_epilogue(
            encoder,
            &s.dots_b,
            &s.value_mu,
            &s.value_k,
            &s.out,
            &s.status,
            d.heads * d.v_dim,
            STATUS_VALUE,
        );
    }

    /// Every head of one layer, in one command buffer. On success `out`
    /// (`heads x v_dim`) holds exactly what the CPU's per-head loop computes,
    /// and `probe`, if given, the values between the kernels. On a refusal
    /// nothing is written to either.
    pub fn layer(
        &self,
        job: &MlaLayer<'_>,
        cache: &DeviceLatentCache,
        out: &mut [i64],
        probe: Option<&mut MlaProbe>,
    ) -> Result<(), MlaRefusal> {
        let tile = self.engine.tile();
        let d = self.check(job, cache, out.len(), tile)?;
        autoreleasepool(|| {
            let s = self.take_scratch(d.sizes)?;
            let mut completed = true;
            let outcome = (|| -> Result<(), MlaRefusal> {
                let planes_a = self.prepare(&s, job)?;
                let pipeline_a = self.engine.heads_pipeline(planes_a, tile)?;
                let pipeline_b = self.engine.heads_pipeline(MAX_PLANES, tile)?;
                let commands = self.engine.queue().new_command_buffer();
                let encoder = commands.new_compute_command_encoder();
                self.encode(encoder, job, cache, &s, &d, pipeline_a, pipeline_b, tile);
                encoder.end_encoding();
                commands.commit();
                commands.wait_until_completed();
                if commands.status() != MTLCommandBufferStatus::Completed {
                    completed = false;
                    return Err(MlaRefusal::Device);
                }
                // SAFETY: the command buffer has completed, and every buffer
                // holds the sizes `check` derived from this job.
                unsafe {
                    let status = read::<u32>(&s.status, 1)[0];
                    if status != 0 {
                        return Err(MlaRefusal::Status(status));
                    }
                    out.copy_from_slice(&read::<i64>(&s.out, out.len()));
                    if let Some(probe) = probe {
                        probe.qa = read(&s.qa, d.heads * d.rank);
                        probe.scores = read(&s.scores, d.heads * d.positions);
                        probe.u = read(&s.u, d.heads * d.rank);
                    }
                }
                Ok(())
            })();
            // A command buffer that did not complete may still use the
            // scratch buffers: they are dropped, never reused.
            if completed {
                self.return_scratch(s);
            }
            outcome
        })
    }

    /// GPU seconds per layer for `repeats` copies of `job` encoded in one
    /// command buffer (the timestamps of the command buffer, divided). The
    /// work is exactly [`Self::layer`]'s.
    pub fn time_layer(
        &self,
        job: &MlaLayer<'_>,
        cache: &DeviceLatentCache,
        repeats: usize,
    ) -> Result<f64, MlaRefusal> {
        if repeats == 0 {
            return Err(MlaRefusal::Shape);
        }
        let tile = self.engine.tile();
        let out_len = job.value.heads.saturating_mul(job.value.rows);
        let d = self.check(job, cache, out_len, tile)?;
        autoreleasepool(|| {
            let s = self.take_scratch(d.sizes)?;
            let outcome = (|| -> Result<f64, MlaRefusal> {
                let planes_a = self.prepare(&s, job)?;
                let pipeline_a = self.engine.heads_pipeline(planes_a, tile)?;
                let pipeline_b = self.engine.heads_pipeline(MAX_PLANES, tile)?;
                let commands = self.engine.queue().new_command_buffer();
                let encoder = commands.new_compute_command_encoder();
                for _ in 0..repeats {
                    self.encode(encoder, job, cache, &s, &d, pipeline_a, pipeline_b, tile);
                }
                encoder.end_encoding();
                commands.commit();
                commands.wait_until_completed();
                if commands.status() != MTLCommandBufferStatus::Completed {
                    return Err(MlaRefusal::Device);
                }
                Ok(gpu_seconds(commands) / repeats as f64)
            })();
            if outcome.is_ok() {
                self.return_scratch(s);
            }
            outcome
        })
    }

    /// The kernels against [`reference_layer`] on this device: narrow and
    /// wide queries, the exp table's edges, a padded rank, more positions
    /// than a score stripe, cached values at the i32 extremes, and each
    /// refusal. Any difference, or a refusal where the reference accepts (or
    /// the reverse), is an error naming the case.
    pub fn self_test(&self) -> Result<SelfTestReport, String> {
        let mut rng = SplitMix64(SELF_TEST_SEED);
        let mut report = SelfTestReport::default();
        let mut cases: Vec<(String, TestLayer)> = Vec::new();
        for &(heads, rank, nope, rope_dim, v_dim, positions) in &[
            (1usize, 1usize, 1usize, 2usize, 1usize, 1usize),
            (3, 16, 8, 4, 8, 5),
            (2, 20, 17, 6, 5, 40),
            (5, 24, 9, 2, 3, 17),
        ] {
            cases.push((
                format!("{heads} heads, rank {rank}, {positions} positions"),
                TestLayer::random(
                    &mut rng,
                    [heads, rank, nope, rope_dim, v_dim],
                    positions,
                    46,
                    1 << 28,
                ),
            ));
        }
        // Wide queries: small k makes every |qa| far above 2^32.
        cases.push((
            "wide queries".to_string(),
            TestLayer::random(&mut rng, [3, 16, 8, 4, 8], 37, 24, 1 << 10),
        ));
        cases.push((
            "the exp table's edges".to_string(),
            TestLayer::exp_edges(&mut rng),
        ));
        for (name, layer) in &cases {
            let want = reference_layer(layer, &self.table)
                .ok_or_else(|| format!("MLA self-test {name}: the reference refused"))?;
            let got = self.run_test_layer(layer)?;
            match got {
                Ok((out, probe)) => {
                    if out != want.out
                        || probe.qa != want.qa
                        || probe.scores != want.scores
                        || probe.u != want.u
                    {
                        return Err(format!(
                            "MLA self-test {name}: the GPU differs from the reference"
                        ));
                    }
                    report.compared += 1;
                }
                Err(refusal) => {
                    return Err(format!("MLA self-test {name}: refused ({refusal})"));
                }
            }
        }
        for (name, layer, bit) in [
            (
                "a key output beyond 2^62",
                TestLayer::key_overflow(&mut rng),
                STATUS_KEY,
            ),
            (
                "a score beyond 2^62",
                TestLayer::score_overflow(&mut rng, false),
                STATUS_SCORE,
            ),
            (
                "a score product beyond 2^127",
                TestLayer::score_overflow(&mut rng, true),
                STATUS_SCORE,
            ),
            (
                "a value output beyond 2^62",
                TestLayer::value_overflow(&mut rng),
                STATUS_VALUE,
            ),
        ] {
            if reference_layer(&layer, &self.table).is_some() {
                return Err(format!("MLA self-test {name}: the reference accepts it"));
            }
            match self.run_test_layer(&layer)? {
                Err(MlaRefusal::Status(bits)) if bits & bit != 0 => report.refused += 1,
                other => {
                    return Err(format!(
                        "MLA self-test {name}: expected status {bit:#x}, got {:?}",
                        other.map(|_| ())
                    ));
                }
            }
        }
        Ok(report)
    }

    /// Upload a test layer and run it with a probe.
    #[allow(clippy::type_complexity)]
    fn run_test_layer(
        &self,
        layer: &TestLayer,
    ) -> Result<Result<(Vec<i64>, MlaProbe), MlaRefusal>, String> {
        let [heads, rank, nope, rope_dim, v_dim] = layer.dims;
        let engine = &self.engine;
        let storage = crate::metal_exact::Storage::Shared;
        let key = engine.upload(&le_bytes(&layer.wk), heads * rank, nope, storage)?;
        let value = engine.upload(&le_bytes(&layer.wv), heads * v_dim, rank, storage)?;
        let mut cache = self.new_cache(rank, rope_dim);
        self.sync(&mut cache, &layer.latent, &layer.rope_keys, layer.positions)
            .map_err(|refusal| format!("MLA self-test cache: {refusal}"))?;
        let job = layer.job(&key, &value);
        let mut out = vec![0i64; heads * v_dim];
        let mut probe = MlaProbe::default();
        Ok(self
            .layer(&job, &cache, &mut out, Some(&mut probe))
            .map(|()| (out, probe)))
    }
}

/// A layer for the self-test: weights, scales, queries and a cache.
struct TestLayer {
    /// heads, rank, nope, rope_dim, v_dim.
    dims: [usize; 5],
    positions: usize,
    wk: Vec<i16>,
    wv: Vec<i16>,
    key_mu: Vec<i32>,
    key_k: Vec<u8>,
    value_mu: Vec<i32>,
    value_k: Vec<u8>,
    queries: Vec<i64>,
    rope_queries: Vec<i64>,
    latent: Vec<i32>,
    rope_keys: Vec<i32>,
    lambda: i64,
}

impl TestLayer {
    /// Random weights over the whole INT16 range, row scales with `k` from
    /// `key_shift` up, Q16 queries up to 2^16, latents up to `magnitude`
    /// with i32::MIN and i32::MAX present, and RoPE keys up to 2^14.
    fn random(
        rng: &mut SplitMix64,
        dims: [usize; 5],
        positions: usize,
        key_shift: u8,
        magnitude: i64,
    ) -> Self {
        let [heads, rank, nope, rope_dim, v_dim] = dims;
        let weights = |rng: &mut SplitMix64, n: usize| -> Vec<i16> {
            (0..n).map(|_| rng.symmetric(32_767) as i16).collect()
        };
        let mu = |rng: &mut SplitMix64| ((1u64 << 30) + rng.next_u64() % (1 << 30)) as i32;
        let mut latent: Vec<i32> = (0..positions * rank)
            .map(|_| rng.symmetric(magnitude) as i32)
            .collect();
        latent[0] = i32::MIN;
        let last = latent.len() - 1;
        latent[last] = i32::MAX;
        TestLayer {
            dims,
            positions,
            wk: weights(rng, heads * rank * nope),
            wv: weights(rng, heads * v_dim * rank),
            key_mu: (0..heads * rank).map(|_| mu(rng)).collect(),
            key_k: (0..heads * rank)
                .map(|r| key_shift + (r % 7) as u8)
                .collect(),
            value_mu: (0..heads * v_dim).map(|_| mu(rng)).collect(),
            value_k: (0..heads * v_dim).map(|r| 46 + (r % 9) as u8).collect(),
            queries: (0..heads * nope).map(|_| rng.symmetric(1 << 16)).collect(),
            rope_queries: (0..heads * rope_dim)
                .map(|_| rng.symmetric(1 << 20))
                .collect(),
            latent,
            rope_keys: (0..positions * rope_dim)
                .map(|_| rng.symmetric(1 << 14) as i32)
                .collect(),
            lambda: 77_509_384,
        }
    }

    /// qa = (2^16, 0, ...) and qp = 0 with lambda = 2^30, so score i is the
    /// cached latent value (i, 0) exactly; those values put s - max at the
    /// exp table's edges.
    fn exp_edges(rng: &mut SplitMix64) -> Self {
        let mut layer = Self::random(rng, [2, 16, 8, 4, 8], 10, 46, 1 << 20);
        let [heads, rank, nope, _, _] = layer.dims;
        let one = 1i32 << 16;
        let column = [
            0,
            -1,
            -255,
            -256,
            -257,
            -(16 * one - 1),
            -(16 * one),
            -(16 * one + 1),
            i32::MIN,
            -(8 * one + 129),
        ];
        for (i, &v) in column.iter().enumerate() {
            layer.latent[i * rank] = v;
        }
        layer.wk.fill(0);
        for h in 0..heads {
            // Row 0 of head h: weight 1 on column 0, mu 2^30, k 30.
            layer.wk[h * rank * nope] = 1;
            for r in 0..rank {
                let row = h * rank + r;
                (layer.key_mu[row], layer.key_k[row]) =
                    if r == 0 { (1 << 30, 30) } else { (0, 16) };
            }
            layer.queries[h * nope] = 1 << 16;
        }
        layer.rope_queries.fill(0);
        layer.lambda = 1 << 30;
        layer
    }

    /// Head 1's key rows at k = 16 and the largest mu against a query of
    /// 2^40 on positive weights: its qa exceeds 2^62.
    fn key_overflow(rng: &mut SplitMix64) -> Self {
        let mut layer = Self::random(rng, [2, 16, 8, 4, 8], 3, 46, 1 << 20);
        let [_, rank, nope, _, _] = layer.dims;
        for r in 0..rank {
            (layer.key_mu[rank + r], layer.key_k[rank + r]) = (i32::MAX, 16);
        }
        layer.wk[rank * nope..2 * rank * nope].fill(32_767);
        layer.queries[nope..2 * nope].fill(1 << 40);
        layer
    }

    /// Positive qa near 2^49 (or near 2^61 for `product`) against latents
    /// of 2^31 - 1 and the largest lambda: every score is beyond 2^62 (or
    /// its product beyond 2^127).
    fn score_overflow(rng: &mut SplitMix64, product: bool) -> Self {
        let mut layer = Self::random(rng, [2, 32, 8, 4, 8], 3, 46, 1 << 20);
        let [heads, rank, _, _, _] = layer.dims;
        layer.wk.fill(32_767);
        for row in 0..heads * rank {
            (layer.key_mu[row], layer.key_k[row]) = (i32::MAX, 16);
        }
        // Key dots are 8 * 32,767 * q, so qa is about 2^49 for q = 2^16 and
        // 2^61 for q = 2^28. Score dots are 32 * qa * (2^31 - 1): about 2^85
        // (a score near 2^70) and 2^97 (a product near 2^128).
        layer.queries.fill(if product { 1 << 28 } else { 1 << 16 });
        layer.latent.fill(i32::MAX);
        layer.rope_queries.fill(0);
        layer.lambda = LAMBDA_LIMIT - 1;
        layer
    }

    /// Head 0's first value row of +32,767 at k = 16 and the largest mu,
    /// against attention outputs near 2^30.
    fn value_overflow(rng: &mut SplitMix64) -> Self {
        let mut layer = Self::random(rng, [2, 16, 8, 4, 8], 3, 46, 1 << 20);
        let [_, rank, _, _, v_dim] = layer.dims;
        layer.latent.fill(1 << 30);
        layer.wv[..rank].fill(32_767);
        for row in 0..v_dim {
            (layer.value_mu[row], layer.value_k[row]) = (i32::MAX, 16);
        }
        layer
    }

    fn job<'a>(&'a self, key: &'a ResidentI16, value: &'a ResidentI16) -> MlaLayer<'a> {
        let [heads, rank, _, _, v_dim] = self.dims;
        MlaLayer {
            key: HeadPhase {
                matrix: key,
                first_row: 0,
                heads,
                rows: rank,
            },
            key_mu: &self.key_mu,
            key_k: &self.key_k,
            value: HeadPhase {
                matrix: value,
                first_row: 0,
                heads,
                rows: v_dim,
            },
            value_mu: &self.value_mu,
            value_k: &self.value_k,
            queries: &self.queries,
            rope_queries: &self.rope_queries,
            lambda: self.lambda,
            positions: self.positions,
        }
    }
}

/// Every value of [`reference_layer`].
struct ReferenceLayer {
    qa: Vec<i64>,
    scores: Vec<i64>,
    u: Vec<i64>,
    out: Vec<i64>,
}

/// floor(dot * mu / 2^k), or `None` beyond 2^62 (`arith::dyadic_epilogue`).
fn reference_epilogue(dot: i128, mu: i32, k: u8) -> Option<i64> {
    let v = (dot * i128::from(mu)) >> k;
    (v.unsigned_abs() <= ACTIVATION_LIMIT).then_some(v as i64)
}

/// `arith::exp_q16` with the given table.
fn reference_exp(x: i64, table: &[i64]) -> i64 {
    const ONE: i64 = 1 << 16;
    if x >= 0 {
        return ONE;
    }
    if x <= -(16 * ONE) {
        return 0;
    }
    let offset = x + 16 * ONE;
    let index = (offset >> 8) as usize;
    let fraction = offset & 255;
    if index >= EXP_TABLE_LEN - 1 {
        return table[EXP_TABLE_LEN - 1];
    }
    let (low, high) = (table[index], table[index + 1]);
    low + (((high - low) * fraction) >> 8)
}

/// Independent reference for the self-test: the per-head loop of
/// `layer_forward` in i128, without the CPU's narrow path. `None` where the
/// CPU refuses.
fn reference_layer(layer: &TestLayer, table: &[i64]) -> Option<ReferenceLayer> {
    let [heads, rank, nope, rope_dim, v_dim] = layer.dims;
    let positions = layer.positions;
    let wk = le_bytes(&layer.wk);
    let wv = le_bytes(&layer.wv);
    let mut qa = Vec::with_capacity(heads * rank);
    let mut scores = Vec::with_capacity(heads * positions);
    let mut u = Vec::with_capacity(heads * rank);
    let mut out = Vec::with_capacity(heads * v_dim);
    for h in 0..heads {
        let query = &layer.queries[h * nope..(h + 1) * nope];
        let guard: u128 = query.iter().map(|v| u128::from(v.unsigned_abs())).sum();
        if guard >= ACCUMULATOR_GUARD {
            return None;
        }
        let rows = h * rank..(h + 1) * rank;
        let dots = reference_dots(&wk[rows.start * nope * 2..rows.end * nope * 2], nope, query);
        let head_qa = dots
            .iter()
            .zip(rows.clone())
            .map(|(&dot, row)| reference_epilogue(dot, layer.key_mu[row], layer.key_k[row]))
            .collect::<Option<Vec<i64>>>()?;
        let qp = &layer.rope_queries[h * rope_dim..(h + 1) * rope_dim];
        let mut head_scores = Vec::with_capacity(positions);
        for i in 0..positions {
            let latent = &layer.latent[i * rank..(i + 1) * rank];
            let keys = &layer.rope_keys[i * rope_dim..(i + 1) * rope_dim];
            let dot: i128 = head_qa
                .iter()
                .zip(latent)
                .map(|(&a, &c)| i128::from(a) * i128::from(c))
                .chain(
                    qp.iter()
                        .zip(keys)
                        .map(|(&a, &c)| i128::from(a) * i128::from(c)),
                )
                .sum();
            let scaled = dot.checked_mul(i128::from(layer.lambda))?;
            let score = scaled >> 46;
            if score.unsigned_abs() > ACTIVATION_LIMIT {
                return None;
            }
            head_scores.push(score as i64);
        }
        let top = head_scores.iter().copied().max()?;
        let weights: Vec<i64> = head_scores
            .iter()
            .map(|&s| reference_exp(s - top, table))
            .collect();
        let total: i128 = weights.iter().map(|&w| i128::from(w)).sum();
        let head_u: Vec<i64> = (0..rank)
            .map(|r| {
                let acc: i128 = weights
                    .iter()
                    .enumerate()
                    .map(|(i, &w)| i128::from(w) * i128::from(layer.latent[i * rank + r]))
                    .sum();
                (acc / total) as i64
            })
            .collect();
        let rows = h * v_dim..(h + 1) * v_dim;
        let dots = reference_dots(
            &wv[rows.start * rank * 2..rows.end * rank * 2],
            rank,
            &head_u,
        );
        for (&dot, row) in dots.iter().zip(rows) {
            out.push(reference_epilogue(
                dot,
                layer.value_mu[row],
                layer.value_k[row],
            )?);
        }
        qa.extend(head_qa);
        scores.extend(head_scores);
        u.extend(head_u);
    }
    Some(ReferenceLayer { qa, scores, u, out })
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
    fn mla_source_is_integer_only() {
        let code = strip_comments(MLA_SOURCE);
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
        assert!(!code.contains('/'), "the exact kernels must not divide");
        assert!(!code.contains('%'), "the exact kernels must not use modulo");
        // The constants the bounds are proved for.
        assert!(code.contains(&format!("constant uint MLA_FLUSH = {FLUSH_POSITIONS}u;")));
        assert!(code.contains("constant uint HEAD_PLANES = 7u;"));
        assert!(code.contains(&format!(
            "constant uint EXP_STEPS = {}u;",
            EXP_TABLE_LEN - 1
        )));
        assert!(code.contains(&format!("constant uint ST_KEY = {STATUS_KEY}u;")));
        assert!(code.contains(&format!("constant uint ST_SCORE = {STATUS_SCORE}u;")));
        assert!(code.contains(&format!("constant uint ST_OUTPUT = {STATUS_OUTPUT}u;")));
        assert!(code.contains(&format!("constant uint ST_VALUE = {STATUS_VALUE}u;")));
        assert!(code.contains(&format!("#define SOFTMAX_THREADS {SOFTMAX_THREADS}u")));
        assert_eq!(MAX_PLANES, 7);
    }

    /// The reference's own helpers at their edges.
    #[test]
    fn mla_reference_helpers_hold_their_edges() {
        let limit = ACTIVATION_LIMIT as i64;
        assert_eq!(
            reference_epilogue(i128::from(limit), 1 << 30, 30),
            Some(limit)
        );
        assert_eq!(reference_epilogue(i128::from(limit) + 1, 1 << 30, 30), None);
        assert_eq!(
            reference_epilogue(-i128::from(limit), 1 << 30, 30),
            Some(-limit)
        );
        assert_eq!(
            reference_epilogue(-i128::from(limit) - 1, 1 << 30, 30),
            None
        );
        // floor, not truncation, for negative values.
        assert_eq!(reference_epilogue(-1, 1 << 30, 31), Some(-1));
        let table: Vec<i64> = (0..EXP_TABLE_LEN as i64).map(|i| 16 * i).collect();
        assert_eq!(reference_exp(0, &table), 1 << 16);
        assert_eq!(reference_exp(-(16 << 16), &table), 0);
        assert_eq!(reference_exp(-(16 << 16) + 1, &table), 0);
        assert_eq!(reference_exp(-1, &table), 16 * 4095 + ((16 * 255) >> 8));
    }

    /// The cache copy checks the host cache on every sync: a rewritten last
    /// row, a shrink, and a truncate followed by different rows each make the
    /// copy start over and end equal to the host. Growth and scratch beyond the
    /// device-memory budget are refused with nothing written, and memory
    /// released returns to the budget.
    #[test]
    fn mla_cache_copy_checks_the_host_and_keeps_to_the_budget() {
        let engine = Arc::new(MetalExactI16::new().expect("Metal device"));
        let table: Vec<i64> = (0..EXP_TABLE_LEN as i64).map(|i| (i * i) >> 8).collect();
        let mla = MetalExactMla::new(engine, &table).expect("MLA attention self-test");
        let (rank, rope_dim) = (16usize, 4usize);
        let mut rng = SplitMix64(0x7417_B0D6);
        let row = |rng: &mut SplitMix64, width: usize, limit: i64| -> Vec<i32> {
            (0..width).map(|_| rng.symmetric(limit) as i32).collect()
        };
        let mut latent: Vec<i32> = (0..40).flat_map(|_| row(&mut rng, rank, 1 << 20)).collect();
        let mut keys: Vec<i32> = (0..40)
            .flat_map(|_| row(&mut rng, rope_dim, 1 << 14))
            .collect();
        let mut cache = mla.new_cache(rank, rope_dim);
        let sync = |cache: &mut DeviceLatentCache, latent: &[i32], keys: &[i32], n: usize| {
            mla.sync(cache, latent, keys, n)
                .map(|report| (report.copied, report.reset))
        };
        assert_eq!(sync(&mut cache, &latent, &keys, 20), Ok((20, false)));
        assert_eq!(sync(&mut cache, &latent, &keys, 30), Ok((10, false)));
        assert_eq!(sync(&mut cache, &latent, &keys, 30), Ok((0, false)));
        // The last copied row rewritten in place.
        latent[29 * rank] ^= 1;
        assert_eq!(sync(&mut cache, &latent, &keys, 30), Ok((30, true)));
        assert_eq!(
            cache.read(),
            (latent[..30 * rank].to_vec(), keys[..30 * rope_dim].to_vec())
        );
        // The host shrinks below the copy.
        latent.truncate(25 * rank);
        keys.truncate(25 * rope_dim);
        assert_eq!(sync(&mut cache, &latent, &keys, 25), Ok((25, true)));
        // A truncate, then different rows appended past the copy's length.
        latent.truncate(20 * rank);
        keys.truncate(20 * rope_dim);
        for _ in 0..12 {
            latent.extend(row(&mut rng, rank, 1 << 20));
            keys.extend(row(&mut rng, rope_dim, 1 << 14));
        }
        assert_eq!(sync(&mut cache, &latent, &keys, 32), Ok((32, true)));
        assert_eq!(
            cache.read(),
            (latent[..32 * rank].to_vec(), keys[..32 * rope_dim].to_vec())
        );
        // The budget: a growth beyond it is refused and leaves the copy as it
        // was; within it, the copy grows; dropped, its memory returns.
        let held = mla.device_bytes_in_use();
        let rows = 4096usize;
        let big_latent = vec![7i32; rows * rank];
        let big_keys = vec![-7i32; rows * rope_dim];
        let mut big = mla.new_cache(rank, rope_dim);
        mla.set_device_budget(held + 1024);
        assert_eq!(
            sync(&mut big, &big_latent, &big_keys, rows),
            Err(MlaRefusal::Memory)
        );
        assert_eq!((big.rows(), big.capacity()), (0, 0));
        mla.set_device_budget(u64::MAX);
        assert_eq!(
            sync(&mut big, &big_latent, &big_keys, rows),
            Ok((rows, false))
        );
        assert!(mla.device_bytes_in_use() > held);
        drop(big);
        assert_eq!(mla.device_bytes_in_use(), held);
        // Scratch for a longer context than any pooled set, beyond the budget:
        // refused before anything runs, and the output is untouched.
        let layer = TestLayer::random(&mut rng, [2, 16, 8, 4, 8], 3_000, 46, 1 << 20);
        let storage = crate::metal_exact::Storage::Shared;
        let key = mla
            .engine
            .upload(&le_bytes(&layer.wk), 2 * 16, 8, storage)
            .expect("upload");
        let value = mla
            .engine
            .upload(&le_bytes(&layer.wv), 2 * 8, 16, storage)
            .expect("upload");
        let mut long = mla.new_cache(16, 4);
        assert!(sync(&mut long, &layer.latent, &layer.rope_keys, layer.positions).is_ok());
        let job = layer.job(&key, &value);
        let mut out = vec![0x7E57i64; 2 * 8];
        mla.set_device_budget(mla.device_bytes_in_use());
        assert_eq!(
            mla.layer(&job, &long, &mut out, None),
            Err(MlaRefusal::Memory)
        );
        assert!(out.iter().all(|&v| v == 0x7E57));
        mla.set_device_budget(u64::MAX);
        let want = reference_layer(&layer, &table).expect("in domain");
        assert_eq!(mla.layer(&job, &long, &mut out, None), Ok(()));
        assert_eq!(out, want.out);
    }

    #[test]
    fn mla_self_test_passes_on_this_device() {
        let engine = Arc::new(MetalExactI16::new().expect("Metal device"));
        // A table of the right length with the exp table's shape: the
        // self-test compares against its own reference with the same table.
        let table: Vec<i64> = (0..EXP_TABLE_LEN as i64).map(|i| (i * i) >> 8).collect();
        let mla = MetalExactMla::new(engine, &table).expect("MLA attention self-test");
        let report = mla.self_test_report();
        eprintln!("exact MLA attention self-test: {report:?}");
        assert!(report.compared >= 6 && report.refused == 4, "{report:?}");
    }
}
