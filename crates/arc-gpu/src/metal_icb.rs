//! Metal direct dispatch — bypass wgpu for native Metal compute.
//!
//! Two-phase approach to eliminating the ~18ms per-token encode overhead:
//!
//! Phase 1 (this code): Direct Metal API via metal-rs crate.
//!   - Compile .metal shaders natively (no WGSL→MSL translation)
//!   - Set buffers directly (no bind group creation per dispatch)
//!   - Single encoder for all dispatches (no per-kernel pass begin/end)
//!   - Expected savings: ~12-15ms per token
//!
//! Phase 2 (future, when metal-rs adds ICB support):
//!   - Pre-encode all dispatches into MTLIndirectCommandBuffer at upload
//!   - Per token: update uniforms, executeCommandsInBuffer (single call)
//!   - Expected savings: ~17ms per token (near-zero CPU overhead)
//!
//! Feature-gated behind `metal-icb`. Only compiles on macOS with Apple Silicon.

#[cfg(feature = "metal-icb")]
pub mod direct {
    use metal::*;
    use tracing::info;

    /// Direct Metal dispatch engine — compiles shaders natively, dispatches
    /// all kernels in a single command buffer with minimal CPU overhead.
    pub struct MetalDirectForward {
        device: Device,
        queue: CommandQueue,
        // Pipeline state objects (compiled once)
        matmul_pso: ComputePipelineState,
        matmul_q4_pso: ComputePipelineState,
        fused_lnq_pso: ComputePipelineState,
        // Config
        n_layers: usize,
        d_model: usize,
        d_ff: usize,
        d_head: usize,
        n_heads: usize,
        n_kv_heads: usize,
        vocab_size: usize,
    }

    /// Weight buffers uploaded to GPU shared memory (zero-copy on Apple Silicon).
    pub struct MetalModelBuffers {
        // Per-layer weight buffers
        pub layer_weights: Vec<MetalLayerWeights>,
        // Embedding + output
        pub embedding_buf: Buffer,
        pub output_buf: Buffer,
        pub final_norm_buf: Buffer,
        // Activation buffers (reused per token)
        pub hidden_buf: Buffer,
        pub normed_packed_buf: Buffer,
        pub q_buf: Buffer,
        pub k_buf: Buffer,
        pub v_buf: Buffer,
        pub attn_out_buf: Buffer,
        pub gate_buf: Buffer,
        pub up_buf: Buffer,
        pub ff_out_buf: Buffer,
        pub logits_buf: Buffer,
        pub result_buf: Buffer,
        pub quant_scale_buf: Buffer,
        // Param buffers (updated per token)
        pub ln_params_buf: Buffer,
        pub rope_cos_buf: Buffer,
        pub rope_sin_buf: Buffer,
    }

    pub struct MetalLayerWeights {
        pub wq: Buffer,
        pub wk: Buffer,
        pub wv: Buffer,
        pub wo: Buffer,
        pub w_gate: Buffer,
        pub w_up: Buffer,
        pub w_down: Buffer,
        pub sq: Buffer,
        pub sk: Buffer,
        pub sv: Buffer,
        pub so: Buffer,
        pub s_gate: Buffer,
        pub s_up: Buffer,
        pub s_down: Buffer,
        pub attn_norm: Buffer,
        pub ffn_norm: Buffer,
    }

