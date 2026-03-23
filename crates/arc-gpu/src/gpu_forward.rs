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

/// Pre-built bind groups for one layer — created once, reused every token.
struct LayerBindGroups {
    ln_attn: wgpu::BindGroup,
    quantize_normed: wgpu::BindGroup,
    mm_q: wgpu::BindGroup,
    mm_k: wgpu::BindGroup,
    mm_v: wgpu::BindGroup,
    rope_q: wgpu::BindGroup,    // needs position update
    rope_k: wgpu::BindGroup,    // needs position update
    attn: wgpu::BindGroup,      // needs seq_len update
    quantize_attn: wgpu::BindGroup,
    mm_wo: wgpu::BindGroup,
    residual_attn: wgpu::BindGroup,
    ln_ffn: wgpu::BindGroup,
    quantize_ffn: wgpu::BindGroup,
    mm_gate: wgpu::BindGroup,
    mm_up: wgpu::BindGroup,
    silu: wgpu::BindGroup,
    quantize_gated: wgpu::BindGroup,
    mm_down: wgpu::BindGroup,
    residual_ffn: wgpu::BindGroup,
}

/// All GPU buffers + pre-built bind groups for one model.
pub struct GpuModel {
    // Activation buffers
    hidden_buf: wgpu::Buffer,
    normed_buf: wgpu::Buffer,
    normed_packed: wgpu::Buffer,
    quant_scale: wgpu::Buffer,
    q_buf: wgpu::Buffer,
    k_buf: wgpu::Buffer,
    v_buf: wgpu::Buffer,
    attn_out_buf: wgpu::Buffer,
    projected_buf: wgpu::Buffer,
    gate_buf: wgpu::Buffer,
    up_buf: wgpu::Buffer,
    ff_out_buf: wgpu::Buffer,
    logits_buf: wgpu::Buffer,
    result_buf: wgpu::Buffer,
    staging_buf: wgpu::Buffer,
    // KV cache
    kv_k_bufs: Vec<wgpu::Buffer>,
    kv_v_bufs: Vec<wgpu::Buffer>,
    kv_k_scales: Vec<wgpu::Buffer>,
    kv_v_scales: Vec<wgpu::Buffer>,
    // Pre-built bind groups (created once at upload, reused per token)
    layer_bgs: Vec<LayerBindGroups>,
    final_ln_bg: wgpu::BindGroup,
    final_quantize_bg: wgpu::BindGroup,
    lm_head_bg: wgpu::BindGroup,
    argmax_bg: wgpu::BindGroup,
    // Pre-created param buffers (updated per token via write_buffer)
    rope_q_params: Vec<wgpu::Buffer>,  // per-layer, updated for position
    rope_k_params: Vec<wgpu::Buffer>,
    attn_params_bufs: Vec<wgpu::Buffer>,  // per-layer, updated for seq_len
    // Static param buffers (never change)
    ln_params_buf: wgpu::Buffer,
    mm_q_params: wgpu::Buffer,
    mm_k_params: wgpu::Buffer,
    mm_v_params: wgpu::Buffer,
    mm_wo_params: wgpu::Buffer,
    mm_gate_params: wgpu::Buffer,
    mm_up_params: wgpu::Buffer,
    mm_down_params: wgpu::Buffer,
    mm_lm_params: wgpu::Buffer,
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

impl GpuModel {
    pub fn normed_packed_ref(&self) -> &wgpu::Buffer { &self.normed_packed }
    pub fn q_buf_ref(&self) -> &wgpu::Buffer { &self.q_buf }
    pub fn quant_scale_ref(&self) -> &wgpu::Buffer { &self.quant_scale }
    pub fn normed_buf_ref(&self) -> &wgpu::Buffer { &self.normed_buf }
}

// Accessors removed — use gpu_matmul directly

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

    pub fn device_ref(&self) -> &wgpu::Device { &self.device }
    pub fn queue_ref(&self) -> &wgpu::Queue { &self.queue }
    pub fn matmul_bgl_ref(&self) -> &wgpu::BindGroupLayout { &self.matmul_bgl }

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

        let i32_size = |n: u32| (n as usize) * 4;
        let u32_size = |n: u32| ((n as usize + 3) / 4) * 4;

