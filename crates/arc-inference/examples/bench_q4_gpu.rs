//! Benchmark Q4 direct + GPU matmul.
//! Usage: cargo run --example bench_q4_gpu --features candle --release -- /path/to/model.arc-int8

use arc_inference::cached_integer_model::*;
use arc_inference::q4_engine::*;
use std::time::Instant;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let path = args.get(1).expect("Usage: bench_q4_gpu <model.arc-int8|model.gguf>");

    let model = if path.ends_with(".arc-int8") {
        load_cached_model_binary(path).unwrap()
    } else {
        load_cached_model(path).unwrap()
    };

    let cfg = &model.config;
    println!("Model: {}L d={} ff={}", cfg.n_layers, cfg.d_model, cfg.d_ff);

    // Benchmark Q4 conversion
    println!("\n=== Q4 Conversion ===");
    let q4_start = Instant::now();
    let q4_wq = Q4Weights::from_i8(&model.layers[0].wq);
    let q4_gate = Q4Weights::from_i8(&model.layers[0].w_gate);
    let q4_down = Q4Weights::from_i8(&model.layers[0].w_down);
    println!("Q4 conversion (3 matrices): {:.1} ms", q4_start.elapsed().as_secs_f64() * 1000.0);
    println!("Memory: Wq {} KB (was {} KB), W_gate {} KB (was {} KB)",
        q4_wq.memory_bytes() / 1024, model.layers[0].wq.memory_bytes() / 1024,
        q4_gate.memory_bytes() / 1024, model.layers[0].w_gate.memory_bytes() / 1024);

    // Create test input
    let d = cfg.d_model;
    let dff = cfg.d_ff;
    let input_d: Vec<i64> = (0..d).map(|i| (i as i64 % 200 - 100) * 65536 / 100).collect();
    let input_ff: Vec<i64> = (0..dff).map(|i| (i as i64 % 200 - 100) * 65536 / 100).collect();
    let input_d_q = QuantizedInput::from_i64(&input_d);
    let input_ff_q = QuantizedInput::from_i64(&input_ff);

    // Benchmark INT8 vs Q4 matmul
    println!("\n=== INT8 vs Q4 Matmul Speed ===");

    // Warmup
    let _ = matmul_fast(&model.layers[0].wq, &input_d, d, d);
    let _ = matmul_q4(&q4_wq, &input_d, d, d);

    let iters = 20;

    // INT8 Wq
    let start = Instant::now();
    for _ in 0..iters { let _ = matmul_fast(&model.layers[0].wq, &input_d, d, d); }
    let i8_wq = start.elapsed().as_secs_f64() * 1000.0 / iters as f64;

    // Q4 Wq
    let start = Instant::now();
    for _ in 0..iters { let _ = matmul_q4(&q4_wq, &input_d, d, d); }
    let q4_wq_ms = start.elapsed().as_secs_f64() * 1000.0 / iters as f64;

    // Q4 Wq with preq
    let mut out_d = vec![0i64; d];
    let start = Instant::now();
    for _ in 0..iters { matmul_q4_preq(&q4_wq, &input_d_q, d, &mut out_d); }
    let q4_wq_preq = start.elapsed().as_secs_f64() * 1000.0 / iters as f64;

    println!("  Wq  [{}×{}]: INT8 {:.2}ms, Q4 {:.2}ms, Q4+preq {:.2}ms ({}x)",
        d, d, i8_wq, q4_wq_ms, q4_wq_preq,
        format!("{:.1}", i8_wq / q4_wq_preq.max(0.001)));

    // INT8 W_gate
    let start = Instant::now();
    for _ in 0..iters { let _ = matmul_fast(&model.layers[0].w_gate, &input_d, d, dff); }
    let i8_gate = start.elapsed().as_secs_f64() * 1000.0 / iters as f64;

    // Q4 W_gate with preq
    let mut out_ff = vec![0i64; dff];
    let start = Instant::now();
    for _ in 0..iters { matmul_q4_preq(&q4_gate, &input_d_q, d, &mut out_ff); }
    let q4_gate_preq = start.elapsed().as_secs_f64() * 1000.0 / iters as f64;

    println!("  gate[{}×{}]: INT8 {:.2}ms, Q4+preq {:.2}ms ({}x)",
        dff, d, i8_gate, q4_gate_preq,
        format!("{:.1}", i8_gate / q4_gate_preq.max(0.001)));

    // INT8 W_down
    let start = Instant::now();
    for _ in 0..iters { let _ = matmul_fast(&model.layers[0].w_down, &input_ff, dff, d); }
    let i8_down = start.elapsed().as_secs_f64() * 1000.0 / iters as f64;

    let mut out_d2 = vec![0i64; d];
    let start = Instant::now();
    for _ in 0..iters { matmul_q4_preq(&q4_down, &input_ff_q, dff, &mut out_d2); }
    let q4_down_preq = start.elapsed().as_secs_f64() * 1000.0 / iters as f64;

    println!("  down[{}×{}]: INT8 {:.2}ms, Q4+preq {:.2}ms ({}x)",
        d, dff, i8_down, q4_down_preq,
        format!("{:.1}", i8_down / q4_down_preq.max(0.001)));

    // Estimate total with Q4
    let total_i8_est = (i8_wq * 4.0 + i8_gate * 2.0 + i8_down) * cfg.n_layers as f64;
    let total_q4_est = (q4_wq_preq * 4.0 + q4_gate_preq * 2.0 + q4_down_preq) * cfg.n_layers as f64;
    println!("\n  Estimated total: INT8 {:.0}ms, Q4 {:.0}ms, speedup {:.1}x",
        total_i8_est, total_q4_est, total_i8_est / total_q4_est.max(0.001));

    // GPU test
    println!("\n=== GPU Matmul ===");
    match arc_gpu::gpu_matmul::GpuMatmul::new(dff, cfg.vocab_size) {
        Ok(gpu) => {
            let gw = gpu.upload_weights(&model.layers[0].wq.data, d, d, None);
            let input_i8: Vec<i8> = input_d_q.data.clone();

            // Warmup
            let _ = gpu.matmul(&gw, &input_i8);

            let start = Instant::now();
            for _ in 0..iters { let _ = gpu.matmul(&gw, &input_i8); }
            let gpu_ms = start.elapsed().as_secs_f64() * 1000.0 / iters as f64;
            println!("  GPU Wq [{}×{}]: {:.2}ms ({}x vs INT8 CPU)",
                d, d, gpu_ms, format!("{:.1}", i8_wq / gpu_ms.max(0.001)));
        }
        Err(e) => println!("  No GPU: {}", e),
    }
}
