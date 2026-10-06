//! `arc-modern` commands for the GPU path. The binary dispatches here, so its
//! own commands are unchanged:
//!
//! * `golden ... --gpu`: the golden run on a GPU; the same `arc.modern-run.v1`
//!   document (and matrix digest) as the CPU run, plus a `gpu` section;
//! * `gpu-info`: every adapter wgpu can see on this machine;
//! * `gpu-check`: the Proof Kit's GPU mode. Self-test of every kernel against
//!   the CPU operators, the golden prompts, the digest match against a pinned
//!   CPU golden, the first divergent forward and layer/op on a mismatch, the
//!   adapter (vendor, device, backend, driver) and prefill/decode tok/s.

use std::path::{Path, PathBuf};
use std::time::Instant;

use arc_gpu::modern::{self as gpu, EngineOptions, GpuEngine, OpLab};
use serde_json::{Value, json};

use super::{Divergence, engine_from, generate_gpu, gpu_error, kat, localize};
use crate::modern::arith::Selection;
use crate::modern::bpe::ByteLevelBpe;
use crate::modern::chat::{ChatPrompt, render};
use crate::modern::model::{GenerationRequest, ModernConfig};
use crate::modern::package::{self, PackageDigest};
use crate::modern::{ModernError, PROFILE, hex_lower};

/// Usage of the GPU commands (printed after the binary's own usage).
pub const USAGE: &str =
    "GPU (portable WGSL kernels on Vulkan, DX12 or Metal; same digests as the CPU):
  golden    --package PKG --cases CASES.json --out RUN.json --gpu
            [--tokenizer tokenizer.json] [--gpu-adapter N|NAME] [--gpu-batch N]
            [--gpu-max-positions N]
  gpu-info  [--json-out OUT.json]
  gpu-check --package PKG --tokenizer tokenizer.json --cases CASES.json
            --golden GOLDEN.json --out RESULT.json [--run-out RUN.json]
            [--gpu-adapter N|NAME] [--gpu-batch N] [--gpu-max-positions N]
            [--self-test-rounds N] [--trace-forwards N]
  ARC_GPU_ADAPTER selects the adapter too; WGPU_BACKEND=vulkan|dx12|metal
  restricts the backends.";

const DEFAULT_TODAY: &str = "06 October 2026";
const SOFTWARE_LABEL: &str =
    "software rasterizer: proves exactness; its timings say nothing about GPU speed";
const HARDWARE_LABEL: &str =
    "measured on this GPU: one stream, token selection on the CPU, logits read back every forward";

struct Args<'a> {
    items: &'a [String],
}

impl Args<'_> {
    fn value(&self, name: &str) -> Option<&str> {
        self.items
            .iter()
            .position(|a| a == name)
            .and_then(|i| self.items.get(i + 1))
            .map(String::as_str)
    }

    fn required(&self, name: &str) -> Result<&str, ModernError> {
        self.value(name)
            .ok_or_else(|| ModernError::Invalid(format!("missing {name}\n\n{USAGE}")))
    }

    fn path(&self, name: &str) -> Result<PathBuf, ModernError> {
        self.required(name).map(PathBuf::from)
    }

    fn number(&self, name: &str, default: usize) -> Result<usize, ModernError> {
        match self.value(name) {
            None => Ok(default),
            Some(text) => text
                .parse()
                .map_err(|_| ModernError::Invalid(format!("{name} must be a number"))),
        }
    }

    fn engine_options(&self) -> Result<EngineOptions, ModernError> {
        let max_positions = match self.value("--gpu-max-positions") {
            None => None,
            Some(_) => Some(self.number("--gpu-max-positions", 0)?),
        };
        Ok(EngineOptions {
            adapter: self
                .value("--gpu-adapter")
                .map(str::to_string)
                .or_else(|| std::env::var("ARC_GPU_ADAPTER").ok()),
            max_positions,
            batch: Some(self.number("--gpu-batch", 1)?),
        })
    }
}

