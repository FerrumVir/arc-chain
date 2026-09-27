//! Sequential same-GGUF quality comparison for the corrected ARC Llama profile.
//!
//! It runs the pinned local llama.cpp reference process and ARC in separate
//! processes/scopes for every case, writes one JSON object per completed case,
//! and uses a separately committed pure-greedy ARC diagnostic mode so the
//! source and ARC sampling semantics are directly comparable.

use arc_inference::cached_integer_model::{
    GGUF_INTERLEAVED_ROPE_I8_INFERENCE_PROFILE, GGUF_LLAMA_GREEDY_GENERATION_SEMANTICS_V1,
    load_cached_model_canonical_i8_interleaved_rope,
};
use arc_inference::llama_spm_tokenizer::{
    GGUF_LLAMA_SPM_TOKENIZER_PROFILE_V1, LlamaGgufSpmTokenizer,
};
use arc_inference::model_artifact::ModelArtifactCommitment;
use serde_json::{Value, json};
use std::fs::OpenOptions;
use std::io::Write;
use std::process::Command;
use std::time::Instant;

const MAX_TOKENS: u32 = 12;
const PROMPT_FORMAT: &str = "arc.llama2.minimal-inst.v1";
const TOKENIZER_EVIDENCE: &str = GGUF_LLAMA_SPM_TOKENIZER_PROFILE_V1;
const GENERATION_SEMANTICS: &str = GGUF_LLAMA_GREEDY_GENERATION_SEMANTICS_V1;
const CASES: &[(&str, &str, &[&str])] = &[
    (
        "paris_control",
        "What is the capital of France?",
        &["paris"],
    ),
    ("arithmetic", "What is 2 + 2?", &["4", "four"]),
    (
        "greeting",
        "Say hello in one short sentence.",
        &["hello", "hi"],
    ),
    (
        "unicode",
        "Reply with exactly: café 世界.",
        &["café", "世界"],
    ),
    (
        "short_instruction",
        "Name the color of a ripe banana in one word.",
        &["yellow"],
    ),
    (
        "continuation",
        "Continue this sequence with four more words: one, two, three,",
        &["four"],
    ),
];