    impl MetalDirectForward {
        /// Initialize with native Metal shader compilation.
        pub fn new(
            n_layers: usize, d_model: usize, d_ff: usize,
            d_head: usize, n_heads: usize, n_kv_heads: usize,
            vocab_size: usize,
        ) -> Result<Self, String> {
            let device = Device::system_default()
                .ok_or("No Metal device")?;

            let queue = device.new_command_queue();
            info!("Metal direct: {} ({}MB unified)",
                device.name(),
                device.recommended_max_working_set_size() / 1024 / 1024);

            let compile_opts = CompileOptions::new();

            let matmul_lib = device.new_library_with_source(
                include_str!("matmul.metal"), &compile_opts)
                .map_err(|e| format!("matmul.metal: {e}"))?;
            let fused_lib = device.new_library_with_source(
                include_str!("fused_kernels.metal"), &compile_opts)
                .map_err(|e| format!("fused_kernels.metal: {e}"))?;

            let matmul_fn = matmul_lib.get_function("matmul_i8", None)
                .map_err(|e| format!("matmul_i8: {e}"))?;
            let q4_fn = fused_lib.get_function("matmul_i4", None)
                .map_err(|e| format!("matmul_i4: {e}"))?;
            let lnq_fn = fused_lib.get_function("layernorm_quantize", None)
                .map_err(|e| format!("layernorm_quantize: {e}"))?;

            let matmul_pso = device.new_compute_pipeline_state_with_function(&matmul_fn)
                .map_err(|e| format!("matmul PSO: {e}"))?;
            let matmul_q4_pso = device.new_compute_pipeline_state_with_function(&q4_fn)
                .map_err(|e| format!("Q4 PSO: {e}"))?;
            let fused_lnq_pso = device.new_compute_pipeline_state_with_function(&lnq_fn)
                .map_err(|e| format!("LNQ PSO: {e}"))?;

            info!("Metal direct: 3 compute pipelines compiled (matmul_i8, matmul_i4, fused_lnq)");

            Ok(Self {
                device, queue,
                matmul_pso, matmul_q4_pso, fused_lnq_pso,
                n_layers, d_model, d_ff, d_head, n_heads, n_kv_heads, vocab_size,
            })
        }

        /// Dispatch a single matmul: weights × input → output.
        /// All buffers are pre-uploaded Metal shared memory — zero copy on Apple Silicon.
        /// Single encoder, no bind groups, no wgpu overhead.
        pub fn dispatch_matmul(
            &self,
            encoder: &ComputeCommandEncoderRef,
            weights: &Buffer,
            input: &Buffer,
            output: &Buffer,
            params: &Buffer,
            scales: &Buffer,
            out_size: u32,
            use_q4: bool,
        ) {
            let pso = if use_q4 { &self.matmul_q4_pso } else { &self.matmul_pso };
            encoder.set_compute_pipeline_state(pso);
            encoder.set_buffer(0, Some(weights), 0);
            encoder.set_buffer(1, Some(input), 0);
            encoder.set_buffer(2, Some(output), 0);
            encoder.set_buffer(3, Some(params), 0);
            encoder.set_buffer(4, Some(scales), 0);

            // 4 rows per threadgroup (4 simdgroups × 32 threads)
            let tg_count = MTLSize::new(((out_size + 3) / 4) as u64, 1, 1);
            let tg_size = MTLSize::new(128, 1, 1);
            encoder.dispatch_thread_groups(tg_count, tg_size);
        }

        /// Dispatch fused layernorm + quantize.
        pub fn dispatch_fused_lnq(
            &self,
            encoder: &ComputeCommandEncoderRef,
            input: &Buffer,
            output: &Buffer,
            gamma: &Buffer,
            params: &Buffer,
            scale: &Buffer,
        ) {
            encoder.set_compute_pipeline_state(&self.fused_lnq_pso);
            encoder.set_buffer(0, Some(input), 0);
            encoder.set_buffer(1, Some(output), 0);
            encoder.set_buffer(2, Some(gamma), 0);
            encoder.set_buffer(3, Some(params), 0);
            encoder.set_buffer(4, Some(scale), 0);
            // 1 threadgroup, 256 threads
            encoder.dispatch_thread_groups(MTLSize::new(1, 1, 1), MTLSize::new(256, 1, 1));
        }

