//! Bounded validation of one real GGUF through the canonical ARC INT8 executor.
//!
//! Usage: cargo run -p arc-inference --example validate_canonical_artifact \
//!   --features candle --release -- --model /absolute/path/model.gguf [--max-tokens 8]

use arc_inference::cached_integer_model::{
    CANONICAL_REWARD_INFERENCE_PROFILE, GGUF_INTERLEAVED_ROPE_I8_INFERENCE_PROFILE,
    load_cached_model_canonical_i8, load_cached_model_canonical_i8_interleaved_rope,
};
use arc_inference::model_artifact::ModelArtifactCommitment;
use serde_json::json;
use std::time::Instant;

const DEFAULT_MAX_TOKENS: u32 = 32;
const MAX_ALLOWED_TOKENS: u32 = 32;
const GENERATION_SEMANTICS_ID: &str = "arc.whole-model-generation.v2";
const CASES: &[(&str, &str, &[&str])] = &[
    (
        "paris",
        "What is the capital of France? Answer with one word.",
        &["paris"],
    ),
    (
        "arithmetic",
        "What is 2 + 2? Answer with just the number.",
        &["4", "four"],
    ),
    ("greeting", "Say hello in one short sentence.", &["hello"]),
];

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let mut model_path: Option<String> = None;
    let mut max_tokens = DEFAULT_MAX_TOKENS;
    let mut interleaved_rope = false;
    let mut reference_minimal_case = false;
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
            "--interleaved-rope" => interleaved_rope = true,
            "--reference-minimal-case" => reference_minimal_case = true,
            "-h" | "--help" => {
                println!(
                    "usage: {} --model PATH [--max-tokens 1..32] [--interleaved-rope --reference-minimal-case]",
                    args[0]
                );
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
    let profile = if interleaved_rope {
        GGUF_INTERLEAVED_ROPE_I8_INFERENCE_PROFILE
    } else {
        CANONICAL_REWARD_INFERENCE_PROFILE
    };
    eprintln!("phase=model_load_start profile={profile}");
    let mut model = (if interleaved_rope {
        load_cached_model_canonical_i8_interleaved_rope(&model_path)
    } else {
        load_cached_model_canonical_i8(&model_path)
    })
    .unwrap_or_else(|error| {
        eprintln!("canonical INT8 load failed: {error}");
        std::process::exit(1);
    });
    eprintln!(
        "phase=model_load_done layers={} memory_bytes={}",
        model.config.n_layers,
        model.memory_bytes()
    );
    if !model.has_all_transformer_layers() || !model.has_canonical_i8_profile() {
        eprintln!("loaded model is not a complete canonical INT8 model");
        std::process::exit(1);
    }
    model.enforce_canonical_i8_profile();
    if !model.has_canonical_i8_profile() {
        eprintln!("canonical INT8 profile enforcement failed");
        std::process::exit(1);
    }

    let run_cases: Vec<(&str, &str, &[&str])> = if reference_minimal_case {
        vec![(
            "reference_minimal_paris",
            "What is the capital of France?",
            &["paris"],
        )]
    } else {
        CASES.to_vec()
    };
    let mut cases = Vec::with_capacity(run_cases.len());
    let mut determinism_gate_passed = true;
    let mut reported_hashes_match = true;
    let mut quality_gate_passed = true;
    for (case_name, user_prompt, expected_answers) in run_cases {
        // This reference diagnostic deliberately uses minimal Llama-2
        // instruction syntax and injects no uncommitted system message.
        let templated = if reference_minimal_case {
            format!("[INST] {user_prompt} [/INST]")
        } else {
            arc_inference::cached_integer_model::CachedIntegerModel::apply_llama2_chat_template(
                user_prompt,
            )
        };
        let prompt_tokens = model.encode(&templated);
        let runs_per_case = if reference_minimal_case { 1 } else { 2 };
        let mut runs = Vec::with_capacity(runs_per_case);
        let mut first_tokens = None;
        let mut case_hashes_match = true;
        let mut case_reported_hashes_match = true;
        for run_index in 0..runs_per_case {
            eprintln!("phase=generation_start case={case_name} run={run_index}");
            let start = Instant::now();
            let (tokens, output_hash) = model
                .try_generate_v2(&prompt_tokens, max_tokens, &model.config.eos_tokens)
                .unwrap_or_else(|error| {
                    eprintln!("canonical generation failed: {error}");
                    std::process::exit(1);
                });
            if tokens.is_empty() {
                eprintln!("canonical generation returned zero output tokens");
                std::process::exit(1);
            }
            let elapsed_ms = start.elapsed().as_millis();
            let token_bytes: Vec<u8> = tokens
                .iter()
                .flat_map(|token| token.to_le_bytes())
                .collect();
            let token_bytes_hash = blake3::hash(&token_bytes);
            let reported_hash_matches = output_hash.0 == *token_bytes_hash.as_bytes();
            case_reported_hashes_match = case_reported_hashes_match && reported_hash_matches;
            if let Some(previous) = &first_tokens {
                case_hashes_match = case_hashes_match && previous == &tokens;
            } else {
                first_tokens = Some(tokens.clone());
            }
            let decoded = model.decode_v2(&tokens);
            eprintln!(
                "phase=generation_done case={case_name} run={run_index} tokens={} elapsed_ms={elapsed_ms}",
                tokens.len()
            );
            runs.push(json!({
                "tokens": tokens,
                "decoded": decoded,
                "token_count": tokens.len(),
                "token_bytes_hash": format!("0x{}", hex::encode(token_bytes_hash.as_bytes())),
                "output_hash": format!("0x{}", hex::encode(output_hash.0)),
                "reported_hash_matches_token_bytes": reported_hash_matches,
                "elapsed_ms": elapsed_ms,
            }));
        }
        let decoded = runs[0]["decoded"]
            .as_str()
            .unwrap_or_default()
            .to_ascii_lowercase();
        let answer_match = expected_answers
            .iter()
            .any(|answer| decoded.contains(answer));
        let case_deterministic = case_hashes_match && case_reported_hashes_match;
        determinism_gate_passed = determinism_gate_passed && case_deterministic;
        reported_hashes_match = reported_hashes_match && case_reported_hashes_match;
        quality_gate_passed = quality_gate_passed && answer_match;
        cases.push(json!({
            "name": case_name,
            "prompt": user_prompt,
            "expected_answers": expected_answers,
            "answer_match": answer_match,
            "deterministic": case_deterministic,
            "runs": runs,
        }));
    }

    let gate_passed = determinism_gate_passed && quality_gate_passed && reported_hashes_match;
    let report = json!({
        "artifact_path": model_path,
        "artifact_size_bytes": artifact.size_bytes(),
        "artifact_blake3": format!("0x{}", hex::encode(artifact_id.0)),
        "profile": profile,
        "model_arithmetic_profile": model.arithmetic_profile(),
        "canonical_execution_profile": model.canonical_execution_profile(),
        "generation_semantics_id": GENERATION_SEMANTICS_ID,
        "prompt_format": if reference_minimal_case { "arc.llama2.minimal-inst.v1" } else { "legacy-harness-llama2-system-v1" },
        "tokenizer": if reference_minimal_case { "arc.llama-gguf-greedy-reference-vectors.v1" } else { "legacy-generic-encode" },
        "max_tokens": max_tokens,
        "cases": cases,
        "determinism_gate_passed": determinism_gate_passed,
        "quality_gate_passed": quality_gate_passed,
        "reported_hashes_match_token_bytes": reported_hashes_match,
        "gate_passed": gate_passed,
    });

    println!(
        "{}",
        serde_json::to_string_pretty(&report).expect("JSON serialization")
    );
    if !gate_passed {
        std::process::exit(1);
    }
}
