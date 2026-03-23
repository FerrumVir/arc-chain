//! Cached Integer Model Benchmark — Production-speed deterministic inference.
//!
//! Loads GGUF model ONCE at startup, then generates tokens using cached weights
//! with KV cache and rayon parallelism. Pure i64, cross-platform deterministic.
//!
//! Usage:
//!   cargo run --release --bin arc-bench-cached-integer --features candle -- \
//!     --model /path/to/model.gguf --tokens 16

use arc_inference::cached_integer_model::load_cached_model;
use std::time::Instant;

fn hex_encode(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{:02x}", b)).collect()
}

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive("arc_inference=info".parse().unwrap())
        )
        .init();

    let args: Vec<String> = std::env::args().collect();
    let model_path = args.iter().position(|a| a == "--model")
        .map(|i| args[i + 1].clone())
        .expect("Usage: --model <path>");
    let max_tokens: u32 = args.iter().position(|a| a == "--tokens")
        .map(|i| args[i + 1].parse().unwrap())
        .unwrap_or(8);
    let runs: usize = args.iter().position(|a| a == "--runs")
        .map(|i| args[i + 1].parse().unwrap())
        .unwrap_or(1);

    let is_llama3 = model_path.contains("3.1") || model_path.contains("llama-3");
    let prompt: Vec<u32> = if is_llama3 {
        vec![128000, 128006, 882, 128007, 271, 3923, 374, 220, 17, 10, 17, 30, 128009, 128006, 78191, 128007, 271]
    } else {
        vec![1, 518, 25580, 29962, 1724, 338, 29871, 29906, 29974, 29906, 29973, 518, 29914, 25580, 29962]
    };
    let eos: Vec<u32> = if is_llama3 { vec![128001, 128009] } else { vec![2] };

    eprintln!("═══════════════════════════════════════════════════════════");
    eprintln!("ARC Chain — Cached Integer Model (Production Speed)");
    eprintln!("═══════════════════════════════════════════════════════════");
    eprintln!("  Platform: {} {}", std::env::consts::OS, std::env::consts::ARCH);
    eprintln!("  Model:    {}", model_path);
    eprintln!("  Tokens:   {}", max_tokens);
    eprintln!("  Runs:     {}", runs);
    eprintln!("  Engine:   Cached i64 + KV cache + rayon parallel");
    eprintln!();

    // ONE-TIME model load
    let load_start = Instant::now();
    let model = load_cached_model(&model_path).expect("Failed to load model");
    let load_time = load_start.elapsed();
    eprintln!("  Model loaded in {:.1}s (one-time cost)", load_time.as_secs_f64());
    eprintln!("  Config: {} layers, d_model={}, {} heads, d_ff={}, vocab={}",
        model.config.n_layers, model.config.d_model, model.config.n_heads,
        model.config.d_ff, model.config.vocab_size);
    eprintln!();

    let mut all_hashes = Vec::new();

    for run in 0..runs {
        let start = Instant::now();
        let (tokens, hash) = model.generate(&prompt, max_tokens, &eos);
        let elapsed = start.elapsed();
        let per_token = elapsed.as_millis() as f64 / tokens.len().max(1) as f64;

        eprintln!("  Run {}: {} tokens in {:.1}s ({:.0}ms/tok), hash={}",
            run, tokens.len(), elapsed.as_secs_f64(), per_token,
            hex_encode(&hash.0[..8]));
        if run == 0 {
            eprintln!("  Generated: {:?}", tokens);
        }
        all_hashes.push(hash);
    }

    let deterministic = all_hashes.windows(2).all(|w| w[0] == w[1]);

    let json = serde_json::json!({
        "benchmark": "CachedIntegerModel",
        "model_path": model_path,
        "platform": format!("{} {}", std::env::consts::OS, std::env::consts::ARCH),
        "engine": "cached_i64_kv_cache_rayon",
        "runs": runs,
        "max_tokens": max_tokens,
        "deterministic": deterministic,
        "output_hash": hex_encode(&all_hashes[0].0),
        "load_time_ms": load_time.as_millis() as u64,
    });
    println!("{}", serde_json::to_string_pretty(&json).unwrap());
}
