//! Perplexity evaluation for the deterministic integer inference engine.
//!
//! Measures quality (perplexity) of INT8 quantized inference against a text corpus.
//! This provides the missing quality metric for the determinism paper.
//!
//! The inference forward pass is 100% deterministic integer arithmetic.
//! Only the perplexity *measurement* (log-softmax) uses f64 — this is standard
//! practice even for floating-point models.
//!
//! Usage:
//!   cargo run --example eval_perplexity --features candle --release -- \
//!       /path/to/model.gguf /path/to/wikitext-2-raw/wiki.test.raw [max_tokens]
//!
//! The text file should be raw text (e.g., WikiText-2 test split).
//! Download WikiText-2: https://huggingface.co/datasets/wikitext (wikitext-2-raw-v1)

use arc_inference::cached_integer_model::load_cached_model;
use std::time::Instant;

const ONE: f64 = 65536.0; // Q16 fixed-point scale

/// Compute log-softmax in f64 from Q16 integer logits.
///
/// The inference engine produces logits in Q16 (deterministic, integer-only).
/// We convert to f64 for the perplexity *measurement*, which is standard —
/// even FP16 models measure perplexity in f64.
fn log_softmax_f64(logits_q16: &[i64]) -> Vec<f64> {
    // Convert Q16 logits to f64
    let logits: Vec<f64> = logits_q16.iter().map(|&x| x as f64 / ONE).collect();

    // Numerically stable log-softmax: log(exp(x_i) / sum(exp(x_j)))
    //   = x_i - log(sum(exp(x_j)))
    //   = x_i - max - log(sum(exp(x_j - max)))
    let max = logits.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
    let log_sum_exp: f64 = logits.iter().map(|&x| (x - max).exp()).sum::<f64>().ln() + max;

    logits.iter().map(|&x| x - log_sum_exp).collect()
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 3 {
        eprintln!("Usage: {} <model.gguf> <text_file> [max_tokens]", args[0]);
        eprintln!();
        eprintln!("Evaluates perplexity of the INT8 integer engine on a text corpus.");
        eprintln!("Use WikiText-2 test split for standard benchmarking.");
        std::process::exit(1);
    }

    let model_path = &args[1];
    let text_path = &args[2];
    let max_tokens: usize = args.get(3).and_then(|s| s.parse().ok()).unwrap_or(0);

    // Load model
    println!("=== ARC Chain Perplexity Evaluation ===");
    println!("Model: {}", model_path);
    println!("Text:  {}", text_path);
    println!();

    let load_start = Instant::now();
    let model = load_cached_model(model_path).expect("Failed to load model");
    println!("Model loaded in {:.2}s", load_start.elapsed().as_secs_f64());
    println!("Config: {} layers, d_model={}, vocab={}",
        model.config.n_layers, model.config.d_model, model.config.vocab_size);
    println!("Weight hash: 0x{}", hex::encode(&model.weight_hash().0[..8]));
    println!();

    // Read tokens: either pre-tokenized JSON ([1, 2, 3, ...]) or raw text
    let raw = std::fs::read_to_string(text_path).expect("Failed to read input file");
    let tokens: Vec<u32> = if text_path.ends_with(".json") {
        // Pre-tokenized: JSON array of token IDs (use HuggingFace tokenizer for accuracy)
        let ids: Vec<u64> = serde_json::from_str(&raw).expect("Invalid JSON token array");
        println!("Pre-tokenized input: {} tokens", ids.len());
        ids.into_iter().map(|id| id as u32).collect()
    } else {
        // Raw text: use model's built-in tokenizer (greedy longest-match)
        let toks = model.encode(&raw);
        println!("Text: {} chars -> {} tokens (model tokenizer)", raw.len(), toks.len());
        toks
    };
    let n_tokens = if max_tokens > 0 && max_tokens < tokens.len() {
        max_tokens
    } else {
        tokens.len()
    };
    println!("Evaluating {} tokens", n_tokens);

    if n_tokens < 2 {
        eprintln!("Need at least 2 tokens for perplexity evaluation.");
        std::process::exit(1);
    }

    // Evaluate perplexity
    // PPL = exp( -1/N * sum_{i=1}^{N} log P(token_i | token_{<i}) )
    let mut cache = arc_inference::cached_integer_model::KVCache::new(model.config.n_layers);
    let mut neg_log_likelihood_sum: f64 = 0.0;
    let mut n_evaluated: usize = 0;

    let eval_start = Instant::now();

    // Feed BOS token
    let _ = model.forward_one_token(1, &mut cache);

    // Feed each token and measure probability of the NEXT token
    for i in 0..n_tokens - 1 {
        let token = tokens[i];
        let next_token = tokens[i + 1] as usize;

        // Forward pass: deterministic integer arithmetic (Q16)
        let logits_q16 = model.forward_one_token(token, &mut cache);

        // Measurement: compute log-softmax in f64 from Q16 logits
        let log_probs = log_softmax_f64(&logits_q16);

        // Extract log-probability of the true next token
        if next_token < log_probs.len() {
            neg_log_likelihood_sum -= log_probs[next_token];
        } else {
            // OOV token — use worst-case
            neg_log_likelihood_sum -= (-(model.config.vocab_size as f64)).ln();
        }
        n_evaluated += 1;

        // Progress reporting
        if (i + 1) % 200 == 0 || i == n_tokens - 2 {
            let elapsed = eval_start.elapsed().as_secs_f64();
            let running_ppl = (neg_log_likelihood_sum / n_evaluated as f64).exp();
            let tok_per_sec = (i + 1) as f64 / elapsed;
            print!("\r[{}/{}] PPL: {:.2}  ({:.1} tok/s)     ",
                i + 1, n_tokens - 1, running_ppl, tok_per_sec);
            use std::io::Write;
            std::io::stdout().flush().ok();
        }
    }
    println!();

    let total_time = eval_start.elapsed();
    let perplexity = (neg_log_likelihood_sum / n_evaluated as f64).exp();
    let avg_nll = neg_log_likelihood_sum / n_evaluated as f64;

    println!();
    println!("=== Results ===");
    println!("Tokens evaluated:  {}", n_evaluated);
    println!("Avg NLL:           {:.4}", avg_nll);
    println!("Perplexity:        {:.2}", perplexity);
    println!("Bits per byte:     {:.2}", avg_nll / (2.0_f64).ln());
    println!("Time:              {:.1}s ({:.1} tok/s)",
        total_time.as_secs_f64(),
        n_evaluated as f64 / total_time.as_secs_f64());
    println!();
    println!("Model:         {} (INT8 integer engine)", model_path);
    println!("Weight hash:   0x{}", hex::encode(&model.weight_hash().0[..8]));
    println!("Deterministic: all forward pass computations use pure integer arithmetic");
    println!("Measurement:   log-softmax computed in f64 from Q16 logits (standard practice)");

    // Determinism check: re-run first 10 tokens and verify same logits
    println!();
    println!("--- Determinism Verification ---");
    let mut all_match = true;
    let verify_count = n_tokens.min(10);
    for i in 0..verify_count {
        let logits1 = {
            let mut c = arc_inference::cached_integer_model::KVCache::new(model.config.n_layers);
            let _ = model.forward_one_token(1, &mut c);
            for &t in &tokens[..i] { let _ = model.forward_one_token(t, &mut c); }
            model.forward_one_token(tokens[i], &mut c)
        };
        let logits2 = {
            let mut c = arc_inference::cached_integer_model::KVCache::new(model.config.n_layers);
            let _ = model.forward_one_token(1, &mut c);
            for &t in &tokens[..i] { let _ = model.forward_one_token(t, &mut c); }
            model.forward_one_token(tokens[i], &mut c)
        };
        if logits1 != logits2 {
            println!("FAIL: Token {} produced different logits on re-run!", i);
            all_match = false;
        }
    }
    if all_match {
        println!("PASS: Logits are bitwise identical across runs (verified {} tokens)", verify_count);
    }
}
