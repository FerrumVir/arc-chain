//! The forward pass on the GPU: weight upload, the static step list,
//! execution, per-operation tracing and read-back.

use std::collections::HashMap;

use super::device::{AdapterReport, GpuContext};
use super::kernels::{self, ELEMENT_WG, GEMV_ROWS, Kernel, PARAM_BYTES, Pipelines};
use super::{DyadicRef, GpuModernError, LayerRef, ModelShape, status};

/// Most tokens one GPU forward pass can carry (the cursor holds 64 ids).
pub const MAX_BATCH: usize = 64;
/// Most cached positions: keeps Z <= 2^31 and |sum_j w_j v_j| < 2^63 in
/// attention.
pub const MAX_POSITIONS: usize = 1 << 15;
/// Widest projection input whose i32 digit sums cannot overflow:
/// 127 * 128 * K <= 2^31 - 1.
pub const MAX_PROJECTION_INPUT: usize = 132_104;
/// Embedding row chunks the lookup kernel can bind.
const MAX_EMBED_CHUNKS: usize = 4;
/// Entries of the spec §5.1 exp table.
const EXP_TABLE_LEN: usize = 4097;

/// How to build an engine.
#[derive(Debug, Clone, Default)]
pub struct EngineOptions {
    /// Adapter index or name substring; default: the best hardware GPU,
    /// software rasterizers last.
    pub adapter: Option<String>,
    /// KV-cache capacity in positions; default and maximum: the model's
    /// `max_seq` (at most [`MAX_POSITIONS`]).
    pub max_positions: Option<usize>,
    /// Most tokens per forward pass (1..=64, default 1); prompts are fed in
    /// batches of this size. Results do not depend on it.
    pub batch: Option<usize>,
}

/// BLAKE3 of one traced intermediate (little-endian i64 values).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TraceEntry {
    pub name: String,
    pub blake3: [u8; 32],
}

fn u32_of(value: usize) -> u32 {
    u32::try_from(value).unwrap_or(u32::MAX)
}

pub(crate) struct Chunk {
    pub(crate) weights: wgpu::Buffer,
    pub(crate) scales: wgpu::Buffer,
    pub(crate) row_base: usize,
    pub(crate) rows: usize,
}

/// A dyadic matrix on the device, split into row chunks that each fit one
/// storage binding.
pub(crate) struct Matrix {
    pub(crate) words: usize,
    pub(crate) rows_per_chunk: usize,
    pub(crate) chunks: Vec<Chunk>,
}

struct Layer {
    attn_norm: wgpu::Buffer,
    wq: Matrix,
    wk: Matrix,
    wv: Matrix,
    wo: Matrix,
    ffn_norm: wgpu::Buffer,
    w_gate: Matrix,
    w_up: Matrix,
    w_down: Matrix,
    k_cache: wgpu::Buffer,
    v_cache: wgpu::Buffer,
}

/// Activation and scratch buffers, each sized for a full batch.
struct Activations {
    status: wgpu::Buffer,
    cursor: wgpu::Buffer,
    hidden: wgpu::Buffer,
    normed: wgpu::Buffer,
    q: wgpu::Buffer,
    k: wgpu::Buffer,
    v: wgpu::Buffer,
    attended: wgpu::Buffer,
    proj: wgpu::Buffer,
    gate: wgpu::Buffer,
    up: wgpu::Buffer,
    logits: wgpu::Buffer,
    digits: wgpu::Buffer,
    ctrl: wgpu::Buffer,
    scratch: wgpu::Buffer,
}

/// Workgroup counts as a function of the batch size `t`.
#[derive(Debug, Clone, Copy)]
enum Dims {
    /// (x, t, 1): element-wise kernels.
    Elements(u32),
    /// (t, 1, 1): one workgroup per token.
    PerToken,
    /// (heads, t, 1): attention.
    Heads(u32),
    /// (x, y, t): GEMV row groups.
    Grid(u32, u32),
}

impl Dims {
    fn resolve(self, t: u32) -> (u32, u32, u32) {
        match self {
            Dims::Elements(x) => (x, t, 1),
            Dims::PerToken => (t, 1, 1),
            Dims::Heads(heads) => (heads, t, 1),
            Dims::Grid(x, y) => (x, y, t),
        }
    }
}

/// An intermediate copied out when tracing: `width` i64 values per token.
struct Probe {
    name: String,
    buffer: wgpu::Buffer,
    width: usize,
}

struct Step {
    kernel: Kernel,
    bind: wgpu::BindGroup,
    dims: Dims,
    probe: Option<Probe>,
}

