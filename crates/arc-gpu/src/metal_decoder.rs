//! One command buffer per decoder token, exact, for ARC's canonical per-row
//! INT8 profile.
//!
//! [`MetalDecoder::step`] encodes a whole Llama decoder token on the GPU:
//! every layer's RMSNorm, the projection-input digit split, the Q, K, V, O,
//! gate, up and down projections, split-half RoPE, the KV-cache append, the
//! online-softmax attention, SiLU and both residual adds, then the final norm
//! and the LM head. With [`Submission::OneCommandBuffer`] the host waits once
//! per token; [`Submission::PerLayer`] and [`Submission::PerDispatch`] compute
//! the same integers with a host round trip after every layer or every kernel,
//! which is how the benchmark measures what each removed round trip costs.
//!
//! Every value is byte-identical to the CPU engine (`cached_integer_model`):
//! the kernels in `metal_exact_decoder.metal` reproduce each CPU operation
//! with the release build's wrapping i64 semantics, floor shifts and
//! truncating divisions, and the projections are the exact GEMV of
//! `metal_exact`. Anything the kernels do not reproduce sets a status bit;
//! [`MetalDecoder::step`] then returns [`DecoderRefusal::Status`] and the
//! caller computes that token on the CPU instead.
//!
//! The decoder keeps its own copy of the KV cache in shared memory. The
//! caller mirrors it with [`MetalDecoder::write_kv`] and reads each token's
//! new rows from [`DecoderStep`].

use std::ops::Range;
use std::sync::Arc;

use metal::{
    Buffer, BufferRef, CommandBufferRef, CommandQueueRef, CompileOptions, ComputeCommandEncoderRef,
    ComputePipelineState, DeviceRef, MTLCommandBufferStatus, MTLResourceOptions, MTLSize,
};
use objc::rc::autoreleasepool;

use crate::metal_exact::{
    BLOCK_BYTES, MAX_COLS, MAX_PLANES, MetalExactGemv, Params, ROWS_PER_SIMDGROUP, ResidentMatrix,
    gpu_seconds,
};

/// Source of the decoder kernels.
pub const DECODER_SOURCE: &str = include_str!("metal_exact_decoder.metal");

/// Status bit: an RMSNorm input of magnitude 2^56 or more.
pub const STATUS_NORM_DOMAIN: u32 = 1;
/// Status bit: a projection input outside the four-digit domain.
pub const STATUS_SPLIT_DOMAIN: u32 = 2;
/// Status bit: `128 * sum|x| * max|s|` above `i64::MAX`.
pub const STATUS_SCALE_BOUND: u32 = 4;
/// Longest vector the RMSNorm kernel accepts: below it, a sum of squares of
/// values under 2^56 cannot reach 2^127.
pub const MAX_NORM_LEN: usize = 32_768;
/// Widest attention head (8 dimensions per lane of a 32-lane simdgroup).
pub const MAX_HEAD_DIM: usize = 256;
/// Most positions the device KV cache can hold: keeps every softmax sum
/// under 2^31.
pub const MAX_KV_CAPACITY: usize = 32_768;
/// Entries of the CPU engine's exp table.
pub const EXP_TABLE_LEN: usize = 4097;

const GROUP_THREADS: u64 = 256;
const ELEMENTWISE_THREADS: u64 = 256;
const ONE: i64 = 1 << 16;

/// The decoder kernels, built once per engine (`MetalExactGemv::decoder_pipelines`).
pub(crate) struct DecoderPipelines {
    rms_norm: ComputePipelineState,
    split: ComputePipelineState,
    rope: ComputePipelineState,
    attention: ComputePipelineState,
    silu: ComputePipelineState,
    residual: ComputePipelineState,
    gemv: Vec<((u32, bool), ComputePipelineState)>,
}

impl DecoderPipelines {
    pub(crate) fn build(device: &DeviceRef) -> Result<Self, String> {
        let options = CompileOptions::new();
        options.set_fast_math_enabled(false);
        let library = device
            .new_library_with_source(DECODER_SOURCE, &options)
            .map_err(|e| format!("decoder kernels failed to compile: {e}"))?;
        let make = |name: &str| -> Result<ComputePipelineState, String> {
            let function = library
                .get_function(name, None)
                .map_err(|e| format!("{name}: {e}"))?;
            device
                .new_compute_pipeline_state_with_function(&function)
                .map_err(|e| format!("{name}: {e}"))
        };
        let mut gemv = Vec::new();
        for rows in ROWS_PER_SIMDGROUP {
            for mul16 in [false, true] {
                let name = format!("exact_gemv_dyn_r{rows}_m{}", u8::from(mul16));
                gemv.push(((rows, mul16), make(&name)?));
            }
        }
        let pipelines = Self {
            rms_norm: make("rms_norm")?,
            split: make("split_planes")?,
            rope: make("rope_store")?,
            attention: make("attention")?,
            silu: make("silu_mul")?,
            residual: make("residual_add")?,
            gemv,
        };
        for (name, pipeline) in [
            ("rms_norm", &pipelines.rms_norm),
            ("split_planes", &pipelines.split),
        ] {
            if pipeline.max_total_threads_per_threadgroup() < GROUP_THREADS {
                return Err(format!(
                    "{name} cannot run {GROUP_THREADS} threads per group"
                ));
            }
        }
        if pipelines.attention.thread_execution_width() != 32 {
            return Err("attention needs 32-lane simdgroups".to_string());
        }
        Ok(pipelines)
    }

    fn gemv_for(&self, rows: u32, mul16: bool) -> Option<&ComputePipelineState> {
        self.gemv
            .iter()
            .find(|(key, _)| *key == (rows, mul16))
            .map(|(_, pipeline)| pipeline)
    }
}

// ---- parameter blocks, mirroring the kernel source -----------------------

#[repr(C)]
#[derive(Clone, Copy)]
struct NormParams {
    n: u32,
    pad: [u32; 3],
}