fn read_json(path: &Path) -> Result<Value, ModernError> {
    let bytes =
        std::fs::read(path).map_err(|e| ModernError::Io(format!("{}: {e}", path.display())))?;
    serde_json::from_slice(&bytes)
        .map_err(|e| ModernError::Invalid(format!("{}: {e}", path.display())))
}

fn write_json(path: &Path, value: &Value) -> Result<(), ModernError> {
    let text = serde_json::to_string_pretty(value)
        .map_err(|e| ModernError::Invalid(format!("JSON: {e}")))?;
    std::fs::write(path, text + "\n")
        .map_err(|e| ModernError::Io(format!("{}: {e}", path.display())))
}

fn load_tokenizer(path: Option<&str>) -> Result<Option<ByteLevelBpe>, ModernError> {
    match path {
        None => Ok(None),
        Some(path) => {
            let bytes = std::fs::read(path).map_err(|e| ModernError::Io(format!("{path}: {e}")))?;
            Ok(Some(ByteLevelBpe::from_json(&bytes)?))
        }
    }
}

fn ids_from(value: Option<&Value>) -> Result<Vec<u32>, ModernError> {
    match value {
        None => Ok(Vec::new()),
        Some(v) => v
            .as_array()
            .ok_or_else(|| ModernError::Invalid("expected a list of token ids".into()))?
            .iter()
            .map(|x| {
                x.as_u64()
                    .and_then(|id| u32::try_from(id).ok())
                    .ok_or_else(|| ModernError::Invalid("token ids must be u32".into()))
            })
            .collect(),
    }
}

/// One finished case: the CPU run's record and digest entry, and timings.
struct CaseRun {
    record: Value,
    entry: Value,
    prompt_tokens: usize,
    prefill_seconds: f64,
    decode_forwards: usize,
    decode_seconds: f64,
}

/// A golden case on the GPU, rendered, tokenised and recorded exactly like
/// the CPU command's `run_case`.
fn run_case(
    engine: &mut GpuEngine,
    config: &ModernConfig,
    case: &Value,
    tokenizer: Option<&ByteLevelBpe>,
) -> Result<CaseRun, ModernError> {
    let id = case
        .get("id")
        .and_then(Value::as_str)
        .ok_or_else(|| ModernError::Invalid("every case needs an id".into()))?
        .to_string();
    let mut rendered = Value::Null;
    let prompt = if let Some(user) = case.get("user").and_then(Value::as_str) {
        let tokenizer = tokenizer
            .ok_or_else(|| ModernError::Invalid(format!("case {id} is text; pass --tokenizer")))?;
        let text = render(&ChatPrompt {
            system: case.get("system").and_then(Value::as_str),
            user,
            thinking: case
                .get("thinking")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            today: case
                .get("today")
                .and_then(Value::as_str)
                .unwrap_or(DEFAULT_TODAY),
        });
        let ids = tokenizer.encode(&text)?;
        rendered = Value::from(text);
        ids
    } else {
        ids_from(case.get("prompt_tokens"))?
    };
    let max_tokens = case
        .get("max_tokens")
        .and_then(Value::as_u64)
        .and_then(|v| usize::try_from(v).ok())
        .ok_or_else(|| ModernError::Invalid(format!("case {id} needs max_tokens")))?;
    let eos = ids_from(case.get("eos"))?;
    let selection = Selection::parse(
        case.get("selection")
            .and_then(Value::as_str)
            .unwrap_or("rp64-argmax"),
    )?;
    let out = generate_gpu(
        engine,
        config,
        &GenerationRequest {
            prompt: &prompt,
            max_tokens,
            eos: &eos,
            selection,
        },
    )?;
    let output_hash = hex_lower(&out.output_hash);
    let logits_digest = hex_lower(&out.logits_digest);
    let hashes: Vec<String> = out.logits_hashes.iter().map(|h| hex_lower(h)).collect();
    let text = tokenizer.map(|t| t.decode(&out.tokens, true));
    let decode_tok_s = if out.decode_seconds > 0.0 {
        out.decode_forwards as f64 / out.decode_seconds
    } else {
        0.0
    };
    let record = json!({
        "id": id,
        "prompt_tokens": prompt,
        "max_tokens": max_tokens,
        "eos": eos,
        "selection": selection.name(),
        "tokens": out.tokens,
        "output_hash": output_hash,
        "logits_hashes": hashes,
        "logits_digest": logits_digest,
        "rendered_prompt": rendered,
        "text": text,
        "prefill_seconds": out.prefill_seconds,
        "decode_seconds": out.decode_seconds,
        "decode_forwards": out.decode_forwards,
        "decode_tok_s": decode_tok_s,
    });
    let entry = json!({
        "id": record["id"],
        "logits_digest": record["logits_digest"],
        "output_hash": record["output_hash"],
        "tokens": record["tokens"],
    });
    Ok(CaseRun {
        record,
        entry,
        prompt_tokens: prompt.len(),
        prefill_seconds: out.prefill_seconds,
        decode_forwards: out.decode_forwards,
        decode_seconds: out.decode_seconds,
    })
}

