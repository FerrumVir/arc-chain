//! Debug: trace values through the forward pass to find where output goes wrong.

use arc_inference::cached_integer_model::{load_cached_model, KVCache};
use arc_inference::integer_lut::*;

fn main() {
    let model = load_cached_model(
        &std::env::args().nth(1).unwrap_or("/tmp/tinyllama-1.1b-chat.Q8_0.gguf".into())
    ).unwrap();
    let cfg = &model.config;
    let d = cfg.d_model;

    println!("Config: {}L d={} h={} kv={} ff={} v={}",
        cfg.n_layers, d, cfg.n_heads, cfg.n_kv_heads, cfg.d_ff, cfg.vocab_size);

    // Token "What" = 1724
    let tok = 1724u32;
    let idx = tok as usize;

    // Embedding
    let emb_scale = model.embedding.scales[idx];
    let hidden: Vec<i64> = model.embedding.data[idx*d..(idx+1)*d]
        .iter().map(|&w| (w as i64) * emb_scale).collect();
    println!("\n=== Embedding (token {}) ===", tok);
    println!("scale={} first8={:?}", emb_scale, &hidden[0..8]);
    println!("range=[{}, {}] mean={}",
        hidden.iter().min().unwrap(), hidden.iter().max().unwrap(),
        hidden.iter().sum::<i64>() / d as i64);

    // First layer forward
    let layer = &model.layers[0];

    // RMSNorm
    let n = d as i64;
    let mut sq_sum: i64 = 0;
    for &x in &hidden { sq_sum += (x * x) >> FRAC_BITS; }
    let mean_sq = sq_sum / n;
    let inv_rms = integer_isqrt(mean_sq + 1);
    println!("\n=== RMSNorm (layer 0) ===");
    println!("sq_sum={} mean_sq={} inv_rms={}", sq_sum, mean_sq, inv_rms);
    println!("gamma[0..4]={:?}", &layer.attn_norm[0..4]);

    let normed: Vec<i64> = hidden.iter().enumerate().map(|(i, &x)| {
        let norm = (x * inv_rms) >> FRAC_BITS;
        (norm * layer.attn_norm[i]) >> FRAC_BITS
    }).collect();
    println!("normed[0..8]={:?}", &normed[0..8]);
    println!("normed range=[{}, {}]", normed.iter().min().unwrap(), normed.iter().max().unwrap());

    // Full forward pass for 1 token
    let mut cache = KVCache::new(cfg.n_layers);
    let logits = model.forward_one_token(tok, &mut cache);

    println!("\n=== Logits ===");
    println!("first8={:?}", &logits[0..8]);
    println!("range=[{}, {}]", logits.iter().min().unwrap(), logits.iter().max().unwrap());

    // Top 5 tokens
    let mut indexed: Vec<(usize, i64)> = logits.iter().enumerate().map(|(i,&v)| (i,v)).collect();
    indexed.sort_by(|a,b| b.1.cmp(&a.1));
    println!("\nTop 5 tokens:");
    for (i, (tok_id, val)) in indexed[0..5].iter().enumerate() {
        let word = model.vocab.get(*tok_id).map(|s| s.as_str()).unwrap_or("?");
        println!("  #{}: token {} = {:?} (logit={})", i+1, tok_id, word, val);
    }

    // Check SiLU for a few values
    println!("\n=== SiLU check ===");
    for x_f in [-2.0f64, -1.0, -0.5, 0.0, 0.5, 1.0, 2.0] {
        let x = (x_f * ONE as f64) as i64;
        let result_int = silu_i64_check(x);
        let result_real = x_f * (1.0 / (1.0 + (-x_f).exp()));
        println!("  SiLU({:.1}) = int:{:.4} real:{:.4} (err={:.2}%)",
            x_f, result_int as f64 / ONE as f64, result_real,
            ((result_int as f64 / ONE as f64 - result_real) / result_real.abs().max(0.001) * 100.0));
    }
}

fn silu_i64_check(x: i64) -> i64 {
    let sig = if x >= 0 {
        let exp_neg = integer_exp(-x);
        (ONE * ONE) / (ONE + exp_neg).max(1)
    } else {
        let exp_pos = integer_exp(x);
        (exp_pos * ONE) / (ONE + exp_pos).max(1)
    };
    (x * sig) >> FRAC_BITS
}