fn check_shape(shape: &ModelShape) -> Result<(), GpuModernError> {
    let positive = [
        shape.n_layers,
        shape.d_model,
        shape.n_heads,
        shape.n_kv_heads,
        shape.d_head,
        shape.d_ff,
        shape.vocab_size,
        shape.max_seq,
    ];
    let valid = positive.iter().all(|&v| v > 0)
        && shape.n_heads.is_multiple_of(shape.n_kv_heads)
        && shape.d_head.is_multiple_of(2)
        && shape.rope_layers.len() == shape.n_layers
        && shape.rms_eps_q32 >= 1
        && (1..=i64::from(u32::MAX)).contains(&shape.attention_lambda)
        && u32::try_from(shape.vocab_size).is_ok();
    if !valid {
        return Err(GpuModernError::Invalid(format!(
            "unsupported model shape: {shape:?}"
        )));
    }
    for (name, width) in [
        ("d_model", shape.d_model),
        ("n_heads * d_head", shape.d_q()),
        ("d_ff", shape.d_ff),
    ] {
        if width > MAX_PROJECTION_INPUT {
            return Err(GpuModernError::Unsupported(format!(
                "projection input {name} = {width} exceeds {MAX_PROJECTION_INPUT} (exact i32 digit sums)"
            )));
        }
    }
    Ok(())
}

pub(crate) fn check_exp_table(table: &[i64]) -> Result<Vec<u32>, GpuModernError> {
    let monotone = table.windows(2).all(|pair| pair[0] <= pair[1]);
    let values: Option<Vec<u32>> = table
        .iter()
        .map(|&v| u32::try_from(v).ok().filter(|&v| v <= 65_536))
        .collect();
    match values {
        Some(values) if table.len() == EXP_TABLE_LEN && monotone => Ok(values),
        _ => Err(GpuModernError::Invalid(
            "exp table must be the 4,097-entry monotone Q16 table of spec §5.1".into(),
        )),
    }
}

pub(crate) fn upload_matrix(
    ctx: &GpuContext,
    name: &str,
    m: &DyadicRef<'_>,
    rows: usize,
    cols: usize,
) -> Result<Matrix, GpuModernError> {
    m.check(name, rows, cols)?;
    let words = cols.div_ceil(4);
    let row_bytes = (words * 4) as u64;
    let rows_per_chunk = usize::try_from(ctx.max_binding_bytes() / row_bytes)
        .unwrap_or(usize::MAX)
        .min(rows);
    if rows_per_chunk == 0 {
        return Err(GpuModernError::Unsupported(format!(
            "{name}: one row of {cols} bytes exceeds the adapter's storage binding limit"
        )));
    }
    let mut chunks = Vec::new();
    let mut row_base = 0;
    while row_base < rows {
        let n = rows_per_chunk.min(rows - row_base);
        let rows_q = &m.q[row_base * cols..(row_base + n) * cols];
        let weights = if cols.is_multiple_of(4) {
            kernels::storage_init(ctx, name, bytemuck::cast_slice(rows_q))
        } else {
            let mut bytes = vec![0u8; n * words * 4];
            for (dst, src) in bytes
                .chunks_exact_mut(words * 4)
                .zip(rows_q.chunks_exact(cols))
            {
                dst[..cols].copy_from_slice(bytemuck::cast_slice(src));
            }
            kernels::storage_init(ctx, name, &bytes)
        };
        let scale_words: Vec<u32> = (row_base..row_base + n)
            .flat_map(|r| [m.mu[r] as u32, u32::from(m.k[r])])
            .collect();
        let scales = kernels::storage_init(ctx, name, bytemuck::cast_slice(&scale_words));
        chunks.push(Chunk {
            weights,
            scales,
            row_base,
            rows: n,
        });
        row_base += n;
    }
    Ok(Matrix {
        words,
        rows_per_chunk,
        chunks,
    })
}

/// Collects tensors one at a time, so a caller can free each host copy right
/// after it is uploaded (peak host memory stays near one model copy).
pub struct GpuEngineBuilder {
    ctx: GpuContext,
    pipelines: Pipelines,
    shape: ModelShape,
    capacity: usize,
    batch: usize,
    exp_table: wgpu::Buffer,
    rope_cos: wgpu::Buffer,
    rope_sin: wgpu::Buffer,
    acts: Activations,
    embed: Option<(Matrix, wgpu::Buffer, wgpu::Buffer)>,
    layers: Vec<Layer>,
    uploaded_bytes: u64,
}

