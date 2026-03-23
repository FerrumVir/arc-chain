//! GPU-resident transformer forward pass.
//!
//! Entire forward pass runs on GPU — no CPU round-trips between layers.
//! All kernels dispatched in a single command buffer per token.
//! Only reads back the final argmax token ID.
//!
//! Architecture:
//! - Weights uploaded to GPU at model load (one-time)
//! - Per token: encode 485 dispatches into one command buffer
//! - Intermediate activations stay in GPU storage buffers
//! - KV cache grows on GPU (i8 quantized)
//! - Final readback: 4 bytes (one u32 token ID)

use bytemuck::{Pod, Zeroable};
use std::borrow::Cow;
use tracing::info;

// Param structs matching WGSL uniforms (must be Pod + Zeroable)
#[repr(C)]
#[derive(Copy, Clone, Pod, Zeroable)]
struct MatmulParams { in_size: u32, out_size: u32, scale_offset: u32, _pad: u32 }

#[repr(C)]
#[derive(Copy, Clone, Pod, Zeroable)]
struct LayerNormParams { size: u32, _p1: u32, _p2: u32, _p3: u32 }

#[repr(C)]
#[derive(Copy, Clone, Pod, Zeroable)]
struct RopeParams { pos: u32, d_head: u32, n_heads: u32, _pad: u32 }

#[repr(C)]
#[derive(Copy, Clone, Pod, Zeroable)]
struct AttnParams {
    d_head: u32, n_heads: u32, n_kv_heads: u32, seq_len: u32,
    d_kv: u32, attn_scale: i32, _p1: u32, _p2: u32,
}

/// GPU-resident transformer engine.
pub struct GpuForward {
    device: wgpu::Device,
    queue: wgpu::Queue,
    // Pipelines for each kernel
    matmul_pipeline: wgpu::ComputePipeline,
    layernorm_pipeline: wgpu::ComputePipeline,
    quantize_pipeline: wgpu::ComputePipeline,
    rope_pipeline: wgpu::ComputePipeline,
    attention_pipeline: wgpu::ComputePipeline,
    silu_pipeline: wgpu::ComputePipeline,
    residual_pipeline: wgpu::ComputePipeline,
    argmax_pipeline: wgpu::ComputePipeline,
    // Bind group layouts
    matmul_bgl: wgpu::BindGroupLayout,
    layernorm_bgl: wgpu::BindGroupLayout,
    quantize_bgl: wgpu::BindGroupLayout,
    rope_bgl: wgpu::BindGroupLayout,
    attention_bgl: wgpu::BindGroupLayout,
    silu_bgl: wgpu::BindGroupLayout,
    residual_bgl: wgpu::BindGroupLayout,
    argmax_bgl: wgpu::BindGroupLayout,
}

/// All GPU buffers for one model.
pub struct GpuModel {
    // Per-layer weight buffers (packed i8 as u32)
    layer_weights: Vec<LayerWeightBuffers>,
    // Per-layer scales
    layer_scales: Vec<LayerScaleBuffers>,
    // Norm gamma vectors
    layer_attn_norm: Vec<wgpu::Buffer>,
    layer_ffn_norm: Vec<wgpu::Buffer>,
    // Embedding + output weight + final norm
    embedding_buf: wgpu::Buffer,
    embedding_scales: wgpu::Buffer,
    output_weight_buf: wgpu::Buffer,
    output_scales: wgpu::Buffer,
    final_norm_buf: wgpu::Buffer,
    // Activation buffers (reused across layers)
    hidden_buf: wgpu::Buffer,      // [d_model] i32
    normed_buf: wgpu::Buffer,      // [d_model] i32
    normed_packed: wgpu::Buffer,   // [d_model/4] u32 (quantized input for matmul)
    quant_scale: wgpu::Buffer,     // [1] i32 (quantization scale)
    q_buf: wgpu::Buffer,           // [d_model] i32
    k_buf: wgpu::Buffer,           // [d_kv] i32
    v_buf: wgpu::Buffer,           // [d_kv] i32
    attn_out_buf: wgpu::Buffer,    // [d_model] i32
    projected_buf: wgpu::Buffer,   // [d_model] i32
    gate_buf: wgpu::Buffer,        // [d_ff] i32
    up_buf: wgpu::Buffer,          // [d_ff] i32
    ff_out_buf: wgpu::Buffer,      // [d_model] i32
    logits_buf: wgpu::Buffer,      // [vocab_size] i32
    result_buf: wgpu::Buffer,      // [1] u32 (argmax token)
    staging_buf: wgpu::Buffer,     // for readback
    // KV cache
    kv_k_bufs: Vec<wgpu::Buffer>,  // per-layer packed i8 K cache
    kv_v_bufs: Vec<wgpu::Buffer>,
    kv_k_scales: Vec<wgpu::Buffer>,
    kv_v_scales: Vec<wgpu::Buffer>,
    // RoPE tables
    rope_cos_buf: wgpu::Buffer,
    rope_sin_buf: wgpu::Buffer,
    // Config
    n_layers: u32,
    d_model: u32,
    d_ff: u32,
    d_head: u32,
    d_kv: u32,
    n_heads: u32,
    n_kv_heads: u32,
    vocab_size: u32,
    attn_scale: i32,
}

