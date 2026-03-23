//! GGUF Integer Engine Benchmark
//!
//! Loads a REAL GGUF model and runs it through the integer-only engine.
//! Cross-platform deterministic — proves the same model produces the
//! same output on ARM, x86, GPU, or any hardware.
//!
//! Usage:
//!   cargo run --release --bin arc-bench-gguf-integer --features candle -- \
//!     --model /path/to/model.gguf --tokens 8

use arc_inference::gguf_integer::generate_integer_from_gguf;
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
        .expect("Usage: --model <path-to-gguf>");
    let max_tokens: u32 = args.iter().position(|a| a == "--tokens")
        .map(|i| args[i + 1].parse().unwrap())
        .unwrap_or(4);
    let runs: usize = args.iter().position(|a| a == "--runs")
        .map(|i| args[i + 1].parse().unwrap())
        .unwrap_or(1);

    let is_llama3 = model_path.contains("3.1") || model_path.contains("llama-3")
        || model_path.contains("Llama-3");

    let input_tokens: Vec<u32> = if is_llama3 {
        vec![128000, 128006, 882, 128007, 271, 3923, 374, 220, 17, 10, 17, 30, 128009, 128006, 78191, 128007, 271]
    } else {
        vec![1, 518, 25580, 29962, 1724, 338, 29871, 29906, 29974, 29906, 29973, 518, 29914, 25580, 29962]
    };

    let eos_tokens: Vec<u32> = if is_llama3 {
        vec![128001, 128009]
    } else {
        vec![2]
    };

    eprintln!("═══════════════════════════════════════════════════════════");
    eprintln!("ARC Chain — GGUF Integer Engine (Cross-Platform Deterministic)");
    eprintln!("═══════════════════════════════════════════════════════════");
    eprintln!("  Model:      {}", model_path);
    eprintln!("  Tokens:     {}", max_tokens);
    eprintln!("  Runs:       {}", runs);
    eprintln!("  Platform:   {} {}", std::env::consts::OS, std::env::consts::ARCH);
    eprintln!("  Engine:     Pure i64 integer arithmetic (no f32)");
    eprintln!();

    let mut all_hashes = Vec::new();
    let mut all_tokens_out = Vec::new();

    for run in 0..runs {
        let start = Instant::now();
        match generate_integer_from_gguf(
            &model_path,
            &input_tokens,
            max_tokens,
            &eos_tokens,
            300_000, // 5 minute timeout
        ) {
            Ok((tokens, hash)) => {
                let elapsed = start.elapsed();
                eprintln!("  Run {}: {} tokens in {:.1}s, hash={}",
                    run, tokens.len(), elapsed.as_secs_f64(),
                    hex_encode(&hash.0[..8]));
                if run == 0 {
                    eprintln!("  Generated: {:?}", tokens);
                }
                all_hashes.push(hash);
                all_tokens_out.push(tokens);
            }
            Err(e) => {
                eprintln!("  Run {} FAILED: {}", run, e);
                std::process::exit(1);
            }
        }
    }

    let deterministic = all_hashes.windows(2).all(|w| w[0] == w[1]);

    let json = serde_json::json!({
        "benchmark": "GgufIntegerEngine",
        "model_path": model_path,
        "platform": format!("{} {}", std::env::consts::OS, std::env::consts::ARCH),
        "engine": "pure_i64_integer",
        "runs": runs,
        "max_tokens": max_tokens,
        "deterministic": deterministic,
        "output_hash": hex_encode(&all_hashes[0].0),
        "generated_tokens": all_tokens_out[0],
    });

    println!("{}", serde_json::to_string_pretty(&json).unwrap());
}
