//! Speculative decoding on the real network model: exactness first, then
//! speed. CI runs this on 4-vCPU runners (`.github/workflows/speculative-bench.yml`).
//!
//! By default the target is loaded exactly as a community worker loads it
//! (`load_cached_model_canonical_i8`, the legacy split-half canonical INT8
//! profile). `--profile interleaved` loads the versioned GGUF interleaved-RoPE
//! profile instead, for both models, to measure the drafters on coherent text.
//! Every job uses the worker's contract (`try_generate`, legacy v1, with the
//! model's EOS tokens). For each public prompt the driver runs:
//!
//! * `none`: the speculative engine with no drafter, which forwards exactly
//!   the rows plain decoding forwards and is the timing baseline;
//! * every requested drafter and `k`.
//!
//! Every run must return the same tokens and output hash as `none`, and for
//! the first prompt `none` must equal the production `try_generate`. Any
//! difference exits non-zero, so the benchmark is also an exactness gate on
//! the real 7B model. It also measures what one multi-row pass costs against
//! one row, which is what decides whether speculation can pay at all.
//!
//! usage: speculative_bench --model GGUF [--draft-model GGUF] [--kernel scalar|simd]
//!        [--profile legacy|interleaved]
//!        [--max-new-tokens N] [--configs ngram:3,ngram2:3,draft:3] [--prompt-limit N]
//!        [--out FILE.json] [--summary FILE.md]

use arc_inference::cached_integer_model::{
    CachedIntegerModel, KVCache, load_cached_model_canonical_i8,
    load_cached_model_canonical_i8_interleaved_rope,
};
use arc_inference::canonical_simd;
use arc_inference::speculative::{
    DEFAULT_MAX_NGRAM, DEFAULT_MIN_NGRAM, DraftModelDrafter, Drafter, GenerationSemantics,
    NgramDrafter, NoDrafter, SpeculativeConfig, SpeculativeOutput, check_draft_compatible,
};
use serde_json::{Value, json};
use std::sync::Arc;
use std::time::Instant;

/// Public prompts written for this benchmark (no third-party text). Each is
/// sent in the Llama-2 chat form `[INST] ... [/INST]`; the pinned GGUF carries
/// no chat-template metadata, so `apply_chat_template` would pass raw text.
const PROMPTS: [(&str, &str); 4] = [
    (
        "chat",
        "What are three simple habits that help people sleep better? Answer in a short numbered list.",
    ),
    (
        "code",
        "Write a Python function fib(n) that returns the n-th Fibonacci number iteratively. Include a docstring and two example calls.",
    ),
    (
        "repetitive",
        "Repeat the following sentence exactly five times, one per line: The deterministic engine gives the same answer on every computer.",
    ),
    (
        "copy",
        "Copy the following paragraph exactly, without changing a word: A deterministic engine computes every value with integer arithmetic, so the same prompt produces the same answer on every computer. Validators can therefore check a worker by running the same computation again and comparing the result.",
    ),
];

/// How much of each answer the report quotes.
const QUOTE_CHARS: usize = 240;

/// Row counts whose verify cost is measured.
const VERIFY_ROWS: [usize; 6] = [1, 2, 3, 4, 5, 8];
const VERIFY_REPEATS: usize = 3;

struct Args {
    model: String,
    draft_model: Option<String>,
    kernel: String,
    profile: String,
    max_new_tokens: u32,
    configs: Vec<(String, usize)>,
    prompt_limit: usize,
    out: Option<String>,
    summary: Option<String>,
}

fn parse_args() -> Result<Args, String> {
    let mut args = Args {
        model: String::new(),
        draft_model: None,
        kernel: "scalar".into(),
        profile: "legacy".into(),
        max_new_tokens: 64,
        configs: vec![("ngram".into(), 3), ("ngram".into(), 7)],
        prompt_limit: PROMPTS.len(),
        out: None,
        summary: None,
    };
    let mut iter = std::env::args().skip(1);
    while let Some(flag) = iter.next() {
        let mut value = || iter.next().ok_or_else(|| format!("{flag} needs a value"));
        match flag.as_str() {
            "--model" => args.model = value()?,
            "--draft-model" => args.draft_model = Some(value()?),
            "--kernel" => args.kernel = value()?,
            "--profile" => args.profile = value()?,
            "--max-new-tokens" => {
                args.max_new_tokens = value()?.parse().map_err(|e| format!("{e}"))?;
            }
            "--configs" => {
                args.configs = value()?
                    .split(',')
                    .filter(|item| !item.is_empty())
                    .map(|item| -> Result<(String, usize), String> {
                        let (name, k) = item
                            .split_once(':')
                            .ok_or_else(|| format!("config {item:?} is not NAME:K"))?;
                        let k = k.parse().map_err(|e| format!("config {item:?}: {e}"))?;
                        Ok((name.to_string(), k))
                    })
                    .collect::<Result<_, String>>()?;
            }
            "--prompt-limit" => {
                args.prompt_limit = value()?.parse().map_err(|e| format!("{e}"))?;
            }
            "--out" => args.out = Some(value()?),
            "--summary" => args.summary = Some(value()?),
            other => return Err(format!("unknown argument {other}")),
        }
    }
    if args.model.is_empty() {
        return Err("--model GGUF is required".into());
    }
    if args.kernel != "scalar" && args.kernel != "simd" {
        return Err("--kernel must be scalar or simd".into());
    }
    if args.profile != "legacy" && args.profile != "interleaved" {
        return Err("--profile must be legacy or interleaved".into());
    }
    Ok(args)
}