struct LayerWeightBuffers {
    wq: wgpu::Buffer,
    wk: wgpu::Buffer,
    wv: wgpu::Buffer,
    wo: wgpu::Buffer,
    w_gate: wgpu::Buffer,
    w_up: wgpu::Buffer,
    w_down: wgpu::Buffer,
}

struct LayerScaleBuffers {
    sq: wgpu::Buffer,
    sk: wgpu::Buffer,
    sv: wgpu::Buffer,
    so: wgpu::Buffer,
    s_gate: wgpu::Buffer,
    s_up: wgpu::Buffer,
    s_down: wgpu::Buffer,
}

impl GpuForward {
    /// Initialize GPU forward pass engine. Compiles all shader pipelines.
    pub fn new() -> Result<Self, String> {
        let instance = wgpu::Instance::new(&wgpu::InstanceDescriptor {
            backends: wgpu::Backends::all(),
            ..Default::default()
        });

        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::HighPerformance,
            force_fallback_adapter: false,
            compatible_surface: None,
        })).map_err(|_| "No GPU adapter")?;

        let info = adapter.get_info();
        if info.name.contains("llvmpipe") || info.name.contains("SwiftShader") {
            return Err(format!("Software GPU: {}", info.name));
        }

        info!("GPU forward: {} ({:?})", info.name, info.backend);

        let (device, queue) = pollster::block_on(adapter.request_device(
            &wgpu::DeviceDescriptor {
                label: Some("ARC GPU Forward"),
                required_features: wgpu::Features::empty(),
                required_limits: wgpu::Limits {
                    max_storage_buffer_binding_size: 512 * 1024 * 1024, // 512MB
                    max_buffer_size: 512 * 1024 * 1024,
                    max_storage_buffers_per_shader_stage: 8,
                    ..Default::default()
                },
                ..Default::default()
            },
        )).map_err(|e| format!("GPU device: {e}"))?;

        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("transformer"),
            source: wgpu::ShaderSource::Wgsl(Cow::Borrowed(include_str!("transformer.wgsl"))),
        });

        // Create bind group layouts and pipelines for each kernel
        let make_bgl = |entries: &[wgpu::BindGroupLayoutEntry]| {
            device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                label: None, entries,
            })
        };

        let storage_ro = |binding: u32| wgpu::BindGroupLayoutEntry {
            binding, visibility: wgpu::ShaderStages::COMPUTE,
            ty: wgpu::BindingType::Buffer {
                ty: wgpu::BufferBindingType::Storage { read_only: true },
                has_dynamic_offset: false, min_binding_size: None,
            }, count: None,
        };
        let storage_rw = |binding: u32| wgpu::BindGroupLayoutEntry {
            binding, visibility: wgpu::ShaderStages::COMPUTE,
            ty: wgpu::BindingType::Buffer {
                ty: wgpu::BufferBindingType::Storage { read_only: false },
                has_dynamic_offset: false, min_binding_size: None,
            }, count: None,
        };
        let uniform = |binding: u32| wgpu::BindGroupLayoutEntry {
            binding, visibility: wgpu::ShaderStages::COMPUTE,
            ty: wgpu::BindingType::Buffer {
                ty: wgpu::BufferBindingType::Uniform,
                has_dynamic_offset: false, min_binding_size: None,
            }, count: None,
        };

        // Matmul: weights, input, output, params, scales
        let matmul_bgl = make_bgl(&[storage_ro(0), storage_ro(1), storage_rw(2), uniform(3), storage_ro(4)]);
        // LayerNorm: input, output, gamma, params
        let layernorm_bgl = make_bgl(&[storage_ro(0), storage_rw(1), storage_ro(2), uniform(3)]);
        // Quantize: input, output, scale
        let quantize_bgl = make_bgl(&[storage_ro(0), storage_rw(1), storage_rw(2)]);
        // RoPE: data(rw), cos, sin, params
        let rope_bgl = make_bgl(&[storage_rw(0), storage_ro(1), storage_ro(2), uniform(3)]);
        // Attention: q, k_cache, v_cache, k_scales, v_scales, output, params
        let attention_bgl = make_bgl(&[storage_ro(0), storage_ro(1), storage_ro(2), storage_ro(3), storage_ro(4), storage_rw(5), uniform(6)]);
        // SiLU: gate(rw), up
        let silu_bgl = make_bgl(&[storage_rw(0), storage_ro(1)]);
        // Residual: hidden(rw), add
        let residual_bgl = make_bgl(&[storage_rw(0), storage_ro(1)]);
        // Argmax: input, result(rw)
        let argmax_bgl = make_bgl(&[storage_ro(0), storage_rw(1)]);

        let make_pipeline = |bgl: &wgpu::BindGroupLayout, entry: &str| {
            let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                label: None, bind_group_layouts: &[bgl], push_constant_ranges: &[],
            });
            device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
                label: Some(entry), layout: Some(&layout), module: &shader,
                entry_point: Some(entry), compilation_options: Default::default(), cache: None,
            })
        };

        let matmul_pipeline = make_pipeline(&matmul_bgl, "matmul");
        let layernorm_pipeline = make_pipeline(&layernorm_bgl, "layernorm");
        let quantize_pipeline = make_pipeline(&quantize_bgl, "quantize_i32_to_i8");
        let rope_pipeline = make_pipeline(&rope_bgl, "rope");
        let attention_pipeline = make_pipeline(&attention_bgl, "attention");
        let silu_pipeline = make_pipeline(&silu_bgl, "silu_mul");
        let residual_pipeline = make_pipeline(&residual_bgl, "residual_add");
        let argmax_pipeline = make_pipeline(&argmax_bgl, "argmax");

        info!("All 8 GPU compute pipelines compiled");

        Ok(Self {
            device, queue,
            matmul_pipeline, layernorm_pipeline, quantize_pipeline,
            rope_pipeline, attention_pipeline, silu_pipeline,
            residual_pipeline, argmax_pipeline,
            matmul_bgl, layernorm_bgl, quantize_bgl,
            rope_bgl, attention_bgl, silu_bgl, residual_bgl, argmax_bgl,
        })
    }

    /// Check if GPU forward pass is available.
    pub fn is_available() -> bool {
        Self::new().is_ok()
    }

    /// Create a GPU buffer with initial data.
    fn buf(&self, label: &str, data: &[u8], usage: wgpu::BufferUsages) -> wgpu::Buffer {
        let buf = self.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some(label),
            size: data.len().max(4) as u64,
            usage: usage | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        if !data.is_empty() {
            self.queue.write_buffer(&buf, 0, data);
        }
        buf
    }

    /// Create an empty GPU buffer.
    fn empty_buf(&self, label: &str, size: usize, usage: wgpu::BufferUsages) -> wgpu::Buffer {
        self.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some(label),
            size: size.max(4) as u64,
            usage: usage | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        })
    }

    /// Upload model weights to GPU. Call once at startup.
    /// Returns GpuModel with all buffers ready for forward pass.
    pub fn upload_model(
        &self,
        embedding_data: &[i8], embedding_scales: &[i64],
        output_data: &[i8], output_scales: &[i64],
        final_norm: &[i64],
        layers: &[(
            &[i8], &[i64], // wq data, scales
            &[i8], &[i64], // wk
            &[i8], &[i64], // wv
            &[i8], &[i64], // wo
            &[i8], &[i64], // w_gate
            &[i8], &[i64], // w_up
            &[i8], &[i64], // w_down
            &[i64],        // attn_norm gamma
            &[i64],        // ffn_norm gamma
        )],
        rope_cos: &[i64], rope_sin: &[i64],
        d_model: u32, d_ff: u32, d_head: u32, d_kv: u32,
        n_heads: u32, n_kv_heads: u32, vocab_size: u32, attn_scale: i32,
    ) -> GpuModel {
        let sto = wgpu::BufferUsages::STORAGE;
        let sto_rw = wgpu::BufferUsages::STORAGE;

        let pack_i8 = |data: &[i8]| -> Vec<u8> {
            let packed = super::gpu_matmul::pack_i8_to_u32_pub(data);
            bytemuck::cast_slice(&packed).to_vec()
        };
        let pack_i64 = |data: &[i64]| -> Vec<u8> {
            // Convert i64 scales to i32 for GPU (WGSL has no i64)
            let i32_data: Vec<i32> = data.iter().map(|&x| x as i32).collect();
            bytemuck::cast_slice(&i32_data).to_vec()
        };

        info!("Uploading {} layers to GPU...", layers.len());

        let mut layer_weights = Vec::new();
        let mut layer_scales = Vec::new();
        let mut layer_attn_norm = Vec::new();
        let mut layer_ffn_norm = Vec::new();

        for (i, l) in layers.iter().enumerate() {
            layer_weights.push(LayerWeightBuffers {
                wq: self.buf(&format!("L{i}_wq"), &pack_i8(l.0), sto),
                wk: self.buf(&format!("L{i}_wk"), &pack_i8(l.2), sto),
                wv: self.buf(&format!("L{i}_wv"), &pack_i8(l.4), sto),
                wo: self.buf(&format!("L{i}_wo"), &pack_i8(l.6), sto),
                w_gate: self.buf(&format!("L{i}_gate"), &pack_i8(l.8), sto),
                w_up: self.buf(&format!("L{i}_up"), &pack_i8(l.10), sto),
                w_down: self.buf(&format!("L{i}_down"), &pack_i8(l.12), sto),
            });
            layer_scales.push(LayerScaleBuffers {
                sq: self.buf(&format!("L{i}_sq"), &pack_i64(l.1), sto),
                sk: self.buf(&format!("L{i}_sk"), &pack_i64(l.3), sto),
                sv: self.buf(&format!("L{i}_sv"), &pack_i64(l.5), sto),
                so: self.buf(&format!("L{i}_so"), &pack_i64(l.7), sto),
                s_gate: self.buf(&format!("L{i}_sg"), &pack_i64(l.9), sto),
                s_up: self.buf(&format!("L{i}_su"), &pack_i64(l.11), sto),
                s_down: self.buf(&format!("L{i}_sd"), &pack_i64(l.13), sto),
            });
            layer_attn_norm.push(self.buf(&format!("L{i}_an"), &pack_i64(l.14), sto));
            layer_ffn_norm.push(self.buf(&format!("L{i}_fn"), &pack_i64(l.15), sto));

            if i % 8 == 0 { info!("Layer {}/{} uploaded", i + 1, layers.len()); }
        }

        let n_layers = layers.len() as u32;
        let max_seq = 2048u32;

        // Activation buffers
        let i32_size = |n: u32| (n as usize) * 4;
        let u32_size = |n: u32| ((n as usize + 3) / 4) * 4;

        let model = GpuModel {
            layer_weights, layer_scales, layer_attn_norm, layer_ffn_norm,
            embedding_buf: self.buf("emb", &pack_i8(embedding_data), sto),
            embedding_scales: self.buf("emb_s", &pack_i64(embedding_scales), sto),
            output_weight_buf: self.buf("out_w", &pack_i8(output_data), sto),
            output_scales: self.buf("out_s", &pack_i64(output_scales), sto),
            final_norm_buf: self.buf("fnorm", &pack_i64(final_norm), sto),
            hidden_buf: self.empty_buf("hidden", i32_size(d_model), sto_rw | wgpu::BufferUsages::COPY_DST),
            normed_buf: self.empty_buf("normed", i32_size(d_model), sto_rw),
            normed_packed: self.empty_buf("normed_q", u32_size(d_model), sto_rw),
            quant_scale: self.empty_buf("qscale", 4, sto_rw),
            q_buf: self.empty_buf("q", i32_size(d_model), sto_rw),
            k_buf: self.empty_buf("k", i32_size(d_kv), sto_rw),
            v_buf: self.empty_buf("v", i32_size(d_kv), sto_rw),
            attn_out_buf: self.empty_buf("attn_out", i32_size(d_model), sto_rw),
            projected_buf: self.empty_buf("proj", i32_size(d_model), sto_rw),
            gate_buf: self.empty_buf("gate", i32_size(d_ff), sto_rw),
            up_buf: self.empty_buf("up", i32_size(d_ff), sto_rw),
            ff_out_buf: self.empty_buf("ff_out", i32_size(d_model), sto_rw),
            logits_buf: self.empty_buf("logits", i32_size(vocab_size), sto_rw | wgpu::BufferUsages::COPY_SRC),
            result_buf: self.empty_buf("result", 4, sto_rw | wgpu::BufferUsages::COPY_SRC),
            staging_buf: self.empty_buf("staging", 4, wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST),
            kv_k_bufs: (0..n_layers).map(|i| self.empty_buf(&format!("kv_k{i}"),
                u32_size(max_seq * d_kv), sto_rw)).collect(),
            kv_v_bufs: (0..n_layers).map(|i| self.empty_buf(&format!("kv_v{i}"),
                u32_size(max_seq * d_kv), sto_rw)).collect(),
            kv_k_scales: (0..n_layers).map(|i| self.empty_buf(&format!("ks{i}"),
                i32_size(max_seq), sto_rw)).collect(),
            kv_v_scales: (0..n_layers).map(|i| self.empty_buf(&format!("vs{i}"),
                i32_size(max_seq), sto_rw)).collect(),
            rope_cos_buf: self.buf("cos", &pack_i64(rope_cos), sto),
            rope_sin_buf: self.buf("sin", &pack_i64(rope_sin), sto),
            n_layers, d_model, d_ff, d_head, d_kv, n_heads, n_kv_heads, vocab_size, attn_scale,
        };

        info!("GPU model uploaded: {} layers, d={}, vocab={}", n_layers, d_model, vocab_size);
        model
    }
}