impl GpuEngineBuilder {
    /// Select an adapter, compile the kernels and allocate activations.
    /// `exp_table` is the CPU engine's spec §5.1 table; `rope_cos`/`rope_sin`
    /// are the package's tables (`max_seq * d_head/2` entries).
    pub fn new(
        shape: &ModelShape,
        exp_table: &[i64],
        rope_cos: &[i32],
        rope_sin: &[i32],
        options: &EngineOptions,
    ) -> Result<Self, GpuModernError> {
        check_shape(shape)?;
        let exp_values = check_exp_table(exp_table)?;
        let capacity = options
            .max_positions
            .unwrap_or(shape.max_seq)
            .min(shape.max_seq);
        if capacity == 0 || capacity > MAX_POSITIONS {
            return Err(GpuModernError::Unsupported(format!(
                "KV capacity {capacity} must be in 1..={MAX_POSITIONS}"
            )));
        }
        let batch = options.batch.unwrap_or(1);
        if batch == 0 || batch > MAX_BATCH {
            return Err(GpuModernError::Invalid(format!(
                "batch {batch} must be in 1..={MAX_BATCH}"
            )));
        }
        let half = shape.d_head / 2;
        let table_len = capacity * half;
        if rope_cos.len() < table_len || rope_sin.len() < table_len {
            return Err(GpuModernError::Invalid(format!(
                "RoPE tables hold {} / {} entries; {table_len} are needed",
                rope_cos.len(),
                rope_sin.len()
            )));
        }
        let ctx = GpuContext::new(options.adapter.as_deref())?;
        let pipelines = Pipelines::build(&ctx)?;

        let max_dim = ctx.limits.max_compute_workgroups_per_dimension;
        let widest = shape.d_model.max(shape.d_ff).max(shape.d_q());
        if kernels::groups(widest, ELEMENT_WG) > max_dim
            || u32_of(shape.n_heads) > max_dim
            || u32_of(batch) > max_dim
        {
            return Err(GpuModernError::Unsupported(
                "model widths exceed the adapter's dispatch limit".into(),
            ));
        }
        let words = widest.div_ceil(4);
        let sizes = [
            ("logits", batch * shape.vocab_size * 8),
            ("gate", batch * shape.d_ff * 8),
            ("digits", batch * 8 * words * 4),
            ("scratch", batch * shape.n_heads * capacity * 8),
            ("kv cache", capacity * shape.d_kv() * 4),
        ];
        for (name, bytes) in sizes {
            if bytes as u64 > ctx.max_binding_bytes() || bytes / 4 > u32::MAX as usize {
                return Err(GpuModernError::Unsupported(format!(
                    "{name} needs {bytes} bytes, beyond one storage binding (lower the batch or context)"
                )));
            }
        }
        let f = |n: usize| (batch * n * 8) as u64;
        let acts = Activations {
            status: kernels::storage_zeroed(&ctx, "status", 16),
            cursor: kernels::uniform(&ctx, "cursor", &kernels::cursor_words(0, &[])),
            hidden: kernels::storage_zeroed(&ctx, "hidden", f(shape.d_model)),
            normed: kernels::storage_zeroed(&ctx, "normed", f(shape.d_model)),
            q: kernels::storage_zeroed(&ctx, "q", f(shape.d_q())),
            k: kernels::storage_zeroed(&ctx, "k", f(shape.d_kv())),
            v: kernels::storage_zeroed(&ctx, "v", f(shape.d_kv())),
            attended: kernels::storage_zeroed(&ctx, "attended", f(shape.d_q())),
            proj: kernels::storage_zeroed(&ctx, "proj", f(shape.d_model)),
            gate: kernels::storage_zeroed(&ctx, "gate", f(shape.d_ff)),
            up: kernels::storage_zeroed(&ctx, "up", f(shape.d_ff)),
            logits: kernels::storage_zeroed(&ctx, "logits", f(shape.vocab_size)),
            digits: kernels::storage_zeroed(&ctx, "digits", (batch * 8 * words * 4) as u64),
            ctrl: kernels::storage_zeroed(&ctx, "ctrl", (batch * 4) as u64),
            scratch: kernels::storage_zeroed(
                &ctx,
                "scratch",
                (batch * shape.n_heads * capacity * 8) as u64,
            ),
        };
        let exp_table = kernels::storage_init(&ctx, "exp", bytemuck::cast_slice(&exp_values));
        let rope_cos = kernels::storage_init(
            &ctx,
            "rope.cos",
            &kernels::i32_bytes(&rope_cos[..table_len]),
        );
        let rope_sin = kernels::storage_init(
            &ctx,
            "rope.sin",
            &kernels::i32_bytes(&rope_sin[..table_len]),
        );
        Ok(Self {
            ctx,
            pipelines,
            shape: shape.clone(),
            capacity,
            batch,
            exp_table,
            rope_cos,
            rope_sin,
            acts,
            embed: None,
            layers: Vec::new(),
            uploaded_bytes: 0,
        })
    }

    /// The adapter this engine will run on.
    pub fn report(&self) -> &AdapterReport {
        &self.ctx.report
    }