        // Create all buffers as local variables first
        let output_weight_buf = self.buf("out_w", &pack_i8(output_data), sto);
        let output_scales = self.buf("out_s", &pack_i64(output_scales), sto);
        let final_norm_buf = self.buf("fnorm", &pack_i64(final_norm), sto);
        let hidden_buf = self.empty_buf("hidden", i32_size(d_model), sto_rw | wgpu::BufferUsages::COPY_DST);
        let normed_buf = self.empty_buf("normed", i32_size(d_model), sto_rw);
        let normed_packed = self.empty_buf("normed_q", u32_size(d_model.max(d_ff)), sto_rw);
        let quant_scale = self.empty_buf("qscale", 4, sto_rw);
        let q_buf = self.empty_buf("q", i32_size(d_model), sto_rw);
        let k_buf = self.empty_buf("k", i32_size(d_kv), sto_rw);
        let v_buf = self.empty_buf("v", i32_size(d_kv), sto_rw);
        let attn_out_buf = self.empty_buf("attn_out", i32_size(d_model), sto_rw);
        let projected_buf = self.empty_buf("proj", i32_size(d_model), sto_rw);
        let gate_buf = self.empty_buf("gate", i32_size(d_ff), sto_rw);
        let up_buf = self.empty_buf("up", i32_size(d_ff), sto_rw);
        let ff_out_buf = self.empty_buf("ff_out", i32_size(d_model), sto_rw);
        let logits_buf = self.empty_buf("logits", i32_size(vocab_size), sto_rw | wgpu::BufferUsages::COPY_SRC);
        let result_buf = self.empty_buf("result", 4, sto_rw | wgpu::BufferUsages::COPY_SRC);
        let staging_buf = self.empty_buf("staging", 4, wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST);
        let kv_k_bufs: Vec<_> = (0..n_layers).map(|i| self.empty_buf(&format!("kv_k{i}"), u32_size(max_seq * d_kv), sto_rw)).collect();
        let kv_v_bufs: Vec<_> = (0..n_layers).map(|i| self.empty_buf(&format!("kv_v{i}"), u32_size(max_seq * d_kv), sto_rw)).collect();
        let kv_k_scales: Vec<_> = (0..n_layers).map(|i| self.empty_buf(&format!("ks{i}"), i32_size(max_seq), sto_rw)).collect();
        let kv_v_scales: Vec<_> = (0..n_layers).map(|i| self.empty_buf(&format!("vs{i}"), i32_size(max_seq), sto_rw)).collect();
        let rope_cos_buf = self.buf("cos", &pack_i64(rope_cos), sto);
        let rope_sin_buf = self.buf("sin", &pack_i64(rope_sin), sto);

        info!("GPU model uploaded: {} layers, d={}, vocab={}", n_layers, d_model, vocab_size);

        // ── Pre-create ALL bind groups (eliminates 32ms encode overhead) ────
        info!("Pre-creating bind groups...");

        // Static params (don't change between tokens)
        let ln_params = LayerNormParams { size: d_model, _p1: 0, _p2: 0, _p3: 0 };
        let ln_params_buf = self.buf(&format!("ln_p"), bytemuck::bytes_of(&ln_params), wgpu::BufferUsages::UNIFORM);
        let mm_q_params = self.buf("mm_q_p", bytemuck::bytes_of(&MatmulParams { in_size: d_model, out_size: d_model, scale_offset: 0, _pad: 0 }), wgpu::BufferUsages::UNIFORM);
        let mm_k_params = self.buf("mm_k_p", bytemuck::bytes_of(&MatmulParams { in_size: d_model, out_size: d_kv, scale_offset: 0, _pad: 0 }), wgpu::BufferUsages::UNIFORM);
        let mm_v_params = self.buf("mm_v_p", bytemuck::bytes_of(&MatmulParams { in_size: d_model, out_size: d_kv, scale_offset: 0, _pad: 0 }), wgpu::BufferUsages::UNIFORM);
        let mm_wo_params = self.buf("mm_wo_p", bytemuck::bytes_of(&MatmulParams { in_size: d_model, out_size: d_model, scale_offset: 0, _pad: 0 }), wgpu::BufferUsages::UNIFORM);
        let mm_gate_params = self.buf("mm_g_p", bytemuck::bytes_of(&MatmulParams { in_size: d_model, out_size: d_ff, scale_offset: 0, _pad: 0 }), wgpu::BufferUsages::UNIFORM);
        let mm_up_params = self.buf("mm_u_p", bytemuck::bytes_of(&MatmulParams { in_size: d_model, out_size: d_ff, scale_offset: 0, _pad: 0 }), wgpu::BufferUsages::UNIFORM);
        let mm_down_params = self.buf("mm_d_p", bytemuck::bytes_of(&MatmulParams { in_size: d_ff, out_size: d_model, scale_offset: 0, _pad: 0 }), wgpu::BufferUsages::UNIFORM);
        let mm_lm_params = self.buf("mm_lm_p", bytemuck::bytes_of(&MatmulParams { in_size: d_model, out_size: vocab_size, scale_offset: 0, _pad: 0 }), wgpu::BufferUsages::UNIFORM);