#[repr(C)]
#[derive(Clone, Copy)]
struct SplitParams {
    n: u32,
    stride: u32,
    max_scale: u64,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct RopeParams {
    n_heads: u32,
    n_kv_heads: u32,
    pairs: u32,
    d_head: u32,
    pos: u32,
    d_kv: u32,
    pad: [u32; 2],
}

#[repr(C)]
#[derive(Clone, Copy)]
struct AttnParams {
    d_head: u32,
    d_kv: u32,
    positions: u32,
    pad: u32,
    attn_scale: i64,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct VecParams {
    n: u32,
    pad: [u32; 3],
}

fn set_params<T: Copy>(encoder: &ComputeCommandEncoderRef, index: u64, value: &T) {
    encoder.set_bytes(
        index,
        std::mem::size_of::<T>() as u64,
        std::ptr::from_ref(value).cast(),
    );
}

// ---- shared-memory buffers -----------------------------------------------

fn new_shared(device: &DeviceRef, bytes: usize) -> Buffer {
    device.new_buffer(bytes.max(16) as u64, MTLResourceOptions::StorageModeShared)
}

fn upload_i64(device: &DeviceRef, values: &[i64]) -> Buffer {
    let buffer = new_shared(device, std::mem::size_of_val(values));
    write_i64(&buffer, 0, values);
    buffer
}

fn write_i64(buffer: &BufferRef, offset: usize, values: &[i64]) {
    let capacity = buffer.length() as usize / 8;
    assert!(
        offset + values.len() <= capacity,
        "write of {} values at {offset} past a buffer of {capacity}",
        values.len()
    );
    // SAFETY: shared-storage buffer holding `capacity` i64 values (checked
    // above); the decoder is borrowed mutably, or the buffer is fresh, so no
    // command buffer that uses it is in flight.
    unsafe {
        std::ptr::copy_nonoverlapping(
            values.as_ptr(),
            buffer.contents().cast::<i64>().add(offset),
            values.len(),
        );
    }
}

fn read_i64(buffer: &BufferRef, offset: usize, len: usize) -> Vec<i64> {
    let capacity = buffer.length() as usize / 8;
    assert!(
        offset + len <= capacity,
        "read of {len} values at {offset} past a buffer of {capacity}"
    );
    // SAFETY: as in `write_i64`; every command buffer has completed.
    unsafe { std::slice::from_raw_parts(buffer.contents().cast::<i64>().add(offset), len).to_vec() }
}

fn upload_u32(device: &DeviceRef, values: &[u32]) -> Buffer {
    let buffer = new_shared(device, std::mem::size_of_val(values));
    // SAFETY: a fresh shared buffer of at least `size_of_val(values)` bytes.
    unsafe {
        std::ptr::copy_nonoverlapping(
            values.as_ptr(),
            buffer.contents().cast::<u32>(),
            values.len(),
        );
    }
    buffer
}

/// The CPU engine's query-to-KV head map: `kv_h = h * n_kv_heads / n_heads`.
fn head_map(n_heads: usize, n_kv_heads: usize) -> Vec<u32> {
    (0..n_heads)
        .map(|h| (h * n_kv_heads / n_heads) as u32)
        .collect()
}

fn write_u32(buffer: &BufferRef, value: u32) {
    // SAFETY: every u32 buffer here holds at least 16 bytes (`new_shared`).
    unsafe { buffer.contents().cast::<u32>().write(value) }
}

fn read_u32(buffer: &BufferRef) -> u32 {
    // SAFETY: as in `write_u32`; every command buffer has completed.
    unsafe { buffer.contents().cast::<u32>().read() }
}

/// `gamma` as the CPU reads it: `gamma[i]` below its length, ONE beyond.
fn padded_gamma(gamma: &[i64], n: usize) -> Vec<i64> {
    (0..n)
        .map(|i| gamma.get(i).copied().unwrap_or(ONE))
        .collect()
}

fn stride_of(n: usize) -> usize {
    n.div_ceil(BLOCK_BYTES) * BLOCK_BYTES
}

// ---- kernel encoders -----------------------------------------------------

fn encode_norm(
    encoder: &ComputeCommandEncoderRef,
    pipes: &DecoderPipelines,
    status: &BufferRef,
    x: &BufferRef,
    gamma: &BufferRef,
    y: &BufferRef,
    n: usize,
) {
    encoder.set_compute_pipeline_state(&pipes.rms_norm);
    encoder.set_buffer(0, Some(x), 0);
    encoder.set_buffer(1, Some(gamma), 0);
    encoder.set_buffer(2, Some(y), 0);
    encoder.set_buffer(3, Some(status), 0);
    set_params(
        encoder,
        4,
        &NormParams {
            n: n as u32,
            pad: [0; 3],
        },
    );
    encoder.dispatch_thread_groups(MTLSize::new(1, 1, 1), MTLSize::new(GROUP_THREADS, 1, 1));
}

#[allow(clippy::too_many_arguments)]
fn encode_split(
    encoder: &ComputeCommandEncoderRef,
    pipes: &DecoderPipelines,
    status: &BufferRef,
    x: &BufferRef,
    planes: &BufferRef,
    ctrl: &BufferRef,
    n: usize,
    max_scale: u64,
) {
    encoder.set_compute_pipeline_state(&pipes.split);
    encoder.set_buffer(0, Some(x), 0);
    encoder.set_buffer(1, Some(planes), 0);
    encoder.set_buffer(2, Some(ctrl), 0);
    encoder.set_buffer(3, Some(status), 0);
    set_params(
        encoder,
        4,
        &SplitParams {
            n: n as u32,
            stride: stride_of(n) as u32,
            max_scale,
        },
    );
    encoder.dispatch_thread_groups(MTLSize::new(1, 1, 1), MTLSize::new(GROUP_THREADS, 1, 1));
}

/// Q, K and V in; Q rotated in place, K rotated into its cache row, V copied.
struct RopeBuffers<'a> {
    q: &'a BufferRef,
    k: &'a BufferRef,
    v: &'a BufferRef,
    k_cache: &'a BufferRef,
    v_cache: &'a BufferRef,
    cos: &'a BufferRef,
    sin: &'a BufferRef,
}

fn encode_rope(
    encoder: &ComputeCommandEncoderRef,
    pipes: &DecoderPipelines,
    buffers: &RopeBuffers<'_>,
    params: RopeParams,
) {
    encoder.set_compute_pipeline_state(&pipes.rope);
    encoder.set_buffer(0, Some(buffers.q), 0);
    encoder.set_buffer(1, Some(buffers.k), 0);
    encoder.set_buffer(2, Some(buffers.v), 0);
    encoder.set_buffer(3, Some(buffers.k_cache), 0);
    encoder.set_buffer(4, Some(buffers.v_cache), 0);
    encoder.set_buffer(5, Some(buffers.cos), 0);
    encoder.set_buffer(6, Some(buffers.sin), 0);
    set_params(encoder, 7, &params);
    let heads = u64::from(params.n_heads + params.n_kv_heads);
    let pairs = u64::from(params.pairs);
    encoder.dispatch_threads(
        MTLSize::new(pairs, heads, 1),
        MTLSize::new(pairs.min(64), 1, 1),
    );
}

/// Attention inputs: rotated queries, one layer's caches, the exp table and
/// the query-to-KV head map.
struct AttnBuffers<'a> {
    q: &'a BufferRef,
    k_cache: &'a BufferRef,
    v_cache: &'a BufferRef,
    out: &'a BufferRef,
    lut: &'a BufferRef,
    head_kv: &'a BufferRef,
}