    /// Upload the tied embedding (lookup and LM head) and the final norm.
    pub fn embed(
        &mut self,
        embed: &DyadicRef<'_>,
        final_norm: &[i64],
    ) -> Result<(), GpuModernError> {
        let s = &self.shape;
        if final_norm.len() != s.d_model {
            return Err(GpuModernError::Invalid("final_norm size".into()));
        }
        let matrix = upload_matrix(&self.ctx, "embed", embed, s.vocab_size, s.d_model)?;
        if matrix.chunks.len() > MAX_EMBED_CHUNKS {
            return Err(GpuModernError::Unsupported(format!(
                "the embedding needs {} row chunks on this adapter; the lookup kernel binds {MAX_EMBED_CHUNKS}",
                matrix.chunks.len()
            )));
        }
        let scale_words: Vec<u32> = embed
            .mu
            .iter()
            .zip(embed.k)
            .flat_map(|(&mu, &k)| [mu as u32, u32::from(k)])
            .collect();
        let scales = kernels::storage_init(
            &self.ctx,
            "embed.scales",
            bytemuck::cast_slice(&scale_words),
        );
        let norm = kernels::storage_init(&self.ctx, "final_norm", &kernels::i64_bytes(final_norm));
        self.uploaded_bytes +=
            (embed.q.len() + scale_words.len() * 8 + final_norm.len() * 8) as u64;
        self.embed = Some((matrix, scales, norm));
        Ok(())
    }

    /// Upload the next layer (layers must arrive in order).
    pub fn layer(&mut self, layer: &LayerRef<'_>) -> Result<(), GpuModernError> {
        let s = &self.shape;
        let l = self.layers.len();
        if l >= s.n_layers {
            return Err(GpuModernError::Invalid(
                "more layers than the shape declares".into(),
            ));
        }
        if layer.attn_norm.len() != s.d_model || layer.ffn_norm.len() != s.d_model {
            return Err(GpuModernError::Invalid(format!("layers.{l}: norm sizes")));
        }
        let ctx = &self.ctx;
        let name = |m: &str| format!("layers.{l}.{m}");
        let (d_model, d_q, d_kv, d_ff) = (s.d_model, s.d_q(), s.d_kv(), s.d_ff);
        let uploaded = Layer {
            attn_norm: kernels::storage_init(
                ctx,
                &name("attn_norm"),
                &kernels::i64_bytes(layer.attn_norm),
            ),
            wq: upload_matrix(ctx, &name("wq"), &layer.wq, d_q, d_model)?,
            wk: upload_matrix(ctx, &name("wk"), &layer.wk, d_kv, d_model)?,
            wv: upload_matrix(ctx, &name("wv"), &layer.wv, d_kv, d_model)?,
            wo: upload_matrix(ctx, &name("wo"), &layer.wo, d_model, d_q)?,
            ffn_norm: kernels::storage_init(
                ctx,
                &name("ffn_norm"),
                &kernels::i64_bytes(layer.ffn_norm),
            ),
            w_gate: upload_matrix(ctx, &name("w_gate"), &layer.w_gate, d_ff, d_model)?,
            w_up: upload_matrix(ctx, &name("w_up"), &layer.w_up, d_ff, d_model)?,
            w_down: upload_matrix(ctx, &name("w_down"), &layer.w_down, d_model, d_ff)?,
            k_cache: kernels::storage_zeroed(
                ctx,
                &name("k_cache"),
                (self.capacity * d_kv * 4) as u64,
            ),
            v_cache: kernels::storage_zeroed(
                ctx,
                &name("v_cache"),
                (self.capacity * d_kv * 4) as u64,
            ),
        };
        let matrices = [
            &layer.wq,
            &layer.wk,
            &layer.wv,
            &layer.wo,
            &layer.w_gate,
            &layer.w_up,
            &layer.w_down,
        ];
        self.uploaded_bytes += matrices
            .iter()
            .map(|m| (m.q.len() + m.rows * 8) as u64)
            .sum::<u64>()
            + (2 * d_model * 8) as u64;
        self.layers.push(uploaded);
        Ok(())
    }

    /// Build the step list; fails if any upload or binding failed.
    pub fn finish(self) -> Result<GpuEngine, GpuModernError> {
        let Some((embed, embed_scales, final_norm)) = self.embed else {
            return Err(GpuModernError::Invalid(
                "the embedding was not uploaded".into(),
            ));
        };
        if self.layers.len() != self.shape.n_layers {
            return Err(GpuModernError::Invalid(format!(
                "{} of {} layers uploaded",
                self.layers.len(),
                self.shape.n_layers
            )));
        }
        let steps = {
            let mut plan = Plan {
                ctx: &self.ctx,
                pipelines: &self.pipelines,
                acts: &self.acts,
                params: HashMap::new(),
                steps: Vec::new(),
                max_dim: self.ctx.limits.max_compute_workgroups_per_dimension,
            };
            let s = &self.shape;
            plan.embed(s, &embed, &embed_scales);
            for (l, layer) in self.layers.iter().enumerate() {
                plan.layer(
                    s,
                    l,
                    layer,
                    self.capacity,
                    &self.exp_table,
                    &self.rope_cos,
                    &self.rope_sin,
                );
            }
            let a = &self.acts;
            plan.rms_norm(s, &a.hidden, &final_norm, "final_norm");
            plan.split(&a.normed, s.d_model);
            plan.gemv(&embed, s.vocab_size, &a.logits, "logits");
            plan.steps
        };
        let trace_bytes = steps
            .iter()
            .filter_map(|step| step.probe.as_ref())
            .map(|probe| (probe.width * 8) as u64)
            .sum();
        self.ctx.check_errors()?;
        let logits_staging = kernels::staging(
            &self.ctx,
            "logits.staging",
            (self.batch * self.shape.vocab_size * 8) as u64,
        );
        let status_staging = kernels::staging(&self.ctx, "status.staging", 16);
        Ok(GpuEngine {
            pipelines: self.pipelines,
            shape: self.shape,
            capacity: self.capacity,
            batch: self.batch,
            acts: self.acts,
            layers: self.layers,
            tables: vec![
                self.exp_table,
                self.rope_cos,
                self.rope_sin,
                embed_scales,
                final_norm,
            ],
            embed,
            steps,
            logits_staging,
            status_staging,
            trace_bytes,
            uploaded_bytes: self.uploaded_bytes,
            positions: 0,
            poisoned: false,
            lost: false,
            ctx: self.ctx,
        })
    }
}