fn rate(count: f64, seconds: f64) -> f64 {
    if seconds > 0.0 { count / seconds } else { 0.0 }
}

/// Facts about the engine every GPU document carries.
fn gpu_section(engine: &GpuEngine, upload_seconds: f64) -> Value {
    let report = engine.report();
    json!({
        "adapter": report,
        "dot_path": engine.dot_path(),
        "batch": engine.batch(),
        "kv_capacity": engine.capacity(),
        "dispatches_per_forward": engine.dispatches(),
        "uploaded_bytes": engine.uploaded_bytes(),
        "upload_seconds": upload_seconds,
        "speed_label": if report.software { SOFTWARE_LABEL } else { HARDWARE_LABEL },
    })
}

/// The run document (`arc.modern-run.v1`) from finished cases.
fn run_document(
    digest: &PackageDigest,
    cases: &[CaseRun],
    load_seconds: f64,
    gpu_facts: Value,
) -> Result<Value, ModernError> {
    let entries: Vec<Value> = cases.iter().map(|c| c.entry.clone()).collect();
    let matrix_text = crate::model_package::canonical_json(&Value::from(entries))
        .map_err(|e| ModernError::Invalid(format!("canonical JSON: {e}")))?;
    let matrix_digest = blake3::hash(matrix_text.as_bytes()).to_hex().to_string();
    let all_argmax = cases
        .iter()
        .all(|c| c.record["selection"].as_str() == Some("argmax"));
    let generation = if all_argmax {
        Selection::Argmax.semantics()
    } else {
        Selection::Rp64Argmax.semantics()
    };
    let prompt_total: usize = cases.iter().map(|c| c.prompt_tokens).sum();
    let prefill_total: f64 = cases.iter().map(|c| c.prefill_seconds).sum();
    let forwards_total: usize = cases.iter().map(|c| c.decode_forwards).sum();
    let decode_total: f64 = cases.iter().map(|c| c.decode_seconds).sum();
    Ok(json!({
        "schema": "arc.modern-run.v1",
        "package": {"sha256": digest.sha256, "blake3": digest.blake3, "bytes": digest.bytes},
        "profile": PROFILE,
        "generation": generation,
        "kernel": "gpu-wgsl",
        "platform": {
            "os": std::env::consts::OS,
            "arch": std::env::consts::ARCH,
        },
        "gpu": gpu_facts,
        "cases": cases.iter().map(|c| c.record.clone()).collect::<Vec<_>>(),
        "matrix_digest": matrix_digest,
        "timing": {
            "load_seconds": load_seconds,
            "prompt_tokens": prompt_total,
            "prefill_seconds": prefill_total,
            "prefill_tok_s": rate(prompt_total as f64, prefill_total),
            "decode_forwards": forwards_total,
            "decode_seconds": decode_total,
            "decode_tok_s": rate(forwards_total as f64, decode_total),
        },
    }))
}

fn case_list(cases: &Value) -> Result<&Vec<Value>, ModernError> {
    cases
        .get("cases")
        .and_then(Value::as_array)
        .ok_or_else(|| ModernError::Invalid("cases file has no cases".into()))
}