/// Load a GGUF in the canonical INT8 profile the run asked for.
fn load_model(path: &str, interleaved: bool) -> Result<CachedIntegerModel, String> {
    let loaded = if interleaved {
        load_cached_model_canonical_i8_interleaved_rope(path)
    } else {
        load_cached_model_canonical_i8(path)
    };
    loaded.map_err(|e| format!("load {path}: {e}"))
}

fn seconds(nanos: u64) -> f64 {
    nanos as f64 / 1e9
}

fn median(values: &mut [f64]) -> f64 {
    values.sort_by(f64::total_cmp);
    values[values.len() / 2]
}

/// Peak resident memory on Linux, from the kernel's own accounting.
fn peak_rss_mib() -> Option<f64> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    let line = status.lines().find(|line| line.starts_with("VmHWM:"))?;
    let kib: f64 = line.split_whitespace().nth(1)?.parse().ok()?;
    Some(kib / 1024.0)
}

/// Milliseconds one pass of `rows` rows takes, after `prefix` is cached.
fn verify_cost_curve(model: &CachedIntegerModel, prefix: &[u32]) -> Vec<Value> {
    let mut cache = KVCache::new(model.config.n_layers);
    if model
        .prefill_canonical_i8_batched(prefix, &mut cache, 64, false)
        .is_none()
    {
        for &token in prefix {
            let _ = model.forward_one_token(token, &mut cache);
        }
    }
    let base = cache.seq_len;
    let mut single_ms = 0.0;
    let mut curve = Vec::new();
    for rows in VERIFY_ROWS {
        // Real token ids from the prompt, so activations are realistic.
        let tokens: Vec<u32> = prefix.iter().copied().cycle().skip(3).take(rows).collect();
        let mut times = Vec::with_capacity(VERIFY_REPEATS);
        for _ in 0..VERIFY_REPEATS {
            let started = Instant::now();
            let logits = model.forward_rows_exact(&tokens, &mut cache);
            times.push(started.elapsed().as_secs_f64() * 1e3);
            assert_eq!(logits.len(), rows);
            cache.truncate(base);
        }
        let ms = median(&mut times);
        if rows == 1 {
            single_ms = ms;
        }
        let ratio = if single_ms > 0.0 { ms / single_ms } else { 0.0 };
        println!("verify pass: {rows} rows {ms:.0} ms = {ratio:.2} x one row");
        curve.push(json!({
            "rows": rows,
            "median_ms": ms,
            "cost_vs_one_row": ratio,
            "samples_ms": times,
        }));
    }
    curve
}

fn run_json(name: &str, k: usize, out: &SpeculativeOutput, identical: bool) -> Value {
    let stats = &out.stats;
    let decode_s = seconds(stats.decode_nanos());
    let decode_rate = if decode_s > 0.0 {
        stats.emitted_tokens as f64 / decode_s
    } else {
        0.0
    };
    json!({
        "drafter": name,
        "k": k,
        "identical_to_plain": identical,
        "tokens": out.tokens.len(),
        "output_hash": hex::encode(out.output_hash.0),
        "prefill_s": seconds(stats.prefill_nanos),
        "decode_s": decode_s,
        "total_s": seconds(stats.total_nanos),
        "decode_tokens_per_s": decode_rate,
        "passes": stats.passes,
        "drafted_passes": stats.drafted_passes,
        "drafted_tokens": stats.drafted_tokens,
        "accepted_tokens": stats.accepted_tokens,
        "target_rows": stats.target_rows,
        "tokens_per_pass": stats.tokens_per_pass(),
        "acceptance_rate": stats.acceptance_rate(),
        "draft_s": seconds(stats.draft_nanos),
        "verify_s": seconds(stats.verify_nanos),
    })
}

