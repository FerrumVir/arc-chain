use arc_inference::cached_integer_model::load_cached_model;
use arc_inference::cached_integer_model::KVCache;
use arc_inference::integer_lut::ONE;
use std::time::Instant;

fn main() {
    let path = std::env::args().nth(1).unwrap_or("/tmp/llama-2-7b-chat.Q4_K_M.gguf".into());
    let max_tok: u32 = std::env::args().nth(2).and_then(|s| s.parse().ok()).unwrap_or(32);

    println!("Loading 7B model...");
    let start = Instant::now();
    let mut model = load_cached_model(&path).unwrap();
    println!("Loaded in {:.1}s, {} MB", start.elapsed().as_secs_f64(), model.memory_bytes() / (1024*1024));

    println!("Converting to Q4...");
    let q4_start = Instant::now();
    model.enable_q4();
    println!("Q4 conversion in {:.1}s", q4_start.elapsed().as_secs_f64());
    
    // Q4 memory estimate
    let q4_mem: usize = model.q4_layers.as_ref().map(|layers| {
        layers.iter().map(|l| {
            l.wq.memory_bytes() + l.wk.memory_bytes() + l.wv.memory_bytes() +
            l.wo.memory_bytes() + l.w_gate.memory_bytes() + l.w_up.memory_bytes() +
            l.w_down.memory_bytes()
        }).sum()
    }).unwrap_or(0);
    println!("Q4 weight memory: {} MB", q4_mem / (1024*1024));

    let prompts = vec![
        "[INST] What is 2+2? [/INST]",
        "[INST] What is the capital of France? [/INST]",
        "[INST] Write a haiku about the ocean [/INST]",
        "[INST] Hello, how are you? [/INST]",
    ];

    for prompt in &prompts {
        let tokens = model.encode(prompt);
        let eos = vec![2u32, 0];
        let (generated, hash) = model.generate(&tokens, max_tok, &eos);
        let decoded = model.decode(&generated);
        println!("\nPrompt: {}", &prompt[7..prompt.len()-8]); // strip [INST]
        println!("Output: {}", decoded.trim());
        println!("Hash: 0x{} ({} tokens)", hex::encode(&hash.0[..8]), generated.len());
    }

    // Determinism check
    println!("\n=== Determinism ===");
    let tokens = model.encode("[INST] What is 2+2? [/INST]");
    let eos = vec![2u32, 0];
    let (_, h1) = model.generate(&tokens, max_tok, &eos);
    let (_, h2) = model.generate(&tokens, max_tok, &eos);
    let (_, h3) = model.generate(&tokens, max_tok, &eos);
    println!("Run 1: 0x{}", hex::encode(&h1.0[..8]));
    println!("Run 2: 0x{}", hex::encode(&h2.0[..8]));
    println!("Run 3: 0x{}", hex::encode(&h3.0[..8]));
    println!("All identical: {}", h1 == h2 && h2 == h3);
}
