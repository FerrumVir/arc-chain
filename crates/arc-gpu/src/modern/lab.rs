//! Single-operation harness: runs one kernel on caller-provided inputs.
//!
//! Used for known-answer tests against the CPU engine's operators (exact
//! values and identical refusals on edge cases) and as a pre-flight self-test
//! on an unfamiliar adapter or driver before a full model run.

use super::device::{AdapterReport, GpuContext};
use super::engine::{check_exp_table, upload_matrix};
use super::kernels::{self, ELEMENT_WG, GEMV_ROWS, Kernel, Pipelines};
use super::{DyadicRef, GpuModernError, status};

/// One attention problem in the layout the model uses: `q` holds `n_heads`
/// heads of `d_head`; `keys`/`values` hold `positions` rows of
/// `n_kv_heads * d_head` i32 values.
#[derive(Debug, Clone, Copy)]
pub struct AttentionCase<'a> {
    pub q: &'a [i64],
    pub keys: &'a [i32],
    pub values: &'a [i32],
    pub positions: usize,
    pub n_heads: usize,
    pub n_kv_heads: usize,
    pub d_head: usize,
    pub lambda: i64,
}

struct Dispatch<'a> {
    kernel: Kernel,
    entries: Vec<(u32, &'a wgpu::Buffer)>,
    dims: (u32, u32, u32),
}

/// Compiled kernels on one adapter, run one operation at a time.
pub struct OpLab {
    ctx: GpuContext,
    pipelines: Pipelines,
}

impl OpLab {
    /// Open an adapter (index or name substring; default: the best GPU).
    pub fn new(adapter: Option<&str>) -> Result<Self, GpuModernError> {
        let ctx = GpuContext::new(adapter)?;
        let pipelines = Pipelines::build(&ctx)?;
        Ok(Self { ctx, pipelines })
    }

    pub fn report(&self) -> &AdapterReport {
        &self.ctx.report
    }