fn main() -> Result<(), String> {
    let args = parse_args()?;
    let started = Instant::now();

    let simd = args.kernel == "simd";
    if simd && !canonical_simd::dotprod_available() {
        return Err("the vectorised kernel is not available on this CPU".into());
    }
    canonical_simd::set_fast_canonical_kernel(simd);

    let interleaved = args.profile == "interleaved";
    let load_started = Instant::now();
    let model = load_model(&args.model, interleaved)?;
    let load_s = load_started.elapsed().as_secs_f64();
    if !model.has_canonical_i8_profile() || !model.has_all_transformer_layers() {
        return Err("the target did not load as a complete canonical INT8 model".into());
    }
    println!(
        "target: {} layers, d_model {}, vocab {}, profile {:?}, loaded in {load_s:.0} s, {} rayon threads, kernel {}",
        model.config.n_layers,
        model.config.d_model,
        model.config.vocab_size,
        model.arithmetic_profile(),
        rayon::current_num_threads(),
        args.kernel
    );

    let mut draft_report = json!(null);
    let draft: Option<Arc<CachedIntegerModel>> = match &args.draft_model {
        None => None,
        Some(path) => {
            let draft_started = Instant::now();
            let draft = load_model(path, interleaved)?;
            let compatible = check_draft_compatible(&model, &draft);
            draft_report = json!({
                "path": path,
                "layers": draft.config.n_layers,
                "d_model": draft.config.d_model,
                "n_kv_heads": draft.config.n_kv_heads,
                "vocab": draft.config.vocab_size,
                "load_s": draft_started.elapsed().as_secs_f64(),
                "compatible": compatible.is_ok(),
                "incompatibility": compatible.as_ref().err(),
            });
            println!("draft model: {draft_report}");
            compatible.map(|()| Arc::new(draft)).ok()
        }
    };

    let prompts: Vec<(&str, Vec<u32>)> = PROMPTS
        .iter()
        .take(args.prompt_limit)
        .map(|(kind, text)| (*kind, model.encode(&format!("[INST] {text} [/INST]"))))
        .collect();
    if prompts.is_empty() {
        return Err("--prompt-limit leaves no prompt to run".into());
    }

    // What one pass of r rows costs against one row decides whether a pass
    // that verifies guesses can ever be cheaper than plain decoding.
    let verify_curve = verify_cost_curve(&model, &prompts[0].1);

    let eos = model.config.eos_tokens.clone();
    let semantics = GenerationSemantics::LegacyV1;
    let mut all_identical = true;
    let mut prompt_reports = Vec::new();
    for (index, (kind, prompt)) in prompts.iter().enumerate() {
        println!(
            "\n== {kind}: {} prompt tokens, {} new tokens",
            prompt.len(),
            args.max_new_tokens
        );
        let mut runs = Vec::new();

        let baseline = model
            .try_generate_speculative(
                prompt,
                args.max_new_tokens,
                &eos,
                semantics,
                &mut NoDrafter,
                SpeculativeConfig::with_max_draft(0),
            )
            .map_err(|e| e.to_string())?;
        let mut production_matches = Value::Null;
        if index == 0 {
            let plain_started = Instant::now();
            let (tokens, hash) = model
                .try_generate(prompt, args.max_new_tokens, &eos)
                .map_err(|e| e.to_string())?;
            let plain_s = plain_started.elapsed().as_secs_f64();
            let same = tokens == baseline.tokens && hash == baseline.output_hash;
            all_identical &= same;
            production_matches = json!({ "identical": same, "total_s": plain_s });
            println!("production try_generate: identical={same} total {plain_s:.1} s");
        }
        println!(
            "none: {} tokens, prefill {:.1} s, decode {:.1} s ({:.3} tok/s), hash {}",
            baseline.tokens.len(),
            seconds(baseline.stats.prefill_nanos),
            seconds(baseline.stats.decode_nanos()),
            baseline.stats.emitted_tokens as f64 / seconds(baseline.stats.decode_nanos()),
            hex::encode(baseline.output_hash.0)
        );
        runs.push(run_json("none", 0, &baseline, true));
        let answer: String = model
            .decode(&baseline.tokens)
            .chars()
            .take(QUOTE_CHARS)
            .collect();
        println!("answer: {answer:?}");

        for (name, k) in &args.configs {
            let mut drafter: Box<dyn Drafter> = match name.as_str() {
                "ngram" => Box::new(NgramDrafter::new(DEFAULT_MIN_NGRAM, DEFAULT_MAX_NGRAM)),
                // Classic prompt lookup: two- to four-token suffixes.
                "ngram2" => Box::new(NgramDrafter::new(2, 4)),
                "draft" => match &draft {
                    Some(draft) => Box::new(DraftModelDrafter::new(Arc::clone(draft))),
                    None => {
                        println!("skipping draft:{k}: no compatible draft model");
                        continue;
                    }
                },
                other => return Err(format!("unknown drafter {other}")),
            };
            let out = model
                .try_generate_speculative(
                    prompt,
                    args.max_new_tokens,
                    &eos,
                    semantics,
                    drafter.as_mut(),
                    SpeculativeConfig::with_max_draft(*k),
                )
                .map_err(|e| e.to_string())?;
            let identical =
                out.tokens == baseline.tokens && out.output_hash == baseline.output_hash;
            all_identical &= identical;
            let speedup =
                seconds(baseline.stats.decode_nanos()) / seconds(out.stats.decode_nanos());
            println!(
                "{name}:{k}: identical={identical} decode {:.1} s ({speedup:.2}x), {:.2} tokens/pass, acceptance {:.0}%, passes {}",
                seconds(out.stats.decode_nanos()),
                out.stats.tokens_per_pass(),
                100.0 * out.stats.acceptance_rate(),
                out.stats.passes
            );
            runs.push(run_json(name, *k, &out, identical));
        }
        prompt_reports.push(json!({
            "kind": kind,
            "prompt_tokens": prompt.len(),
            "answer_head": answer,
            "production_try_generate": production_matches,
            "runs": runs,
        }));
    }

    let report = json!({
        "schema": "arc.speculative-bench.v1",
        "label": "CI measurement",
        "model": args.model,
        "profile": model.arithmetic_profile(),
        "kernel": args.kernel,
        "rayon_threads": rayon::current_num_threads(),
        "max_new_tokens": args.max_new_tokens,
        "load_s": load_s,
        "draft_model": draft_report,
        "verify_cost": verify_curve,
        "prompts": prompt_reports,
        "all_identical": all_identical,
        "peak_rss_mib": peak_rss_mib(),
        "elapsed_s": started.elapsed().as_secs_f64(),
    });
    if let Some(path) = &args.out {
        let text = serde_json::to_string_pretty(&report).map_err(|e| e.to_string())?;
        std::fs::write(path, text).map_err(|e| format!("write {path}: {e}"))?;
    }
    if let Some(path) = &args.summary {
        std::fs::write(path, markdown(&report)).map_err(|e| format!("write {path}: {e}"))?;
    }
    if !all_identical {
        return Err("a speculative run differed from plain decoding".into());
    }
    println!("\nall runs identical to plain decoding");
    Ok(())
}

