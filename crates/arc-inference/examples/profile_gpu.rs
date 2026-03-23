//! Profile GPU forward pass — find where 140ms goes.

use arc_inference::cached_integer_model::*;
use arc_gpu::gpu_forward::GpuForward;
use arc_gpu::gpu_matmul::GpuMatmul;
use std::time::Instant;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let path = args.get(1).expect("Usage: profile_gpu <model>");

    let model = if path.ends_with(".arc-int8") {
        load_cached_model_binary(path).unwrap()
    } else {
        load_cached_model(path).unwrap()
    };
    let cfg = &model.config;
    let d = cfg.d_model;
    let dff = cfg.d_ff;

    println!("Model: {}L d={} ff={}", cfg.n_layers, d, dff);

    // Test 1: How fast is a single GPU matmul dispatch (pooled)?
    println!("\n=== Single matmul dispatch overhead ===");
    let gpu_mm = GpuMatmul::new(dff, cfg.vocab_size).unwrap();
    let gw = gpu_mm.upload_weights(&model.layers[0].wq.data, d, d);
    let input_i8: Vec<i8> = vec![1; d];

    // Warmup
    for _ in 0..3 { let _ = gpu_mm.matmul(&gw, &input_i8); }

    let n = 50;
    let start = Instant::now();
    for _ in 0..n { let _ = gpu_mm.matmul(&gw, &input_i8); }
    let per_dispatch = start.elapsed().as_secs_f64() * 1000.0 / n as f64;
    println!("  Single matmul dispatch: {:.3} ms", per_dispatch);
    println!("  × ~485 dispatches = {:.0} ms estimated", per_dispatch * 485.0);

    // Test 2: How fast is the full GPU forward pass?
    println!("\n=== Full GPU forward pass ===");
    let gpu = GpuForward::new().unwrap();
    let layer_data: Vec<_> = model.layers.iter().map(|l| (
        l.wq.data.as_slice(), l.wq.scales.as_slice(),
        l.wk.data.as_slice(), l.wk.scales.as_slice(),
        l.wv.data.as_slice(), l.wv.scales.as_slice(),
        l.wo.data.as_slice(), l.wo.scales.as_slice(),
        l.w_gate.data.as_slice(), l.w_gate.scales.as_slice(),
        l.w_up.data.as_slice(), l.w_up.scales.as_slice(),
        l.w_down.data.as_slice(), l.w_down.scales.as_slice(),
        l.attn_norm.as_slice(), l.ffn_norm.as_slice(),
    )).collect();
    let gpu_model = gpu.upload_model(
        &model.embedding.data, &model.embedding.scales,
        &model.output_weight.data, &model.output_weight.scales,
        &model.final_norm, &layer_data,
        &cfg.rope_cos, &cfg.rope_sin,
        d as u32, dff as u32, cfg.d_head as u32, cfg.d_kv as u32,
        cfg.n_heads as u32, cfg.n_kv_heads as u32, cfg.vocab_size as u32,
        cfg.attn_scale as i32,
    );

    // Warmup
    for _ in 0..3 { gpu.forward_one_token(&gpu_model, 1, 0); }

    let n = 10;
    let start = Instant::now();
    for _ in 0..n { gpu.forward_one_token(&gpu_model, 1, 0); }
    let gpu_fwd = start.elapsed().as_secs_f64() * 1000.0 / n as f64;
    println!("  Full forward: {:.2} ms", gpu_fwd);

    // Test 3: What's the BIND GROUP creation overhead?
    println!("\n=== Bind group creation overhead ===");
    let start = Instant::now();
    for _ in 0..10000 {
        let _bg = gpu.device_ref().create_bind_group(&wgpu::BindGroupDescriptor {
            label: None,
            layout: gpu.matmul_bgl_ref(),
            entries: &[
                wgpu::BindGroupEntry { binding: 0, resource: gw.buffer_ref().as_entire_binding() },
                wgpu::BindGroupEntry { binding: 1, resource: gpu_model.normed_packed_ref().as_entire_binding() },
                wgpu::BindGroupEntry { binding: 2, resource: gpu_model.q_buf_ref().as_entire_binding() },
                wgpu::BindGroupEntry { binding: 3, resource: gpu_model.quant_scale_ref().as_entire_binding() },
                wgpu::BindGroupEntry { binding: 4, resource: gpu_model.normed_buf_ref().as_entire_binding() },
            ],
        });
    }
    let bg_us = start.elapsed().as_micros() as f64 / 10000.0;
    println!("  Bind group creation: {:.1} µs each", bg_us);
    println!("  × ~485 per token = {:.1} ms", bg_us * 485.0 / 1000.0);

    // Test 4: What's the UNIFORM BUFFER creation overhead?
    println!("\n=== Uniform buffer creation overhead ===");
    let start = Instant::now();
    for _ in 0..10000 {
        let buf = gpu.device_ref().create_buffer(&wgpu::BufferDescriptor {
            label: None, size: 16,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        gpu.queue_ref().write_buffer(&buf, 0, &[0u8; 16]);
    }
    let ub_us = start.elapsed().as_micros() as f64 / 10000.0;
    println!("  Uniform buffer create+write: {:.1} µs each", ub_us);
    println!("  × ~485 per token = {:.1} ms", ub_us * 485.0 / 1000.0);

    // Breakdown
    println!("\n=== Estimated breakdown ===");
    let bg_ms = bg_us * 485.0 / 1000.0;
    let ub_ms = ub_us * 485.0 / 1000.0;
    let dispatch_overhead = per_dispatch * 485.0;
    println!("  Bind groups:    {:.1} ms", bg_ms);
    println!("  Uniform bufs:   {:.1} ms", ub_ms);
    println!("  Dispatch total:  {:.1} ms (single matmul × 485)", dispatch_overhead);
    println!("  Actual forward:  {:.1} ms", gpu_fwd);
    println!("  Compute time:    {:.1} ms (forward - overhead)", gpu_fwd - bg_ms - ub_ms);
    println!("\n  Theoretical floor: {:.1} ms ({}GB / 800GB/s)",
        model.memory_bytes() as f64 / 800e9 * 1000.0, model.memory_bytes() / (1024*1024*1024));
}