fn encode_attention(
    encoder: &ComputeCommandEncoderRef,
    pipes: &DecoderPipelines,
    buffers: &AttnBuffers<'_>,
    params: AttnParams,
    n_heads: usize,
) {
    encoder.set_compute_pipeline_state(&pipes.attention);
    encoder.set_buffer(0, Some(buffers.q), 0);
    encoder.set_buffer(1, Some(buffers.k_cache), 0);
    encoder.set_buffer(2, Some(buffers.v_cache), 0);
    encoder.set_buffer(3, Some(buffers.out), 0);
    encoder.set_buffer(4, Some(buffers.lut), 0);
    encoder.set_buffer(5, Some(buffers.head_kv), 0);
    set_params(encoder, 6, &params);
    encoder.dispatch_thread_groups(MTLSize::new(n_heads as u64, 1, 1), MTLSize::new(32, 1, 1));
}

fn encode_silu(
    encoder: &ComputeCommandEncoderRef,
    pipes: &DecoderPipelines,
    gate: &BufferRef,
    up: &BufferRef,
    act: &BufferRef,
    lut: &BufferRef,
    n: usize,
) {
    encoder.set_compute_pipeline_state(&pipes.silu);
    encoder.set_buffer(0, Some(gate), 0);
    encoder.set_buffer(1, Some(up), 0);
    encoder.set_buffer(2, Some(act), 0);
    encoder.set_buffer(3, Some(lut), 0);
    set_params(
        encoder,
        4,
        &VecParams {
            n: n as u32,
            pad: [0; 3],
        },
    );
    encoder.dispatch_threads(
        MTLSize::new(n as u64, 1, 1),
        MTLSize::new(ELEMENTWISE_THREADS, 1, 1),
    );
}

fn encode_residual(
    encoder: &ComputeCommandEncoderRef,
    pipes: &DecoderPipelines,
    hidden: &BufferRef,
    delta: &BufferRef,
    n: usize,
) {
    encoder.set_compute_pipeline_state(&pipes.residual);
    encoder.set_buffer(0, Some(hidden), 0);
    encoder.set_buffer(1, Some(delta), 0);
    set_params(
        encoder,
        2,
        &VecParams {
            n: n as u32,
            pad: [0; 3],
        },
    );
    encoder.dispatch_threads(
        MTLSize::new(n as u64, 1, 1),
        MTLSize::new(ELEMENTWISE_THREADS, 1, 1),
    );
}

/// One full-matrix canonical projection whose digit-plane count the GPU
/// reads from `ctrl[0]`.
fn encode_gemv(
    encoder: &ComputeCommandEncoderRef,
    engine: &MetalExactGemv,
    pipes: &DecoderPipelines,
    matrix: &ResidentMatrix,
    digits: &BufferRef,
    ctrl: &BufferRef,
    out: &BufferRef,
) {
    let tile = engine.tile;
    let pipeline = pipes
        .gemv_for(tile.rows_per_simdgroup, tile.mul16)
        .expect("a dynamic-plane pipeline exists for every valid tile");
    let (groups, threads) = engine.geometry(pipeline, matrix.n_rows, tile);
    encoder.set_compute_pipeline_state(pipeline);
    encoder.set_buffer(0, Some(&matrix.weights), 0);
    encoder.set_buffer(1, Some(digits), 0);
    encoder.set_buffer(2, Some(&matrix.scales), 0);
    encoder.set_buffer(3, Some(out), 0);
    set_params(
        encoder,
        4,
        &Params {
            rows: matrix.n_rows as u32,
            row_offset: 0,
            blocks: (matrix.stride / BLOCK_BYTES) as u32,
            mode: 0,
        },
    );
    encoder.set_buffer(5, Some(ctrl), 0);
    encoder.dispatch_thread_groups(groups, threads);
}

// ---- public types --------------------------------------------------------

/// Dimensions of a Llama-family decoder in the canonical profile.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub struct DecoderShape {
    pub n_layers: usize,
    pub d_model: usize,
    pub n_heads: usize,
    pub n_kv_heads: usize,
    pub d_head: usize,
    pub d_kv: usize,
    pub d_ff: usize,
    pub vocab: usize,
    pub attn_scale: i64,
    /// Rows of the RoPE tables.
    pub max_seq: usize,
    /// Positions the device KV cache holds.
    pub kv_capacity: usize,
}