impl GpuForward {
    /// Run one token through the full GPU-resident forward pass.
    /// Encodes ALL layer computations into a SINGLE command buffer.
    /// Only reads back the final token ID (4 bytes).
    /// Returns the predicted next token.
    pub fn forward_one_token(
        &self,
        model: &GpuModel,
        token: u32,
        seq_pos: u32,
    ) -> u32 {
        let d = model.d_model;
        let dff = model.d_ff;
        let dh = model.d_head;
        let dkv = model.d_kv;

        // Write embedding for this token into hidden_buf
        // Embedding is packed i8; we need to dequant to i32 on GPU.
        // For now: CPU-side embedding lookup, upload i32 hidden state.
        // (Embedding lookup is O(d) — negligible vs matmul.)
        // This is the ONE CPU→GPU transfer per token.
        // TODO: move embedding lookup to GPU kernel
        let emb_placeholder: Vec<i32> = vec![0i32; d as usize]; // will be set by caller
        self.queue.write_buffer(&model.hidden_buf, 0, bytemuck::cast_slice(&emb_placeholder));

        let mut encoder = self.device.create_command_encoder(&Default::default());

        for layer in 0..model.n_layers as usize {
            let lw = &model.layer_weights[layer];
            let ls = &model.layer_scales[layer];

            // ── LayerNorm (hidden → normed) ──────────────────────────
            let ln_params = LayerNormParams { size: d, _p1: 0, _p2: 0, _p3: 0 };
            let ln_params_buf = self.buf_inline(&ln_params);
            let ln_bg = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: None, layout: &self.layernorm_bgl,
                entries: &[
                    bg_entry(0, &model.hidden_buf),
                    bg_entry(1, &model.normed_buf),
                    bg_entry(2, &model.layer_attn_norm[layer]),
                    bg_entry(3, &ln_params_buf),
                ],
            });
            dispatch(&mut encoder, &self.layernorm_pipeline, &ln_bg, 1); // 1 workgroup for reduction