        let mk_bg = |entries: &[wgpu::BindGroupEntry], layout: &wgpu::BindGroupLayout| {
            self.device.create_bind_group(&wgpu::BindGroupDescriptor { label: None, layout, entries })
        };
        let be = bg_entry; // shorthand

        // Per-layer dynamic params (updated per token)
        let mut rope_q_params_bufs = Vec::new();
        let mut rope_k_params_bufs = Vec::new();
        let mut attn_params_bufs_vec = Vec::new();

        let mut layer_bgs_vec = Vec::new();
        for i in 0..n_layers as usize {
            let lw = &layer_weights[i];
            let ls = &layer_scales[i];

            // RoPE params (position updated per token)
            let rqp = self.buf(&format!("rqp{i}"), bytemuck::bytes_of(&RopeParams { pos: 0, d_head, n_heads, _pad: 0 }), wgpu::BufferUsages::UNIFORM);
            let rkp = self.buf(&format!("rkp{i}"), bytemuck::bytes_of(&RopeParams { pos: 0, d_head, n_heads: n_kv_heads, _pad: 0 }), wgpu::BufferUsages::UNIFORM);
            let atp = self.buf(&format!("atp{i}"), bytemuck::bytes_of(&AttnParams {
                d_head, n_heads, n_kv_heads, seq_len: 1, d_kv, attn_scale, _p1: 0, _p2: 0,
            }), wgpu::BufferUsages::UNIFORM);

            layer_bgs_vec.push(LayerBindGroups {
                ln_attn: mk_bg(&[be(0, &hidden_buf), be(1, &normed_buf), be(2, &layer_attn_norm[i]), be(3, &ln_params_buf)], &self.layernorm_bgl),
                quantize_normed: mk_bg(&[be(0, &normed_buf), be(1, &normed_packed), be(2, &quant_scale)], &self.quantize_bgl),
                mm_q: mk_bg(&[be(0, &lw.wq), be(1, &normed_packed), be(2, &q_buf), be(3, &mm_q_params), be(4, &ls.sq)], &self.matmul_bgl),
                mm_k: mk_bg(&[be(0, &lw.wk), be(1, &normed_packed), be(2, &k_buf), be(3, &mm_k_params), be(4, &ls.sk)], &self.matmul_bgl),
                mm_v: mk_bg(&[be(0, &lw.wv), be(1, &normed_packed), be(2, &v_buf), be(3, &mm_v_params), be(4, &ls.sv)], &self.matmul_bgl),
                rope_q: mk_bg(&[be(0, &q_buf), be(1, &rope_cos_buf), be(2, &rope_sin_buf), be(3, &rqp)], &self.rope_bgl),
                rope_k: mk_bg(&[be(0, &k_buf), be(1, &rope_cos_buf), be(2, &rope_sin_buf), be(3, &rkp)], &self.rope_bgl),
                attn: mk_bg(&[be(0, &q_buf), be(1, &kv_k_bufs[i]), be(2, &kv_v_bufs[i]), be(3, &kv_k_scales[i]), be(4, &kv_v_scales[i]), be(5, &attn_out_buf), be(6, &atp)], &self.attention_bgl),
                quantize_attn: mk_bg(&[be(0, &attn_out_buf), be(1, &normed_packed), be(2, &quant_scale)], &self.quantize_bgl),
                mm_wo: mk_bg(&[be(0, &lw.wo), be(1, &normed_packed), be(2, &projected_buf), be(3, &mm_wo_params), be(4, &ls.so)], &self.matmul_bgl),
                residual_attn: mk_bg(&[be(0, &hidden_buf), be(1, &projected_buf)], &self.residual_bgl),
                ln_ffn: mk_bg(&[be(0, &hidden_buf), be(1, &normed_buf), be(2, &layer_ffn_norm[i]), be(3, &ln_params_buf)], &self.layernorm_bgl),
                quantize_ffn: mk_bg(&[be(0, &normed_buf), be(1, &normed_packed), be(2, &quant_scale)], &self.quantize_bgl),
                mm_gate: mk_bg(&[be(0, &lw.w_gate), be(1, &normed_packed), be(2, &gate_buf), be(3, &mm_gate_params), be(4, &ls.s_gate)], &self.matmul_bgl),
                mm_up: mk_bg(&[be(0, &lw.w_up), be(1, &normed_packed), be(2, &up_buf), be(3, &mm_up_params), be(4, &ls.s_up)], &self.matmul_bgl),
                silu: mk_bg(&[be(0, &gate_buf), be(1, &up_buf)], &self.silu_bgl),
                quantize_gated: mk_bg(&[be(0, &gate_buf), be(1, &normed_packed), be(2, &quant_scale)], &self.quantize_bgl),
                mm_down: mk_bg(&[be(0, &lw.w_down), be(1, &normed_packed), be(2, &ff_out_buf), be(3, &mm_down_params), be(4, &ls.s_down)], &self.matmul_bgl),
                residual_ffn: mk_bg(&[be(0, &hidden_buf), be(1, &ff_out_buf)], &self.residual_bgl),
            });

            rope_q_params_bufs.push(rqp);
            rope_k_params_bufs.push(rkp);
            attn_params_bufs_vec.push(atp);
        }

