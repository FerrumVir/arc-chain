//! `arc-mla`: DeepSeek-V3-architecture models (Moonlight-16B-A3B; the Kimi K2
//! text architecture) on ARC's deterministic integer engine, profile
//! `arc.hf-deepseek-v3.mla-moe.i8-dyadic-row.q16.v1`, with pipeline stages.
//!
//! This is an additional engine path. It does not touch consensus, rewards or
//! native inference.
//!
//! Subcommands (run with no arguments for usage): convert, verify, inspect,
//! golden, stage, ppl, tokenize, render, and the weight-slice commands
//! slice-plan, slice, slice-manifest, slice-verify, slice-assemble.

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Instant;

use arc_inference::canonical_simd;
use arc_inference::modern::arith::{self, Selection};
use arc_inference::modern::convert::SourceManifest;
use arc_inference::modern::mla::boundary::{Boundary, BoundarySequence, boundary_digest};
use arc_inference::modern::mla::config::ExpertFormat;
use arc_inference::modern::mla::convert::{self, blake3_hex};
use arc_inference::modern::mla::model::{StageInput, StageModel};
use arc_inference::modern::mla::package::{self, StageSpec};
use arc_inference::modern::mla::slices::{self, SliceManifest, SliceSource};
use arc_inference::modern::mla::{RUN_SCHEMA, STAGE_RUN_SCHEMA};
use arc_inference::modern::model::GenerationRequest;
use arc_inference::modern::tiktoken::{
    MOONLIGHT_SPECIAL_TOKENS, TiktokenBpe, render_moonlight_chat,
};
use arc_inference::modern::{ModernError, hex_lower};
use serde_json::{Value, json};

const USAGE: &str = "usage: arc-mla <command> [options]

  convert   --source-dir DIR --source-manifest SRC.json --out PKG [--layers A:B]
            [--experts i8|i4g32] [--manifest-out MANIFEST.json] [--report OUT.json] [--threads N]
  verify    --package PKG --manifest MANIFEST.json [--full-digest]
  inspect   --package PKG
  golden    --package PKG --cases CASES.json --out RUN.json
            [--tokenizer-dir DIR] [--special-tokens N] [--kernel scalar|simd] [--threads N]
  stage     --package PKG (--run RUN.json | --input BOUNDARY.bin) --out BOUNDARY.bin
            --report STAGE.json [--layers A:B] [--manifest MANIFEST.json]
            [--kernel scalar|simd] [--threads N]
  ppl       --package PKG --tokens TOKENS.json --out OUT.json
            [--window N] [--max-tokens N] [--kernel scalar|simd] [--threads N]
  tokenize  --tokenizer-dir DIR --input IN.jsonl --out OUT.jsonl [--special-tokens N]
  render    --user TEXT [--system TEXT]

  slice-plan     --source-dir DIR --source-manifest SRC.json --out PLAN.json
                 [UNITS] [--expert-groups G] [--experts i8|i4g32]
  slice          --source-dir DIR --source-manifest SRC.json --out-dir SLICES
                 [UNITS] [--expert-groups G] [--experts i8|i4g32] [--report OUT.json] [--threads N]
                 [--discard]
  slice-manifest --source-dir DIR --source-manifest SRC.json --out-dir SLICES --out MANIFEST.json
                 [--expert-groups G] [--experts i8|i4g32]
  slice-verify   --manifest MANIFEST.json --slices SLICES [--only NAME[,NAME..]] [--segments]
  slice-assemble --manifest MANIFEST.json --slices SLICES --out PKG [--layers A:B]

UNITS selects what to slice: --layers A:B, --embed, --head (everything when
none is given). A slice is a segment (embed, a dense layer, head) or, for an
MoE layer, its core (attention, norms, router, shared experts) and G groups of
its routed experts. Slices are files named by their BLAKE3 (spec section 14).
Experts default to i4g32 for checkpoints that ship INT4 experts (repacked,
never requantised) and to i8 otherwise. `slice --discard` hashes the slices
and writes the unit records without storing the slice files.