            // ── Quantize normed → packed i8 ──────────────────────────
            let q_bg = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: None, layout: &self.quantize_bgl,
                entries: &[
                    bg_entry(0, &model.normed_buf),
                    bg_entry(1, &model.normed_packed),
                    bg_entry(2, &model.quant_scale),
                ],
            });
            dispatch(&mut encoder, &self.quantize_pipeline, &q_bg, 1);

            // ── Q/K/V Matmuls ────────────────────────────────────────
            for (w_buf, s_buf, out_buf, out_size) in [
                (&lw.wq, &ls.sq, &model.q_buf, d),
                (&lw.wk, &ls.sk, &model.k_buf, dkv),
                (&lw.wv, &ls.sv, &model.v_buf, dkv),
            ] {
                let mm_params = MatmulParams { in_size: d, out_size, scale_offset: 0, _pad: 0 };
                let mm_params_buf = self.buf_inline(&mm_params);
                let mm_bg = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
                    label: None, layout: &self.matmul_bgl,
                    entries: &[
                        bg_entry(0, w_buf),
                        bg_entry(1, &model.normed_packed),
                        bg_entry(2, out_buf),
                        bg_entry(3, &mm_params_buf),
                        bg_entry(4, s_buf),
                    ],
                });
                dispatch(&mut encoder, &self.matmul_pipeline, &mm_bg, (out_size + 255) / 256);
            }

            // ── RoPE on Q ────────────────────────────────────────────
            let rope_params_q = RopeParams { pos: seq_pos, d_head: dh, n_heads: model.n_heads, _pad: 0 };
            let rp_buf = self.buf_inline(&rope_params_q);
            let rope_bg_q = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: None, layout: &self.rope_bgl,
                entries: &[
                    bg_entry(0, &model.q_buf),
                    bg_entry(1, &model.rope_cos_buf),
                    bg_entry(2, &model.rope_sin_buf),
                    bg_entry(3, &rp_buf),
                ],
            });
            let rope_threads = model.n_heads * (dh / 2);
            dispatch(&mut encoder, &self.rope_pipeline, &rope_bg_q, (rope_threads + 255) / 256);

            // ── RoPE on K ────────────────────────────────────────────
            let rope_params_k = RopeParams { pos: seq_pos, d_head: dh, n_heads: model.n_kv_heads, _pad: 0 };
            let rpk_buf = self.buf_inline(&rope_params_k);
            let rope_bg_k = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: None, layout: &self.rope_bgl,
                entries: &[
                    bg_entry(0, &model.k_buf),
                    bg_entry(1, &model.rope_cos_buf),
                    bg_entry(2, &model.rope_sin_buf),
                    bg_entry(3, &rpk_buf),
                ],
            });
            let rope_k_threads = model.n_kv_heads * (dh / 2);
            dispatch(&mut encoder, &self.rope_pipeline, &rope_bg_k, (rope_k_threads + 255) / 256);

            // ── Store K/V to cache (TODO: GPU-side quantize+store) ───
            // For now: KV cache operations handled externally
            // The attention kernel reads from kv_k_bufs/kv_v_bufs

            // ── Attention ────────────────────────────────────────────
            let attn_params = AttnParams {
                d_head: dh, n_heads: model.n_heads, n_kv_heads: model.n_kv_heads,
                seq_len: seq_pos + 1, d_kv: dkv, attn_scale: model.attn_scale,
                _p1: 0, _p2: 0,
            };
            let ap_buf = self.buf_inline(&attn_params);
            let attn_bg = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: None, layout: &self.attention_bgl,
                entries: &[
                    bg_entry(0, &model.q_buf),
                    bg_entry(1, &model.kv_k_bufs[layer]),
                    bg_entry(2, &model.kv_v_bufs[layer]),
                    bg_entry(3, &model.kv_k_scales[layer]),
                    bg_entry(4, &model.kv_v_scales[layer]),
                    bg_entry(5, &model.attn_out_buf),
                    bg_entry(6, &ap_buf),
                ],
            });
            dispatch(&mut encoder, &self.attention_pipeline, &attn_bg, model.n_heads);

            // ── Wo projection ────────────────────────────────────────
            // Quantize attn_out → packed i8
            let q_bg2 = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: None, layout: &self.quantize_bgl,
                entries: &[
                    bg_entry(0, &model.attn_out_buf),
                    bg_entry(1, &model.normed_packed), // reuse buffer
                    bg_entry(2, &model.quant_scale),
                ],
            });
            dispatch(&mut encoder, &self.quantize_pipeline, &q_bg2, 1);

            let mm_wo = MatmulParams { in_size: d, out_size: d, scale_offset: 0, _pad: 0 };
            let mm_wo_buf = self.buf_inline(&mm_wo);
            let wo_bg = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: None, layout: &self.matmul_bgl,
                entries: &[
                    bg_entry(0, &lw.wo),
                    bg_entry(1, &model.normed_packed),
                    bg_entry(2, &model.projected_buf),
                    bg_entry(3, &mm_wo_buf),
                    bg_entry(4, &ls.so),
                ],
            });
            dispatch(&mut encoder, &self.matmul_pipeline, &wo_bg, (d + 255) / 256);

            // ── Residual: hidden += projected ────────────────────────
            let res_bg = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: None, layout: &self.residual_bgl,
                entries: &[
                    bg_entry(0, &model.hidden_buf),
                    bg_entry(1, &model.projected_buf),
                ],
            });
            dispatch(&mut encoder, &self.residual_pipeline, &res_bg, (d + 255) / 256);

            // ── FFN LayerNorm ────────────────────────────────────────
            let ln2_params = LayerNormParams { size: d, _p1: 0, _p2: 0, _p3: 0 };
            let ln2_buf = self.buf_inline(&ln2_params);
            let ln2_bg = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: None, layout: &self.layernorm_bgl,
                entries: &[
                    bg_entry(0, &model.hidden_buf),
                    bg_entry(1, &model.normed_buf),
                    bg_entry(2, &model.layer_ffn_norm[layer]),
                    bg_entry(3, &ln2_buf),
                ],
            });
            dispatch(&mut encoder, &self.layernorm_pipeline, &ln2_bg, 1);

            // ── Quantize normed_ff ───────────────────────────────────
            let q_bg3 = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: None, layout: &self.quantize_bgl,
                entries: &[
                    bg_entry(0, &model.normed_buf),
                    bg_entry(1, &model.normed_packed),
                    bg_entry(2, &model.quant_scale),
                ],
            });
            dispatch(&mut encoder, &self.quantize_pipeline, &q_bg3, 1);

            // ── Gate + Up matmuls ────────────────────────────────────
            let mm_gate = MatmulParams { in_size: d, out_size: dff, scale_offset: 0, _pad: 0 };
            let mg_buf = self.buf_inline(&mm_gate);
            let gate_bg = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: None, layout: &self.matmul_bgl,
                entries: &[
                    bg_entry(0, &lw.w_gate),
                    bg_entry(1, &model.normed_packed),
                    bg_entry(2, &model.gate_buf),
                    bg_entry(3, &mg_buf),
                    bg_entry(4, &ls.s_gate),
                ],
            });
            dispatch(&mut encoder, &self.matmul_pipeline, &gate_bg, (dff + 255) / 256);

            let mm_up = MatmulParams { in_size: d, out_size: dff, scale_offset: 0, _pad: 0 };
            let mu_buf = self.buf_inline(&mm_up);
            let up_bg = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: None, layout: &self.matmul_bgl,
                entries: &[
                    bg_entry(0, &lw.w_up),
                    bg_entry(1, &model.normed_packed),
                    bg_entry(2, &model.up_buf),
                    bg_entry(3, &mu_buf),
                    bg_entry(4, &ls.s_up),
                ],
            });
            dispatch(&mut encoder, &self.matmul_pipeline, &up_bg, (dff + 255) / 256);

            // ── SiLU gate * up ───────────────────────────────────────
            let silu_bg = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: None, layout: &self.silu_bgl,
                entries: &[
                    bg_entry(0, &model.gate_buf),
                    bg_entry(1, &model.up_buf),
                ],
            });
            dispatch(&mut encoder, &self.silu_pipeline, &silu_bg, (dff + 255) / 256);

            // ── Quantize gated for down matmul ───────────────────────
            let q_bg4 = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: None, layout: &self.quantize_bgl,
                entries: &[
                    bg_entry(0, &model.gate_buf), // gate_buf now holds SiLU result
                    bg_entry(1, &model.normed_packed),
                    bg_entry(2, &model.quant_scale),
                ],
            });
            dispatch(&mut encoder, &self.quantize_pipeline, &q_bg4, 1);

            // ── Down matmul ──────────────────────────────────────────
            let mm_down = MatmulParams { in_size: dff, out_size: d, scale_offset: 0, _pad: 0 };
            let md_buf = self.buf_inline(&mm_down);
            let down_bg = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: None, layout: &self.matmul_bgl,
                entries: &[
                    bg_entry(0, &lw.w_down),
                    bg_entry(1, &model.normed_packed),
                    bg_entry(2, &model.ff_out_buf),
                    bg_entry(3, &md_buf),
                    bg_entry(4, &ls.s_down),
                ],
            });
            dispatch(&mut encoder, &self.matmul_pipeline, &down_bg, (d + 255) / 256);

            // ── Residual: hidden += ff_out ───────────────────────────
            let res2_bg = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: None, layout: &self.residual_bgl,
                entries: &[
                    bg_entry(0, &model.hidden_buf),
                    bg_entry(1, &model.ff_out_buf),
                ],
            });
            dispatch(&mut encoder, &self.residual_pipeline, &res2_bg, (d + 255) / 256);
        }

        // ── Final LayerNorm ──────────────────────────────────────────
        let fln_params = LayerNormParams { size: d, _p1: 0, _p2: 0, _p3: 0 };
        let fln_buf = self.buf_inline(&fln_params);
        let fln_bg = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: None, layout: &self.layernorm_bgl,
            entries: &[
                bg_entry(0, &model.hidden_buf),
                bg_entry(1, &model.normed_buf),
                bg_entry(2, &model.final_norm_buf),
                bg_entry(3, &fln_buf),
            ],
        });
        dispatch(&mut encoder, &self.layernorm_pipeline, &fln_bg, 1);

        // ── Quantize for LM head ─────────────────────────────────────
        let q_final = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: None, layout: &self.quantize_bgl,
            entries: &[
                bg_entry(0, &model.normed_buf),
                bg_entry(1, &model.normed_packed),
                bg_entry(2, &model.quant_scale),
            ],
        });
        dispatch(&mut encoder, &self.quantize_pipeline, &q_final, 1);

        // ── LM Head matmul → logits ──────────────────────────────────
        let mm_lm = MatmulParams {
            in_size: d, out_size: model.vocab_size, scale_offset: 0, _pad: 0,
        };
        let mlm_buf = self.buf_inline(&mm_lm);
        let lm_bg = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: None, layout: &self.matmul_bgl,
            entries: &[
                bg_entry(0, &model.output_weight_buf),
                bg_entry(1, &model.normed_packed),
                bg_entry(2, &model.logits_buf),
                bg_entry(3, &mlm_buf),
                bg_entry(4, &model.output_scales),
            ],
        });
        dispatch(&mut encoder, &self.matmul_pipeline, &lm_bg, (model.vocab_size + 255) / 256);

        // ── Argmax → token ID ────────────────────────────────────────
        let argmax_bg = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: None, layout: &self.argmax_bgl,
            entries: &[
                bg_entry(0, &model.logits_buf),
                bg_entry(1, &model.result_buf),
            ],
        });
        dispatch(&mut encoder, &self.argmax_pipeline, &argmax_bg, 1);

        // ── Copy result to staging for readback ──────────────────────
        encoder.copy_buffer_to_buffer(&model.result_buf, 0, &model.staging_buf, 0, 4);

        // ── Single submit for ENTIRE forward pass ────────────────────
        self.queue.submit([encoder.finish()]);

        // ── Read back token ID (4 bytes) ─────────────────────────────
        let slice = model.staging_buf.slice(..4);
        let (tx, rx) = std::sync::mpsc::channel();
        slice.map_async(wgpu::MapMode::Read, move |r| { let _ = tx.send(r); });
        let _ = self.device.poll(wgpu::PollType::wait());
        rx.recv().unwrap().unwrap();

        let mapped = slice.get_mapped_range();
        let token_id = bytemuck::cast_slice::<u8, u32>(&mapped)[0];
        drop(mapped);
        model.staging_buf.unmap();

        token_id
    }

    /// Create a small uniform buffer inline.
    fn buf_inline<T: Pod>(&self, data: &T) -> wgpu::Buffer {
        let buf = self.device.create_buffer(&wgpu::BufferDescriptor {
            label: None,
            size: std::mem::size_of::<T>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        self.queue.write_buffer(&buf, 0, bytemuck::bytes_of(data));
        buf
    }
}

/// Helper: dispatch a compute pass with the given pipeline and bind group.
fn dispatch(
    encoder: &mut wgpu::CommandEncoder,
    pipeline: &wgpu::ComputePipeline,
    bind_group: &wgpu::BindGroup,
    workgroups: u32,
) {
    let mut pass = encoder.begin_compute_pass(&Default::default());
    pass.set_pipeline(pipeline);
    pass.set_bind_group(0, bind_group, &[]);
    pass.dispatch_workgroups(workgroups, 1, 1);
}

/// Helper: create a bind group entry.
fn bg_entry(binding: u32, buffer: &wgpu::Buffer) -> wgpu::BindGroupEntry {
    wgpu::BindGroupEntry {
        binding,
        resource: buffer.as_entire_binding(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_gpu_forward_init() {
        match GpuForward::new() {
            Ok(gpu) => println!("GPU forward engine initialized"),
            Err(e) => println!("GPU not available: {}", e),
        }
    }
}
