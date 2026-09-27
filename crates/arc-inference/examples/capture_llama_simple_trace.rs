//! Capture direct greedy sampler traces from the locally patched pinned llama.cpp.
//!
//! The reference patch emits `ARC_REFERENCE_TRACE_JSON:` on stderr. This
//! program records that source-provided token trace without re-tokenizing
//! rendered completion text.

use arc_inference::llama_spm_tokenizer::{
    GGUF_LLAMA_SPM_TOKENIZER_PROFILE_V1, LlamaGgufSpmTokenizer,
};
use serde_json::{Value, json};
use std::fs::OpenOptions;
use std::io::Write;
use std::process::Command;
use std::time::Instant;

const MAX_TOKENS: u32 = 12;
const CASES: &[(&str, &str)] = &[
    ("paris_control", "What is the capital of France?"),
    ("arithmetic", "What is 2 + 2?"),
    ("greeting", "Say hello in one short sentence."),
    ("unicode", "Reply with exactly: café 世界."),
    (
        "short_instruction",
        "Name the color of a ripe banana in one word.",
    ),
    (
        "continuation",
        "Continue this sequence with four more words: one, two, three,",
    ),
];

fn trace(stderr: &[u8]) -> Result<Value, String> {
    let stderr = std::str::from_utf8(stderr).map_err(|error| error.to_string())?;
    let marker = "ARC_REFERENCE_TRACE_JSON:";
    let line = stderr
        .lines()
        .find_map(|line| line.strip_prefix(marker))
        .ok_or_else(|| format!("instrumented reference did not emit {marker:?}"))?;
    serde_json::from_str(line).map_err(|error| format!("parse reference trace JSON: {error}"))
}

fn main() {
    let mut model = None;
    let mut reference_bin = None;
    let mut output = None;
    let mut args = std::env::args().skip(1);
    while let Some(flag) = args.next() {
        match flag.as_str() {
            "--model" => model = args.next(),
            "--reference-bin" => reference_bin = args.next(),
            "--output-jsonl" => output = args.next(),
            "--help" | "-h" => {
                println!(
                    "usage: capture_llama_simple_trace --model GGUF --reference-bin LLAMA_SIMPLE --output-jsonl PATH"
                );
                return;
            }
            _ => panic!("unknown argument: {flag}"),
        }
    }
    let model = model.expect("--model required");
    let reference_bin = reference_bin.expect("--reference-bin required");
    let output = output.expect("--output-jsonl required");
    let tokenizer = LlamaGgufSpmTokenizer::from_gguf(&model).expect("load header-only tokenizer");
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(output)
        .expect("open output");
    for (case_name, user_prompt) in CASES {
        let prompt = format!("[INST] {user_prompt} [/INST]");
        let start = Instant::now();
        let process = Command::new(&reference_bin)
            .args([
                "-m",
                &model,
                "-ngl",
                "0",
                "-n",
                &MAX_TOKENS.to_string(),
                &prompt,
            ])
            .output()
            .expect("launch llama-simple");
        if !process.status.success() {
            panic!(
                "llama-simple failed: {}",
                String::from_utf8_lossy(&process.stderr)
            );
        }
        let trace = trace(&process.stderr).expect("direct reference trace");
        let stdout = String::from_utf8(process.stdout).expect("reference stdout UTF-8");
        let prefix = format!("<s> {prompt}");
        let completion = stdout
            .strip_prefix(&prefix)
            .expect("source stdout prompt prefix")
            .trim_end_matches('\n')
            .to_string();
        let text_ids = tokenizer
            .encode_prompt(&prompt)
            .expect("ARC SPM prompt encoding");
        let trace_ids: Vec<u32> =
            serde_json::from_value(trace["prompt_token_ids"].clone()).expect("trace prompt IDs");
        let record = json!({
            "case": case_name,
            "minimal_prompt": prompt,
            "reference_sampler": "llama.cpp greedy",
            "reference_max_tokens": MAX_TOKENS,
            "reference_completion_text": completion,
            "reference_process_wall_ms": start.elapsed().as_millis(),
            "reference_trace": trace,
            "arc_tokenizer_profile": GGUF_LLAMA_SPM_TOKENIZER_PROFILE_V1,
            "arc_text_prompt_ids": text_ids,
            "tokenizer_match": text_ids == trace_ids,
        });
        serde_json::to_writer(&mut file, &record).expect("write JSONL record");
        file.write_all(b"\n").expect("newline");
        file.flush().expect("flush");
        eprintln!("phase=reference_done case={case_name}");
    }
}
