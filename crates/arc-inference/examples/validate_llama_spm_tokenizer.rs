//! Header-only reference validation for the versioned LLaMA GGUF SPM profile.

use arc_inference::llama_spm_tokenizer::{
    GGUF_LLAMA_SPM_TOKENIZER_PROFILE_V1, LlamaGgufSpmTokenizer,
};
use serde_json::json;
use std::process::Command;

const VECTORS: &[&str] = &[
    "[INST] What is the capital of France? [/INST]",
    "[INST] What is 2 + 2? [/INST]",
    "[INST] Say hello in one short sentence. [/INST]",
    "[INST] Reply with exactly: café 世界. [/INST]",
    "[INST] Name the color of a ripe banana in one word. [/INST]",
    "[INST] Continue this sequence with four more words: one, two, three, [/INST]",
    "  leading and   repeated spaces\twith a newline\n",
    "Punctuation!? (x/y): 1,234.50%",
    "Unicode: café — 世界 — naïve",
    "digits 00042 + 17 = 59",
    "<s> special </s>",
];

fn parse_ids(output: &[u8]) -> Result<Vec<u32>, String> {
    let text = std::str::from_utf8(output).map_err(|error| format!("reference UTF-8: {error}"))?;
    let start = text
        .find('[')
        .ok_or_else(|| format!("no token list in {text:?}"))?;
    let end = text[start..]
        .find(']')
        .map(|offset| start + offset)
        .ok_or_else(|| format!("unterminated token list in {text:?}"))?;
    let payload = &text[start + 1..end];
    if payload.trim().is_empty() {
        return Ok(Vec::new());
    }
    payload
        .split(',')
        .map(|part| {
            part.trim()
                .parse::<u32>()
                .map_err(|error| error.to_string())
        })
        .collect()
}

fn reference_ids(binary: &str, model: &str, text: &str) -> Result<Vec<u32>, String> {
    let output = Command::new(binary)
        .args(["-m", model, "-p", text, "--ids"])
        .output()
        .map_err(|error| format!("launch llama-tokenize: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "llama-tokenize status={} stderr={}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    parse_ids(&output.stdout)
}

fn main() {
    let mut model = None;
    let mut reference_bin = None;
    let mut output_jsonl = None;
    let mut corpus = None;
    let mut args = std::env::args().skip(1);
    while let Some(flag) = args.next() {
        match flag.as_str() {
            "--model" => model = args.next(),
            "--reference-bin" => reference_bin = args.next(),
            "--output-jsonl" => output_jsonl = args.next(),
            // One JSON object per line with a "text" field, e.g.
            // scripts/arc_conformance/data/tokenizer_corpus.jsonl (M3).
            "--corpus" => corpus = args.next(),
            "--help" | "-h" => {
                println!(
                    "usage: validate_llama_spm_tokenizer --model GGUF --reference-bin LLAMA_TOKENIZE [--corpus JSONL] [--output-jsonl PATH]"
                );
                return;
            }
            _ => panic!("unknown argument: {flag}"),
        }
    }
    let mut texts: Vec<String> = VECTORS.iter().map(|text| (*text).to_string()).collect();
    if let Some(path) = corpus {
        let body = std::fs::read_to_string(&path).expect("read --corpus");
        for line in body.lines().filter(|line| !line.trim().is_empty()) {
            let case: serde_json::Value = serde_json::from_str(line).expect("corpus line is JSON");
            texts.push(
                case["text"]
                    .as_str()
                    .expect("corpus case has a text")
                    .to_string(),
            );
        }
    }
    let model = model.expect("--model is required");
    let reference_bin = reference_bin.expect("--reference-bin is required");
    let tokenizer =
        LlamaGgufSpmTokenizer::from_gguf(&model).expect("load header-only LLaMA SPM tokenizer");
    assert_eq!(tokenizer.profile(), GGUF_LLAMA_SPM_TOKENIZER_PROFILE_V1);
    let mut records = Vec::new();
    for text in &texts {
        let reference =
            reference_ids(&reference_bin, &model, text).expect("reference tokenization");
        let arc = tokenizer.encode_prompt(text).expect("ARC SPM tokenization");
        let matches = arc == reference;
        records.push(
            json!({ "text": text, "reference_ids": reference, "arc_ids": arc, "matches": matches }),
        );
    }
    let result = json!({
        "profile": GGUF_LLAMA_SPM_TOKENIZER_PROFILE_V1,
        "model": model,
        "vectors": records,
        "all_match": records.iter().all(|record| record["matches"] == true),
    });
    let rendered = serde_json::to_string_pretty(&result).expect("serialize result");
    if let Some(path) = output_jsonl {
        std::fs::write(path, format!("{rendered}\n")).expect("write JSON result");
    } else {
        println!("{rendered}");
    }
    if result["all_match"] != true {
        std::process::exit(1);
    }
}