    /// Which exact i8 dot product the projections use.
    pub fn dot_path(&self) -> &'static str {
        self.pipelines.dot_path
    }

    fn i64_buffer(&self, label: &str, values: &[i64]) -> wgpu::Buffer {
        kernels::storage_init(&self.ctx, label, &kernels::i64_bytes(values))
    }

    fn zeroed(&self, label: &str, values: usize) -> wgpu::Buffer {
        kernels::storage_zeroed(&self.ctx, label, (values * 8) as u64)
    }

    fn params(&self, words: [u32; 8]) -> wgpu::Buffer {
        kernels::uniform(&self.ctx, "params", &words)
    }

    fn cursor(&self, pos0: usize, tokens: &[u32]) -> wgpu::Buffer {
        kernels::uniform(&self.ctx, "cursor", &kernels::cursor_words(pos0, tokens))
    }

    /// Run the dispatches in order, then read `values` i64 from `out`; a set
    /// status bit becomes [`GpuModernError::Domain`].
    fn execute(
        &self,
        dispatches: &[Dispatch<'_>],
        status_buffer: &wgpu::Buffer,
        out: &wgpu::Buffer,
        values: usize,
    ) -> Result<Vec<i64>, GpuModernError> {
        let bytes = (values * 8) as u64;
        let out_staging = kernels::staging(&self.ctx, "out.staging", bytes);
        let status_staging = kernels::staging(&self.ctx, "status.staging", 16);
        let binds: Vec<wgpu::BindGroup> = dispatches
            .iter()
            .map(|d| self.pipelines.bind(&self.ctx, d.kernel, &d.entries))
            .collect();
        let mut encoder = self
            .ctx
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("op lab"),
            });
        {
            let mut pass = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor {
                label: Some("op lab"),
                timestamp_writes: None,
            });
            for (dispatch, bind) in dispatches.iter().zip(&binds) {
                pass.set_pipeline(self.pipelines.get(dispatch.kernel));
                pass.set_bind_group(0, bind, &[]);
                let (x, y, z) = dispatch.dims;
                pass.dispatch_workgroups(x, y, z);
            }
        }
        encoder.copy_buffer_to_buffer(out, 0, &out_staging, 0, bytes);
        encoder.copy_buffer_to_buffer(status_buffer, 0, &status_staging, 0, 16);
        self.ctx.queue.submit(Some(encoder.finish()));
        let data =
            kernels::read_staging(&self.ctx, &[(&status_staging, 16), (&out_staging, bytes)])?;
        let bits = u32::from_le_bytes([data[0][0], data[0][1], data[0][2], data[0][3]]);
        if bits != 0 {
            return Err(GpuModernError::Domain(status::describe(bits)));
        }
        Ok(kernels::i64s(&data[1]))
    }

    /// `out = W x` (spec §5.2), exactly like the CPU `project`.
    pub fn project(&self, m: &DyadicRef<'_>, x: &[i64]) -> Result<Vec<i64>, GpuModernError> {
        if x.len() != m.cols {
            return Err(GpuModernError::Invalid("projection input width".into()));
        }
        let matrix = upload_matrix(&self.ctx, "lab.matrix", m, m.rows, m.cols)?;
        let status_buffer = kernels::storage_zeroed(&self.ctx, "status", 16);
        let input = self.i64_buffer("x", x);
        let digits = kernels::storage_zeroed(&self.ctx, "digits", (8 * matrix.words * 4) as u64);
        let ctrl = kernels::storage_zeroed(&self.ctx, "ctrl", 16);
        let out = self.zeroed("out", m.rows);
        let split_params = self.params([m.cols as u32, matrix.words as u32, 0, 0, 0, 0, 0, 0]);
        let max_dim = self.ctx.limits.max_compute_workgroups_per_dimension;
        let gemv_params: Vec<wgpu::Buffer> = matrix
            .chunks
            .iter()
            .map(|c| {
                self.params([
                    c.rows as u32,
                    matrix.words as u32,
                    c.row_base as u32,
                    m.rows as u32,
                    0,
                    0,
                    0,
                    0,
                ])
            })
            .collect();
        let mut dispatches = vec![Dispatch {
            kernel: Kernel::Split,
            entries: vec![
                (0, &status_buffer),
                (1, &split_params),
                (2, &input),
                (3, &digits),
                (4, &ctrl),
            ],
            dims: (1, 1, 1),
        }];
        for (chunk, params) in matrix.chunks.iter().zip(&gemv_params) {
            let (gx, gy) = kernels::grid_2d(kernels::groups(chunk.rows, GEMV_ROWS), max_dim);
            dispatches.push(Dispatch {
                kernel: Kernel::Gemv,
                entries: vec![
                    (0, &status_buffer),
                    (1, params),
                    (2, &chunk.weights),
                    (3, &chunk.scales),
                    (4, &digits),
                    (5, &ctrl),
                    (6, &out),
                ],
                dims: (gx, gy, 1),
            });
        }
        self.execute(&dispatches, &status_buffer, &out, m.rows)
    }

    /// RMS normalisation of one vector (spec §5.4).
    pub fn rms_norm(
        &self,
        x: &[i64],
        gain: &[i64],
        eps_q32: i64,
    ) -> Result<Vec<i64>, GpuModernError> {
        if x.is_empty() || gain.len() != x.len() || eps_q32 < 1 {
            return Err(GpuModernError::Invalid("rms_norm shape".into()));
        }
        let status_buffer = kernels::storage_zeroed(&self.ctx, "status", 16);
        let input = self.i64_buffer("x", x);
        let gains = self.i64_buffer("gain", gain);
        let out = self.zeroed("normed", x.len());
        let eps = eps_q32 as u64;
        let params = self.params([
            x.len() as u32,
            eps as u32,
            (eps >> 32) as u32,
            0,
            0,
            0,
            0,
            0,
        ]);
        let dispatch = Dispatch {
            kernel: Kernel::RmsNorm,
            entries: vec![
                (0, &status_buffer),
                (1, &params),
                (2, &input),
                (3, &gains),
                (4, &out),
            ],
            dims: (1, 1, 1),
        };
        self.execute(&[dispatch], &status_buffer, &out, x.len())
    }

    /// Split-half RoPE (spec §5.5) of `heads` heads of `d_head` at one
    /// position, with that position's `cos`/`sin` rows (`d_head/2` each).
    pub fn rope(
        &self,
        data: &[i64],
        d_head: usize,
        cos: &[i32],
        sin: &[i32],
    ) -> Result<Vec<i64>, GpuModernError> {
        let half = d_head / 2;
        if d_head == 0
            || !d_head.is_multiple_of(2)
            || !data.len().is_multiple_of(d_head)
            || cos.len() != half
            || sin.len() != half
        {
            return Err(GpuModernError::Invalid("rope shape".into()));
        }
        let heads = data.len() / d_head;
        let status_buffer = kernels::storage_zeroed(&self.ctx, "status", 16);
        let values = self.i64_buffer("data", data);
        let cos_table = kernels::storage_init(&self.ctx, "cos", &kernels::i32_bytes(cos));
        let sin_table = kernels::storage_init(&self.ctx, "sin", &kernels::i32_bytes(sin));
        let params = self.params([
            heads as u32,
            d_head as u32,
            half as u32,
            data.len() as u32,
            0,
            0,
            0,
            0,
        ]);
        let cursor = self.cursor(0, &[0]);
        let dispatch = Dispatch {
            kernel: Kernel::Rope,
            entries: vec![
                (0, &status_buffer),
                (1, &params),
                (2, &cursor),
                (3, &values),
                (4, &cos_table),
                (5, &sin_table),
            ],
            dims: (kernels::groups(heads * half, ELEMENT_WG), 1, 1),
        };
        self.execute(&[dispatch], &status_buffer, &values, data.len())
    }

    /// Two-pass attention for every query head (spec §5.6).
    pub fn attention(
        &self,
        case: &AttentionCase<'_>,
        exp_table: &[i64],
    ) -> Result<Vec<i64>, GpuModernError> {
        let exp_values = check_exp_table(exp_table)?;
        let stride = case.n_kv_heads * case.d_head;
        let width = case.n_heads * case.d_head;
        let shape_ok = case.positions > 0
            && case.n_kv_heads > 0
            && case.n_heads.is_multiple_of(case.n_kv_heads)
            && case.q.len() == width
            && case.keys.len() == case.positions * stride
            && case.values.len() == case.positions * stride
            && (1..=i64::from(u32::MAX)).contains(&case.lambda);
        if !shape_ok {
            return Err(GpuModernError::Invalid("attention shape".into()));
        }
        let status_buffer = kernels::storage_zeroed(&self.ctx, "status", 16);
        let q = self.i64_buffer("q", case.q);
        let keys = kernels::storage_init(&self.ctx, "keys", &kernels::i32_bytes(case.keys));
        let values = kernels::storage_init(&self.ctx, "values", &kernels::i32_bytes(case.values));
        let scratch = self.zeroed("scratch", case.n_heads * case.positions);
        let out = self.zeroed("attended", width);
        let table = kernels::storage_init(&self.ctx, "exp", bytemuck::cast_slice(&exp_values));
        let params = self.params([
            case.d_head as u32,
            (case.n_heads / case.n_kv_heads) as u32,
            stride as u32,
            case.lambda as u32,
            case.positions as u32,
            case.n_heads as u32,
            0,
            0,
        ]);
        let cursor = self.cursor(case.positions - 1, &[0]);
        let dispatch = Dispatch {
            kernel: Kernel::Attention,
            entries: vec![
                (0, &status_buffer),
                (1, &params),
                (2, &cursor),
                (3, &q),
                (4, &keys),
                (5, &values),
                (6, &scratch),
                (7, &out),
                (8, &table),
            ],
            dims: (case.n_heads as u32, 1, 1),
        };
        self.execute(&[dispatch], &status_buffer, &out, width)
    }

    /// Gated SiLU of each (gate, up) pair (spec §5.7).
    pub fn gated_silu(
        &self,
        gate: &[i64],
        up: &[i64],
        exp_table: &[i64],
    ) -> Result<Vec<i64>, GpuModernError> {
        if gate.is_empty() || gate.len() != up.len() {
            return Err(GpuModernError::Invalid("gated SiLU shape".into()));
        }
        let exp_values = check_exp_table(exp_table)?;
        let status_buffer = kernels::storage_zeroed(&self.ctx, "status", 16);
        let gates = self.i64_buffer("gate", gate);
        let ups = self.i64_buffer("up", up);
        let table = kernels::storage_init(&self.ctx, "exp", bytemuck::cast_slice(&exp_values));
        let params = self.params([gate.len() as u32, 0, 0, 0, 0, 0, 0, 0]);
        let cursor = self.cursor(0, &[0]);
        let dispatch = Dispatch {
            kernel: Kernel::Silu,
            entries: vec![
                (0, &status_buffer),
                (1, &params),
                (2, &gates),
                (3, &ups),
                (4, &cursor),
                (8, &table),
            ],
            dims: (kernels::groups(gate.len(), ELEMENT_WG), 1, 1),
        };
        self.execute(&[dispatch], &status_buffer, &gates, gate.len())
    }

    /// `h + delta` (spec §5.8).
    pub fn residual(&self, h: &[i64], delta: &[i64]) -> Result<Vec<i64>, GpuModernError> {
        if h.is_empty() || h.len() != delta.len() {
            return Err(GpuModernError::Invalid("residual shape".into()));
        }
        let status_buffer = kernels::storage_zeroed(&self.ctx, "status", 16);
        let hidden = self.i64_buffer("hidden", h);
        let deltas = self.i64_buffer("delta", delta);
        let params = self.params([h.len() as u32, 0, 0, 0, 0, 0, 0, 0]);
        let cursor = self.cursor(0, &[0]);
        let dispatch = Dispatch {
            kernel: Kernel::Residual,
            entries: vec![
                (0, &status_buffer),
                (1, &params),
                (2, &hidden),
                (3, &deltas),
                (4, &cursor),
            ],
            dims: (kernels::groups(h.len(), ELEMENT_WG), 1, 1),
        };
        self.execute(&[dispatch], &status_buffer, &hidden, h.len())
    }

    /// Embedding rows of `tokens` (spec §5.3), concatenated.
    pub fn embed(&self, m: &DyadicRef<'_>, tokens: &[u32]) -> Result<Vec<i64>, GpuModernError> {
        if tokens.is_empty() || tokens.len() > super::MAX_BATCH {
            return Err(GpuModernError::Invalid("embedding batch".into()));
        }
        if tokens.iter().any(|&t| t as usize >= m.rows) {
            return Err(GpuModernError::Domain(
                "token id is outside the vocabulary".into(),
            ));
        }
        let matrix = upload_matrix(&self.ctx, "lab.embed", m, m.rows, m.cols)?;
        if matrix.chunks.len() > 4 {
            return Err(GpuModernError::Unsupported(
                "embedding needs more than 4 chunks".into(),
            ));
        }
        let scale_words: Vec<u32> =
            m.mu.iter()
                .zip(m.k)
                .flat_map(|(&mu, &k)| [mu as u32, u32::from(k)])
                .collect();
        let scales = kernels::storage_init(&self.ctx, "scales", bytemuck::cast_slice(&scale_words));
        let status_buffer = kernels::storage_zeroed(&self.ctx, "status", 16);
        let out = self.zeroed("hidden", tokens.len() * m.cols);
        let params = self.params([
            m.cols as u32,
            matrix.words as u32,
            matrix.rows_per_chunk as u32,
            m.rows as u32,
            0,
            0,
            0,
            0,
        ]);
        let cursor = self.cursor(0, tokens);
        let chunk = |i: usize| &matrix.chunks[if i < matrix.chunks.len() { i } else { 0 }].weights;
        let dispatch = Dispatch {
            kernel: Kernel::Embed,
            entries: vec![
                (0, &status_buffer),
                (1, &params),
                (2, &cursor),
                (3, chunk(0)),
                (4, chunk(1)),
                (5, chunk(2)),
                (6, chunk(3)),
                (7, &scales),
                (9, &out),
            ],
            dims: (kernels::groups(m.cols, ELEMENT_WG), tokens.len() as u32, 1),
        };
        self.execute(&[dispatch], &status_buffer, &out, tokens.len() * m.cols)
    }
}
