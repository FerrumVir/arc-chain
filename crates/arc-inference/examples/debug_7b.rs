use arc_inference::cached_integer_model::{load_cached_model, KVCache};
use arc_inference::integer_lut::*;

fn main() {
    let model = load_cached_model("/tmp/llama-2-7b-chat.Q4_K_M.gguf").unwrap();
    let cfg = &model.config;
    
    // Process "[INST] What is 2+2? [/INST]" token by token
    let prompt_text = "[INST] What is 2+2? [/INST]";
    let tokens = model.encode(prompt_text);
    println!("Prompt: {} -> {} tokens: {:?}", prompt_text, tokens.len(), tokens);
    
    let mut cache = KVCache::new(cfg.n_layers);
    
    // BOS
    let _ = model.forward_one_token(1, &mut cache);
    
    // Process all prompt tokens
    let mut last_logits = vec![0i64; cfg.vocab_size];
    for &tok in &tokens {
        last_logits = model.forward_one_token(tok, &mut cache);
    }
    
    // Show top 10 predictions after the full prompt
    let mut indexed: Vec<(usize, i64)> = last_logits.iter().enumerate().map(|(i,&v)| (i,v)).collect();
    indexed.sort_by(|a,b| b.1.cmp(&a.1));
    
    println!("\nTop 10 next tokens after prompt:");
    for (rank, (tok_id, logit)) in indexed[0..10].iter().enumerate() {
        let word = model.vocab.get(*tok_id).map(|s| s.as_str()).unwrap_or("?");
        let real = *logit as f64 / ONE as f64;
        println!("  #{}: token {:5} = {:15?} logit={:10} ({:.2})", rank+1, tok_id, word, logit, real);
    }
    
    // Check where "4" is
    // Token for "4" in Llama vocab
    for candidate in [29946u32, 29871, 29945, 29900, 29896] { // likely token IDs for "4", " ", "5", "0", "1"
        let word = model.vocab.get(candidate as usize).map(|s| s.as_str()).unwrap_or("?");
        let logit = last_logits[candidate as usize];
        println!("  Token {} = {:?} logit={}", candidate, word, logit);
    }
}
