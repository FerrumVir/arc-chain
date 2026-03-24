use arc_inference::cached_integer_model::{load_cached_model, KVCache};
use arc_inference::integer_lut::*;

fn main() {
    let model = load_cached_model("/tmp/llama-2-7b-chat.Q4_K_M.gguf").unwrap();
    let cfg = &model.config;
    let prompt = "[INST] What is 2+2? [/INST]";
    let tokens = model.encode(prompt);
    
    let mut cache = KVCache::new(cfg.n_layers);
    let _ = model.forward_one_token(1, &mut cache); // BOS
    for &tok in &tokens {
        let _ = model.forward_one_token(tok, &mut cache);
    }
    
    // Generate step by step, show top 5 at each step
    let mut generated = Vec::new();
    for step in 0..10 {
        let last = generated.last().copied().unwrap_or(*tokens.last().unwrap());
        let mut logits = model.forward_one_token(last, &mut cache);
        
        // Apply repetition penalty (same as generate())
        for &prev_tok in generated.iter().rev().take(64) {
            let idx = prev_tok as usize;
            if idx < logits.len() {
                if logits[idx] > 0 { logits[idx] = logits[idx] * 5 / 6; }
                else { logits[idx] = logits[idx] * 6 / 5; }
            }
        }
        
        let mut indexed: Vec<(usize, i64)> = logits.iter().enumerate().map(|(i,&v)| (i,v)).collect();
        indexed.sort_by(|a,b| b.1.cmp(&a.1));
        let next = indexed[0].0 as u32;
        let word = model.vocab.get(next as usize).map(|s| s.as_str()).unwrap_or("?");
        
        print!("Step {}: -> {} {:?}  [", step, next, word);
        for (tid, logit) in indexed[0..5].iter() {
            let w = model.vocab.get(*tid).map(|s| s.as_str()).unwrap_or("?");
            print!(" {}:{}", w.replace('\n', "\\n"), logit);
        }
        println!(" ]");
        
        // Check where "4" ranks
        let four_logit = logits[29946];
        let four_rank = indexed.iter().position(|(id,_)| *id == 29946).unwrap_or(99999);
        if step >= 5 { // after "2 + 2 ="
            println!("  '4' (29946) rank={} logit={}", four_rank, four_logit);
        }
        
        generated.push(next);
        if next == 2 { break; }
    }
    
    let decoded = model.decode(&generated);
    println!("\nFull output: {}", decoded);
}