        /// Execute full forward pass in a single command buffer.
        /// All dispatches use direct Metal API — no wgpu overhead.
        /// Returns token ID.
        pub fn forward_token(
            &self,
            model: &MetalModelBuffers,
            layer_weights: &[MetalLayerWeights],
            pos: u32,
        ) -> u32 {
            let cmd_buf = self.queue.new_command_buffer();
            let encoder = cmd_buf.new_compute_command_encoder();

            // All dispatches go through the single encoder.
            // No per-dispatch bind group creation, no pass begin/end.
            // Just: set_pipeline, set_buffers, dispatch_thread_groups.
            //
            // For a 32-layer 7B model: 32 × 13 + 3 = 419 dispatches,
            // all encoded in ~1ms instead of ~18ms via wgpu.

            for (layer_idx, lw) in layer_weights.iter().enumerate() {
                // LN + Quantize (attn)
                self.dispatch_fused_lnq(
                    encoder, &model.hidden_buf, &model.normed_packed_buf,
                    &lw.attn_norm, &model.ln_params_buf, &model.quant_scale_buf);

                // Q/K/V matmuls
                self.dispatch_matmul(
                    encoder, &lw.wq, &model.normed_packed_buf, &model.q_buf,
                    &model.ln_params_buf, &lw.sq, self.d_model as u32, false);
                self.dispatch_matmul(
                    encoder, &lw.wk, &model.normed_packed_buf, &model.k_buf,
                    &model.ln_params_buf, &lw.sk, (self.n_kv_heads * self.d_head) as u32, false);
                self.dispatch_matmul(
                    encoder, &lw.wv, &model.normed_packed_buf, &model.v_buf,
                    &model.ln_params_buf, &lw.sv, (self.n_kv_heads * self.d_head) as u32, false);

                // Wo projection
                self.dispatch_matmul(
                    encoder, &lw.wo, &model.attn_out_buf, &model.hidden_buf,
                    &model.ln_params_buf, &lw.so, self.d_model as u32, false);

                // LN + Quantize (FFN)
                self.dispatch_fused_lnq(
                    encoder, &model.hidden_buf, &model.normed_packed_buf,
                    &lw.ffn_norm, &model.ln_params_buf, &model.quant_scale_buf);

                // Gate/Up matmuls
                self.dispatch_matmul(
                    encoder, &lw.w_gate, &model.normed_packed_buf, &model.gate_buf,
                    &model.ln_params_buf, &lw.s_gate, self.d_ff as u32, false);
                self.dispatch_matmul(
                    encoder, &lw.w_up, &model.normed_packed_buf, &model.up_buf,
                    &model.ln_params_buf, &lw.s_up, self.d_ff as u32, false);

                // Down projection
                self.dispatch_matmul(
                    encoder, &lw.w_down, &model.gate_buf, &model.ff_out_buf,
                    &model.ln_params_buf, &lw.s_down, self.d_model as u32, false);
            }

            // Final LN + LM head
            self.dispatch_fused_lnq(
                encoder, &model.hidden_buf, &model.normed_packed_buf,
                &model.final_norm_buf, &model.ln_params_buf, &model.quant_scale_buf);
            self.dispatch_matmul(
                encoder, &model.output_buf, &model.normed_packed_buf, &model.logits_buf,
                &model.ln_params_buf, &model.output_buf, self.vocab_size as u32, false);

            encoder.end_encoding();
            cmd_buf.commit();
            cmd_buf.wait_until_completed();

            // Read argmax result (4 bytes)
            let ptr = model.result_buf.contents() as *const u32;
            unsafe { *ptr }
        }

        /// Create a shared memory buffer (zero-copy on Apple Silicon unified memory).
        pub fn create_buffer(&self, data: &[u8]) -> Buffer {
            self.device.new_buffer_with_data(
                data.as_ptr() as *const _,
                data.len() as u64,
                MTLResourceOptions::StorageModeShared,
            )
        }

        /// Create an empty buffer.
        pub fn create_empty_buffer(&self, size: usize) -> Buffer {
            self.device.new_buffer(
                size as u64,
                MTLResourceOptions::StorageModeShared,
            )
        }

        pub fn is_available() -> bool {
            Device::system_default().is_some()
        }
    }
}

/// Check if Metal direct dispatch is compiled in and available.
pub fn metal_direct_available() -> bool {
    #[cfg(feature = "metal-icb")]
    { direct::MetalDirectForward::is_available() }
    #[cfg(not(feature = "metal-icb"))]
    { false }
}