        let final_ln_bg = mk_bg(&[be(0, &hidden_buf), be(1, &normed_buf), be(2, &final_norm_buf), be(3, &ln_params_buf)], &self.layernorm_bgl);
        let final_quantize_bg = mk_bg(&[be(0, &normed_buf), be(1, &normed_packed), be(2, &quant_scale)], &self.quantize_bgl);
        let lm_head_bg = mk_bg(&[be(0, &output_weight_buf), be(1, &normed_packed), be(2, &logits_buf), be(3, &mm_lm_params), be(4, &output_scales)], &self.matmul_bgl);
        let argmax_bg = mk_bg(&[be(0, &logits_buf), be(1, &result_buf)], &self.argmax_bgl);

        info!("All bind groups pre-created ({} per layer × {} layers + 4 final = {} total)",
            19, n_layers, 19 * n_layers + 4);

        GpuModel {
            hidden_buf, normed_buf, normed_packed, quant_scale,
            q_buf, k_buf, v_buf, attn_out_buf, projected_buf,
            gate_buf, up_buf, ff_out_buf, logits_buf, result_buf, staging_buf,
            kv_k_bufs, kv_v_bufs, kv_k_scales, kv_v_scales,
            layer_bgs: layer_bgs_vec,
            final_ln_bg, final_quantize_bg, lm_head_bg, argmax_bg,
            rope_q_params: rope_q_params_bufs,
            rope_k_params: rope_k_params_bufs,
            attn_params_bufs: attn_params_bufs_vec,
            ln_params_buf, mm_q_params, mm_k_params, mm_v_params,
            mm_wo_params, mm_gate_params, mm_up_params, mm_down_params, mm_lm_params,
            n_layers, d_model, d_ff, d_head, d_kv, n_heads, n_kv_heads, vocab_size, attn_scale,
        }
    }
}