A package holds a layer range [A, B) of the model (the whole model when
converted without --layers). Its routed experts are INT8 dyadic rows, or with
--experts i4g32 INT4 values with BF16 group-32 scales (spec section 13).
`stage --layers` executes a sub-range of a package, reading only those tensors. The tokenizer directory holds tiktoken.model and
tokenizer_config.json.";

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

fn configure_kernel(args: &Args) -> Result<String, ModernError> {
    let kernel = args
        .value("--kernel")
        .unwrap_or_else(|| "scalar".to_string());
    // Read the ARC_FAST_CANONICAL_KERNEL default first, so the explicit
    // choice below is the one that stays in force.
    let _ = canonical_simd::fast_canonical_kernel_enabled();
    match kernel.as_str() {
        "scalar" => canonical_simd::set_fast_canonical_kernel(false),
        "simd" => {
            if !canonical_simd::dotprod_available() {
                return Err(ModernError::Invalid(
                    "--kernel simd needs NEON dotprod (arm64) or AVX2 (x86-64)".into(),
                ));
            }
            canonical_simd::set_fast_canonical_kernel(true);
        }
        other => return Err(ModernError::Invalid(format!("unknown kernel {other}"))),
    }
    canonical_simd::set_projection_census_enabled(true);
    canonical_simd::reset_projection_census();
    Ok(kernel)
}

fn platform() -> Value {
    json!({
        "os": std::env::consts::OS,
        "arch": std::env::consts::ARCH,
        "logical_cpus": std::thread::available_parallelism().map(|n| n.get()).unwrap_or(0),
        "rayon_threads": rayon::current_num_threads(),
        "simd_available": canonical_simd::dotprod_available(),
    })
}

fn census() -> Value {
    serde_json::to_value(canonical_simd::projection_census()).unwrap_or(Value::Null)
}

fn canonical_blake3(value: &Value) -> Result<String, ModernError> {
    let text = arc_inference::model_package::canonical_json(value)
        .map_err(|e| ModernError::Invalid(format!("canonical JSON: {e}")))?;
    Ok(blake3_hex(text.as_bytes()))
}