impl DecoderShape {
    fn validate(&self) -> Result<(), String> {
        let fail =
            |what: &str| -> Result<(), String> { Err(format!("decoder shape {self:?}: {what}")) };
        if self.d_model == 0 || self.d_model > MAX_NORM_LEN || self.d_model > MAX_COLS {
            return fail("d_model must be in 1..=32768");
        }
        if self.n_heads == 0 || self.n_heads.checked_mul(self.d_head) != Some(self.d_model) {
            return fail("d_model must equal n_heads * d_head");
        }
        if self.n_kv_heads == 0 || self.n_kv_heads.checked_mul(self.d_head) != Some(self.d_kv) {
            return fail("d_kv must equal n_kv_heads * d_head");
        }
        if self.d_head < 2 || self.d_head > MAX_HEAD_DIM || !self.d_head.is_multiple_of(2) {
            return fail("d_head must be even and at most 256");
        }
        if self.d_ff == 0
            || self.d_ff > MAX_COLS
            || self.vocab == 0
            || self.vocab > i32::MAX as usize
        {
            return fail("d_ff must be in 1..=131071 and vocab positive");
        }
        if self.kv_capacity == 0 || self.kv_capacity > MAX_KV_CAPACITY || self.max_seq == 0 {
            return fail("kv_capacity must be in 1..=32768 and max_seq positive");
        }
        Ok(())
    }
}

/// One decoder layer's resident projections and norm gains.
pub struct DecoderLayerWeights {
    pub wq: Arc<ResidentMatrix>,
    pub wk: Arc<ResidentMatrix>,
    pub wv: Arc<ResidentMatrix>,
    pub wo: Arc<ResidentMatrix>,
    pub w_gate: Arc<ResidentMatrix>,
    pub w_up: Arc<ResidentMatrix>,
    pub w_down: Arc<ResidentMatrix>,
    pub attn_norm: Vec<i64>,
    pub ffn_norm: Vec<i64>,
}

/// Everything the decoder reads besides the KV cache.
pub struct DecoderWeights {
    pub layers: Vec<DecoderLayerWeights>,
    pub final_norm: Vec<i64>,
    pub output: Arc<ResidentMatrix>,
    /// `max_seq * d_head / 2` entries each, position-major.
    pub rope_cos: Vec<i64>,
    pub rope_sin: Vec<i64>,
    /// The CPU engine's `EXP_LUT`.
    pub exp_lut: Vec<i64>,
}

/// When the host waits for the GPU.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub enum Submission {
    /// One command buffer per token: one host round trip.
    OneCommandBuffer,
    /// A command buffer per layer (and one for the head).
    PerLayer,
    /// A command buffer per kernel.
    PerDispatch,
}

/// What one token step produced.
#[derive(Debug, Clone)]
pub struct DecoderStep {
    /// The new KV-cache rows, one per layer of the range.
    pub k_rows: Vec<Vec<i64>>,
    pub v_rows: Vec<Vec<i64>>,
    /// The residual stream after the range's last layer.
    pub hidden: Vec<i64>,
    /// LM-head logits, when requested.
    pub logits: Option<Vec<i64>>,
    /// GPU time of the step's command buffers.
    pub gpu_seconds: f64,
    pub command_buffers: usize,
    pub dispatches: usize,
}

/// Why a step produced no result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DecoderRefusal {
    /// A kernel set these status bits; compute the token on the CPU.
    Status(u32),
    /// A command buffer did not complete.
    Device,
    /// The input does not fit the decoder.
    Input(String),
}

impl std::fmt::Display for DecoderRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DecoderRefusal::Status(bits) => write!(f, "GPU status bits {bits:#x}"),
            DecoderRefusal::Device => f.write_str("GPU command buffer did not complete"),
            DecoderRefusal::Input(what) => write!(f, "input refused: {what}"),
        }
    }
}

struct LayerState {
    weights: DecoderLayerWeights,
    attn_norm: Buffer,
    ffn_norm: Buffer,
    k_cache: Buffer,
    v_cache: Buffer,
    qkv_scale: u64,
    o_scale: u64,
    gate_up_scale: u64,
    down_scale: u64,
}

struct Activations {
    hidden: Buffer,
    normed: Buffer,
    q: Buffer,
    k: Buffer,
    v: Buffer,
    attn: Buffer,
    proj: Buffer,
    gate: Buffer,
    up: Buffer,
    act: Buffer,
    ff: Buffer,
    logits: Buffer,
    digits: Buffer,
    ctrl: Buffer,
    status: Buffer,
}

/// Command buffers for one step, ended according to the [`Submission`].
struct Recorder<'q> {
    queue: &'q CommandQueueRef,
    submission: Submission,
    commands: &'q CommandBufferRef,
    encoder: &'q ComputeCommandEncoderRef,
    pending: usize,
    gpu_seconds: f64,
    command_buffers: usize,
    dispatches: usize,
}

impl<'q> Recorder<'q> {
    fn new(queue: &'q CommandQueueRef, submission: Submission) -> Self {
        let commands = queue.new_command_buffer();
        let encoder = commands.new_compute_command_encoder();
        Self {
            queue,
            submission,
            commands,
            encoder,
            pending: 0,
            gpu_seconds: 0.0,
            command_buffers: 0,
            dispatches: 0,
        }
    }

    fn after_dispatch(&mut self) -> Result<(), DecoderRefusal> {
        self.dispatches += 1;
        self.pending += 1;
        if self.submission == Submission::PerDispatch {
            self.submit(true)
        } else {
            Ok(())
        }
    }

    fn after_layer(&mut self) -> Result<(), DecoderRefusal> {
        if self.submission == Submission::PerLayer && self.pending > 0 {
            self.submit(true)
        } else {
            Ok(())
        }
    }

    fn submit(&mut self, reopen: bool) -> Result<(), DecoderRefusal> {
        self.encoder.end_encoding();
        self.commands.commit();
        self.commands.wait_until_completed();
        if self.commands.status() != MTLCommandBufferStatus::Completed {
            return Err(DecoderRefusal::Device);
        }
        if self.pending > 0 {
            self.gpu_seconds += gpu_seconds(self.commands);
            self.command_buffers += 1;
        }
        self.pending = 0;
        if reopen {
            let queue = self.queue;
            let commands = queue.new_command_buffer();
            self.commands = commands;
            self.encoder = commands.new_compute_command_encoder();
        }
        Ok(())
    }

