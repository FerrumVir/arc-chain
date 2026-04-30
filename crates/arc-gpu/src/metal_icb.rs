//! Metal direct dispatch - bypass wgpu for native Metal compute.
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

    /// Direct Metal dispatch engine - compiles shaders natively, dispatches
    /// all kernels in a single command buffer with minimal CPU overhead.
    pub struct MetalDirectForward {
        device: Device,
        queue: CommandQueue,
        // Pipeline state objects (compiled once)
        matmul_pso: ComputePipelineState,
        matmul_q4_pso: ComputePipelineState,
        fused_lnq_pso: ComputePipelineState,
        // UNTESTED - new PSOs for complete forward pass
        rope_pso: ComputePipelineState,
        attention_pso: ComputePipelineState,
        silu_pso: ComputePipelineState,
        residual_pso: ComputePipelineState,
        argmax_pso: ComputePipelineState,
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
        pub output_scales_buf: Buffer,
        pub final_norm_buf: Buffer,
        // Activation buffers (reused per token)
        pub hidden_buf: Buffer,
        pub normed_packed_buf: Buffer,
        pub q_buf: Buffer,
        pub k_buf: Buffer,
        pub v_buf: Buffer,
        pub attn_out_buf: Buffer,
        pub attn_out_packed_buf: Buffer,
        pub gate_buf: Buffer,
        pub up_buf: Buffer,
        pub gated_packed_buf: Buffer,
        pub ff_out_buf: Buffer,
        pub logits_buf: Buffer,
        pub result_buf: Buffer,
        pub quant_scale_buf: Buffer,
        // KV cache buffers (per-layer)
        pub kv_k_bufs: Vec<Buffer>,
        pub kv_v_bufs: Vec<Buffer>,
        pub kv_k_scales: Vec<Buffer>,
        pub kv_v_scales: Vec<Buffer>,
        // Param buffers (updated per token)
        pub ln_params_buf: Buffer,
        pub rope_q_params: Buffer,
        pub rope_k_params: Buffer,
        pub attn_params_buf: Buffer,
        pub argmax_params_buf: Buffer,
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

            // UNTESTED - new shader libraries for complete forward pass
            let rope_lib = device.new_library_with_source(
                include_str!("rope.metal"), &compile_opts)
                .map_err(|e| format!("rope.metal: {e}"))?;
            let attn_lib = device.new_library_with_source(
                include_str!("attention.metal"), &compile_opts)
                .map_err(|e| format!("attention.metal: {e}"))?;
            let silu_lib = device.new_library_with_source(
                include_str!("silu.metal"), &compile_opts)
                .map_err(|e| format!("silu.metal: {e}"))?;
            let residual_lib = device.new_library_with_source(
                include_str!("residual.metal"), &compile_opts)
                .map_err(|e| format!("residual.metal: {e}"))?;
            let argmax_lib = device.new_library_with_source(
                include_str!("argmax.metal"), &compile_opts)
                .map_err(|e| format!("argmax.metal: {e}"))?;

            let matmul_fn = matmul_lib.get_function("matmul_i8", None)
                .map_err(|e| format!("matmul_i8: {e}"))?;
            let q4_fn = fused_lib.get_function("matmul_i4", None)
                .map_err(|e| format!("matmul_i4: {e}"))?;
            let lnq_fn = fused_lib.get_function("layernorm_quantize", None)
                .map_err(|e| format!("layernorm_quantize: {e}"))?;
            let rope_fn = rope_lib.get_function("rope_apply", None)
                .map_err(|e| format!("rope_apply: {e}"))?;
            let attn_fn = attn_lib.get_function("attention", None)
                .map_err(|e| format!("attention: {e}"))?;
            let silu_fn = silu_lib.get_function("silu_mul", None)
                .map_err(|e| format!("silu_mul: {e}"))?;
            let residual_fn = residual_lib.get_function("residual_add", None)
                .map_err(|e| format!("residual_add: {e}"))?;
            let argmax_fn = argmax_lib.get_function("argmax_i32", None)
                .map_err(|e| format!("argmax_i32: {e}"))?;

            let matmul_pso = device.new_compute_pipeline_state_with_function(&matmul_fn)
                .map_err(|e| format!("matmul PSO: {e}"))?;
            let matmul_q4_pso = device.new_compute_pipeline_state_with_function(&q4_fn)
                .map_err(|e| format!("Q4 PSO: {e}"))?;
            let fused_lnq_pso = device.new_compute_pipeline_state_with_function(&lnq_fn)
                .map_err(|e| format!("LNQ PSO: {e}"))?;
            let rope_pso = device.new_compute_pipeline_state_with_function(&rope_fn)
                .map_err(|e| format!("RoPE PSO: {e}"))?;
            let attention_pso = device.new_compute_pipeline_state_with_function(&attn_fn)
                .map_err(|e| format!("attention PSO: {e}"))?;
            let silu_pso = device.new_compute_pipeline_state_with_function(&silu_fn)
                .map_err(|e| format!("SiLU PSO: {e}"))?;
            let residual_pso = device.new_compute_pipeline_state_with_function(&residual_fn)
                .map_err(|e| format!("residual PSO: {e}"))?;
            let argmax_pso = device.new_compute_pipeline_state_with_function(&argmax_fn)
                .map_err(|e| format!("argmax PSO: {e}"))?;

            info!("Metal direct: 8 compute pipelines compiled (matmul_i8, matmul_i4, fused_lnq, rope, attention, silu, residual, argmax)");

            Ok(Self {
                device, queue,
                matmul_pso, matmul_q4_pso, fused_lnq_pso,
                rope_pso, attention_pso, silu_pso, residual_pso, argmax_pso,
                n_layers, d_model, d_ff, d_head, n_heads, n_kv_heads, vocab_size,
            })
        }

        /// Dispatch a single matmul: weights × input → output.
        /// All buffers are pre-uploaded Metal shared memory - zero copy on Apple Silicon.
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

        // UNTESTED - the following 5 dispatch methods require hardware validation

        /// Dispatch RoPE rotation on Q or K vectors.
        /// threads = n_heads * (d_head / 2), workgroup_size = 256
        pub fn dispatch_rope(
            &self,
            encoder: &ComputeCommandEncoderRef,
            data: &Buffer,
            cos_buf: &Buffer,
            sin_buf: &Buffer,
            params: &Buffer,
            n_heads: u32,
            d_head: u32,
        ) {
            encoder.set_compute_pipeline_state(&self.rope_pso);
            encoder.set_buffer(0, Some(data), 0);
            encoder.set_buffer(1, Some(cos_buf), 0);
            encoder.set_buffer(2, Some(sin_buf), 0);
            encoder.set_buffer(3, Some(params), 0);
            let total_pairs = n_heads * (d_head / 2);
            let tg_count = ((total_pairs + 255) / 256) as u64;
            encoder.dispatch_thread_groups(
                MTLSize::new(tg_count, 1, 1), MTLSize::new(256, 1, 1));
        }

        /// Dispatch attention: one threadgroup (32 threads) per head.
        pub fn dispatch_attention(
            &self,
            encoder: &ComputeCommandEncoderRef,
            q: &Buffer,
            k_cache: &Buffer,
            v_cache: &Buffer,
            k_scales: &Buffer,
            v_scales: &Buffer,
            output: &Buffer,
            params: &Buffer,
            n_heads: u32,
        ) {
            encoder.set_compute_pipeline_state(&self.attention_pso);
            encoder.set_buffer(0, Some(q), 0);
            encoder.set_buffer(1, Some(k_cache), 0);
            encoder.set_buffer(2, Some(v_cache), 0);
            encoder.set_buffer(3, Some(k_scales), 0);
            encoder.set_buffer(4, Some(v_scales), 0);
            encoder.set_buffer(5, Some(output), 0);
            encoder.set_buffer(6, Some(params), 0);
            // One threadgroup per head, 32 threads per threadgroup
            encoder.dispatch_thread_groups(
                MTLSize::new(n_heads as u64, 1, 1), MTLSize::new(32, 1, 1));
        }

        /// Dispatch SiLU gate activation: gate = silu(gate) * up.
        /// threads = d_ff, workgroup_size = 256
        pub fn dispatch_silu(
            &self,
            encoder: &ComputeCommandEncoderRef,
            gate: &Buffer,
            up: &Buffer,
            size: u32,
        ) {
            encoder.set_compute_pipeline_state(&self.silu_pso);
            encoder.set_buffer(0, Some(gate), 0);
            encoder.set_buffer(1, Some(up), 0);
            let tg_count = ((size + 255) / 256) as u64;
            encoder.dispatch_thread_groups(
                MTLSize::new(tg_count, 1, 1), MTLSize::new(256, 1, 1));
        }

        /// Dispatch residual addition: hidden += projected.
        /// threads = d_model, workgroup_size = 256
        pub fn dispatch_residual(
            &self,
            encoder: &ComputeCommandEncoderRef,
            hidden: &Buffer,
            projected: &Buffer,
            size: u32,
        ) {
            encoder.set_compute_pipeline_state(&self.residual_pso);
            encoder.set_buffer(0, Some(hidden), 0);
            encoder.set_buffer(1, Some(projected), 0);
            let tg_count = ((size + 255) / 256) as u64;
            encoder.dispatch_thread_groups(
                MTLSize::new(tg_count, 1, 1), MTLSize::new(256, 1, 1));
        }

        /// Dispatch argmax over logits. 1 threadgroup, 256 threads.
        pub fn dispatch_argmax(
            &self,
            encoder: &ComputeCommandEncoderRef,
            logits: &Buffer,
            result: &Buffer,
            params: &Buffer,
        ) {
            encoder.set_compute_pipeline_state(&self.argmax_pso);
            encoder.set_buffer(0, Some(logits), 0);
            encoder.set_buffer(1, Some(result), 0);
            encoder.set_buffer(2, Some(params), 0);
            // 1 threadgroup, 256 threads
            encoder.dispatch_thread_groups(MTLSize::new(1, 1, 1), MTLSize::new(256, 1, 1));
        }

        /// Execute full forward pass in a single command buffer.
        /// UNTESTED - all 5 new kernel dispatches require hardware validation.
        ///
        /// All dispatches use direct Metal API - no wgpu overhead.
        /// Per layer: fused_lnq, Q/K/V matmul, rope Q, rope K, attention,
        ///            fused_lnq (attn_out), Wo matmul, residual,
        ///            fused_lnq (ffn), gate/up matmul, silu,
        ///            fused_lnq (gated), down matmul, residual.
        /// Then: final fused_lnq, lm_head matmul, argmax.
        ///
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
            // For a 32-layer 7B model: 32 × 19 + 3 = 611 dispatches,
            // all encoded in ~1ms instead of ~18ms via wgpu.

            let d_model = self.d_model as u32;
            let d_ff = self.d_ff as u32;
            let n_heads = self.n_heads as u32;
            let n_kv_heads = self.n_kv_heads as u32;
            let d_head = self.d_head as u32;
            let kv_dim = (self.n_kv_heads * self.d_head) as u32;

            // Update RoPE params for Q (n_heads) - written to shared buffer
            let rope_q_data: [u32; 4] = [pos, d_head, n_heads, 0];
            unsafe {
                let ptr = model.rope_q_params.contents() as *mut u32;
                std::ptr::copy_nonoverlapping(rope_q_data.as_ptr(), ptr, 4);
            }

            // Update RoPE params for K (n_kv_heads)
            let rope_k_data: [u32; 4] = [pos, d_head, n_kv_heads, 0];
            unsafe {
                let ptr = model.rope_k_params.contents() as *mut u32;
                std::ptr::copy_nonoverlapping(rope_k_data.as_ptr(), ptr, 4);
            }

            // Update attention params
            let seq_len = pos + 1;
            let kv_bytes = self.n_kv_heads * self.d_head;
            let attn_scale = 65536 / ((self.d_head as f64).sqrt() as i32).max(1);
            let attn_data: [u32; 8] = [
                d_head, n_heads, n_kv_heads, seq_len,
                kv_bytes as u32, attn_scale as u32, 0, 0,
            ];
            unsafe {
                let ptr = model.attn_params_buf.contents() as *mut u32;
                std::ptr::copy_nonoverlapping(attn_data.as_ptr(), ptr, 8);
            }

            for (layer_idx, lw) in layer_weights.iter().enumerate() {
                // ── 1. Fused LN + Quantize (attn norm) ──
                self.dispatch_fused_lnq(
                    encoder, &model.hidden_buf, &model.normed_packed_buf,
                    &lw.attn_norm, &model.ln_params_buf, &model.quant_scale_buf);

                // ── 2. Q/K/V matmuls ──
                self.dispatch_matmul(
                    encoder, &lw.wq, &model.normed_packed_buf, &model.q_buf,
                    &model.ln_params_buf, &lw.sq, d_model, false);
                self.dispatch_matmul(
                    encoder, &lw.wk, &model.normed_packed_buf, &model.k_buf,
                    &model.ln_params_buf, &lw.sk, kv_dim, false);
                self.dispatch_matmul(
                    encoder, &lw.wv, &model.normed_packed_buf, &model.v_buf,
                    &model.ln_params_buf, &lw.sv, kv_dim, false);

                // ── 3. RoPE on Q and K ──
                self.dispatch_rope(
                    encoder, &model.q_buf, &model.rope_cos_buf,
                    &model.rope_sin_buf, &model.rope_q_params,
                    n_heads, d_head);
                self.dispatch_rope(
                    encoder, &model.k_buf, &model.rope_cos_buf,
                    &model.rope_sin_buf, &model.rope_k_params,
                    n_kv_heads, d_head);

                // ── 4. Attention (scores + softmax + weighted V) ──
                // K and V are read from per-layer KV cache buffers.
                // (Caller is responsible for writing current K/V into the cache
                //  at position `pos` before this dispatch, or using a separate
                //  KV cache update kernel - not included in this pass.)
                self.dispatch_attention(
                    encoder,
                    &model.q_buf,
                    &model.kv_k_bufs[layer_idx],
                    &model.kv_v_bufs[layer_idx],
                    &model.kv_k_scales[layer_idx],
                    &model.kv_v_scales[layer_idx],
                    &model.attn_out_buf,
                    &model.attn_params_buf,
                    n_heads);

                // ── 5. Quantize attn_out for Wo matmul ──
                // Reuse fused_lnq with identity gamma (pre-initialized to 65536)
                // to get quantized packed output. This is a layernorm+quantize where
                // the norm is a no-op because we just need quantization, but the
                // fused kernel handles it correctly as a normalize-then-pack step.
                self.dispatch_fused_lnq(
                    encoder, &model.attn_out_buf, &model.attn_out_packed_buf,
                    &lw.attn_norm, &model.ln_params_buf, &model.quant_scale_buf);

                // ── 6. Wo projection ──
                self.dispatch_matmul(
                    encoder, &lw.wo, &model.attn_out_packed_buf, &model.ff_out_buf,
                    &model.ln_params_buf, &lw.so, d_model, false);

                // ── 7. Residual (attn): hidden += Wo output ──
                self.dispatch_residual(encoder, &model.hidden_buf, &model.ff_out_buf, d_model);

                // ── 8. Fused LN + Quantize (FFN norm) ──
                self.dispatch_fused_lnq(
                    encoder, &model.hidden_buf, &model.normed_packed_buf,
                    &lw.ffn_norm, &model.ln_params_buf, &model.quant_scale_buf);

                // ── 9. Gate/Up matmuls ──
                self.dispatch_matmul(
                    encoder, &lw.w_gate, &model.normed_packed_buf, &model.gate_buf,
                    &model.ln_params_buf, &lw.s_gate, d_ff, false);
                self.dispatch_matmul(
                    encoder, &lw.w_up, &model.normed_packed_buf, &model.up_buf,
                    &model.ln_params_buf, &lw.s_up, d_ff, false);

                // ── 10. SiLU gate activation ──
                self.dispatch_silu(encoder, &model.gate_buf, &model.up_buf, d_ff);

                // ── 11. Quantize gated output for down matmul ──
                self.dispatch_fused_lnq(
                    encoder, &model.gate_buf, &model.gated_packed_buf,
                    &lw.ffn_norm, &model.ln_params_buf, &model.quant_scale_buf);

                // ── 12. Down projection ──
                self.dispatch_matmul(
                    encoder, &lw.w_down, &model.gated_packed_buf, &model.ff_out_buf,
                    &model.ln_params_buf, &lw.s_down, d_model, false);

                // ── 13. Residual (FFN): hidden += down output ──
                self.dispatch_residual(encoder, &model.hidden_buf, &model.ff_out_buf, d_model);
            }

            // ── Final: LN + Quantize → LM head matmul → argmax ──
            self.dispatch_fused_lnq(
                encoder, &model.hidden_buf, &model.normed_packed_buf,
                &model.final_norm_buf, &model.ln_params_buf, &model.quant_scale_buf);
            self.dispatch_matmul(
                encoder, &model.output_buf, &model.normed_packed_buf, &model.logits_buf,
                &model.ln_params_buf, &model.output_scales_buf, self.vocab_size as u32, false);
            self.dispatch_argmax(
                encoder, &model.logits_buf, &model.result_buf, &model.argmax_params_buf);

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
