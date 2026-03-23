//! Profile where time is spent in the INT8 forward pass.
//! Usage: cargo run --example profile_int8 --features candle --release -- /path/to/model

use arc_inference::cached_integer_model::*;
use arc_inference::integer_lut::*;
use std::time::Instant;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let model_path = args.get(1).expect("Usage: profile_int8 <model>");

    let model = if model_path.ends_with(".arc-int8") {
        load_cached_model_binary(model_path).unwrap()
    } else {
        load_cached_model(model_path).unwrap()
    };

    let cfg = &model.config;
    println!("Model: {}L d={} h={} kv={} ff={} v={}",
        cfg.n_layers, cfg.d_model, cfg.n_heads, cfg.n_kv_heads, cfg.d_ff, cfg.vocab_size);

    // Profile a single forward pass (one token)
    let prompt = model.encode("What is 2+2?");
    let mut cache = KVCache::new(cfg.n_layers);

    // Process prompt to build KV cache
    for &tok in &prompt {
        model.forward_one_token(tok, &mut cache);
    }

    // Now profile ONE generation step
    let last_tok = *prompt.last().unwrap();

    // Run 3 times, take the last (warm caches)
    for _ in 0..2 {
        let _ = model.forward_one_token(last_tok, &mut cache);
        cache.seq_len -= 1; // reset so we can rerun
        let layer_count = cfg.n_layers;
        for l in 0..layer_count {
            let kv_len = cfg.d_kv;
            cache.k_data[l].truncate(cache.seq_len * kv_len);
            cache.k_scales[l].truncate(cache.seq_len);
            cache.v_data[l].truncate(cache.seq_len * kv_len);
            cache.v_scales[l].truncate(cache.seq_len);
        }
    }

    let start = Instant::now();
    let _logits = model.forward_one_token(last_tok, &mut cache);
    let total = start.elapsed();

    println!("\nTotal forward pass: {:.2} ms", total.as_secs_f64() * 1000.0);

    // Profile individual matmul sizes
    println!("\n--- Matmul profiling (10 iterations each) ---");

    let d = cfg.d_model;
    let dff = cfg.d_ff;
    let dkv = cfg.d_kv;
    let vocab = cfg.vocab_size;

    // Create test inputs
    let input_d: Vec<i64> = (0..d).map(|i| (i as i64 % 200 - 100) * ONE / 100).collect();
    let input_ff: Vec<i64> = (0..dff).map(|i| (i as i64 % 200 - 100) * ONE / 100).collect();

    let profile_matmul = |name: &str, w: &I8Weights, input: &[i64], ins: usize, outs: usize| {
        // Warmup
        let _ = matmul_fast(w, input, ins, outs);

        let start = Instant::now();
        let iters = 10;
        for _ in 0..iters {
            let _ = matmul_fast(w, input, ins, outs);
        }
        let elapsed = start.elapsed().as_secs_f64() * 1000.0 / iters as f64;
        let per_layer = elapsed;
        let all_layers = per_layer * cfg.n_layers as f64;
        println!("  {:<12} [{:>5}×{:>5}]: {:.2} ms/call × {} layers = {:.1} ms total",
            name, outs, ins, per_layer, cfg.n_layers, all_layers);
    };

    let l = &model.layers[0];
    profile_matmul("Wq", &l.wq, &input_d, d, d);
    profile_matmul("Wk", &l.wk, &input_d, d, dkv);
    profile_matmul("Wv", &l.wv, &input_d, d, dkv);
    profile_matmul("Wo", &l.wo, &input_d, d, d);
    profile_matmul("W_gate", &l.w_gate, &input_d, d, dff);
    profile_matmul("W_up", &l.w_up, &input_d, d, dff);
    profile_matmul("W_down", &l.w_down, &input_ff, dff, d);

    println!("\n  {:<12} [{:>5}×{:>5}]:", "LM_head", vocab, d);
    let _ = matmul_fast(&model.output_weight, &input_d, d, vocab);
    let start = Instant::now();
    for _ in 0..10 {
        let _ = matmul_fast(&model.output_weight, &input_d, d, vocab);
    }
    let lm_ms = start.elapsed().as_secs_f64() * 1000.0 / 10.0;
    println!("  {:<12} [{:>5}×{:>5}]: {:.2} ms/call × 1 = {:.1} ms total",
        "LM_head", vocab, d, lm_ms, lm_ms);

    // Estimate breakdown
    let q_time = {
        let _ = matmul_fast(&l.wq, &input_d, d, d);
        let s = Instant::now();
        for _ in 0..10 { let _ = matmul_fast(&l.wq, &input_d, d, d); }
        s.elapsed().as_secs_f64() * 1000.0 / 10.0
    };

    let gate_time = {
        let _ = matmul_fast(&l.w_gate, &input_d, d, dff);
        let s = Instant::now();
        for _ in 0..10 { let _ = matmul_fast(&l.w_gate, &input_d, d, dff); }
        s.elapsed().as_secs_f64() * 1000.0 / 10.0
    };

    let total_matmul_est = (q_time * 4.0 + gate_time * 3.0) * cfg.n_layers as f64 + lm_ms;
    println!("\n--- Estimated breakdown ---");
    println!("  Matmuls:    ~{:.0} ms ({:.0}% of {:.0} ms total)",
        total_matmul_est, total_matmul_est / total.as_secs_f64() / 10.0, total.as_secs_f64() * 1000.0);
    println!("  Other:      ~{:.0} ms (attention, layernorm, rope, embedding, argmax)",
        total.as_secs_f64() * 1000.0 - total_matmul_est);
}
