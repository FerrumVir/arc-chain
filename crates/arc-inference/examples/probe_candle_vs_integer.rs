// Side-by-side: candle's reference quantized Llama forward vs our integer
// forward, on the same GGUF file + same BOS input. If argmax disagrees or
// top-K distributions differ dramatically, our integer path is buggy.
//
// This is the FP16-quality ground truth we've been missing. candle's
// quantized_llama::ModelWeights.forward() is what llama.cpp/HF-grade
// inference produces on this exact GGUF; any deviation is our bug.
use arc_inference::cached_integer_model::{load_cached_model, KVCache};
use candle_core::{Device, Tensor};
use candle_transformers::models::quantized_llama::ModelWeights;

const MODEL_PATH: &str = "/Users/tjdunham/.arc-models/llama-2-7b.gguf";

fn top_k(logits: &[f32], k: usize) -> Vec<(usize, f32)> {
    let mut indexed: Vec<(usize, f32)> = logits.iter().enumerate().map(|(i, &v)| (i, v)).collect();
    indexed.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap());
    indexed.truncate(k);
    indexed
}

fn top_k_i64(logits: &[i64], k: usize) -> Vec<(usize, i64)> {
    let mut indexed: Vec<(usize, i64)> = logits.iter().enumerate().map(|(i, &v)| (i, v)).collect();
    indexed.sort_by(|a, b| b.1.cmp(&a.1));
    indexed.truncate(k);
    indexed
}

fn main() {
    let device = Device::Cpu;

    // ─── Candle reference path ───────────────────────────────────────
    eprintln!("Loading candle reference model...");
    let mut file = std::fs::File::open(MODEL_PATH).expect("open gguf");
    let content = candle_core::quantized::gguf_file::Content::read(&mut file).expect("parse gguf");
    let mut candle_model = ModelWeights::from_gguf(content, &mut file, &device).expect("from_gguf");
    eprintln!("Loaded candle model.");

    // BOS token (1) forward - batch=1, seq=1
    let bos = Tensor::new(&[1u32], &device).unwrap().unsqueeze(0).unwrap();
    let candle_logits = candle_model.forward(&bos, 0).expect("candle forward");
    let candle_logits = candle_logits.squeeze(0).unwrap();
    let candle_logits = if candle_logits.dims().len() == 2 {
        candle_logits.get(candle_logits.dim(0).unwrap() - 1).unwrap()
    } else {
        candle_logits
    };
    let candle_vec: Vec<f32> = candle_logits.to_vec1().expect("to_vec");
    let c_top = top_k(&candle_vec, 10);
    eprintln!("\n=== CANDLE (reference) ===");
    eprintln!("argmax = {} ({:.3})", c_top[0].0, c_top[0].1);
    eprintln!("top-10:");
    for (i, (idx, v)) in c_top.iter().enumerate() {
        eprintln!("  {:>2}. token={:>5}  logit={:>8.3}", i + 1, idx, v);
    }

    // ─── Our integer path ────────────────────────────────────────────
    eprintln!("\nLoading our integer model...");
    let integer_model = load_cached_model(MODEL_PATH).expect("our load");
    eprintln!("  block_i8_layers installed: {}", integer_model.block_i8_layers.is_some());
    eprintln!("  block_i8_output installed: {}", integer_model.block_i8_output.is_some());

    let mut cache = KVCache::new(integer_model.config.n_layers);
    let our_logits = integer_model.forward_one_token(1, &mut cache);
    let our_top = top_k_i64(&our_logits, 10);
    eprintln!("\n=== OURS (integer forward with block-i8) ===");
    let our_argmax_logit_real = our_top[0].1 as f64 / 65536.0;
    eprintln!("argmax = {} ({:.3})", our_top[0].0, our_argmax_logit_real);
    eprintln!("top-10:");
    for (i, (idx, v)) in our_top.iter().enumerate() {
        eprintln!("  {:>2}. token={:>5}  logit={:>8.3}", i + 1, idx, *v as f64 / 65536.0);
    }

    // ─── Comparison ─────────────────────────────────────────────────
    eprintln!("\n=== COMPARISON ===");
    eprintln!("candle argmax: {}", c_top[0].0);
    eprintln!("our argmax:    {}", our_top[0].0);
    if c_top[0].0 == our_top[0].0 {
        eprintln!("✓ argmax matches");
    } else {
        eprintln!("✗ argmax diverges");
    }

    // How many of candle's top-10 appear in our top-100?
    let our_top100: std::collections::HashSet<usize> = top_k_i64(&our_logits, 100)
        .iter().map(|(i, _)| *i).collect();
    let overlap = c_top.iter().filter(|(i, _)| our_top100.contains(i)).count();
    eprintln!("candle top-10 ∩ our top-100: {} / 10", overlap);

    // If our argmax is in candle's top-100, the model is directionally sane.
    let candle_top100: std::collections::HashSet<usize> = top_k(&candle_vec, 100)
        .iter().map(|(i, _)| *i).collect();
    if candle_top100.contains(&our_top[0].0) {
        eprintln!("✓ our argmax is in candle's top-100");
    } else {
        eprintln!("✗ our argmax is NOT in candle's top-100 (fully diverged)");
    }

    // Pearson correlation on full logit vectors would be ideal; rank
    // correlation suffices as a quick signal.
    let c_rank: std::collections::HashMap<usize, usize> = top_k(&candle_vec, candle_vec.len())
        .iter().enumerate().map(|(rank, (idx, _))| (*idx, rank)).collect();
    let our_rank: std::collections::HashMap<usize, usize> = top_k_i64(&our_logits, our_logits.len())
        .iter().enumerate().map(|(rank, (idx, _))| (*idx, rank)).collect();

    // Average |rank_candle - rank_ours| over candle's top-100.
    let mean_rank_diff: f64 = top_k(&candle_vec, 100).iter()
        .map(|(idx, _)| {
            let cr = c_rank[idx] as i64;
            let o_r = *our_rank.get(idx).unwrap_or(&(candle_vec.len() - 1)) as i64;
            (o_r - cr).abs() as f64
        }).sum::<f64>() / 100.0;
    eprintln!("mean rank-diff on candle top-100: {:.1}", mean_rank_diff);
    eprintln!("(0 = perfect, candle.len()/2 = random)");
}