fn reference_completion(
    reference_bin: &str,
    model: &str,
    prompt: &str,
) -> Result<(String, u128), String> {
    let start = Instant::now();
    let output = Command::new(reference_bin)
        .args([
            "-m",
            model,
            "-ngl",
            "0",
            "-n",
            &MAX_TOKENS.to_string(),
            prompt,
        ])
        .output()
        .map_err(|error| format!("launch llama-simple: {error}"))?;
    let elapsed_ms = start.elapsed().as_millis();
    if !output.status.success() {
        return Err(format!(
            "llama-simple failed status={} stderr={}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    let stdout = String::from_utf8(output.stdout)
        .map_err(|error| format!("reference stdout UTF-8: {error}"))?;
    let prefix = format!("<s> {prompt}");
    let completion = stdout
        .strip_prefix(&prefix)
        .ok_or_else(|| format!("reference stdout did not start with expected prompt: {stdout:?}"))?
        .trim_end_matches('\n')
        .to_string();
    Ok((completion, elapsed_ms))
}

fn case_record(
    case_name: &str,
    user_prompt: &str,
    expected_terms: &[&str],
    model_path: &str,
    artifact_blake3: &str,
    reference_bin: &str,
    tokenizer: &LlamaGgufSpmTokenizer,
) -> Value {
    let prompt = format!("[INST] {user_prompt} [/INST]");
    let reference_prompt_ids = match tokenizer.encode_prompt(&prompt) {
        Ok(ids) => ids,
        Err(error) => return json!({ "case": case_name, "error": error.to_string() }),
    };
    let (reference_text, reference_wall_ms) =
        match reference_completion(reference_bin, model_path, &prompt) {
            Ok(result) => result,
            Err(error) => return json!({ "case": case_name, "error": error }),
        };
    // This is only a rendering check. Actual sampled source IDs are captured
    // by `capture_llama_simple_trace`; they must never be reconstructed from
    // completion text because SPM has non-unique leading-space segmentations.
    let reference_completion_retokenized_ids = tokenizer
        .encode_prompt(&reference_text)
        .map(|mut ids| {
            ids.remove(0);
            ids
        })
        .unwrap_or_default();

    // The model is allocated only after the reference child has exited. This
    // scope ends before the next reference case starts.
    let arc = (|| -> Result<Value, String> {
        let load_start = Instant::now();
        let model = load_cached_model_canonical_i8_interleaved_rope(model_path)
            .map_err(|error| format!("ARC load: {error}"))?;
        let arc_load_ms = load_start.elapsed().as_millis();
        if model.canonical_execution_profile() != Some(GGUF_INTERLEAVED_ROPE_I8_INFERENCE_PROFILE) {
            return Err("ARC model did not retain the requested canonical profile".into());
        }
        let encoded_arc_prompt_ids = tokenizer
            .encode_prompt(&prompt)
            .map_err(|error| format!("ARC SPM prompt tokenizer: {error}"))?;
        let tokenizer_match = encoded_arc_prompt_ids == reference_prompt_ids;
        // Always feed the source's exact token IDs into both generation
        // comparisons. This isolates arithmetic/generation quality even when
        // the separately recorded text tokenizer gate fails.
        let arc_prompt_ids = reference_prompt_ids.clone();
        let generation_start = Instant::now();
        let (tokens, output_hash) = model
            .try_generate_v2_greedy(&arc_prompt_ids[1..], MAX_TOKENS, &model.config.eos_tokens)
            .map_err(|error| format!("ARC generation: {error}"))?;
        let arc_generation_ms = generation_start.elapsed().as_millis();
        let token_bytes: Vec<u8> = tokens
            .iter()
            .flat_map(|token| token.to_le_bytes())
            .collect();
        let hash_matches = output_hash.0 == *blake3::hash(&token_bytes).as_bytes();
        let retokenized_completion_ids_match = tokens == reference_completion_retokenized_ids;
        let arc_decoded = model.decode_v2_content(&tokens);
        let semantic_terms_match = expected_terms.iter().any(|term| {
            arc_decoded
                .to_ascii_lowercase()
                .contains(&term.to_ascii_lowercase())
                && reference_text
                    .to_ascii_lowercase()
                    .contains(&term.to_ascii_lowercase())
        });
        Ok(json!({
            "arc_text_tokenizer_ids": encoded_arc_prompt_ids,
            "arc_generation_prompt_ids": arc_prompt_ids,
            "used_reference_prompt_ids": true,
            "arc_tokens": tokens,
            "arc_decoded": arc_decoded,
            "arc_load_ms": arc_load_ms,
            "arc_generation_ms": arc_generation_ms,
            "arc_output_hash": format!("0x{}", hex::encode(output_hash.0)),
            "arc_output_hash_matches_tokens": hash_matches,
            "tokenizer_match": tokenizer_match,
            "retokenized_completion_ids_match": retokenized_completion_ids_match,
            "sampled_id_reference_available_in_this_record": false,
            "semantic_terms_match": semantic_terms_match,
        }))
    })();

    match arc {
        Ok(arc) => json!({
            "case": case_name,
            "user_prompt": user_prompt,
            "expected_terms": expected_terms,
            "minimal_prompt": prompt,
            "artifact_blake3": artifact_blake3,
            "profile": GGUF_INTERLEAVED_ROPE_I8_INFERENCE_PROFILE,
            "prompt_format": PROMPT_FORMAT,
            "tokenizer_evidence": TOKENIZER_EVIDENCE,
            "generation_semantics": GENERATION_SEMANTICS,
            "max_tokens": MAX_TOKENS,
            "reference_prompt_ids": reference_prompt_ids,
            "reference_completion_retokenized_ids": reference_completion_retokenized_ids,
            "reference_completion": reference_text,
            "reference_wall_ms": reference_wall_ms,
            "arc": arc,
        }),
        Err(error) => json!({
            "case": case_name,
            "user_prompt": user_prompt,
            "artifact_blake3": artifact_blake3,
            "error": error,
        }),
    }
}

fn main() {
    let mut model_path = None;
    let mut reference_bin = None;
    let mut output_path = None;
    let mut requested_case = None;
    let args: Vec<String> = std::env::args().collect();
    let mut index = 1;
    while index < args.len() {
        let value = match args[index].as_str() {
            "--model" => &mut model_path,
            "--reference-bin" => &mut reference_bin,
            "--output-jsonl" => &mut output_path,
            "--case" => &mut requested_case,
            "--help" | "-h" => {
                println!(
                    "usage: {} --model GGUF --reference-bin LLAMA_SIMPLE --output-jsonl PATH [--case CASE]",
                    args[0]
                );
                return;
            }
            other => {
                eprintln!("unknown argument: {other}");
                std::process::exit(2);
            }
        };
        index += 1;
        *value = args.get(index).cloned();
        if value.is_none() {
            eprintln!("missing value for {}", args[index - 1]);
            std::process::exit(2);
        }
        index += 1;
    }
    let (Some(model_path), Some(reference_bin), Some(output_path)) =
        (model_path, reference_bin, output_path)
    else {
        eprintln!("--model, --reference-bin, and --output-jsonl are required");
        std::process::exit(2);
    };
    let artifact = ModelArtifactCommitment::from_path(&model_path).unwrap_or_else(|error| {
        eprintln!("artifact identity failed: {error}");
        std::process::exit(1);
    });
    let artifact_blake3 = format!("0x{}", hex::encode(artifact.model_id().0));
    let tokenizer = LlamaGgufSpmTokenizer::from_gguf(&model_path).unwrap_or_else(|error| {
        eprintln!("load versioned GGUF SPM tokenizer: {error}");
        std::process::exit(1);
    });
    let mut output = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&output_path)
        .unwrap_or_else(|error| {
            eprintln!("open JSONL output: {error}");
            std::process::exit(1);
        });
    for (case_name, user_prompt, expected_terms) in CASES {
        if requested_case
            .as_deref()
            .is_some_and(|requested| requested != *case_name)
        {
            continue;
        }
        eprintln!("phase=case_start case={case_name}");
        let record = case_record(
            case_name,
            user_prompt,
            expected_terms,
            &model_path,
            &artifact_blake3,
            &reference_bin,
            &tokenizer,
        );
        serde_json::to_writer(&mut output, &record).expect("serialize case JSON");
        output.write_all(b"\n").expect("append JSONL newline");
        output.flush().expect("flush completed case");
        eprintln!(
            "phase=case_done case={case_name} success={}",
            record.get("error").is_none()
        );
    }
}