    fn finish(mut self) -> Result<(f64, usize, usize), DecoderRefusal> {
        self.submit(false)?;
        Ok((self.gpu_seconds, self.command_buffers, self.dispatches))
    }
}

/// A decoder resident on the GPU: weights, norms, RoPE tables, exp table, a
/// KV cache per layer and the activation buffers.
pub struct MetalDecoder {
    engine: Arc<MetalExactGemv>,
    shape: DecoderShape,
    layers: Vec<LayerState>,
    output: Arc<ResidentMatrix>,
    output_scale: u64,
    final_norm: Buffer,
    rope_cos: Buffer,
    rope_sin: Buffer,
    lut: Buffer,
    head_kv: Buffer,
    io: Activations,
}

fn check_matrix(
    matrix: &ResidentMatrix,
    rows: usize,
    cols: usize,
    name: &str,
) -> Result<u64, String> {
    if matrix.n_rows != rows || matrix.n_cols != cols {
        return Err(format!(
            "{name} is {}x{}, expected {rows}x{cols}",
            matrix.n_rows, matrix.n_cols
        ));
    }
    matrix
        .max_abs_scale
        .map(|scale| scale as u64)
        .ok_or_else(|| format!("{name} has an i64::MIN scale"))
}

impl MetalDecoder {
    /// Upload everything except the weights (already resident) and allocate
    /// the KV cache.
    pub fn new(
        engine: Arc<MetalExactGemv>,
        shape: DecoderShape,
        weights: DecoderWeights,
    ) -> Result<Self, String> {
        shape.validate()?;
        engine.decoder_pipelines()?;
        let s = shape;
        if weights.layers.len() != s.n_layers {
            return Err(format!(
                "{} layers of weights for {} layers",
                weights.layers.len(),
                s.n_layers
            ));
        }
        let pairs = s.d_head / 2;
        let table = s
            .max_seq
            .checked_mul(pairs)
            .ok_or("RoPE table size overflows")?;
        if weights.rope_cos.len() < table || weights.rope_sin.len() < table {
            return Err(format!("RoPE tables need {table} entries each"));
        }
        if weights.exp_lut.len() != EXP_TABLE_LEN {
            return Err(format!("the exp table needs {EXP_TABLE_LEN} entries"));
        }
        let device: &DeviceRef = &engine.device;
        let kv_bytes = s
            .kv_capacity
            .checked_mul(s.d_kv)
            .and_then(|n| n.checked_mul(8))
            .ok_or("KV cache size overflows")?;
        let mut layers = Vec::with_capacity(s.n_layers);
        for (index, w) in weights.layers.into_iter().enumerate() {
            let name = |m: &str| format!("layer {index} {m}");
            let q = check_matrix(&w.wq, s.d_model, s.d_model, &name("wq"))?;
            let k = check_matrix(&w.wk, s.d_kv, s.d_model, &name("wk"))?;
            let v = check_matrix(&w.wv, s.d_kv, s.d_model, &name("wv"))?;
            let o = check_matrix(&w.wo, s.d_model, s.d_model, &name("wo"))?;
            let g = check_matrix(&w.w_gate, s.d_ff, s.d_model, &name("w_gate"))?;
            let u = check_matrix(&w.w_up, s.d_ff, s.d_model, &name("w_up"))?;
            let d = check_matrix(&w.w_down, s.d_model, s.d_ff, &name("w_down"))?;
            let attn_norm = upload_i64(device, &padded_gamma(&w.attn_norm, s.d_model));
            let ffn_norm = upload_i64(device, &padded_gamma(&w.ffn_norm, s.d_model));
            layers.push(LayerState {
                weights: w,
                attn_norm,
                ffn_norm,
                k_cache: new_shared(device, kv_bytes),
                v_cache: new_shared(device, kv_bytes),
                qkv_scale: q.max(k).max(v),
                o_scale: o,
                gate_up_scale: g.max(u),
                down_scale: d,
            });
        }
        let output_scale = check_matrix(&weights.output, s.vocab, s.d_model, "output")?;
        let head_kv_buffer = upload_u32(device, &head_map(s.n_heads, s.n_kv_heads));
        let widest = s.d_model.max(s.d_ff);
        let io = Activations {
            hidden: new_shared(device, s.d_model * 8),
            normed: new_shared(device, s.d_model * 8),
            q: new_shared(device, s.d_model * 8),
            k: new_shared(device, s.d_kv * 8),
            v: new_shared(device, s.d_kv * 8),
            attn: new_shared(device, s.d_model * 8),
            proj: new_shared(device, s.d_model * 8),
            gate: new_shared(device, s.d_ff * 8),
            up: new_shared(device, s.d_ff * 8),
            act: new_shared(device, s.d_ff * 8),
            ff: new_shared(device, s.d_model * 8),
            logits: new_shared(device, s.vocab * 8),
            digits: new_shared(device, MAX_PLANES * stride_of(widest)),
            ctrl: new_shared(device, 16),
            status: new_shared(device, 16),
        };
        Ok(Self {
            shape: s,
            layers,
            output: weights.output,
            output_scale,
            final_norm: upload_i64(device, &padded_gamma(&weights.final_norm, s.d_model)),
            rope_cos: upload_i64(device, &weights.rope_cos[..table]),
            rope_sin: upload_i64(device, &weights.rope_sin[..table]),
            lut: upload_i64(device, &weights.exp_lut),
            head_kv: head_kv_buffer,
            io,
            engine,
        })
    }

    pub fn shape(&self) -> &DecoderShape {
        &self.shape
    }