/// Load the package and upload it to the selected GPU, freeing the host copy.
fn load_engine(
    package_path: &Path,
    options: &EngineOptions,
) -> Result<(GpuEngine, ModernConfig, f64, f64), ModernError> {
    let start = Instant::now();
    let model = package::load_package(package_path)?;
    let load_seconds = start.elapsed().as_secs_f64();
    let config = model.config.clone();
    let start = Instant::now();
    let engine = engine_from(model, options)?;
    let upload_seconds = start.elapsed().as_secs_f64();
    eprintln!(
        "GPU: {} ({}, {} {}), dot: {}, {} dispatches per forward",
        engine.report().name,
        engine.report().backend,
        engine.report().driver,
        engine.report().driver_info,
        engine.dot_path(),
        engine.dispatches()
    );
    Ok((engine, config, load_seconds, upload_seconds))
}

/// `arc-modern golden ... --gpu`.
pub fn golden(items: &[String]) -> Result<(), ModernError> {
    let args = Args { items };
    let options = args.engine_options()?;
    let package_path = args.path("--package")?;
    let digest = package::digest_file(&package_path)?;
    let (mut engine, config, load_seconds, upload_seconds) = load_engine(&package_path, &options)?;
    let tokenizer = load_tokenizer(args.value("--tokenizer"))?;
    let cases = read_json(&args.path("--cases")?)?;
    let mut runs = Vec::new();
    for case in case_list(&cases)? {
        let run = run_case(&mut engine, &config, case, tokenizer.as_ref())?;
        eprintln!(
            "case {}: {} tokens, output_hash {}",
            run.record["id"], run.record["tokens"], run.record["output_hash"]
        );
        runs.push(run);
    }
    let document = run_document(
        &digest,
        &runs,
        load_seconds,
        gpu_section(&engine, upload_seconds),
    )?;
    write_json(&args.path("--out")?, &document)?;
    println!(
        "matrix_digest {}",
        document["matrix_digest"].as_str().unwrap_or_default()
    );
    Ok(())
}

/// `arc-modern gpu-info`.
pub fn info(items: &[String]) -> Result<(), ModernError> {
    let args = Args { items };
    let document = json!({
        "schema": "arc.gpu-adapters.v1",
        "adapters": gpu::list_adapters(),
        "note": "WGPU_BACKEND=vulkan|dx12|metal restricts backends; --gpu-adapter N|NAME or ARC_GPU_ADAPTER selects one",
    });
    if let Some(path) = args.value("--json-out") {
        write_json(Path::new(path), &document)?;
    }
    println!(
        "{}",
        serde_json::to_string_pretty(&document).unwrap_or_default()
    );
    Ok(())
}

/// The forwarded token sequence of a golden case: its prompt, then every
/// generated token except the last (which is never forwarded).
fn forwarded_tokens(case: &Value) -> Result<Vec<u32>, ModernError> {
    let mut tokens = ids_from(case.get("prompt_tokens"))?;
    let generated = ids_from(case.get("tokens"))?;
    tokens.extend(&generated[..generated.len().saturating_sub(1)]);
    Ok(tokens)
}

