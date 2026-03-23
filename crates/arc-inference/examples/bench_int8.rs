//! Benchmark INT8 cached model inference.
//!
//! Usage: cargo run --example bench_int8 --features candle --release -- /path/to/model.gguf [num_tokens]

use arc_inference::cached_integer_model::{load_cached_model, load_cached_model_binary};
use std::time::Instant;

fn main() {

    let args: Vec<String> = std::env::args().collect();
    if args.len() < 2 {
        eprintln!("Usage: {} <model.gguf|model.arc-int8> [num_tokens] [--save path]", args[0]);
        std::process::exit(1);
    }

    let model_path = &args[1];
    let num_tokens = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(8u32);
    let save_path = args.iter().position(|a| a == "--save").and_then(|i| args.get(i + 1));

    println!("=== ARC Chain INT8 Inference Benchmark ===");
    println!("Model: {}", model_path);
    println!("Tokens to generate: {}", num_tokens);
    println!();

    // Load model (GGUF or binary)
    let load_start = Instant::now();
    let model = if model_path.ends_with(".arc-int8") {
        load_cached_model_binary(model_path).expect("Failed to load binary model")
    } else {
        load_cached_model(model_path).expect("Failed to load GGUF model")
    };
    let load_time = load_start.elapsed();
    let mem_mb = model.memory_bytes() / (1024 * 1024);

    println!("Model loaded in {:.2}s", load_time.as_secs_f64());
    println!("Memory: {} MB (INT8)", mem_mb);
    println!("Config: {} layers, d_model={}, n_heads={}, n_kv_heads={}, d_ff={}, vocab={}",
        model.config.n_layers, model.config.d_model, model.config.n_heads,
        model.config.n_kv_heads, model.config.d_ff, model.config.vocab_size);
    println!("Vocab entries: {}", model.vocab.len());
    println!();

    // Weight hash (for cross-platform verification)
    let whash = model.weight_hash();
    println!("Weight hash: 0x{}", hex::encode(&whash.0[..16]));

    // Save binary weights if requested
    if let Some(save_to) = save_path {
        println!("\nSaving INT8 weights to {}...", save_to);
        model.save_weights(save_to).expect("Failed to save weights");
        let file_size = std::fs::metadata(save_to).map(|m| m.len()).unwrap_or(0);
        println!("Saved: {} MB", file_size / (1024 * 1024));
    }

    // Test prompt
    let prompt_text = "What is 2+2?";
    let prompt_tokens = model.encode(prompt_text);
    println!("Prompt: \"{}\"", prompt_text);
    println!("Encoded: {:?} ({} tokens)", prompt_tokens, prompt_tokens.len());

    // Warmup run
    println!("\n--- Warmup ---");
    let eos = vec![2u32, 0];
    let warmup_start = Instant::now();
    let (warmup_tokens, warmup_hash) = model.generate(&prompt_tokens, 2, &eos);
    let warmup_ms = warmup_start.elapsed().as_millis();
    println!("Warmup: {} tokens in {}ms", warmup_tokens.len(), warmup_ms);

    // Benchmark run
    println!("\n--- Benchmark ---");
    let bench_start = Instant::now();
    let (generated, hash) = model.generate(&prompt_tokens, num_tokens, &eos);
    let bench_elapsed = bench_start.elapsed();
    let total_ms = bench_elapsed.as_millis() as u64;
    let n_gen = generated.len() as u64;
    let ms_per_token = if n_gen > 0 { total_ms / n_gen } else { 0 };

    let output_text = model.decode(&generated);

    println!("Generated {} tokens in {}ms ({} ms/token)", n_gen, total_ms, ms_per_token);
    println!("Output hash: 0x{}", hex::encode(&hash.0[..8]));
    println!("Token IDs: {:?}", generated);
    println!("Decoded: \"{}\"", output_text);

    // Determinism check: run again, verify identical hash
    println!("\n--- Determinism Check ---");
    let (gen2, hash2) = model.generate(&prompt_tokens, num_tokens, &eos);
    if hash == hash2 && generated == gen2 {
        println!("PASS: Two runs produce identical output (hash: 0x{})", hex::encode(&hash.0[..8]));
    } else {
        println!("FAIL: Non-deterministic! hash1={}, hash2={}",
            hex::encode(&hash.0[..8]), hex::encode(&hash2.0[..8]));
    }

    // Run 5 more times for consistency
    let mut all_match = true;
    for i in 0..5 {
        let (_, hi) = model.generate(&prompt_tokens, num_tokens, &eos);
        if hi != hash {
            println!("FAIL: Run {} diverged!", i + 3);
            all_match = false;
        }
    }
    if all_match {
        println!("PASS: 7 consecutive runs all produce hash 0x{}", hex::encode(&hash.0[..8]));
    }

    println!("\n=== Summary ===");
    println!("Model: {} layers, {} params (INT8)", model.config.n_layers,
        model.config.n_layers as u64 * (
            model.config.d_model as u64 * model.config.d_model as u64 * 4 +
            model.config.d_ff as u64 * model.config.d_model as u64 * 3
        ) + model.config.vocab_size as u64 * model.config.d_model as u64 * 2
    );
    println!("Memory: {} MB", mem_mb);
    println!("Speed: {} ms/token ({} tokens in {}ms)", ms_per_token, n_gen, total_ms);
    println!("Hash: 0x{}", hex::encode(&hash.0[..8]));
    println!("Deterministic: {}", if all_match { "YES" } else { "NO" });
}