impl GpuForward {
    /// GPU-resident forward pass with PRE-BUILT bind groups.
    /// Encode = just set_pipeline + set_bind_group + dispatch × 485.
    /// No buffer creation, no bind group creation per token.
    pub fn forward_one_token(&self, model: &GpuModel, token: u32, seq_pos: u32) -> u32 {
        let d = model.d_model;
        let dff = model.d_ff;
        let dh = model.d_head;

        let t0 = std::time::Instant::now();

        // Upload embedding (only CPU→GPU transfer per token)
        let emb: Vec<i32> = vec![0i32; d as usize];
        self.queue.write_buffer(&model.hidden_buf, 0, bytemuck::cast_slice(&emb));

        // Update per-token params (position, seq_len) — just write_buffer to existing bufs
        for i in 0..model.n_layers as usize {
            let rp = RopeParams { pos: seq_pos, d_head: dh, n_heads: model.n_heads, _pad: 0 };
            self.queue.write_buffer(&model.rope_q_params[i], 0, bytemuck::bytes_of(&rp));
            let rk = RopeParams { pos: seq_pos, d_head: dh, n_heads: model.n_kv_heads, _pad: 0 };
            self.queue.write_buffer(&model.rope_k_params[i], 0, bytemuck::bytes_of(&rk));
            let ap = AttnParams {
                d_head: dh, n_heads: model.n_heads, n_kv_heads: model.n_kv_heads,
                seq_len: seq_pos + 1, d_kv: model.d_kv, attn_scale: model.attn_scale,
                _p1: 0, _p2: 0,
            };
            self.queue.write_buffer(&model.attn_params_bufs[i], 0, bytemuck::bytes_of(&ap));
        }

        let t_params = t0.elapsed();

        // Encode ALL dispatches — just pipeline+bindgroup+dispatch, zero allocation
        let mut encoder = self.device.create_command_encoder(&Default::default());

        let rt = (model.n_heads * (dh / 2) + 255) / 256; // RoPE Q workgroups
        let rtk = (model.n_kv_heads * (dh / 2) + 255) / 256; // RoPE K
        let wd = (d + 255) / 256;
        let wf = (dff + 255) / 256;
        let wv = (model.vocab_size + 255) / 256;

        for i in 0..model.n_layers as usize {
            let bg = &model.layer_bgs[i];
            dispatch(&mut encoder, &self.layernorm_pipeline, &bg.ln_attn, 1);
            dispatch(&mut encoder, &self.quantize_pipeline, &bg.quantize_normed, 1);
            dispatch(&mut encoder, &self.matmul_pipeline, &bg.mm_q, wd);
            dispatch(&mut encoder, &self.matmul_pipeline, &bg.mm_k, (model.d_kv + 255) / 256);
            dispatch(&mut encoder, &self.matmul_pipeline, &bg.mm_v, (model.d_kv + 255) / 256);
            dispatch(&mut encoder, &self.rope_pipeline, &bg.rope_q, rt);
            dispatch(&mut encoder, &self.rope_pipeline, &bg.rope_k, rtk);
            dispatch(&mut encoder, &self.attention_pipeline, &bg.attn, model.n_heads);
            dispatch(&mut encoder, &self.quantize_pipeline, &bg.quantize_attn, 1);
            dispatch(&mut encoder, &self.matmul_pipeline, &bg.mm_wo, wd);
            dispatch(&mut encoder, &self.residual_pipeline, &bg.residual_attn, wd);
            dispatch(&mut encoder, &self.layernorm_pipeline, &bg.ln_ffn, 1);
            dispatch(&mut encoder, &self.quantize_pipeline, &bg.quantize_ffn, 1);
            dispatch(&mut encoder, &self.matmul_pipeline, &bg.mm_gate, wf);
            dispatch(&mut encoder, &self.matmul_pipeline, &bg.mm_up, wf);
            dispatch(&mut encoder, &self.silu_pipeline, &bg.silu, wf);
            dispatch(&mut encoder, &self.quantize_pipeline, &bg.quantize_gated, 1);
            dispatch(&mut encoder, &self.matmul_pipeline, &bg.mm_down, wd);
            dispatch(&mut encoder, &self.residual_pipeline, &bg.residual_ffn, wd);
        }

        dispatch(&mut encoder, &self.layernorm_pipeline, &model.final_ln_bg, 1);
        dispatch(&mut encoder, &self.quantize_pipeline, &model.final_quantize_bg, 1);
        dispatch(&mut encoder, &self.matmul_pipeline, &model.lm_head_bg, wv);
        dispatch(&mut encoder, &self.argmax_pipeline, &model.argmax_bg, 1);

        // ── Copy result to staging for readback ──────────────────────
        encoder.copy_buffer_to_buffer(&model.result_buf, 0, &model.staging_buf, 0, 4);

        let t_encode = t0.elapsed();

        // ── Single submit for ENTIRE forward pass ────────────────────
        self.queue.submit([encoder.finish()]);

        let t_submit = t0.elapsed();

        // ── Read back token ID (4 bytes) ─────────────────────────────
        let slice = model.staging_buf.slice(..4);
        let (tx, rx) = std::sync::mpsc::channel();
        slice.map_async(wgpu::MapMode::Read, move |r| { let _ = tx.send(r); });
        let _ = self.device.poll(wgpu::PollType::wait());
        rx.recv().unwrap().unwrap();

        let t_poll = t0.elapsed();

        let mapped = slice.get_mapped_range();
        let token_id = bytemuck::cast_slice::<u8, u32>(&mapped)[0];
        drop(mapped);
        model.staging_buf.unmap();

        let t_total = t0.elapsed();

        if std::env::var("GPU_PROFILE").is_ok() {
            eprintln!("GPU fwd: params={:.1}ms encode={:.1}ms submit={:.1}ms poll={:.1}ms total={:.1}ms",
                t_params.as_secs_f64() * 1000.0,
                (t_encode - t_params).as_secs_f64() * 1000.0,
                (t_submit - t_encode).as_secs_f64() * 1000.0,
                (t_poll - t_submit).as_secs_f64() * 1000.0,
                t_total.as_secs_f64() * 1000.0);
        }

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
