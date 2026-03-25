//! Cross-platform determinism test for 7B models.
//!
//! Runs multiple prompts at varying token lengths and reports hashes.
//! Compare ARM output against x86 output to verify cross-platform determinism.
//!
//! Usage: cargo run --example cross_platform_7b --features candle --release -- /path/to/model.gguf

use arc_inference::cached_integer_model::load_cached_model;
use std::time::Instant;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let model_path = args.get(1).expect("Usage: cross_platform_7b <model.gguf>");

    let model = load_cached_model(model_path).expect("Failed to load model");
    let whash = hex::encode(&model.weight_hash().0[..16]);

    println!("=== Cross-Platform 7B Determinism Test ===");
    println!("Platform: {} ({})", std::env::consts::ARCH, std::env::consts::OS);
    println!("Model: {}", model_path);
    println!("Config: {} layers, d_model={}, vocab={}", model.config.n_layers, model.config.d_model, model.config.vocab_size);
    println!("Weight hash: 0x{}", whash);
    println!("Memory: {} MB", model.memory_bytes() / (1024 * 1024));
    println!();

    // Prompts designed to produce long outputs (no early EOS)
    let prompts: Vec<(&str, u32)> = vec![
        ("[INST] What is 2+2? [/INST]", 8),
        ("[INST] Explain what a blockchain is [/INST]", 32),
        ("[INST] Write a detailed explanation of how photosynthesis works [/INST]", 64),
        ("[INST] Write a long essay about the history of computing from Ada Lovelace to modern AI [/INST]", 128),
        ("[INST] Write a comprehensive guide to learning Rust programming language for beginners [/INST]", 256),
        ("[INST] Write a very detailed technical paper about distributed systems consensus algorithms [/INST]", 512),
    ];

    let eos = vec![2u32]; // </s> only, no 0

    for (prompt, max_tokens) in &prompts {
        let tokens = model.encode(prompt);
        let start = Instant::now();
        let (generated, hash) = model.generate(&tokens, *max_tokens, &eos);
        let elapsed = start.elapsed();
        let n = generated.len();
        let ms_tok = if n > 0 { elapsed.as_millis() as u64 / n as u64 } else { 0 };

        // Verify determinism: run again
        let (gen2, hash2) = model.generate(&tokens, *max_tokens, &eos);
        let det = if hash == hash2 && generated == gen2 { "PASS" } else { "FAIL" };

        let decoded = model.decode(&generated);
        let preview: String = decoded.chars().take(80).collect();

        println!("tokens={:>4} | hash=0x{} | det={} | {}ms/tok | \"{}...\"",
            n, hex::encode(&hash.0[..8]), det, ms_tok, preview.replace('\n', " "));
    }

    println!();
    println!("Weight hash: 0x{}", whash);
    println!("Platform: {}", std::env::consts::ARCH);
}