    /// INT8 weight bytes one token reads: every layer's seven projections and
    /// the LM head.
    pub fn weight_bytes_per_token(&self) -> usize {
        let layers: usize = self
            .layers
            .iter()
            .map(|l| {
                let w = &l.weights;
                [&w.wq, &w.wk, &w.wv, &w.wo, &w.w_gate, &w.w_up, &w.w_down]
                    .iter()
                    .map(|m| m.weight_bytes())
                    .sum::<usize>()
            })
            .sum();
        layers + self.output.weight_bytes()
    }

    /// Mirror one CPU KV-cache row into the device cache.
    pub fn write_kv(
        &mut self,
        layer: usize,
        pos: usize,
        k: &[i64],
        v: &[i64],
    ) -> Result<(), String> {
        let d_kv = self.shape.d_kv;
        if layer >= self.layers.len()
            || pos >= self.shape.kv_capacity
            || k.len() != d_kv
            || v.len() != d_kv
        {
            return Err(format!(
                "KV row {layer}/{pos} does not fit the device cache"
            ));
        }
        write_i64(&self.layers[layer].k_cache, pos * d_kv, k);
        write_i64(&self.layers[layer].v_cache, pos * d_kv, v);
        Ok(())
    }

    /// One device KV-cache row.
    pub fn read_kv(&self, layer: usize, pos: usize) -> Result<(Vec<i64>, Vec<i64>), String> {
        let d_kv = self.shape.d_kv;
        if layer >= self.layers.len() || pos >= self.shape.kv_capacity {
            return Err(format!("KV row {layer}/{pos} is outside the device cache"));
        }
        Ok((
            read_i64(&self.layers[layer].k_cache, pos * d_kv, d_kv),
            read_i64(&self.layers[layer].v_cache, pos * d_kv, d_kv),
        ))
    }

    /// Run layers `layers` for one token at position `pos`, starting from the
    /// residual stream `hidden_in`, and the final norm and LM head if `logits`.
    /// Rows `0..pos` of every layer's device KV cache must already mirror the
    /// CPU cache.
    pub fn step(
        &mut self,
        hidden_in: &[i64],
        pos: usize,
        layers: Range<usize>,
        logits: bool,
        submission: Submission,
    ) -> Result<DecoderStep, DecoderRefusal> {
        let s = self.shape;
        if hidden_in.len() != s.d_model {
            return Err(DecoderRefusal::Input(format!(
                "hidden state of {} values for d_model {}",
                hidden_in.len(),
                s.d_model
            )));
        }
        if layers.start > layers.end || layers.end > s.n_layers {
            return Err(DecoderRefusal::Input(format!("layer range {layers:?}")));
        }
        if pos >= s.kv_capacity || pos >= s.max_seq {
            return Err(DecoderRefusal::Input(format!(
                "position {pos} past the device cache ({}) or the RoPE tables ({})",
                s.kv_capacity, s.max_seq
            )));
        }
        let pipes = self
            .engine
            .decoder_pipelines()
            .map_err(DecoderRefusal::Input)?;
        autoreleasepool(|| {
            write_i64(&self.io.hidden, 0, hidden_in);
            write_u32(&self.io.status, 0);
            let mut rec = Recorder::new(&self.engine.queue, submission);
            for layer in layers.clone() {
                self.encode_layer(&mut rec, pipes, layer, pos)?;
                rec.after_layer()?;
            }
            if logits {
                self.encode_head(&mut rec, pipes)?;
            }
            let (gpu_seconds, command_buffers, dispatches) = rec.finish()?;
            let status = read_u32(&self.io.status);
            if status != 0 {
                return Err(DecoderRefusal::Status(status));
            }
            let mut k_rows = Vec::with_capacity(layers.len());
            let mut v_rows = Vec::with_capacity(layers.len());
            for layer in layers {
                k_rows.push(read_i64(&self.layers[layer].k_cache, pos * s.d_kv, s.d_kv));
                v_rows.push(read_i64(&self.layers[layer].v_cache, pos * s.d_kv, s.d_kv));
            }
            Ok(DecoderStep {
                k_rows,
                v_rows,
                hidden: read_i64(&self.io.hidden, 0, s.d_model),
                logits: logits.then(|| read_i64(&self.io.logits, 0, s.vocab)),
                gpu_seconds,
                command_buffers,
                dispatches,
            })
        })
    }