/// Builds the step list of one forward pass.
struct Plan<'a> {
    ctx: &'a GpuContext,
    pipelines: &'a Pipelines,
    acts: &'a Activations,
    params: HashMap<[u32; 8], wgpu::Buffer>,
    steps: Vec<Step>,
    max_dim: u32,
}

impl Plan<'_> {
    fn param(&mut self, words: [u32; 8]) -> wgpu::Buffer {
        let ctx = self.ctx;
        self.params
            .entry(words)
            .or_insert_with(|| kernels::uniform(ctx, "params", &words))
            .clone()
    }

    fn push(
        &mut self,
        kernel: Kernel,
        entries: &[(u32, &wgpu::Buffer)],
        dims: Dims,
        probe: Option<(&str, &wgpu::Buffer, usize)>,
    ) {
        let bind = self.pipelines.bind(self.ctx, kernel, entries);
        self.steps.push(Step {
            kernel,
            bind,
            dims,
            probe: probe.map(|(name, buffer, width)| Probe {
                name: name.to_string(),
                buffer: buffer.clone(),
                width,
            }),
        });
    }

    fn embed(&mut self, s: &ModelShape, embed: &Matrix, scales: &wgpu::Buffer) {
        let p = self.param([
            u32_of(s.d_model),
            u32_of(embed.words),
            u32_of(embed.rows_per_chunk),
            u32_of(s.vocab_size),
            0,
            0,
            0,
            0,
        ]);
        let a = self.acts;
        let chunk = |i: usize| &embed.chunks[if i < embed.chunks.len() { i } else { 0 }].weights;
        self.push(
            Kernel::Embed,
            &[
                (0, &a.status),
                (1, &p),
                (2, &a.cursor),
                (3, chunk(0)),
                (4, chunk(1)),
                (5, chunk(2)),
                (6, chunk(3)),
                (7, scales),
                (9, &a.hidden),
            ],
            Dims::Elements(kernels::groups(s.d_model, ELEMENT_WG)),
            Some(("embed", &a.hidden, s.d_model)),
        );
    }

    fn rms_norm(&mut self, s: &ModelShape, x: &wgpu::Buffer, gain: &wgpu::Buffer, name: &str) {
        let eps = s.rms_eps_q32 as u64;
        let p = self.param([
            u32_of(s.d_model),
            eps as u32,
            (eps >> 32) as u32,
            0,
            0,
            0,
            0,
            0,
        ]);
        let a = self.acts;
        self.push(
            Kernel::RmsNorm,
            &[(0, &a.status), (1, &p), (2, x), (3, gain), (4, &a.normed)],
            Dims::PerToken,
            Some((name, &a.normed, s.d_model)),
        );
    }

    fn split(&mut self, x: &wgpu::Buffer, n: usize) {
        let p = self.param([u32_of(n), u32_of(n.div_ceil(4)), 0, 0, 0, 0, 0, 0]);
        let a = self.acts;
        self.push(
            Kernel::Split,
            &[
                (0, &a.status),
                (1, &p),
                (2, x),
                (3, &a.digits),
                (4, &a.ctrl),
            ],
            Dims::PerToken,
            None,
        );
    }

    /// out[t][row] for every row of `m` (one dispatch per row chunk); the
    /// probe is attached to the last chunk.
    fn gemv(&mut self, m: &Matrix, out_width: usize, out: &wgpu::Buffer, name: &str) {
        let a = self.acts;
        let last = m.chunks.len() - 1;
        for (index, chunk) in m.chunks.iter().enumerate() {
            let p = self.param([
                u32_of(chunk.rows),
                u32_of(m.words),
                u32_of(chunk.row_base),
                u32_of(out_width),
                0,
                0,
                0,
                0,
            ]);
            let (x, y) = kernels::grid_2d(kernels::groups(chunk.rows, GEMV_ROWS), self.max_dim);
            let probe = (index == last).then_some((name, out, out_width));
            self.push(
                Kernel::Gemv,
                &[
                    (0, &a.status),
                    (1, &p),
                    (2, &chunk.weights),
                    (3, &chunk.scales),
                    (4, &a.digits),
                    (5, &a.ctrl),
                    (6, out),
                ],
                Dims::Grid(x, y),
                probe,
            );
        }
    }

    fn rope(
        &mut self,
        s: &ModelShape,
        data: &wgpu::Buffer,
        heads: usize,
        tables: (&wgpu::Buffer, &wgpu::Buffer),
        name: &str,
    ) {
        let half = s.d_head / 2;
        let p = self.param([
            u32_of(heads),
            u32_of(s.d_head),
            u32_of(half),
            u32_of(heads * s.d_head),
            0,
            0,
            0,
            0,
        ]);
        let a = self.acts;
        self.push(
            Kernel::Rope,
            &[
                (0, &a.status),
                (1, &p),
                (2, &a.cursor),
                (3, data),
                (4, tables.0),
                (5, tables.1),
            ],
            Dims::Elements(kernels::groups(heads * half, ELEMENT_WG)),
            Some((name, data, heads * s.d_head)),
        );
    }

    fn residual(&mut self, s: &ModelShape, delta: &wgpu::Buffer, name: &str) {
        let p = self.param([u32_of(s.d_model), 0, 0, 0, 0, 0, 0, 0]);
        let a = self.acts;
        self.push(
            Kernel::Residual,
            &[
                (0, &a.status),
                (1, &p),
                (2, &a.hidden),
                (3, delta),
                (4, &a.cursor),
            ],
            Dims::Elements(kernels::groups(s.d_model, ELEMENT_WG)),
            Some((name, &a.hidden, s.d_model)),
        );
    }

    #[allow(clippy::too_many_arguments)]
    fn layer(
        &mut self,
        s: &ModelShape,
        l: usize,
        layer: &Layer,
        capacity: usize,
        exp_table: &wgpu::Buffer,
        rope_cos: &wgpu::Buffer,
        rope_sin: &wgpu::Buffer,
    ) {
        let a = self.acts;
        let n = |op: &str| format!("layer{l}.{op}");
        self.rms_norm(s, &a.hidden, &layer.attn_norm, &n("attn_norm"));
        self.split(&a.normed, s.d_model);
        self.gemv(&layer.wq, s.d_q(), &a.q, &n("q"));
        self.gemv(&layer.wk, s.d_kv(), &a.k, &n("k"));
        self.gemv(&layer.wv, s.d_kv(), &a.v, &n("v"));
        if s.rope_layers[l] {
            self.rope(s, &a.q, s.n_heads, (rope_cos, rope_sin), &n("q_rope"));
            self.rope(s, &a.k, s.n_kv_heads, (rope_cos, rope_sin), &n("k_rope"));
        }
        let kv = self.param([u32_of(s.d_kv()), 0, 0, 0, 0, 0, 0, 0]);
        self.push(
            Kernel::KvStore,
            &[
                (0, &a.status),
                (1, &kv),
                (2, &a.cursor),
                (3, &a.k),
                (4, &a.v),
                (5, &layer.k_cache),
                (6, &layer.v_cache),
            ],
            Dims::Elements(kernels::groups(s.d_kv(), ELEMENT_WG)),
            None,
        );
        let attention_name = n("attention");
        let attn = self.param([
            u32_of(s.d_head),
            u32_of(s.n_heads / s.n_kv_heads),
            u32_of(s.d_kv()),
            u32::try_from(s.attention_lambda).unwrap_or(u32::MAX),
            u32_of(capacity),
            u32_of(s.n_heads),
            0,
            0,
        ]);
        self.push(
            Kernel::Attention,
            &[
                (0, &a.status),
                (1, &attn),
                (2, &a.cursor),
                (3, &a.q),
                (4, &layer.k_cache),
                (5, &layer.v_cache),
                (6, &a.scratch),
                (7, &a.attended),
                (8, exp_table),
            ],
            Dims::Heads(u32_of(s.n_heads)),
            Some((attention_name.as_str(), &a.attended, s.d_q())),
        );
        self.split(&a.attended, s.d_q());
        self.gemv(&layer.wo, s.d_model, &a.proj, &n("o_proj"));
        self.residual(s, &a.proj, &n("attn_residual"));
        self.rms_norm(s, &a.hidden, &layer.ffn_norm, &n("ffn_norm"));
        self.split(&a.normed, s.d_model);
        self.gemv(&layer.w_gate, s.d_ff, &a.gate, &n("gate"));
        self.gemv(&layer.w_up, s.d_ff, &a.up, &n("up"));
        let silu_name = n("silu");
        let silu = self.param([u32_of(s.d_ff), 0, 0, 0, 0, 0, 0, 0]);
        self.push(
            Kernel::Silu,
            &[
                (0, &a.status),
                (1, &silu),
                (2, &a.gate),
                (3, &a.up),
                (4, &a.cursor),
                (8, exp_table),
            ],
            Dims::Elements(kernels::groups(s.d_ff, ELEMENT_WG)),
            Some((silu_name.as_str(), &a.gate, s.d_ff)),
        );
        self.split(&a.gate, s.d_ff);
        self.gemv(&layer.w_down, s.d_model, &a.proj, &n("down"));
        self.residual(s, &a.proj, &n("ffn_residual"));
    }
}

