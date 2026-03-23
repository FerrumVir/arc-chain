//! Integer Engine Cross-Platform Determinism Benchmark
//!
//! Builds a test transformer using the integer-only engine and runs
//! multi-token generation. Outputs the hash for cross-platform comparison.
//!
//! Usage: cargo run --release --bin arc-bench-integer

use arc_inference::integer_engine::{build_test_model, IntTransformerModel};
use arc_inference::integer_lut::*;
use std::time::Instant;

fn hex_encode(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{:02x}", b)).collect()
}

fn main() {
    let runs: usize = std::env::args()
        .position(|a| a == "--runs")
        .map(|i| std::env::args().nth(i + 1).unwrap().parse().unwrap())
        .unwrap_or(100);

    eprintln!("═══════════════════════════════════════════════════════════");
    eprintln!("ARC Chain — Integer Engine Cross-Platform Benchmark");
    eprintln!("═══════════════════════════════════════════════════════════");
    eprintln!("  Platform: {} {}", std::env::consts::OS, std::env::consts::ARCH);
    eprintln!("  Runs:     {}", runs);
    eprintln!();

    // Build test models of increasing size
    let configs = vec![
        ("2-layer tiny (v=50, d=32, h=2, ff=64)", 50, 32, 2, 64, 2),
        ("4-layer small (v=100, d=64, h=2, ff=128)", 100, 64, 2, 128, 4),
        ("4-layer medium (v=200, d=128, h=4, ff=256)", 200, 128, 4, 256, 4),
    ];

    let prompt = vec![1u32, 2, 3, 4, 5];
    let max_tokens = 16u32;

    let mut results = Vec::new();

    for (name, vocab, d_model, n_heads, d_ff, n_layers) in &configs {
        eprintln!("  Building {}...", name);
        let model = build_test_model(*vocab, *d_model, *n_heads, *d_ff, *n_layers);

        // Warmup
        let _ = model.generate_with_hash(&prompt, max_tokens, 99);

        // Benchmark
        let mut hashes = Vec::new();
        let mut latencies = Vec::new();
        let mut first_tokens = None;

        for i in 0..runs {
            let start = Instant::now();
            let (tokens, hash) = model.generate_with_hash(&prompt, max_tokens, 99);
            let elapsed = start.elapsed().as_micros() as u64;
            latencies.push(elapsed);
            hashes.push(hash);

            if i == 0 {
                first_tokens = Some(tokens.clone());
                eprintln!("    Generated tokens: {:?}", tokens);
                eprintln!("    Output hash: {}", hex_encode(&hash.0[..16]));
            }
        }

        // Check determinism
        let all_match = hashes.windows(2).all(|w| w[0] == w[1]);

        // Statistics
        latencies.sort();
        let avg = latencies.iter().sum::<u64>() as f64 / latencies.len() as f64;
        let p50 = latencies[latencies.len() / 2];
        let p99 = latencies[(latencies.len() as f64 * 0.99) as usize];

        eprintln!("    Deterministic: {} ({} runs)", all_match, runs);
        eprintln!("    Avg: {:.1}us, p50: {}us, p99: {}us", avg, p50, p99);
        eprintln!();

        results.push(serde_json::json!({
            "name": name,
            "vocab_size": vocab,
            "d_model": d_model,
            "n_heads": n_heads,
            "d_ff": d_ff,
            "n_layers": n_layers,
            "runs": runs,
            "deterministic": all_match,
            "output_hash": hex_encode(&hashes[0].0),
            "generated_tokens": first_tokens,
            "avg_us": avg,
            "p50_us": p50,
            "p99_us": p99,
        }));
    }

    let output = serde_json::json!({
        "benchmark": "IntegerEngine",
        "platform": format!("{} {}", std::env::consts::OS, std::env::consts::ARCH),
        "models": results,
    });

    println!("{}", serde_json::to_string_pretty(&output).unwrap());
}
