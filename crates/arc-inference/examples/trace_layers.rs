//! Trace activation magnitudes through each layer to find where signal dies.

use arc_inference::cached_integer_model::{load_cached_model, KVCache};
use arc_inference::integer_lut::ONE;

fn stats(v: &[i64]) -> (i64, i64, f64) {
    let min = *v.iter().min().unwrap_or(&0);
    let max = *v.iter().max().unwrap_or(&0);
    let mean = v.iter().sum::<i64>() as f64 / v.len() as f64;
    (min, max, mean)
}

fn main() {
    let path = std::env::args().nth(1).unwrap_or("/tmp/tinyllama-1.1b-chat.Q8_0.gguf".into());
    let model = load_cached_model(&path).unwrap();
    let cfg = &model.config;

    // Process BOS + "What is 2+2?" and trace hidden states
    let prompt = vec![1u32, 1724, 338, 29871, 29906, 29974, 29906, 29973]; // BOS + "What is 2+2?"

    let mut cache = KVCache::new(cfg.n_layers);
    println!("Processing {} tokens through {} layers, d={}", prompt.len(), cfg.n_layers, cfg.d_model);

    for (t_idx, &tok) in prompt.iter().enumerate() {
        let logits = model.forward_one_token(tok, &mut cache);
        let (min, max, mean) = stats(&logits);
        let top = logits.iter().enumerate().max_by_key(|(_,v)| *v).unwrap();
        let top_word = model.vocab.get(top.0).map(|s| s.as_str()).unwrap_or("?");
        println!("  tok[{}]={:5} -> logits[{:.0},{:.0}] top={}({}) {:?}",
            t_idx, tok, min as f64 / ONE as f64, max as f64 / ONE as f64,
            top.0, *top.1, top_word);
    }

    // Generate 4 more tokens
    println!("\nGeneration:");
    let mut last = *prompt.last().unwrap();
    for i in 0..4 {
        let logits = model.forward_one_token(last, &mut cache);
        let top = logits.iter().enumerate().max_by_key(|(_,v)| *v).unwrap();
        let top_word = model.vocab.get(top.0).map(|s| s.as_str()).unwrap_or("?");
        let (min, max, _) = stats(&logits);
        println!("  gen[{}] -> logits[{:.1},{:.1}] next={}({}) {:?}",
            i, min as f64 / ONE as f64, max as f64 / ONE as f64,
            top.0, *top.1, top_word);
        last = top.0 as u32;
    }
}
