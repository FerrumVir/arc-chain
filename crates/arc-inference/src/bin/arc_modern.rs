//! `arc-modern`: the CLI path for modern models (SmolLM3-3B) on ARC's
//! deterministic integer engine, profile `arc.hf-llama.i8-dyadic-row.q16.v1`.
//!
//! This is an additional engine path. It does not touch consensus, rewards or
//! native inference, and it does not change the network's canonical model.
//!
//! Subcommands (run with no arguments for usage):
//! convert, verify, inspect, generate, golden, ppl, tokenize, render.

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Instant;

use arc_inference::canonical_simd;
use arc_inference::modern::arith::{self, Selection};
use arc_inference::modern::bench::{self, BenchOptions};
use arc_inference::modern::bpe::ByteLevelBpe;
use arc_inference::modern::chat::{ChatPrompt, render};
use arc_inference::modern::convert::{self, SourceManifest};
use arc_inference::modern::engine::Spec;
use arc_inference::modern::kernels::Kernel;
use arc_inference::modern::model::{GenerationRequest, ModernModel, TokenForward, generate_with};
use arc_inference::modern::package;
use arc_inference::modern::{ModernError, PROFILE, hex_lower};
use serde_json::{Value, json};

const USAGE: &str = "usage: arc-modern <command> [options]

  convert   --source-dir DIR --source-manifest SRC.json --out PKG
            [--manifest-out MANIFEST.json] [--threads N]
  verify    --package PKG --manifest MANIFEST.json
  inspect   --package PKG
  generate  --package PKG --tokenizer tokenizer.json --prompt TEXT
            [--system TEXT] [--today \"06 October 2026\"] [--think]
            [--max-tokens N] [--selection rp64-argmax|argmax] [--eos ID,...]
            [--kernel SPEC] [--threads N] [--json-out OUT.json]
  golden    --package PKG --cases CASES.json --out RUN.json
            [--tokenizer tokenizer.json] [--kernel SPEC] [--threads N]
  ppl       --package PKG --tokens TOKENS.json --out OUT.json
            [--window N] [--max-tokens N] [--kernel SPEC] [--threads N]
  bench     --package PKG --out BENCH.json [--specs SPEC,...] [--threads N]
            [--scaling N,...] [--contexts N,...] [--decode N]
            [--tokens-from GOLDEN.json] [--profile] [--micro] [--bandwidth]
  tokenize  --tokenizer tokenizer.json --input IN.jsonl --out OUT.jsonl
  render    --user TEXT [--system TEXT] [--today DATE] [--think]

  SPEC selects the forward pass and kernel; every spec computes the same
  logits. scalar: reference forward, scalar kernel (the default). simd: fast
  engine, fastest SIMD kernel on this CPU. legacy: reference forward, the
  earlier limb kernel. ref:KERNEL or fast:KERNEL with KERNEL one of scalar,
  avx2, neon, auto. ARC_MODERN_KERNEL=SPEC changes the default.";

const DEFAULT_TODAY: &str = "06 October 2026";

struct Args {
    items: Vec<String>,
}

impl Args {
    fn value(&self, name: &str) -> Option<String> {
        self.items
            .iter()
            .position(|a| a == name)
            .and_then(|i| self.items.get(i + 1).cloned())
    }

    fn required(&self, name: &str) -> Result<String, ModernError> {
        self.value(name)
            .ok_or_else(|| ModernError::Invalid(format!("missing {name}\n\n{USAGE}")))
    }

    fn path(&self, name: &str) -> Result<PathBuf, ModernError> {
        self.required(name).map(PathBuf::from)
    }

    fn flag(&self, name: &str) -> bool {
        self.items.iter().any(|a| a == name)
    }

    fn number(&self, name: &str, default: usize) -> Result<usize, ModernError> {
        match self.value(name) {
            None => Ok(default),
            Some(text) => text
                .parse()
                .map_err(|_| ModernError::Invalid(format!("{name} must be a number"))),
        }
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

fn configure_threads(args: &Args) -> Result<usize, ModernError> {
    let threads = args.number("--threads", 0)?;
    if threads > 0 {
        rayon::ThreadPoolBuilder::new()
            .num_threads(threads)
            .build_global()
            .map_err(|e| ModernError::Invalid(format!("thread pool: {e}")))?;
    }
    Ok(rayon::current_num_threads())
}

/// The `--kernel` spec as given (default `scalar`, or `ARC_MODERN_KERNEL`),
/// parsed and applied process-wide, with every kernel census counting.
fn configure_kernel(args: &Args) -> Result<(String, Spec), ModernError> {
    let name = match args.value("--kernel") {
        Some(name) => name,
        None => std::env::var("ARC_MODERN_KERNEL")
            .ok()
            .filter(|v| !v.is_empty())
            .unwrap_or_else(|| "scalar".to_string()),
    };
    let spec = Spec::parse(&name)?;
    spec.apply();
    Spec::start_census();
    Ok((name, spec))
}

fn platform() -> Value {
    json!({
        "os": std::env::consts::OS,
        "arch": std::env::consts::ARCH,
        "logical_cpus": std::thread::available_parallelism().map(|n| n.get()).unwrap_or(0),
        "rayon_threads": rayon::current_num_threads(),
        "simd_available": canonical_simd::dotprod_available(),
        "kernels_available": Kernel::available_kernels()
            .iter()
            .map(|k| k.name())
            .collect::<Vec<_>>(),
    })
}

fn census(spec: Spec) -> Value {
    serde_json::to_value(spec.census()).unwrap_or(Value::Null)
}

fn load(path: &Path) -> Result<(ModernModel, f64), ModernError> {
    let start = Instant::now();
    let model = package::load_package(path)?;
    Ok((model, start.elapsed().as_secs_f64()))
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

fn parse_eos(text: Option<String>) -> Result<Vec<u32>, ModernError> {
    match text {
        None => Ok(vec![128_012]),
        Some(list) => list
            .split(',')
            .filter(|s| !s.trim().is_empty())
            .map(|s| {
                s.trim()
                    .parse()
                    .map_err(|_| ModernError::Invalid(format!("bad EOS id {s}")))
            })
            .collect(),
    }
}

fn cmd_convert(args: &Args) -> Result<(), ModernError> {
    configure_threads(args)?;
    let source = SourceManifest::read(&args.path("--source-manifest")?)?;
    let out = args.path("--out")?;
    let report = convert::convert(&args.path("--source-dir")?, &source, &out)?;
    let manifest_text = package::manifest_text(&report.manifest)?;
    if let Some(path) = args.value("--manifest-out") {
        std::fs::write(&path, format!("{manifest_text}\n"))
            .map_err(|e| ModernError::Io(format!("{path}: {e}")))?;
    }
    let summary = json!({
        "package": report.digest.to_json(),
        "manifest_blake3": report.manifest.get("manifest_blake3"),
        "seconds": report.seconds,
        "scale_stats": report.scale_stats.to_json(),
        "platform": platform(),
    });
    println!(
        "{}",
        serde_json::to_string_pretty(&summary).unwrap_or_default()
    );
    Ok(())
}

fn cmd_verify(args: &Args) -> Result<(), ModernError> {
    let manifest_path = args.path("--manifest")?;
    let manifest = std::fs::read(&manifest_path)
        .map_err(|e| ModernError::Io(format!("{}: {e}", manifest_path.display())))?;
    let digest = package::verify_package(&args.path("--package")?, &manifest)?;
    println!(
        "{}",
        serde_json::to_string_pretty(&json!({"verified": true, "package": digest.to_json()}))
            .unwrap_or_default()
    );
    Ok(())
}

fn cmd_inspect(args: &Args) -> Result<(), ModernError> {
    let header = package::read_package_header(&args.path("--package")?)?;
    let summary = json!({
        "schema": header.value.get("schema"),
        "profile": header.value.get("profile"),
        "model": header.value.get("model"),
        "source": header.value.get("source"),
        "tensors": header.entries.len(),
        "data_start": header.data_start,
        "kv_bytes_per_position": header.config.kv_bytes_per_position(),
    });
    println!(
        "{}",
        serde_json::to_string_pretty(&summary).unwrap_or_default()
    );
    Ok(())
}

struct CaseResult {
    record: Value,
    digest_entry: Value,
    prompt_tokens: usize,
    prefill_seconds: f64,
    decode_forwards: usize,
    decode_seconds: f64,
}

fn run_case(
    model: &ModernModel,
    runner: &mut dyn TokenForward,
    case: &Value,
    tokenizer: Option<&ByteLevelBpe>,
) -> Result<CaseResult, ModernError> {
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
    let out = generate_with(
        runner,
        &model.config,
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
    let digest_entry = json!({
        "id": record["id"],
        "logits_digest": record["logits_digest"],
        "output_hash": record["output_hash"],
        "tokens": record["tokens"],
    });
    Ok(CaseResult {
        record,
        digest_entry,
        prompt_tokens: prompt.len(),
        prefill_seconds: out.prefill_seconds,
        decode_forwards: out.decode_forwards,
        decode_seconds: out.decode_seconds,
    })
}

fn cmd_golden(args: &Args) -> Result<(), ModernError> {
    let threads = configure_threads(args)?;
    let (kernel, spec) = configure_kernel(args)?;
    let package_path = args.path("--package")?;
    let digest = package::digest_file(&package_path)?;
    let (model, load_seconds) = load(&package_path)?;
    let mut runner = spec.runner(&model);
    let tokenizer = match args.value("--tokenizer") {
        Some(path) => {
            let bytes =
                std::fs::read(&path).map_err(|e| ModernError::Io(format!("{path}: {e}")))?;
            Some(ByteLevelBpe::from_json(&bytes)?)
        }
        None => None,
    };
    let cases = read_json(&args.path("--cases")?)?;
    let list = cases
        .get("cases")
        .and_then(Value::as_array)
        .ok_or_else(|| ModernError::Invalid("cases file has no cases".into()))?;
    let mut records = Vec::new();
    let mut entries = Vec::new();
    let (mut prompt_total, mut prefill_total, mut forwards_total, mut decode_total) =
        (0usize, 0f64, 0usize, 0f64);
    for case in list {
        let result = run_case(&model, runner.as_mut(), case, tokenizer.as_ref())?;
        eprintln!(
            "case {}: {} tokens, output_hash {}",
            result.record["id"], result.record["tokens"], result.record["output_hash"]
        );
        prompt_total += result.prompt_tokens;
        prefill_total += result.prefill_seconds;
        forwards_total += result.decode_forwards;
        decode_total += result.decode_seconds;
        records.push(result.record);
        entries.push(result.digest_entry);
    }
    let matrix_text = arc_inference::model_package::canonical_json(&Value::from(entries))
        .map_err(|e| ModernError::Invalid(format!("canonical JSON: {e}")))?;
    let matrix_digest = blake3::hash(matrix_text.as_bytes()).to_hex().to_string();
    let selections: Vec<&str> = records
        .iter()
        .filter_map(|r| r.get("selection").and_then(Value::as_str))
        .collect();
    let rate = |count: f64, seconds: f64| if seconds > 0.0 { count / seconds } else { 0.0 };
    let prefill_tok_s = rate(prompt_total as f64, prefill_total);
    let decode_tok_s = rate(forwards_total as f64, decode_total);
    // The run names one semantics family; each case's `selection` is
    // authoritative (the Python reference uses the same convention).
    let generation = if selections.iter().all(|s| *s == "argmax") {
        Selection::Argmax.semantics()
    } else {
        Selection::Rp64Argmax.semantics()
    };
    let run = json!({
        "schema": "arc.modern-run.v1",
        "package": {"sha256": digest.sha256, "blake3": digest.blake3, "bytes": digest.bytes},
        "profile": PROFILE,
        "generation": generation,
        "kernel": kernel,
        "spec": spec.name(),
        "engine": spec.engine_name(),
        "kernel_path": spec.kernel_name(),
        "threads": threads,
        "platform": platform(),
        "cases": records,
        "matrix_digest": matrix_digest,
        "census": census(spec),
        "timing": {
            "load_seconds": load_seconds,
            "prompt_tokens": prompt_total,
            "prefill_seconds": prefill_total,
            "prefill_tok_s": prefill_tok_s,
            "decode_forwards": forwards_total,
            "decode_seconds": decode_total,
            "decode_tok_s": decode_tok_s,
            "weights": model.weight_count(),
        },
    });
    write_json(&args.path("--out")?, &run)?;
    println!("matrix_digest {matrix_digest}");
    Ok(())
}

fn cmd_generate(args: &Args) -> Result<(), ModernError> {
    configure_threads(args)?;
    let (kernel, spec) = configure_kernel(args)?;
    let (model, load_seconds) = load(&args.path("--package")?)?;
    let mut runner = spec.runner(&model);
    let tokenizer_path = args.required("--tokenizer")?;
    let tokenizer_bytes = std::fs::read(&tokenizer_path)
        .map_err(|e| ModernError::Io(format!("{tokenizer_path}: {e}")))?;
    let tokenizer = ByteLevelBpe::from_json(&tokenizer_bytes)?;
    let user = args.required("--prompt")?;
    let system = args.value("--system");
    let today = args
        .value("--today")
        .unwrap_or_else(|| DEFAULT_TODAY.to_string());
    let case = json!({
        "id": "cli",
        "user": user,
        "system": system,
        "thinking": args.flag("--think"),
        "today": today,
        "max_tokens": args.number("--max-tokens", 64)?,
        "eos": parse_eos(args.value("--eos"))?,
        "selection": args.value("--selection").unwrap_or_else(|| "rp64-argmax".to_string()),
    });
    let result = run_case(&model, runner.as_mut(), &case, Some(&tokenizer))?;
    let mut record = result.record;
    record["load_seconds"] = Value::from(load_seconds);
    record["kernel"] = Value::from(kernel);
    record["spec"] = Value::from(spec.name());
    record["census"] = census(spec);
    record["platform"] = platform();
    if let Some(path) = args.value("--json-out") {
        write_json(Path::new(&path), &record)?;
    }
    println!("{}", record["text"].as_str().unwrap_or_default());
    eprintln!(
        "output_hash {} | decode {:.2} tok/s | {} tokens",
        record["output_hash"],
        record["decode_tok_s"].as_f64().unwrap_or(0.0),
        record["tokens"]
    );
    Ok(())
}

/// Negative log-likelihood of `target` under Q16 logits (measurement only).
fn nll(logits: &[i64], target: usize) -> f64 {
    const Q16: f64 = 65_536.0;
    let max = logits.iter().copied().max().unwrap_or(0) as f64 / Q16;
    let sum: f64 = logits.iter().map(|&x| (x as f64 / Q16 - max).exp()).sum();
    max + sum.ln() - logits[target] as f64 / Q16
}

fn cmd_ppl(args: &Args) -> Result<(), ModernError> {
    let threads = configure_threads(args)?;
    let (kernel, spec) = configure_kernel(args)?;
    let (model, _) = load(&args.path("--package")?)?;
    let mut runner = spec.runner(&model);
    let tokens_json = read_json(&args.path("--tokens")?)?;
    let mut tokens = ids_from(tokens_json.get("tokens").or(Some(&tokens_json)))?;
    let limit = args.number("--max-tokens", tokens.len())?;
    tokens.truncate(limit);
    let window = args.number("--window", 512)?.min(model.config.max_seq);
    if window < 2 || tokens.len() < 2 {
        return Err(ModernError::Invalid(
            "ppl needs a window and at least two tokens".into(),
        ));
    }
    let start = Instant::now();
    let mut nll_sum = 0f64;
    let mut scored = 0usize;
    let mut argmax_ids = Vec::new();
    let mut hashes = Vec::new();
    let mut forwards = 0usize;
    for chunk in tokens.chunks(window) {
        if chunk.len() < 2 {
            continue;
        }
        runner.begin(chunk.len());
        for (position, &token) in chunk.iter().enumerate().take(chunk.len() - 1) {
            let logits = runner.forward_token(token)?;
            forwards += 1;
            hashes.push(arith::logits_hash(logits));
            nll_sum += nll(logits, chunk[position + 1] as usize);
            argmax_ids.push(arith::argmax(logits) as u32);
            scored += 1;
        }
    }
    let seconds = start.elapsed().as_secs_f64();
    let out = json!({
        "schema": "arc.modern-ppl.v1",
        "profile": PROFILE,
        "kernel": kernel,
        "spec": spec.name(),
        "threads": threads,
        "window": window,
        "scored_tokens": scored,
        "nll_sum": nll_sum,
        "mean_nll": nll_sum / scored as f64,
        "ppl": (nll_sum / scored as f64).exp(),
        "argmax": argmax_ids,
        "logits_digest": hex_lower(&arith::logits_digest(&hashes)),
        "seconds": seconds,
        "tok_s": forwards as f64 / seconds.max(1e-9),
        "census": census(spec),
        "platform": platform(),
    });
    write_json(&args.path("--out")?, &out)?;
    println!(
        "ppl {:.4} over {scored} tokens",
        (nll_sum / scored as f64).exp()
    );
    Ok(())
}

/// Comma-separated numbers.
fn number_list(text: &str, name: &str) -> Result<Vec<usize>, ModernError> {
    text.split(',')
        .filter(|s| !s.trim().is_empty())
        .map(|s| {
            s.trim()
                .parse()
                .map_err(|_| ModernError::Invalid(format!("{name}: bad number {s}")))
        })
        .collect()
}

/// Token ids for the bench: every case's prompt of a golden or run file, a
/// `{"tokens": [...]}` file, or a plain list.
fn bench_tokens(value: &Value) -> Result<Vec<u32>, ModernError> {
    if let Some(cases) = value.get("cases").and_then(Value::as_array) {
        let mut tokens = Vec::new();
        for case in cases {
            tokens.extend(ids_from(case.get("prompt_tokens"))?);
        }
        return Ok(tokens);
    }
    ids_from(value.get("tokens").or(Some(value)))
}

fn cmd_bench(args: &Args) -> Result<(), ModernError> {
    configure_threads(args)?;
    let package_path = args.path("--package")?;
    let digest = package::digest_file(&package_path)?;
    let (model, load_seconds) = load(&package_path)?;
    let spec_names = args.value("--specs").unwrap_or_else(|| "simd".to_string());
    let specs = spec_names
        .split(',')
        .filter(|s| !s.trim().is_empty())
        .map(|s| Spec::parse(s.trim()))
        .collect::<Result<Vec<_>, _>>()?;
    let source_tokens = match args.value("--tokens-from") {
        Some(path) => bench_tokens(&read_json(Path::new(&path))?)?,
        None => (0..4096u32)
            .map(|i| (i.wrapping_mul(7919) + 13) % model.config.vocab_size as u32)
            .collect(),
    };
    let options = BenchOptions {
        specs,
        threads: args.number("--threads", 0)?,
        scaling: number_list(&args.value("--scaling").unwrap_or_default(), "--scaling")?,
        contexts: number_list(
            &args.value("--contexts").unwrap_or_else(|| "64".to_string()),
            "--contexts",
        )?,
        decode_tokens: args.number("--decode", 16)?,
        source_tokens,
        profile: args.flag("--profile"),
        kernel_micro: args.flag("--micro"),
        bandwidth: args.flag("--bandwidth"),
    };
    let mut result = bench::run(&model, &options)?;
    result["package"] = digest.to_json();
    result["load_seconds"] = json!(load_seconds);
    result["specs_requested"] = json!(spec_names);
    write_json(&args.path("--out")?, &result)?;
    for context in result["contexts"].as_array().into_iter().flatten() {
        for run in context["decode"].as_array().into_iter().flatten() {
            println!(
                "context {} | {} | {} threads | {:.2} tok/s | digest {}",
                context["context"],
                run["spec"].as_str().unwrap_or_default(),
                run["threads"],
                run["tok_s"].as_f64().unwrap_or(0.0),
                run["logits_digest"].as_str().unwrap_or_default()
            );
        }
        println!(
            "context {} | all specs agree: {}",
            context["context"], context["digests_equal"]
        );
    }
    let agree = result["contexts"]
        .as_array()
        .into_iter()
        .flatten()
        .all(|c| c["digests_equal"].as_bool() == Some(true));
    if agree {
        Ok(())
    } else {
        Err(ModernError::Domain(
            "bench: specs computed different logits".into(),
        ))
    }
}

fn cmd_tokenize(args: &Args) -> Result<(), ModernError> {
    let tokenizer_path = args.path("--tokenizer")?;
    let bytes = std::fs::read(&tokenizer_path)
        .map_err(|e| ModernError::Io(format!("{}: {e}", tokenizer_path.display())))?;
    let tokenizer = ByteLevelBpe::from_json(&bytes)?;
    let input_path = args.path("--input")?;
    let input = std::fs::read_to_string(&input_path)
        .map_err(|e| ModernError::Io(format!("{}: {e}", input_path.display())))?;
    let mut lines = Vec::new();
    for line in input.lines().filter(|l| !l.trim().is_empty()) {
        let item: Value = serde_json::from_str(line)
            .map_err(|e| ModernError::Invalid(format!("tokenize input: {e}")))?;
        let text = if let Some(chat) = item.get("chat") {
            render(&ChatPrompt {
                system: chat.get("system").and_then(Value::as_str),
                user: chat.get("user").and_then(Value::as_str).unwrap_or_default(),
                thinking: chat
                    .get("thinking")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
                today: chat
                    .get("today")
                    .and_then(Value::as_str)
                    .unwrap_or(DEFAULT_TODAY),
            })
        } else {
            item.get("text")
                .and_then(Value::as_str)
                .ok_or_else(|| ModernError::Invalid("tokenize lines need text or chat".into()))?
                .to_string()
        };
        let ids = tokenizer.encode(&text)?;
        let line_out = json!({"id": item.get("id"), "text": text, "ids": ids});
        lines.push(serde_json::to_string(&line_out).unwrap_or_default());
    }
    let out = args.path("--out")?;
    std::fs::write(&out, lines.join("\n") + "\n")
        .map_err(|e| ModernError::Io(format!("{}: {e}", out.display())))
}

fn cmd_render(args: &Args) -> Result<(), ModernError> {
    let user = args.required("--user")?;
    let system = args.value("--system");
    let today = args
        .value("--today")
        .unwrap_or_else(|| DEFAULT_TODAY.to_string());
    print!(
        "{}",
        render(&ChatPrompt {
            system: system.as_deref(),
            user: &user,
            thinking: args.flag("--think"),
            today: &today,
        })
    );
    Ok(())
}

fn main() -> ExitCode {
    let items: Vec<String> = std::env::args().skip(1).collect();
    let Some(command) = items.first().cloned() else {
        eprintln!("{USAGE}");
        return ExitCode::from(2);
    };
    let args = Args { items };
    let result = match command.as_str() {
        "convert" => cmd_convert(&args),
        "verify" => cmd_verify(&args),
        "inspect" => cmd_inspect(&args),
        "generate" => cmd_generate(&args),
        "golden" => cmd_golden(&args),
        "ppl" => cmd_ppl(&args),
        "bench" => cmd_bench(&args),
        "tokenize" => cmd_tokenize(&args),
        "render" => cmd_render(&args),
        _ => {
            eprintln!("{USAGE}");
            return ExitCode::from(2);
        }
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("arc-modern {command}: {error}");
            ExitCode::FAILURE
        }
    }
}