fn hash_list(case: &Value) -> Vec<String> {
    case.get("logits_hashes")
        .and_then(Value::as_array)
        .map(|list| {
            list.iter()
                .filter_map(|h| h.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

/// The first forward call whose logits hash differs from the golden's, or
/// `None` when the case matches.
fn first_divergent_forward(expected: &Value, actual: &Value) -> Option<usize> {
    let want = hash_list(expected);
    let got = hash_list(actual);
    if let Some(index) = want.iter().zip(&got).position(|(a, b)| a != b) {
        return Some(index);
    }
    (want.len() != got.len() || want.is_empty()).then_some(want.len().min(got.len()))
}

fn layer_of(op: &str) -> Value {
    op.strip_prefix("layer")
        .and_then(|rest| rest.split('.').next())
        .and_then(|n| n.parse::<u64>().ok())
        .map_or(Value::Null, Value::from)
}

fn divergence_json(divergence: Option<&Divergence>, case_id: &str, compared: usize) -> Value {
    match divergence {
        None => json!({"case": case_id, "op": Value::Null, "ops_compared": compared}),
        Some(d) => json!({
            "case": case_id,
            "forward": d.forward,
            "position": d.forward,
            "op": d.op,
            "layer": layer_of(&d.op),
            "ops_compared": compared,
        }),
    }
}

/// `arc-modern gpu-check`: the Proof Kit's GPU mode.
pub fn check(items: &[String]) -> Result<(), ModernError> {
    let args = Args { items };
    let options = args.engine_options()?;
    let out_path = args.path("--out")?;
    let golden_doc = read_json(&args.path("--golden")?)?;
    let golden_cases = case_list(&golden_doc)?.clone();
    let rounds = args.number("--self-test-rounds", 8)?;
    let trace_forwards = args.number("--trace-forwards", 0)?;
    let package_path = args.path("--package")?;
    let cases = read_json(&args.path("--cases")?)?;
    let tokenizer = load_tokenizer(args.value("--tokenizer"))?;

    // 1. Every kernel against the CPU operators on this adapter and driver.
    let self_test = {
        let lab = OpLab::new(options.adapter.as_deref()).map_err(gpu_error)?;
        let start = Instant::now();
        let report = kat::run(&lab, 0x00A2_C5E1, rounds);
        eprintln!(
            "self-test: {} cases, {} refused on both sides, {} mismatches",
            report.cases, report.refusals, report.mismatch_count
        );
        let passed = report.passed();
        json!({
            "rounds": rounds,
            "seconds": start.elapsed().as_secs_f64(),
            "report": report,
            "pass": passed,
        })
    };

    // 2. The golden prompts on the GPU, case by case.
    let digest = package::digest_file(&package_path)?;
    let (mut engine, config, load_seconds, upload_seconds) = load_engine(&package_path, &options)?;
    let mut runs = Vec::new();
    let mut failure: Option<(usize, String)> = None;
    for (index, case) in case_list(&cases)?.iter().enumerate() {
        match run_case(&mut engine, &config, case, tokenizer.as_ref()) {
            Ok(run) => {
                eprintln!(
                    "case {}: {} tokens, logits_digest {}",
                    run.record["id"], run.record["tokens"], run.record["logits_digest"]
                );
                runs.push(run);
            }
            Err(error) => {
                eprintln!("case {index}: GPU run failed: {error}");
                failure = Some((index, error.to_string()));
                break;
            }
        }
    }
    let gpu_facts = gpu_section(&engine, upload_seconds);
    let document = run_document(&digest, &runs, load_seconds, gpu_facts.clone())?;
    if let Some(path) = args.value("--run-out") {
        write_json(Path::new(path), &document)?;
    }

    // 3. Compare with the pinned CPU golden.
    let expected_digest = golden_doc["matrix_digest"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    let actual_digest = document["matrix_digest"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    let digest_match =
        failure.is_none() && !expected_digest.is_empty() && expected_digest == actual_digest;
    let mut per_case = Vec::new();
    let mut first: Option<(usize, usize)> = failure.as_ref().map(|(index, _)| (*index, usize::MAX));
    for (index, expected) in golden_cases.iter().enumerate() {
        let actual = document["cases"].get(index);
        let divergent = match actual {
            Some(actual) => first_divergent_forward(expected, actual),
            None => Some(0),
        };
        if let (Some(forward), None) = (divergent, first)
            && actual.is_some()
        {
            first = Some((index, forward));
        }
        per_case.push(json!({
            "id": expected["id"],
            "match": actual.is_some() && divergent.is_none(),
            "first_divergent_forward": divergent,
        }));
    }

    // 4. Name the first divergent layer/op (traced CPU and GPU replay of the
    //    golden token sequence), and the optional always-on trace check.
    let mut first_divergence = Value::Null;
    let mut trace_check = Value::Null;
    if (!digest_match && first.is_some()) || trace_forwards > 0 {
        let model = package::load_package(&package_path)?;
        if !digest_match && let Some((case_index, forward)) = first {
            let expected = &golden_cases[case_index];
            let tokens = forwarded_tokens(expected)?;
            let depth = if forward == usize::MAX {
                tokens.len()
            } else {
                forward + 1
            };
            let (divergence, compared) = localize(&model, &mut engine, &tokens, depth)?;
            let id = expected["id"].as_str().unwrap_or_default();
            first_divergence = divergence_json(divergence.as_ref(), id, compared);
        }
        if trace_forwards > 0 && !golden_cases.is_empty() {
            let expected = &golden_cases[0];
            let tokens = forwarded_tokens(expected)?;
            let depth = trace_forwards.min(tokens.len());
            let (divergence, compared) = localize(&model, &mut engine, &tokens, depth)?;
            let id = expected["id"].as_str().unwrap_or_default();
            trace_check = json!({
                "forwards": depth,
                "all_equal": divergence.is_none(),
                "detail": divergence_json(divergence.as_ref(), id, compared),
            });
        }
    }
    let trace_ok = trace_check.is_null() || trace_check["all_equal"].as_bool() == Some(true);
    let self_test_ok = self_test["pass"].as_bool() == Some(true);
    let pass = digest_match && self_test_ok && trace_ok;
    let result = json!({
        "schema": "arc.gpu-proof.v1",
        "profile": PROFILE,
        "pass": pass,
        "package": {"sha256": digest.sha256, "blake3": digest.blake3, "bytes": digest.bytes},
        "golden_package_sha256": golden_doc["package"]["sha256"],
        "gpu": gpu_facts,
        "adapters_available": gpu::list_adapters(),
        "self_test": self_test,
        "golden": {
            "expected_matrix_digest": expected_digest,
            "matrix_digest": actual_digest,
            "match": digest_match,
            "cases": per_case,
            "run_error": failure.map(|(index, message)| json!({"case_index": index, "error": message})),
        },
        "first_divergence": first_divergence,
        "trace_check": trace_check,
        "timing": document["timing"],
        "platform": {"os": std::env::consts::OS, "arch": std::env::consts::ARCH},
    });
    write_json(&out_path, &result)?;
    println!(
        "gpu-check: {} | {} | golden {actual_digest} (expected {expected_digest}) | self-test {} | decode {:.3} tok/s | prefill {:.3} tok/s",
        if pass { "PASS" } else { "FAIL" },
        engine.report().name,
        if self_test_ok { "pass" } else { "FAIL" },
        document["timing"]["decode_tok_s"].as_f64().unwrap_or(0.0),
        document["timing"]["prefill_tok_s"].as_f64().unwrap_or(0.0)
    );
    if pass {
        Ok(())
    } else {
        Err(ModernError::Domain(format!(
            "the GPU result differs from the CPU golden (see {})",
            out_path.display()
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn golden_comparison_finds_the_first_divergent_forward() {
        let golden = json!({"logits_hashes": ["a", "b", "c"]});
        assert_eq!(first_divergent_forward(&golden, &golden), None);
        let changed = json!({"logits_hashes": ["a", "x", "c"]});
        assert_eq!(first_divergent_forward(&golden, &changed), Some(1));
        let short = json!({"logits_hashes": ["a"]});
        assert_eq!(first_divergent_forward(&golden, &short), Some(1));
        assert_eq!(first_divergent_forward(&json!({}), &json!({})), Some(0));
        let case = json!({"prompt_tokens": [5, 6], "tokens": [7, 8, 9]});
        assert_eq!(forwarded_tokens(&case).unwrap(), vec![5, 6, 7, 8]);
        assert_eq!(layer_of("layer12.gate"), json!(12));
        assert_eq!(layer_of("logits"), Value::Null);
    }

    #[test]
    fn options_parse_gpu_flags() {
        let items: Vec<String> = [
            "golden",
            "--gpu",
            "--gpu-batch",
            "4",
            "--gpu-adapter",
            "lavapipe",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        let options = Args { items: &items }.engine_options().unwrap();
        assert_eq!(options.batch, Some(4));
        assert_eq!(options.adapter.as_deref(), Some("lavapipe"));
        assert_eq!(options.max_positions, None);
    }
}
