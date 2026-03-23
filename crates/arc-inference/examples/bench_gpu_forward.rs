//! End-to-end GPU forward pass benchmark.
//! Usage: cargo run --example bench_gpu_forward --features candle --release -- <model.arc-int8>

use arc_inference::cached_integer_model::*;
use arc_gpu::gpu_forward::GpuForward;
use std::time::Instant;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let path = args.get(1).expect("Usage: bench_gpu_forward <model.arc-int8|model.gguf>");

    println!("=== GPU-Resident Forward Pass Benchmark ===\n");

    // Load model
    let model = if path.ends_with(".arc-int8") {
        load_cached_model_binary(path).unwrap()
    } else {
        load_cached_model(path).unwrap()
    };
    let cfg = &model.config;
    println!("Model: {}L d={} h={} ff={} v={}", cfg.n_layers, cfg.d_model, cfg.n_heads, cfg.d_ff, cfg.vocab_size);

    // Init GPU
    let gpu = match GpuForward::new() {
        Ok(g) => g,
        Err(e) => { println!("GPU not available: {}", e); return; }
    };

    // Upload model to GPU
    println!("\nUploading model to GPU...");
    let upload_start = Instant::now();

    let layer_data: Vec<_> = model.layers.iter().map(|l| (
        l.wq.data.as_slice(), l.wq.scales.as_slice(),
        l.wk.data.as_slice(), l.wk.scales.as_slice(),
        l.wv.data.as_slice(), l.wv.scales.as_slice(),
        l.wo.data.as_slice(), l.wo.scales.as_slice(),
        l.w_gate.data.as_slice(), l.w_gate.scales.as_slice(),
        l.w_up.data.as_slice(), l.w_up.scales.as_slice(),
        l.w_down.data.as_slice(), l.w_down.scales.as_slice(),
        l.attn_norm.as_slice(),
        l.ffn_norm.as_slice(),
    )).collect();

    let gpu_model = gpu.upload_model(
        &model.embedding.data, &model.embedding.scales,
        &model.output_weight.data, &model.output_weight.scales,
        &model.final_norm,
        &layer_data,
        &cfg.rope_cos, &cfg.rope_sin,
        cfg.d_model as u32, cfg.d_ff as u32, cfg.d_head as u32, cfg.d_kv as u32,
        cfg.n_heads as u32, cfg.n_kv_heads as u32, cfg.vocab_size as u32,
        cfg.attn_scale as i32,
    );

    println!("Upload done in {:.2}s", upload_start.elapsed().as_secs_f64());

    // Run GPU forward pass
    println!("\nRunning GPU forward pass (token 0)...");
    let start = Instant::now();
    let token = gpu.forward_one_token(&gpu_model, 1, 0); // token 1, position 0
    let elapsed = start.elapsed();
    println!("GPU forward: {:.2} ms → token {}", elapsed.as_secs_f64() * 1000.0, token);

    // Run again (warmed up)
    let start = Instant::now();
    let token2 = gpu.forward_one_token(&gpu_model, token, 1);
    let elapsed2 = start.elapsed();
    println!("GPU forward (warm): {:.2} ms → token {}", elapsed2.as_secs_f64() * 1000.0, token2);

    // Compare with CPU
    println!("\nCPU forward for comparison...");
    let mut cache = KVCache::new(cfg.n_layers);
    let cpu_start = Instant::now();
    let logits = model.forward_one_token(1, &mut cache);
    let cpu_elapsed = cpu_start.elapsed();
    let cpu_token = arc_inference::integer_lut::argmax_i64(&logits) as u32;
    println!("CPU forward: {:.2} ms → token {}", cpu_elapsed.as_secs_f64() * 1000.0, cpu_token);

    println!("\n=== Summary ===");
    println!("GPU: {:.2} ms/token", elapsed2.as_secs_f64() * 1000.0);
    println!("CPU: {:.2} ms/token", cpu_elapsed.as_secs_f64() * 1000.0);
    println!("Speedup: {:.1}x", cpu_elapsed.as_secs_f64() / elapsed2.as_secs_f64().max(0.0001));
}