    fn encode_layer(
        &self,
        rec: &mut Recorder<'_>,
        pipes: &DecoderPipelines,
        layer: usize,
        pos: usize,
    ) -> Result<(), DecoderRefusal> {
        let s = &self.shape;
        let l = &self.layers[layer];
        let w = &l.weights;
        let io = &self.io;
        let engine = self.engine.as_ref();

        encode_norm(
            rec.encoder,
            pipes,
            &io.status,
            &io.hidden,
            &l.attn_norm,
            &io.normed,
            s.d_model,
        );
        rec.after_dispatch()?;
        encode_split(
            rec.encoder,
            pipes,
            &io.status,
            &io.normed,
            &io.digits,
            &io.ctrl,
            s.d_model,
            l.qkv_scale,
        );
        rec.after_dispatch()?;
        for (matrix, out) in [(&w.wq, &io.q), (&w.wk, &io.k), (&w.wv, &io.v)] {
            encode_gemv(
                rec.encoder,
                engine,
                pipes,
                matrix,
                &io.digits,
                &io.ctrl,
                out,
            );
            rec.after_dispatch()?;
        }
        encode_rope(
            rec.encoder,
            pipes,
            &RopeBuffers {
                q: &io.q,
                k: &io.k,
                v: &io.v,
                k_cache: &l.k_cache,
                v_cache: &l.v_cache,
                cos: &self.rope_cos,
                sin: &self.rope_sin,
            },
            RopeParams {
                n_heads: s.n_heads as u32,
                n_kv_heads: s.n_kv_heads as u32,
                pairs: (s.d_head / 2) as u32,
                d_head: s.d_head as u32,
                pos: pos as u32,
                d_kv: s.d_kv as u32,
                pad: [0; 2],
            },
        );
        rec.after_dispatch()?;
        encode_attention(
            rec.encoder,
            pipes,
            &AttnBuffers {
                q: &io.q,
                k_cache: &l.k_cache,
                v_cache: &l.v_cache,
                out: &io.attn,
                lut: &self.lut,
                head_kv: &self.head_kv,
            },
            AttnParams {
                d_head: s.d_head as u32,
                d_kv: s.d_kv as u32,
                positions: (pos + 1) as u32,
                pad: 0,
                attn_scale: s.attn_scale,
            },
            s.n_heads,
        );
        rec.after_dispatch()?;
        encode_split(
            rec.encoder,
            pipes,
            &io.status,
            &io.attn,
            &io.digits,
            &io.ctrl,
            s.d_model,
            l.o_scale,
        );
        rec.after_dispatch()?;
        encode_gemv(
            rec.encoder,
            engine,
            pipes,
            &w.wo,
            &io.digits,
            &io.ctrl,
            &io.proj,
        );
        rec.after_dispatch()?;
        encode_residual(rec.encoder, pipes, &io.hidden, &io.proj, s.d_model);
        rec.after_dispatch()?;
        encode_norm(
            rec.encoder,
            pipes,
            &io.status,
            &io.hidden,
            &l.ffn_norm,
            &io.normed,
            s.d_model,
        );
        rec.after_dispatch()?;
        encode_split(
            rec.encoder,
            pipes,
            &io.status,
            &io.normed,
            &io.digits,
            &io.ctrl,
            s.d_model,
            l.gate_up_scale,
        );
        rec.after_dispatch()?;
        for (matrix, out) in [(&w.w_gate, &io.gate), (&w.w_up, &io.up)] {
            encode_gemv(
                rec.encoder,
                engine,
                pipes,
                matrix,
                &io.digits,
                &io.ctrl,
                out,
            );
            rec.after_dispatch()?;
        }
        encode_silu(
            rec.encoder,
            pipes,
            &io.gate,
            &io.up,
            &io.act,
            &self.lut,
            s.d_ff,
        );
        rec.after_dispatch()?;
        encode_split(
            rec.encoder,
            pipes,
            &io.status,
            &io.act,
            &io.digits,
            &io.ctrl,
            s.d_ff,
            l.down_scale,
        );
        rec.after_dispatch()?;
        encode_gemv(
            rec.encoder,
            engine,
            pipes,
            &w.w_down,
            &io.digits,
            &io.ctrl,
            &io.ff,
        );
        rec.after_dispatch()?;
        encode_residual(rec.encoder, pipes, &io.hidden, &io.ff, s.d_model);
        rec.after_dispatch()
    }

    fn encode_head(
        &self,
        rec: &mut Recorder<'_>,
        pipes: &DecoderPipelines,
    ) -> Result<(), DecoderRefusal> {
        let s = &self.shape;
        let io = &self.io;
        encode_norm(
            rec.encoder,
            pipes,
            &io.status,
            &io.hidden,
            &self.final_norm,
            &io.normed,
            s.d_model,
        );
        rec.after_dispatch()?;
        encode_split(
            rec.encoder,
            pipes,
            &io.status,
            &io.normed,
            &io.digits,
            &io.ctrl,
            s.d_model,
            self.output_scale,
        );
        rec.after_dispatch()?;
        encode_gemv(
            rec.encoder,
            self.engine.as_ref(),
            pipes,
            &self.output,
            &io.digits,
            &io.ctrl,
            &io.logits,
        );
        rec.after_dispatch()
    }
}

// ---- single-kernel runs, for operator tests against the CPU engine -------

/// Run one encoded dispatch on fresh buffers and wait.
fn run_once(
    engine: &MetalExactGemv,
    encode: impl FnOnce(&ComputeCommandEncoderRef),
) -> Result<(), String> {
    let commands = engine.queue.new_command_buffer();
    let encoder = commands.new_compute_command_encoder();
    encode(encoder);
    encoder.end_encoding();
    commands.commit();
    commands.wait_until_completed();
    if commands.status() != MTLCommandBufferStatus::Completed {
        return Err("lab command buffer did not complete".to_string());
    }
    Ok(())
}

impl MetalExactGemv {
    /// RMSNorm of `x` with `gain` on the GPU: (output, status bits).
    #[doc(hidden)]
    pub fn lab_rms_norm(&self, x: &[i64], gain: &[i64]) -> Result<(Vec<i64>, u32), String> {
        if x.is_empty() || x.len() > MAX_NORM_LEN {
            return Err(format!("RMSNorm of {} values", x.len()));
        }
        let pipes = self.decoder_pipelines()?;
        autoreleasepool(|| {
            let device: &DeviceRef = &self.device;
            let input = upload_i64(device, x);
            let gamma = upload_i64(device, &padded_gamma(gain, x.len()));
            let output = new_shared(device, x.len() * 8);
            let status = new_shared(device, 16);
            write_u32(&status, 0);
            run_once(self, |encoder| {
                encode_norm(encoder, pipes, &status, &input, &gamma, &output, x.len());
            })?;
            Ok((read_i64(&output, 0, x.len()), read_u32(&status)))
        })
    }

    /// Digit planes of `x` on the GPU: (planes, `4 * stride` bytes; planes in
    /// use; status bits).
    #[doc(hidden)]
    pub fn lab_split(&self, x: &[i64], max_scale: u64) -> Result<(Vec<i8>, u32, u32), String> {
        if x.is_empty() || x.len() > MAX_COLS {
            return Err(format!("split of {} values", x.len()));
        }
        let pipes = self.decoder_pipelines()?;
        autoreleasepool(|| {
            let device: &DeviceRef = &self.device;
            let stride = stride_of(x.len());
            let input = upload_i64(device, x);
            let planes = new_shared(device, MAX_PLANES * stride);
            let ctrl = new_shared(device, 16);
            let status = new_shared(device, 16);
            write_u32(&status, 0);
            write_u32(&ctrl, 0);
            run_once(self, |encoder| {
                encode_split(
                    encoder,
                    pipes,
                    &status,
                    &input,
                    &planes,
                    &ctrl,
                    x.len(),
                    max_scale,
                );
            })?;
            // SAFETY: the buffer holds MAX_PLANES * stride bytes and the
            // command buffer has completed.
            let bytes = unsafe {
                std::slice::from_raw_parts(planes.contents().cast::<i8>(), MAX_PLANES * stride)
            }
            .to_vec();
            Ok((bytes, read_u32(&ctrl), read_u32(&status)))
        })
    }

