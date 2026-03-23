//! GGUF Model Inference Benchmark
//!
//! Loads a real GGUF model via candle backend, runs inference,
//! and verifies determinism across multiple runs.
//!
//! Usage:
//!   cargo run --release --bin arc-bench-gguf --features candle -- --model /tmp/arc-models/tinyllama-1.1b-q4.gguf
//!   cargo run --release --bin arc-bench-gguf --features candle -- --model /tmp/arc-models/llama-7b-q4.gguf

use arc_inference::candle_backend::GgufEngine;
use std::time::Instant;

fn hex_encode(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{:02x}", b)).collect()
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let model_path = args.iter().position(|a| a == "--model")
        .map(|i| args[i + 1].clone())
        .expect("Usage: --model <path-to-gguf>");
    let runs: usize = args.iter().position(|a| a == "--runs")
        .map(|i| args[i + 1].parse().unwrap())
        .unwrap_or(10);
    let max_tokens: u32 = args.iter().position(|a| a == "--tokens")
        .map(|i| args[i + 1].parse().unwrap())
        .unwrap_or(16);

    eprintln!("═══════════════════════════════════════════════════════════");
    eprintln!("ARC Chain — GGUF Model Inference Benchmark");
    eprintln!("═══════════════════════════════════════════════════════════");
    eprintln!("  Model:      {}", model_path);
    eprintln!("  Runs:       {}", runs);
    eprintln!("  Max tokens: {}", max_tokens);
    eprintln!("  Platform:   {} {}", std::env::consts::OS, std::env::consts::ARCH);
    eprintln!();

    // Load model
    let engine = GgufEngine::new(60_000); // 60s timeout
    let load_start = Instant::now();
    let model_id = engine.load_gguf_file(&model_path)
        .expect("Failed to load GGUF model");
    let load_time = load_start.elapsed();
    eprintln!("  Model ID:   {}", hex_encode(&model_id.0[..16]));
    eprintln!("  Load time:  {:.2}s", load_time.as_secs_f64());
    eprintln!();

    // Detect model type from path and use appropriate prompt tokens.
    // Llama-3 uses a different chat template than Llama-2.
    let is_llama3 = model_path.contains("3.1") || model_path.contains("llama-3")
        || model_path.contains("Llama-3");

    let input_tokens: Vec<u32> = if is_llama3 {
        // Llama-3.1 Instruct format:
        // <|begin_of_text|><|start_header_id|>user<|end_header_id|>\n\nWhat is 2+2?<|eot_id|>
        // <|start_header_id|>assistant<|end_header_id|>\n\n
        vec![
            128000,  // <|begin_of_text|>
            128006,  // <|start_header_id|>
            882,     // user
            128007,  // <|end_header_id|>
            271,     // \n\n
            3923, 374, 220, 17, 10, 17, 30,  // What is 2+2?
            128009,  // <|eot_id|>
            128006,  // <|start_header_id|>
            78191,   // assistant
            128007,  // <|end_header_id|>
            271,     // \n\n
        ]
    } else {
        // Llama-2-Chat format: [INST] What is 2+2? [/INST]
        vec![
            1,      // BOS
            518, 25580, 29962,  // [INST]
            1724, 338, 29871, 29906, 29974, 29906, 29973,  // What is 2+2?
            518, 29914, 25580, 29962,  // [/INST]
        ]
    };
    eprintln!("  Input tokens: {:?}", input_tokens);
    eprintln!();

    // Run inference multiple times and check determinism
    let mut output_hashes = Vec::new();
    let mut latencies = Vec::new();
    let mut first_output = None;

    for i in 0..runs {
        let start = Instant::now();
        let result = engine.generate(&model_id, &input_tokens, max_tokens)
            .expect("Inference failed");
        let elapsed = start.elapsed();
        latencies.push(elapsed.as_millis() as u64);

        if i == 0 {
            first_output = Some(result.output.clone());
            eprintln!("  Run 0: {} tokens in {}ms, output_hash={}",
                result.tokens_used, result.elapsed_ms,
                hex_encode(&result.output_hash.0[..8]));

            // Print generated token IDs
            let tokens: Vec<u32> = result.output.chunks(4)
                .map(|c| u32::from_le_bytes([c[0], c.get(1).copied().unwrap_or(0), c.get(2).copied().unwrap_or(0), c.get(3).copied().unwrap_or(0)]))
                .collect();
            eprintln!("  Generated tokens: {:?}", tokens);
        }

        output_hashes.push(result.output_hash);
    }

    // Check determinism
    let all_match = output_hashes.windows(2).all(|w| w[0] == w[1]);

    // Statistics
    latencies.sort();
    let avg = latencies.iter().sum::<u64>() as f64 / latencies.len() as f64;
    let p50 = latencies[latencies.len() / 2];
    let p99 = latencies[(latencies.len() as f64 * 0.99) as usize];
    let std_dev = {
        let mean = avg;
        let variance = latencies.iter()
            .map(|&x| { let d = x as f64 - mean; d * d })
            .sum::<f64>() / latencies.len() as f64;
        variance.sqrt()
    };

    eprintln!();
    eprintln!("  Results ({} runs):", runs);
    eprintln!("    Deterministic: {}", all_match);
    eprintln!("    Avg latency:   {:.1}ms", avg);
    eprintln!("    p50:           {}ms", p50);
    eprintln!("    p99:           {}ms", p99);
    eprintln!("    Std dev:       {:.1}ms", std_dev);
    eprintln!("    Output hash:   {}", hex_encode(&output_hashes[0].0[..16]));
    eprintln!();

    // JSON output
    let json = serde_json::json!({
        "benchmark": "GgufInference",
        "model_path": model_path,
        "model_id": hex_encode(&model_id.0),
        "platform": format!("{} {}", std::env::consts::OS, std::env::consts::ARCH),
        "runs": runs,
        "max_tokens": max_tokens,
        "deterministic": all_match,
        "output_hash": hex_encode(&output_hashes[0].0),
        "load_time_ms": load_time.as_millis() as u64,
        "latency_avg_ms": avg,
        "latency_p50_ms": p50,
        "latency_p99_ms": p99,
        "latency_stddev_ms": std_dev,
    });
    println!("{}", serde_json::to_string_pretty(&json).unwrap());
}