fn ids_from(value: Option<&Value>) -> Result<Vec<u32>, ModernError> {
    match value {
        None | Some(Value::Null) => Ok(Vec::new()),
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

fn load_tokenizer(args: &Args) -> Result<Option<TiktokenBpe>, ModernError> {
    let Some(dir) = args.value("--tokenizer-dir") else {
        return Ok(None);
    };
    let dir = PathBuf::from(dir);
    let read = |name: &str| {
        let path = dir.join(name);
        std::fs::read(&path).map_err(|e| ModernError::Io(format!("{}: {e}", path.display())))
    };
    let model = read("tiktoken.model")?;
    let config = read("tokenizer_config.json").ok();
    let specials = args.number("--special-tokens", MOONLIGHT_SPECIAL_TOKENS)?;
    Ok(Some(TiktokenBpe::from_files(
        &model,
        config.as_deref(),
        specials,
    )?))
}

fn open_model(args: &Args) -> Result<(StageModel, f64), ModernError> {
    let start = Instant::now();
    let model = StageModel::open(&args.path("--package")?)?;
    Ok((model, start.elapsed().as_secs_f64()))
}

fn cmd_convert(args: &Args) -> Result<(), ModernError> {
    configure_threads(args)?;
    let source = SourceManifest::read(&args.path("--source-manifest")?)?;
    let stage = args
        .value("--layers")
        .map(|s| StageSpec::parse(&s))
        .transpose()?;
    let experts = ExpertFormat::parse(&args.value("--experts").unwrap_or_else(|| "i8".into()))?;
    let report = convert::convert_stage(
        &args.path("--source-dir")?,
        &source,
        stage,
        experts,
        &args.path("--out")?,
    )?;
    if let Some(path) = args.value("--manifest-out") {
        let manifest = report.manifest.as_ref().ok_or_else(|| {
            ModernError::Invalid("--manifest-out needs a whole-model conversion".into())
        })?;
        let text = arc_inference::modern::package::manifest_text(manifest)?;
        std::fs::write(&path, format!("{text}\n"))
            .map_err(|e| ModernError::Io(format!("{path}: {e}")))?;
    }
    let mut summary = report.to_json();
    summary["platform"] = platform();
    if let Some(path) = args.value("--report") {
        write_json(Path::new(&path), &summary)?;
    }
    println!(
        "{}",
        serde_json::to_string_pretty(&summary).unwrap_or_default()
    );
    Ok(())
}

fn cmd_verify(args: &Args) -> Result<(), ModernError> {
    let manifest_path = args.path("--manifest")?;
    let manifest_bytes = std::fs::read(&manifest_path)
        .map_err(|e| ModernError::Io(format!("{}: {e}", manifest_path.display())))?;
    let (model, _) = open_model(args)?;
    let segments = package::segment_digests(model.bytes(), &model.header);
    let mut report = package::verify_against_manifest(&model.header, &segments, &manifest_bytes)?;
    if args.flag("--full-digest") {
        let digest = package::digest_file(&args.path("--package")?)?;
        let manifest: Value = serde_json::from_slice(&manifest_bytes)
            .map_err(|e| ModernError::Invalid(format!("manifest JSON: {e}")))?;
        let pinned = &manifest["full_package"];
        if model.stage() == StageSpec::full(model.config())
            && (pinned["sha256"].as_str() != Some(digest.sha256.as_str())
                || pinned["bytes"].as_u64() != Some(digest.bytes))
        {
            return Err(ModernError::Invalid(format!(
                "package SHA-256 {} ({} bytes) differs from the manifest's {}",
                digest.sha256, digest.bytes, pinned
            )));
        }
        report["package"] = digest.to_json();
    }
    println!(
        "{}",
        serde_json::to_string_pretty(&report).unwrap_or_default()
    );
    Ok(())
}

fn cmd_inspect(args: &Args) -> Result<(), ModernError> {
    let header = package::read_header_file(&args.path("--package")?)?;
    let c = &header.config;
    let int8_weights: u64 = header
        .entries
        .iter()
        .filter(|e| e.dtype == package::Dtype::I8)
        .map(|e| e.bytes)
        .sum();
    let summary = json!({
        "schema": header.value.get("schema"),
        "profile": header.value.get("profile"),
        "model": header.value.get("model"),
        "source": header.value.get("source"),
        "stage": header.stage.to_json(),
        "tensors": header.entries.len(),
        "data_start": header.data_start,
        "bytes": header.file_len,
        "kv_bytes_per_position": c.kv_bytes_per_position(),
        "int8_weights": int8_weights,
    });
    println!(
        "{}",
        serde_json::to_string_pretty(&summary).unwrap_or_default()
    );
    Ok(())
}

/// One golden case: prompt from text (rendered and tokenized) or token ids.
fn case_prompt(
    case: &Value,
    tokenizer: Option<&TiktokenBpe>,
) -> Result<(String, Vec<u32>, Value), ModernError> {
    let id = case
        .get("id")
        .and_then(Value::as_str)
        .ok_or_else(|| ModernError::Invalid("every case needs an id".into()))?
        .to_string();
    if let Some(user) = case.get("user").and_then(Value::as_str) {
        let tokenizer = tokenizer.ok_or_else(|| {
            ModernError::Invalid(format!("case {id} is text; pass --tokenizer-dir"))
        })?;
        let text = render_moonlight_chat(case.get("system").and_then(Value::as_str), user);
        let ids = tokenizer.encode(&text)?;
        Ok((id, ids, Value::from(text)))
    } else {
        Ok((id, ids_from(case.get("prompt_tokens"))?, Value::Null))
    }
}

fn cmd_golden(args: &Args) -> Result<(), ModernError> {
    let threads = configure_threads(args)?;
    let kernel = configure_kernel(args)?;
    let (model, load_seconds) = open_model(args)?;
    let c = model.config().clone();
    if model.stage() != StageSpec::full(&c) {
        return Err(ModernError::Invalid(
            "golden runs need the whole-model package".into(),
        ));
    }
    let segments = package::segment_digests(model.bytes(), &model.header);
    let model_root = package::model_root(&c, &model.header.source, &segments)?;
    let digest = package::digest_file(&args.path("--package")?)?;
    let tokenizer = load_tokenizer(args)?;
    let cases = read_json(&args.path("--cases")?)?;
    let list = cases
        .get("cases")
        .and_then(Value::as_array)
        .ok_or_else(|| ModernError::Invalid("cases file has no cases".into()))?;
    let mut records = Vec::new();
    let mut entries = Vec::new();
    let mut boundary_entries = Vec::new();
    let (mut prompt_total, mut prefill_total, mut forwards_total, mut decode_total) =
        (0usize, 0f64, 0usize, 0f64);
    for case in list {
        let (id, prompt, rendered) = case_prompt(case, tokenizer.as_ref())?;
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
        let out = model.generate(&GenerationRequest {
            prompt: &prompt,
            max_tokens,
            eos: &eos,
            selection,
        })?;
        let boundary_digests: Vec<String> =
            out.boundary_digests.iter().map(|h| hex_lower(h)).collect();
        let decode_tok_s = if out.decode_seconds > 0.0 {
            out.decode_forwards as f64 / out.decode_seconds
        } else {
            0.0
        };
        let text = tokenizer.as_ref().map(|t| t.decode(&out.tokens));
        let record = json!({
            "id": id,
            "prompt_tokens": prompt,
            "max_tokens": max_tokens,
            "eos": eos,
            "selection": selection.name(),
            "tokens": out.tokens,
            "output_hash": hex_lower(&out.output_hash),
            "logits_hashes": out.logits_hashes.iter().map(|h| hex_lower(h)).collect::<Vec<_>>(),
            "logits_digest": hex_lower(&out.logits_digest),
            "boundary_digests": boundary_digests,
            "rendered_prompt": rendered,
            "text": text,
            "prefill_seconds": out.prefill_seconds,
            "decode_seconds": out.decode_seconds,
            "decode_forwards": out.decode_forwards,
            "decode_tok_s": decode_tok_s,
        });
        eprintln!(
            "case {}: {} tokens, output_hash {}",
            record["id"], record["tokens"], record["output_hash"]
        );
        entries.push(json!({
            "id": record["id"],
            "logits_digest": record["logits_digest"],
            "output_hash": record["output_hash"],
            "tokens": record["tokens"],
        }));
        boundary_entries.push(json!({
            "id": record["id"],
            "boundary_digests": record["boundary_digests"],
        }));
        prompt_total += prompt.len();
        prefill_total += out.prefill_seconds;
        forwards_total += out.decode_forwards;
        decode_total += out.decode_seconds;
        records.push(record);
    }
    let rate = |count: f64, seconds: f64| if seconds > 0.0 { count / seconds } else { 0.0 };
    let selections: Vec<&str> = records
        .iter()
        .filter_map(|r| r.get("selection").and_then(Value::as_str))
        .collect();
    let generation = if selections.iter().all(|s| *s == "argmax") {
        Selection::Argmax.semantics()
    } else {
        Selection::Rp64Argmax.semantics()
    };
    let run = json!({
        "schema": RUN_SCHEMA,
        "package": digest.to_json(),
        "profile": c.profile(),
        "model_root": model_root,
        "generation": generation,
        "kernel": kernel,
        "threads": threads,
        "platform": platform(),
        "cases": records,
        "matrix_digest": canonical_blake3(&Value::from(entries))?,
        "boundary_matrix_digest": canonical_blake3(&Value::from(boundary_entries))?,
        "census": census(),
        "timing": {
            "load_seconds": load_seconds,
            "prompt_tokens": prompt_total,
            "prefill_seconds": prefill_total,
            "prefill_tok_s": rate(prompt_total as f64, prefill_total),
            "decode_forwards": forwards_total,
            "decode_seconds": decode_total,
            "decode_tok_s": rate(forwards_total as f64, decode_total),
            "int8_weights": model.weight_count(),
        },
    });
    write_json(&args.path("--out")?, &run)?;
    println!(
        "matrix_digest {}",
        run["matrix_digest"].as_str().unwrap_or("")
    );
    Ok(())
}

/// The forwarded token sequences of a golden run: `prompt ‖ tokens[..n-1]`
/// with `tokens` cut at `max_tokens` (spec §6.4).
fn sequences_from_run(run: &Value) -> Result<Vec<BoundarySequence>, ModernError> {
    let cases = run
        .get("cases")
        .and_then(Value::as_array)
        .ok_or_else(|| ModernError::Invalid("run has no cases".into()))?;
    cases
        .iter()
        .map(|case| {
            let id = case
                .get("id")
                .and_then(Value::as_str)
                .ok_or_else(|| ModernError::Invalid("run case has no id".into()))?
                .to_string();
            let prompt = ids_from(case.get("prompt_tokens"))?;
            let generated = ids_from(case.get("tokens"))?;
            let max_tokens = case
                .get("max_tokens")
                .and_then(Value::as_u64)
                .and_then(|v| usize::try_from(v).ok())
                .ok_or_else(|| ModernError::Invalid(format!("run case {id} has no max_tokens")))?;
            if prompt.is_empty() || generated.is_empty() {
                return Err(ModernError::Invalid(format!(
                    "run case {id} needs prompt and generated tokens"
                )));
            }
            let kept = generated.len().min(max_tokens);
            let mut tokens = prompt.clone();
            tokens.extend_from_slice(&generated[..kept - 1]);
            Ok(BoundarySequence {
                id,
                tokens,
                prompt_len: prompt.len(),
                selection: Selection::parse(
                    case.get("selection")
                        .and_then(Value::as_str)
                        .unwrap_or("rp64-argmax"),
                )?,
                eos: ids_from(case.get("eos"))?,
                max_tokens,
                values: Vec::new(),
            })
        })
        .collect()
}

fn cmd_stage(args: &Args) -> Result<(), ModernError> {
    let threads = configure_threads(args)?;
    let kernel = configure_kernel(args)?;
    let load_start = Instant::now();
    let layers = args
        .value("--layers")
        .map(|s| StageSpec::parse(&s))
        .transpose()?;
    let model = StageModel::open_range(&args.path("--package")?, layers)?;
    let load_seconds = load_start.elapsed().as_secs_f64();
    let c = model.config().clone();
    let stage = model.stage();
    let segments = model.segments();
    let (mut sequences, model_root, input) = match (args.value("--run"), args.value("--input")) {
        (Some(run_path), None) => {
            if stage.first_layer != 0 {
                return Err(ModernError::Invalid(
                    "--run feeds token ids, which only the first stage takes".into(),
                ));
            }
            let run = read_json(Path::new(&run_path))?;
            let root = run
                .get("model_root")
                .and_then(Value::as_str)
                .ok_or_else(|| ModernError::Invalid("run has no model_root".into()))?
                .to_string();
            let sequences = sequences_from_run(&run)?;
            let input = json!({"kind": "tokens", "layer": 0, "run_matrix_digest": run.get("matrix_digest")});
            (sequences, root, input)
        }
        (None, Some(input_path)) => {
            let bytes = std::fs::read(&input_path)
                .map_err(|e| ModernError::Io(format!("{input_path}: {e}")))?;
            let boundary = Boundary::from_bytes(&bytes)?;
            if boundary.profile != c.profile() {
                return Err(ModernError::Invalid(format!(
                    "boundary file was produced under {}; this stage runs {}",
                    boundary.profile,
                    c.profile()
                )));
            }
            if boundary.layer != stage.first_layer || boundary.d_model != c.d_model {
                return Err(ModernError::Invalid(format!(
                    "boundary file is layer {} width {}; this stage starts at layer {} width {}",
                    boundary.layer, boundary.d_model, stage.first_layer, c.d_model
                )));
            }
            let input = json!({
                "kind": "boundary",
                "layer": boundary.layer,
                "file_blake3": blake3_hex(&bytes),
                "digests": boundary.digests(),
            });
            (boundary.sequences, boundary.model_root, input)
        }
        _ => {
            return Err(ModernError::Invalid(
                "stage needs exactly one of --run and --input".into(),
            ));
        }
    };
    let manifest_check = match args.value("--manifest") {
        Some(path) => {
            let bytes =
                std::fs::read(&path).map_err(|e| ModernError::Io(format!("{path}: {e}")))?;
            let report = package::verify_against_manifest(&model.header, &segments, &bytes)?;
            if report["model_root"].as_str() != Some(model_root.as_str()) {
                return Err(ModernError::Invalid(
                    "the input was produced by a different model root than the manifest".into(),
                ));
            }
            report
        }
        None => Value::Null,
    };
    let start = Instant::now();
    let mut positions = 0usize;
    let mut head_cases = Vec::new();
    for sequence in &mut sequences {
        let inputs = if stage.first_layer == 0 {
            None
        } else {
            Some(sequence.values.as_slice())
        };
        let run = model.run_sequence(
            &sequence.tokens,
            inputs,
            sequence.prompt_len,
            sequence.selection,
        )?;
        positions += sequence.tokens.len();
        if stage.has_head(&c) {
            head_cases.push(json!({
                "id": sequence.id,
                "logits_hashes": run.logits_hashes.iter().map(|h| hex_lower(h)).collect::<Vec<_>>(),
                "logits_digest": hex_lower(&arith::logits_digest(&run.logits_hashes)),
                "derived_tokens": run.derived,
                "output_hash": hex_lower(&arith::tokens_hash(&run.derived)),
            }));
        }
        eprintln!(
            "sequence {}: {} positions, boundary {} digest {}",
            sequence.id,
            sequence.tokens.len(),
            stage.end_layer,
            hex_lower(&boundary_digest(&run.hidden, c.d_model))
        );
        sequence.values = run.hidden;
    }
    let seconds = start.elapsed().as_secs_f64();
    let output = Boundary {
        profile: c.profile().to_string(),
        layer: stage.end_layer,
        d_model: c.d_model,
        model_root: model_root.clone(),
        sequences,
    };
    let out_path = args.path("--out")?;
    let (bytes, file_blake3) = output.write(&out_path)?;
    let head = if stage.has_head(&c) {
        Value::from(head_cases)
    } else {
        Value::Null
    };
    let positions_per_s = if seconds > 0.0 {
        positions as f64 / seconds
    } else {
        0.0
    };
    let segment_list: Vec<Value> = segments
        .iter()
        .map(package::SegmentDigest::to_json)
        .collect();
    let report = json!({
        "schema": STAGE_RUN_SCHEMA,
        "profile": c.profile(),
        "model_root": model_root,
        "stage": stage.to_json(),
        "segments": segment_list,
        "manifest_check": manifest_check,
        "input": input,
        "output": {
            "layer": stage.end_layer,
            "file_bytes": bytes,
            "file_blake3": file_blake3,
            "digests": output.digests(),
        },
        "head": head,
        "kernel": kernel,
        "threads": threads,
        "platform": platform(),
        "census": census(),
        "timing": {
            "load_seconds": load_seconds,
            "positions": positions,
            "seconds": seconds,
            "positions_per_s": positions_per_s,
            "layers": stage.end_layer - stage.first_layer,
            "int8_weights": model.weight_count(),
        },
    });
    write_json(&args.path("--report")?, &report)?;
    println!(
        "stage [{}, {}): {positions} positions in {seconds:.1} s; output {}",
        stage.first_layer,
        stage.end_layer,
        out_path.display()
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
    let kernel = configure_kernel(args)?;
    let (model, _) = open_model(args)?;
    let c = model.config().clone();
    if model.stage() != StageSpec::full(&c) {
        return Err(ModernError::Invalid(
            "ppl needs the whole-model package".into(),
        ));
    }
    let tokens_json = read_json(&args.path("--tokens")?)?;
    let mut tokens = ids_from(tokens_json.get("tokens").or(Some(&tokens_json)))?;
    let limit = args.number("--max-tokens", tokens.len())?;
    tokens.truncate(limit);
    let window = args.number("--window", 512)?.min(c.max_seq);
    if window < 2 || tokens.len() < 2 {
        return Err(ModernError::Invalid(
            "ppl needs a window and at least two tokens".into(),
        ));
    }
    let start = Instant::now();
    let (mut nll_sum, mut scored, mut forwards) = (0f64, 0usize, 0usize);
    let mut argmax_ids = Vec::new();
    let mut hashes = Vec::new();
    for chunk in tokens.chunks(window) {
        if chunk.len() < 2 {
            continue;
        }
        let mut cache = model.new_cache();
        for (position, &token) in chunk.iter().enumerate().take(chunk.len() - 1) {
            let (_, logits) = model.forward(StageInput::Token(token), &mut cache, None)?;
            let logits = logits.ok_or_else(|| ModernError::Invalid("no logits".into()))?;
            forwards += 1;
            hashes.push(arith::logits_hash(&logits));
            nll_sum += nll(&logits, chunk[position + 1] as usize);
            argmax_ids.push(arith::argmax(&logits) as u32);
            scored += 1;
        }
    }
    let seconds = start.elapsed().as_secs_f64();
    let out = json!({
        "schema": "arc.mla-ppl.v1",
        "profile": c.profile(),
        "kernel": kernel,
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
        "census": census(),
        "platform": platform(),
    });
    write_json(&args.path("--out")?, &out)?;
    println!(
        "ppl {:.4} over {scored} tokens",
        (nll_sum / scored as f64).exp()
    );
    Ok(())
}

fn cmd_tokenize(args: &Args) -> Result<(), ModernError> {
    let tokenizer = load_tokenizer(args)?.ok_or_else(|| {
        ModernError::Invalid(format!("tokenize needs --tokenizer-dir\n\n{USAGE}"))
    })?;
    let input_path = args.path("--input")?;
    let input = std::fs::read_to_string(&input_path)
        .map_err(|e| ModernError::Io(format!("{}: {e}", input_path.display())))?;
    let mut lines = Vec::new();
    for line in input.lines().filter(|l| !l.trim().is_empty()) {
        let item: Value = serde_json::from_str(line)
            .map_err(|e| ModernError::Invalid(format!("tokenize input: {e}")))?;
        let text = if let Some(chat) = item.get("chat") {
            render_moonlight_chat(
                chat.get("system").and_then(Value::as_str),
                chat.get("user").and_then(Value::as_str).unwrap_or_default(),
            )
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
    print!(
        "{}",
        render_moonlight_chat(args.value("--system").as_deref(), &user)
    );
    Ok(())
}

fn slice_source(args: &Args) -> Result<SliceSource, ModernError> {
    let experts = args
        .value("--experts")
        .map(|e| ExpertFormat::parse(&e))
        .transpose()?;
    SliceSource::open(
        &args.path("--source-dir")?,
        &args.path("--source-manifest")?,
        experts,
        args.number("--expert-groups", 1)?,
    )
}

fn slice_units(args: &Args, src: &SliceSource) -> Result<Vec<slices::Unit>, ModernError> {
    let layers = args
        .value("--layers")
        .map(|s| StageSpec::parse(&s))
        .transpose()?;
    slices::select_units(
        &src.config,
        layers,
        args.flag("--embed"),
        args.flag("--head"),
    )
}

fn cmd_slice_plan(args: &Args) -> Result<(), ModernError> {
    let src = slice_source(args)?;
    let units = slice_units(args, &src)?;
    let plan = slices::plan(&src, &args.path("--source-dir")?, &units)?;
    write_json(&args.path("--out")?, &plan)?;
    println!(
        "{}",
        serde_json::to_string_pretty(&plan).unwrap_or_default()
    );
    Ok(())
}

fn cmd_slice(args: &Args) -> Result<(), ModernError> {
    let threads = configure_threads(args)?;
    let start = Instant::now();
    let src = slice_source(args)?;
    let units = slice_units(args, &src)?;
    let out = args.path("--out-dir")?;
    std::fs::create_dir_all(&out)
        .map_err(|e| ModernError::Io(format!("{}: {e}", out.display())))?;
    let report = slices::convert_units(
        &src,
        &args.path("--source-dir")?,
        &units,
        &out,
        args.flag("--discard"),
    )?;
    let units_json: Vec<Value> = report
        .records
        .iter()
        .zip(&report.seconds)
        .map(|(r, (_, seconds))| {
            json!({
                "unit": r.unit.name(),
                "segment": r.segment.to_json(),
                "slices": r.slices.len(),
                "slice_bytes": r.slices.iter().map(|s| s.bytes).sum::<u64>(),
                "seconds": seconds,
            })
        })
        .collect();
    let summary = json!({
        "profile": src.config.profile(),
        "expert_groups": src.expert_groups,
        "pending": src.hf.pending,
        "units": units_json,
        "shards_read": report.shards_read,
        "seconds": start.elapsed().as_secs_f64(),
        "threads": threads,
        "platform": platform(),
    });
    if let Some(path) = args.value("--report") {
        write_json(Path::new(&path), &summary)?;
    }
    println!(
        "{}",
        serde_json::to_string_pretty(&summary).unwrap_or_default()
    );
    Ok(())
}

fn cmd_slice_manifest(args: &Args) -> Result<(), ModernError> {
    let src = slice_source(args)?;
    let records = slices::read_records(&src, &args.path("--out-dir")?)?;
    if records.is_empty() {
        return Err(ModernError::Invalid("no unit records in --out-dir".into()));
    }
    let manifest = slices::build_manifest(&src, &records)?;
    let path = args.path("--out")?;
    let text = arc_inference::modern::package::manifest_text(&manifest)?;
    std::fs::write(&path, format!("{text}\n"))
        .map_err(|e| ModernError::Io(format!("{}: {e}", path.display())))?;
    println!(
        "{}",
        serde_json::to_string_pretty(&json!({
            "manifest_blake3": manifest["manifest_blake3"],
            "complete": manifest["complete"],
            "model_root": manifest["model_root"],
            "pending": manifest["pending"],
            "segments": manifest["segments"].as_array().map_or(0, Vec::len),
            "slices": manifest["slices"].as_array().map_or(0, Vec::len),
        }))
        .unwrap_or_default()
    );
    Ok(())
}

fn cmd_slice_verify(args: &Args) -> Result<(), ModernError> {
    let manifest = SliceManifest::read(&args.path("--manifest")?)?;
    let only: Vec<String> = args
        .value("--only")
        .map(|s| s.split(',').map(str::to_string).collect())
        .unwrap_or_default();
    let report = slices::verify_slices(
        &manifest,
        &args.path("--slices")?,
        &only,
        args.flag("--segments"),
    )?;
    println!(
        "{}",
        serde_json::to_string_pretty(&report).unwrap_or_default()
    );
    Ok(())
}

fn cmd_slice_assemble(args: &Args) -> Result<(), ModernError> {
    let manifest = SliceManifest::read(&args.path("--manifest")?)?;
    let stage = match args.value("--layers") {
        Some(text) => StageSpec::parse(&text)?,
        None => StageSpec::full(&manifest.config()?),
    };
    let report = slices::assemble_stage(
        &manifest,
        &args.path("--slices")?,
        stage,
        &args.path("--out")?,
    )?;
    println!(
        "{}",
        serde_json::to_string_pretty(&report).unwrap_or_default()
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
        "golden" => cmd_golden(&args),
        "stage" => cmd_stage(&args),
        "ppl" => cmd_ppl(&args),
        "tokenize" => cmd_tokenize(&args),
        "render" => cmd_render(&args),
        "slice-plan" => cmd_slice_plan(&args),
        "slice" => cmd_slice(&args),
        "slice-manifest" => cmd_slice_manifest(&args),
        "slice-verify" => cmd_slice_verify(&args),
        "slice-assemble" => cmd_slice_assemble(&args),
        _ => {
            eprintln!("{USAGE}");
            return ExitCode::from(2);
        }
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("arc-mla {command}: {error}");
            ExitCode::FAILURE
        }
    }
}