    /// Split-half RoPE at `pos` on the GPU for `n_heads` query heads and
    /// `n_kv_heads` key heads: (rotated q, key-cache row, value-cache row).
    #[doc(hidden)]
    #[allow(clippy::too_many_arguments)]
    pub fn lab_rope(
        &self,
        q: &[i64],
        k: &[i64],
        v: &[i64],
        n_heads: usize,
        n_kv_heads: usize,
        d_head: usize,
        pos: usize,
        tables: (&[i64], &[i64]),
    ) -> Result<(Vec<i64>, Vec<i64>, Vec<i64>), String> {
        let pairs = d_head / 2;
        let d_kv = n_kv_heads * d_head;
        let need = (pos + 1) * pairs;
        if d_head < 2
            || !d_head.is_multiple_of(2)
            || q.len() != n_heads * d_head
            || k.len() != d_kv
            || v.len() != d_kv
            || tables.0.len() < need
            || tables.1.len() < need
        {
            return Err("RoPE lab shape".to_string());
        }
        let pipes = self.decoder_pipelines()?;
        autoreleasepool(|| {
            let device: &DeviceRef = &self.device;
            let q_buffer = upload_i64(device, q);
            let k_buffer = upload_i64(device, k);
            let v_buffer = upload_i64(device, v);
            let k_cache = new_shared(device, (pos + 1) * d_kv.max(1) * 8);
            let v_cache = new_shared(device, (pos + 1) * d_kv.max(1) * 8);
            let cos = upload_i64(device, &tables.0[..need]);
            let sin = upload_i64(device, &tables.1[..need]);
            run_once(self, |encoder| {
                encode_rope(
                    encoder,
                    pipes,
                    &RopeBuffers {
                        q: &q_buffer,
                        k: &k_buffer,
                        v: &v_buffer,
                        k_cache: &k_cache,
                        v_cache: &v_cache,
                        cos: &cos,
                        sin: &sin,
                    },
                    RopeParams {
                        n_heads: n_heads as u32,
                        n_kv_heads: n_kv_heads as u32,
                        pairs: pairs as u32,
                        d_head: d_head as u32,
                        pos: pos as u32,
                        d_kv: d_kv as u32,
                        pad: [0; 2],
                    },
                );
            })?;
            Ok((
                read_i64(&q_buffer, 0, q.len()),
                read_i64(&k_cache, pos * d_kv, d_kv),
                read_i64(&v_cache, pos * d_kv, d_kv),
            ))
        })
    }

    /// Attention of `n_heads` query heads over `positions` cached rows of
    /// `n_kv_heads * d_head` keys and values on the GPU.
    #[doc(hidden)]
    #[allow(clippy::too_many_arguments)]
    pub fn lab_attention(
        &self,
        q: &[i64],
        keys: &[i64],
        values: &[i64],
        n_heads: usize,
        n_kv_heads: usize,
        d_head: usize,
        attn_scale: i64,
        exp_lut: &[i64],
    ) -> Result<Vec<i64>, String> {
        let d_kv = n_kv_heads * d_head;
        if n_heads == 0
            || n_kv_heads == 0
            || d_head == 0
            || d_head > MAX_HEAD_DIM
            || q.len() != n_heads * d_head
            || keys.is_empty()
            || !keys.len().is_multiple_of(d_kv)
            || values.len() != keys.len()
            || exp_lut.len() != EXP_TABLE_LEN
        {
            return Err("attention lab shape".to_string());
        }
        let positions = keys.len() / d_kv;
        let pipes = self.decoder_pipelines()?;
        autoreleasepool(|| {
            let device: &DeviceRef = &self.device;
            let q_buffer = upload_i64(device, q);
            let k_cache = upload_i64(device, keys);
            let v_cache = upload_i64(device, values);
            let out = new_shared(device, q.len() * 8);
            let lut = upload_i64(device, exp_lut);
            let head_kv = upload_u32(device, &head_map(n_heads, n_kv_heads));
            run_once(self, |encoder| {
                encode_attention(
                    encoder,
                    pipes,
                    &AttnBuffers {
                        q: &q_buffer,
                        k_cache: &k_cache,
                        v_cache: &v_cache,
                        out: &out,
                        lut: &lut,
                        head_kv: &head_kv,
                    },
                    AttnParams {
                        d_head: d_head as u32,
                        d_kv: d_kv as u32,
                        positions: positions as u32,
                        pad: 0,
                        attn_scale,
                    },
                    n_heads,
                );
            })?;
            Ok(read_i64(&out, 0, q.len()))
        })
    }

    /// `(silu(gate[j]) * up[j]) >> 16` on the GPU.
    #[doc(hidden)]
    pub fn lab_silu_mul(
        &self,
        gate: &[i64],
        up: &[i64],
        exp_lut: &[i64],
    ) -> Result<Vec<i64>, String> {
        if gate.is_empty() || gate.len() != up.len() || exp_lut.len() != EXP_TABLE_LEN {
            return Err("SiLU lab shape".to_string());
        }
        let pipes = self.decoder_pipelines()?;
        autoreleasepool(|| {
            let device: &DeviceRef = &self.device;
            let g = upload_i64(device, gate);
            let u = upload_i64(device, up);
            let act = new_shared(device, gate.len() * 8);
            let lut = upload_i64(device, exp_lut);
            run_once(self, |encoder| {
                encode_silu(encoder, pipes, &g, &u, &act, &lut, gate.len());
            })?;
            Ok(read_i64(&act, 0, gate.len()))
        })
    }
}
