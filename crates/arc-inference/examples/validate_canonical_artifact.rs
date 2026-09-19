//! Bounded validation of one real GGUF through the canonical ARC INT8 executor.
//!
//! Usage: cargo run -p arc-inference --example validate_canonical_artifact \
//!   --features candle --release -- --model /absolute/path/model.gguf [--max-tokens 8]

use arc_inference::cached_integer_model::{
    CANONICAL_REWARD_INFERENCE_PROFILE, load_cached_model_canonical_i8,
};
use arc_inference::model_artifact::ModelArtifactCommitment;
use serde_json::json;
use std::time::Instant;

const DEFAULT_MAX_TOKENS: u32 = 8;
const MAX_ALLOWED_TOKENS: u32 = 32;
const PROMPT: &str = "What is the capital of France?";

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let mut model_path: Option<String> = None;
    let mut max_tokens = DEFAULT_MAX_TOKENS;
    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--model" => {
                i += 1;
                model_path = args.get(i).cloned();
            }
            "--max-tokens" => {
                i += 1;
                max_tokens = args
                    .get(i)
                    .and_then(|value| value.parse().ok())
                    .unwrap_or(0);
            }
            "-h" | "--help" => {
                println!("usage: {} --model PATH [--max-tokens 1..32]", args[0]);
                return;
            }
            other => {
                eprintln!("unknown argument: {other}");
                std::process::exit(2);
            }
        }
        i += 1;
    }
    let Some(model_path) = model_path else {
        eprintln!("--model PATH is required");
        std::process::exit(2);
    };
    if !(1..=MAX_ALLOWED_TOKENS).contains(&max_tokens) {
        eprintln!("--max-tokens must be in 1..={MAX_ALLOWED_TOKENS}");
        std::process::exit(2);
    }

    eprintln!("phase=artifact_hash_start");
    let artifact = ModelArtifactCommitment::from_path(&model_path).unwrap_or_else(|error| {
        eprintln!("artifact identity failed: {error}");
        std::process::exit(1);
    });
    let artifact_id = artifact.model_id();
    eprintln!("phase=artifact_hash_done bytes={}", artifact.size_bytes());
    eprintln!("phase=model_load_start profile=canonical-i8");
    let mut model = load_cached_model_canonical_i8(&model_path).unwrap_or_else(|error| {
        eprintln!("canonical INT8 load failed: {error}");
        std::process::exit(1);
    });
    eprintln!("phase=model_load_done layers={} memory_bytes={}", model.config.n_layers, model.memory_bytes());
    if !model.has_all_transformer_layers() || !model.has_canonical_i8_profile() {
        eprintln!("loaded model is not a complete canonical INT8 model");
        std::process::exit(1);
    }
    model.enforce_canonical_i8_profile();
    if !model.has_canonical_i8_profile() {
        eprintln!("canonical INT8 profile enforcement failed");
        std::process::exit(1);
    }

    // Match the production validator: apply the artifact's template, encode
    // with its vocabulary, prepend its BOS, then use the canonical executor.
    let templated = model.apply_chat_template(PROMPT);
    let mut prompt_tokens = Vec::new();
    prompt_tokens.push(model.config.bos_token);
    prompt_tokens.extend(model.encode(&templated));

    let mut runs = Vec::with_capacity(2);
    let mut first_tokens = None;
    let mut matches = true;
    let mut reported_hashes_match = true;
    for run_index in 0..2 {
        eprintln!("phase=generation_start run={run_index}");
        let start = Instant::now();
        let (tokens, output_hash) = model
            .try_generate(&prompt_tokens, max_tokens, &model.config.eos_tokens)
            .unwrap_or_else(|error| {
                eprintln!("canonical generation failed: {error}");
                std::process::exit(1);
            });
        if tokens.is_empty() {
            eprintln!("canonical generation returned zero output tokens");
            std::process::exit(1);
        }
        let elapsed_ms = start.elapsed().as_millis();
        let token_bytes: Vec<u8> = tokens.iter().flat_map(|token| token.to_le_bytes()).collect();
        let token_bytes_hash = blake3::hash(&token_bytes);
        let reported_hash_matches = output_hash.0 == *token_bytes_hash.as_bytes();
        reported_hashes_match = reported_hashes_match && reported_hash_matches;
        if let Some(previous) = &first_tokens {
            matches = matches && previous == &tokens;
        } else {
            first_tokens = Some(tokens.clone());
        }
        eprintln!("phase=generation_done run={run_index} tokens={} elapsed_ms={elapsed_ms}", tokens.len());
        runs.push(json!({
            "tokens": tokens,
            "decoded": model.decode(&tokens),
            "token_count": tokens.len(),
            "token_bytes_hash": format!("0x{}", hex::encode(token_bytes_hash.as_bytes())),
            "output_hash": format!("0x{}", hex::encode(output_hash.0)),
            "reported_hash_matches_token_bytes": reported_hash_matches,
            "elapsed_ms": elapsed_ms,
        }));
    }

    let gate_passed = matches && reported_hashes_match;
    let report = json!({
        "artifact_path": model_path,
        "artifact_size_bytes": artifact.size_bytes(),
        "artifact_blake3": format!("0x{}", hex::encode(artifact_id.0)),
        "profile": CANONICAL_REWARD_INFERENCE_PROFILE,
        "prompt_token_count": prompt_tokens.len(),
        "max_tokens": max_tokens,
        "runs": runs,
        "matches": matches,
        "reported_hashes_match_token_bytes": reported_hashes_match,
        "gate_passed": gate_passed,
    });

    println!("{}", serde_json::to_string_pretty(&report).expect("JSON serialization"));
    if !gate_passed {
        std::process::exit(1);
    }
}