/// The dyadic-profile model resident on one GPU, with its KV cache.
pub struct GpuEngine {
    ctx: GpuContext,
    pipelines: Pipelines,
    shape: ModelShape,
    capacity: usize,
    batch: usize,
    acts: Activations,
    layers: Vec<Layer>,
    /// Kept alive alongside the bind groups that reference them.
    tables: Vec<wgpu::Buffer>,
    embed: Matrix,
    steps: Vec<Step>,
    logits_staging: wgpu::Buffer,
    status_staging: wgpu::Buffer,
    trace_bytes: u64,
    uploaded_bytes: u64,
    positions: usize,
    poisoned: bool,
    /// Set when a read-back failed: its staging buffers may still be mapping,
    /// so the engine cannot be reused (rebuild it).
    lost: bool,
}

impl GpuEngine {
    /// The adapter in use.
    pub fn report(&self) -> &AdapterReport {
        &self.ctx.report
    }

    /// Which exact i8 dot product the projections use.
    pub fn dot_path(&self) -> &'static str {
        self.pipelines.dot_path
    }

    pub fn shape(&self) -> &ModelShape {
        &self.shape
    }

    /// Cached positions (the next token's position).
    pub fn positions(&self) -> usize {
        self.positions
    }

    /// KV-cache capacity in positions.
    pub fn capacity(&self) -> usize {
        self.capacity
    }

    /// Most tokens per forward pass.
    pub fn batch(&self) -> usize {
        self.batch
    }

    /// Bytes of weights, scales and norms uploaded (KV cache excluded).
    pub fn uploaded_bytes(&self) -> u64 {
        self.uploaded_bytes
    }

    /// Dispatches per forward pass (a cost indicator for small models).
    pub fn dispatches(&self) -> usize {
        self.steps.len()
    }

    /// Forget every cached position (start a new sequence).
    pub fn reset(&mut self) {
        self.positions = 0;
        self.poisoned = false;
    }

    /// One token at the next position; returns its logits.
    pub fn forward(&mut self, token: u32) -> Result<Vec<i64>, GpuModernError> {
        let (mut logits, _) = self.run(&[token], false)?;
        logits
            .pop()
            .ok_or_else(|| GpuModernError::Execution("no logits read back".into()))
    }

    /// Several tokens at consecutive positions in one pass (at most
    /// [`GpuEngine::batch`]); returns each token's logits, in order. Identical
    /// to calling [`GpuEngine::forward`] once per token.
    pub fn forward_batch(&mut self, tokens: &[u32]) -> Result<Vec<Vec<i64>>, GpuModernError> {
        Ok(self.run(tokens, false)?.0)
    }

    /// [`GpuEngine::forward`] that also returns the BLAKE3 of every
    /// intermediate, in forward order, for localising a divergence.
    pub fn forward_traced(
        &mut self,
        token: u32,
    ) -> Result<(Vec<i64>, Vec<TraceEntry>), GpuModernError> {
        let (mut logits, trace) = self.run(&[token], true)?;
        let logits = logits
            .pop()
            .ok_or_else(|| GpuModernError::Execution("no logits read back".into()))?;
        Ok((logits, trace))
    }

    fn encode(&self, pass: &mut wgpu::ComputePass<'_>, step: &Step, t: u32) {
        pass.set_pipeline(self.pipelines.get(step.kernel));
        pass.set_bind_group(0, &step.bind, &[]);
        let (x, y, z) = step.dims.resolve(t);
        pass.dispatch_workgroups(x, y, z);
    }

    fn run(
        &mut self,
        tokens: &[u32],
        trace: bool,
    ) -> Result<(Vec<Vec<i64>>, Vec<TraceEntry>), GpuModernError> {
        if self.lost {
            return Err(GpuModernError::Execution(
                "an earlier read-back failed; build a new engine".into(),
            ));
        }
        if self.poisoned {
            return Err(GpuModernError::Invalid(
                "a previous forward pass failed; reset() before reusing the engine".into(),
            ));
        }
        let count = tokens.len();
        if count == 0 || count > self.batch || (trace && count != 1) {
            return Err(GpuModernError::Invalid(format!(
                "{count} tokens in one pass (batch {}, tracing needs 1)",
                self.batch
            )));
        }
        if self.positions + count > self.capacity {
            return Err(GpuModernError::Domain(format!(
                "position {} is outside the {}-position context",
                self.positions + count - 1,
                self.capacity
            )));
        }
        let vocab = self.shape.vocab_size;
        if tokens.iter().any(|&token| token as usize >= vocab) {
            return Err(GpuModernError::Domain(
                "token id is outside the vocabulary".into(),
            ));
        }
        let t = u32_of(count);
        let cursor = kernels::cursor_words(self.positions, tokens);
        self.ctx
            .queue
            .write_buffer(&self.acts.cursor, 0, bytemuck::cast_slice(&cursor));
        let mut encoder = self
            .ctx
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("arc-modern forward"),
            });
        encoder.clear_buffer(&self.acts.status, 0, None);
        let trace_buffer = trace.then(|| kernels::staging(&self.ctx, "trace", self.trace_bytes));
        let mut probes = Vec::new();
        if let Some(trace_buffer) = &trace_buffer {
            let mut offset = 0u64;
            for step in &self.steps {
                {
                    let mut pass = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor {
                        label: Some(step.kernel.entry()),
                        timestamp_writes: None,
                    });
                    self.encode(&mut pass, step, t);
                }
                if let Some(probe) = &step.probe {
                    let bytes = (probe.width * 8) as u64;
                    encoder.copy_buffer_to_buffer(&probe.buffer, 0, trace_buffer, offset, bytes);
                    probes.push((probe.name.clone(), offset, bytes));
                    offset += bytes;
                }
            }
        } else {
            let mut pass = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor {
                label: Some("arc-modern forward"),
                timestamp_writes: None,
            });
            for step in &self.steps {
                self.encode(&mut pass, step, t);
            }
        }
        let logits_bytes = (count * vocab * 8) as u64;
        encoder.copy_buffer_to_buffer(&self.acts.logits, 0, &self.logits_staging, 0, logits_bytes);
        encoder.copy_buffer_to_buffer(&self.acts.status, 0, &self.status_staging, 0, 16);
        self.ctx.queue.submit(Some(encoder.finish()));
        let mut reads = vec![
            (&self.status_staging, 16u64),
            (&self.logits_staging, logits_bytes),
        ];
        if let Some(trace_buffer) = &trace_buffer {
            reads.push((trace_buffer, self.trace_bytes));
        }
        let data = match kernels::read_staging(&self.ctx, &reads) {
            Ok(data) => data,
            Err(error) => {
                self.poisoned = true;
                self.lost = true;
                return Err(error);
            }
        };
        let bits = u32::from_le_bytes([data[0][0], data[0][1], data[0][2], data[0][3]]);
        if bits != 0 {
            self.poisoned = true;
            return Err(GpuModernError::Domain(status::describe(bits)));
        }
        let logits = kernels::i64s(&data[1])
            .chunks_exact(vocab)
            .map(<[i64]>::to_vec)
            .collect();
        let entries = probes
            .into_iter()
            .map(|(name, offset, bytes)| {
                let start = offset as usize;
                let end = (offset + bytes) as usize;
                TraceEntry {
                    name,
                    blake3: *blake3::hash(&data[2][start..end]).as_bytes(),
                }
            })
            .collect();
        self.positions += count;
        Ok((logits, entries))
    }

    /// BLAKE3 over every cached key then value, layer by layer, as LE i32:
    /// the same digest as the CPU engine's `KvCache::digest`.
    pub fn kv_digest(&self) -> Result<[u8; 32], GpuModernError> {
        let bytes = (self.positions * self.shape.d_kv() * 4) as u64;
        let mut hasher = blake3::Hasher::new();
        if bytes == 0 {
            return Ok(*hasher.finalize().as_bytes());
        }
        let keys = kernels::staging(&self.ctx, "k.staging", bytes);
        let values = kernels::staging(&self.ctx, "v.staging", bytes);
        for layer in &self.layers {
            let mut encoder =
                self.ctx
                    .device
                    .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                        label: Some("kv digest"),
                    });
            encoder.copy_buffer_to_buffer(&layer.k_cache, 0, &keys, 0, bytes);
            encoder.copy_buffer_to_buffer(&layer.v_cache, 0, &values, 0, bytes);
            self.ctx.queue.submit(Some(encoder.finish()));
            let data = kernels::read_staging(&self.ctx, &[(&keys, bytes), (&values, bytes)])?;
            hasher.update(&data[0]);
            hasher.update(&data[1]);
        }
        Ok(*hasher.finalize().as_bytes())
    }

    /// Number of host-visible buffers kept for the bind groups (diagnostics).
    pub fn resident_tables(&self) -> usize {
        self.tables.len() + self.embed.chunks.len() * 2
    }
}

// The uniform layouts in the WGSL files are eight u32 parameters.
const _: () = assert!(PARAM_BYTES == 8 * 4);