fn markdown(report: &Value) -> String {
    let mut out = String::new();
    let kernel = report["kernel"].as_str().unwrap_or("?");
    out.push_str(&format!(
        "### Speculative decoding, CI measurement: {kernel} kernel, {} threads\n\n",
        report["rayon_threads"]
    ));
    out.push_str(&format!(
        "Profile `{}`; {} new tokens per prompt; every run identical to plain decoding: **{}**.\n\n",
        report["profile"].as_str().unwrap_or("?"),
        report["max_new_tokens"],
        report["all_identical"]
    ));
    out.push_str("| rows in one pass | median ms | cost vs one row |\n|---:|---:|---:|\n");
    for row in report["verify_cost"].as_array().into_iter().flatten() {
        out.push_str(&format!(
            "| {} | {:.0} | {:.2} |\n",
            row["rows"],
            row["median_ms"].as_f64().unwrap_or(0.0),
            row["cost_vs_one_row"].as_f64().unwrap_or(0.0)
        ));
    }
    out.push_str(
        "\n| prompt | drafter | k | decode tok/s | decode speedup | end-to-end speedup | tokens per pass | acceptance | identical |\n|---|---|---:|---:|---:|---:|---:|---:|---|\n",
    );
    for prompt in report["prompts"].as_array().into_iter().flatten() {
        let runs = prompt["runs"].as_array().cloned().unwrap_or_default();
        let base_decode = runs
            .first()
            .and_then(|run| run["decode_s"].as_f64())
            .unwrap_or(0.0);
        let base_total = runs
            .first()
            .and_then(|run| run["total_s"].as_f64())
            .unwrap_or(0.0);
        for run in &runs {
            let decode = run["decode_s"].as_f64().unwrap_or(0.0);
            let total = run["total_s"].as_f64().unwrap_or(0.0);
            let ratio = |base: f64, value: f64| if value > 0.0 { base / value } else { 0.0 };
            out.push_str(&format!(
                "| {} | {} | {} | {:.3} | {:.2}x | {:.2}x | {:.2} | {:.0}% | {} |\n",
                prompt["kind"].as_str().unwrap_or("?"),
                run["drafter"].as_str().unwrap_or("?"),
                run["k"],
                run["decode_tokens_per_s"].as_f64().unwrap_or(0.0),
                ratio(base_decode, decode),
                ratio(base_total, total),
                run["tokens_per_pass"].as_f64().unwrap_or(0.0),
                100.0 * run["acceptance_rate"].as_f64().unwrap_or(0.0),
                run["identical_to_plain"]
            ));
        }
    }
    out
}
