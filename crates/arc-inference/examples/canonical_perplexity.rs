//! Perplexity of the canonical integer profile, measured the way llama.cpp's
//! `llama-perplexity` measures it, so the two numbers are comparable on the
//! same GGUF and text (M4).
//!
//! Method (llama.cpp `perplexity()`): tokenize the whole text once with BOS;
//! cut `n_ctx`-token chunks; replace each chunk's first token with BOS; run
//! the chunk; score positions `n_ctx/2 .. n_ctx-2`, where the logits at `j`
//! predict token `j+1`. PPL = exp(mean negative log-likelihood).
//!
//! The model runs the canonical profile end to end: the interleaved-RoPE
//! loader, integer forward, Q16 logits. Only the measurement (log-softmax of
//! the logits) uses f64, as every perplexity tool does.
//!
//! Usage:
//!   cargo run -p arc-inference --example canonical_perplexity --features candle --release -- \
//!     --model /path/model.gguf --text wiki.test.raw [--ctx 512] [--chunks 20] \
//!     [--tokens-out tokens.txt] [--json-out result.json]

use arc_inference::cached_integer_model::{
    GGUF_INTERLEAVED_ROPE_I8_INFERENCE_PROFILE, KVCache,
    load_cached_model_canonical_i8_interleaved_rope,
};
use arc_inference::llama_spm_tokenizer::{
    GGUF_LLAMA_SPM_TOKENIZER_PROFILE_V1, LlamaGgufSpmTokenizer,
};
use arc_inference::model_artifact::ModelArtifactCommitment;
use serde_json::json;
use std::io::Write;
use std::time::Instant;

const Q16: f64 = 65536.0;

fn arg(args: &[String], name: &str) -> Option<String> {
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1).cloned())
}

/// Negative log-likelihood of `target` under Q16 logits, in f64.
fn nll(logits: &[i64], target: u32) -> f64 {
    let max = logits.iter().copied().max().unwrap_or(0) as f64 / Q16;
    let sum: f64 = logits.iter().map(|&x| (x as f64 / Q16 - max).exp()).sum();
    let log_z = max + sum.ln();
    log_z - logits[target as usize] as f64 / Q16
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let model_path = arg(&args, "--model").expect("--model PATH");
    let text_path = arg(&args, "--text").expect("--text PATH");
    let n_ctx: usize = arg(&args, "--ctx").map_or(512, |v| v.parse().expect("--ctx"));
    let max_chunks: usize = arg(&args, "--chunks").map_or(20, |v| v.parse().expect("--chunks"));
    assert!(
        n_ctx >= 4 && n_ctx.is_multiple_of(2),
        "--ctx must be even and at least 4"
    );

    let artifact = ModelArtifactCommitment::from_path(&model_path).expect("hash artifact");
    let tokenizer = LlamaGgufSpmTokenizer::from_gguf(&model_path).expect("tokenizer");
    let text = std::fs::read_to_string(&text_path).expect("read --text");
    let tokens = tokenizer.encode_prompt(&text).expect("tokenize");
    if let Some(path) = arg(&args, "--tokens-out") {
        let mut file = std::fs::File::create(path).expect("--tokens-out");
        for token in &tokens {
            writeln!(file, "{token}").expect("write token");
        }
    }

    let load = Instant::now();
    let model = load_cached_model_canonical_i8_interleaved_rope(&model_path).expect("load model");
    assert_eq!(
        model.canonical_execution_profile(),
        Some(GGUF_INTERLEAVED_ROPE_I8_INFERENCE_PROFILE)
    );
    assert!(
        n_ctx <= model.config.max_seq,
        "--ctx exceeds the model window"
    );
    let load_s = load.elapsed().as_secs_f64();
    let bos = tokens[0];

    let chunks = (tokens.len() / n_ctx).min(max_chunks);
    let first = n_ctx / 2;
    let mut total_nll = 0.0f64;
    let mut scored = 0usize;
    let mut per_chunk = Vec::with_capacity(chunks);
    let run = Instant::now();
    for chunk in 0..chunks {
        let mut window = tokens[chunk * n_ctx..(chunk + 1) * n_ctx].to_vec();
        window[0] = bos;
        let mut cache = KVCache::new(model.config.n_layers);
        // Every position's logits are needed. Batched prefill returns them all
        // bit-identically (M7); fall back to token-at-a-time if it refuses.
        let logits: Vec<Vec<i64>> = model
            .prefill_canonical_i8_batched(&window, &mut cache, 64, true)
            .unwrap_or_else(|| {
                let mut cache = KVCache::new(model.config.n_layers);
                window
                    .iter()
                    .map(|&token| model.forward_one_token(token, &mut cache))
                    .collect()
            });
        let mut chunk_nll = 0.0;
        for j in first..n_ctx - 1 {
            chunk_nll += nll(&logits[j], window[j + 1]);
        }
        let count = n_ctx - 1 - first;
        total_nll += chunk_nll;
        scored += count;
        let running = (total_nll / scored as f64).exp();
        per_chunk.push(
            json!({"chunk": chunk, "nll_mean": chunk_nll / count as f64, "running_ppl": running}),
        );
        eprintln!("[{}/{}] running PPL {running:.4}", chunk + 1, chunks);
    }
    let ppl = (total_nll / scored.max(1) as f64).exp();
    let result = json!({
        "schema": "arc.m4.perplexity.v1",
        "method": "llama.cpp perplexity: BOS per chunk, score positions n_ctx/2..n_ctx-2",
        "artifact_blake3": artifact.model_id().to_hex(),
        "profile": GGUF_INTERLEAVED_ROPE_I8_INFERENCE_PROFILE,
        "tokenizer": GGUF_LLAMA_SPM_TOKENIZER_PROFILE_V1,
        "text": text_path,
        "text_tokens": tokens.len(),
        "n_ctx": n_ctx,
        "chunks": chunks,
        "scored_tokens": scored,
        "ppl": ppl,
        "load_s": load_s,
        "run_s": run.elapsed().as_secs_f64(),
        "per_chunk": per_chunk,
    });
    let text = serde_json::to_string_pretty(&result).expect("json");
    if let Some(path) = arg(&args, "--json-out") {
        std::fs::write(path, &text).expect("--json-out");
    }
    println!("{text}");
}
