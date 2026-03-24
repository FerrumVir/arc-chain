//! Test candle float backend with proper tokenizer from GGUF vocab.

use arc_inference::candle_backend::GgufEngine;
use arc_inference::cached_integer_model::load_cached_model;

fn main() {
    let path = std::env::args().nth(1).unwrap_or("/tmp/llama-2-7b-chat.Q4_K_M.gguf".into());
    let max_tok: u32 = std::env::args().nth(2).and_then(|s| s.parse().ok()).unwrap_or(32);

    // Load integer model just for the tokenizer
    println!("Loading tokenizer from GGUF...");
    let int_model = load_cached_model(&path).unwrap();
    println!("Vocab: {} entries", int_model.vocab.len());

    // Load candle model for inference
    let engine = GgufEngine::new(120_000);
    let model_id = engine.load_gguf_file(&path).unwrap();
    println!("Candle model loaded: {}", hex::encode(&model_id.0[..8]));

    let prompts = vec![
        "[INST] What is 2+2? [/INST]",
        "[INST] Write a haiku about the ocean [/INST]",
        "[INST] Explain what a blockchain is in one sentence [/INST]",
        "[INST] Hello, how are you? [/INST]",
        "The capital of France is",
        "1+1=",
    ];

    for prompt in &prompts {
        // Use proper SentencePiece tokenizer from the integer model
        let mut tokens: Vec<u32> = vec![1]; // BOS
        tokens.extend(int_model.encode(prompt));

        println!("\nPrompt: {:50} ({} tokens)", &prompt[..prompt.len().min(50)], tokens.len());

        let result = engine.generate(&model_id, &tokens, max_tok).unwrap();

        let output_tokens: Vec<u32> = result.output.chunks(4)
            .map(|c| u32::from_le_bytes([c[0], c.get(1).copied().unwrap_or(0),
                c.get(2).copied().unwrap_or(0), c.get(3).copied().unwrap_or(0)]))
            .collect();

        let decoded = int_model.decode(&output_tokens);
        let ms_tok = if result.tokens_used > 0 { result.elapsed_ms / result.tokens_used as u64 } else { 0 };

        println!("  Output: {:?}", &decoded[..decoded.len().min(120)]);
        println!("  {} tok, {}ms/tok, hash=0x{}", result.tokens_used, ms_tok,
            hex::encode(&result.output_hash.0[..8]));
    }
}
