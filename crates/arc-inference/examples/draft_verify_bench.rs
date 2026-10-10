//! Same-model drafter plus exact verifier on the real network model:
//! exactness first, then speed. CI runs this on 4-vCPU runners
//! (`.github/workflows/draft-verify-bench.yml`).
//!
//! The target is Llama-2-7B-Chat Q4_K_M loaded as the canonical INT8 legacy
//! profile (`--profile legacy`, what workers run) or the GGUF interleaved-RoPE
//! profile (`--profile interleaved`). The drafter is llama.cpp running the
//! same GGUF in another process (`tools/llama-drafter`). For each public
//! prompt the bench runs, on the worker's contract (`try_generate`):
//!
//! * `reference`: production `try_generate`, the exactness reference;
//! * `plain`: the verifier with drafting off (plain exact decoding after the
//!   same multi-row prefill), the speed baseline;
//! * `drafted`: llama.cpp proposing and ARC verifying with the measured draft
//!   policy, on the whole model;
//! * for the first prompt of each category, `drafted` again on a 2-stage and a
//!   4-stage split.
//!
//! Every run must return the reference's tokens and output hash; any
//! difference exits non-zero. Times are wall clock on shared CI runners.
//!
//! usage: draft_verify_bench --model GGUF --drafter PATH --drafter-model GGUF
//!        [--profile legacy|interleaved] [--kernel scalar|simd]
//!        [--max-new-tokens N] [--cap K] [--out FILE.json] [--summary FILE.md]

use arc_inference::cached_integer_model::{
    CachedIntegerModel, load_cached_model_canonical_i8,
    load_cached_model_canonical_i8_interleaved_rope,
};
use arc_inference::canonical_simd;
use arc_inference::draft_verify::{
    DraftOutput, DraftPolicy, DraftStats, DraftVerifyConfig, ExactSemantics, NoDrafter,
    ProcessDrafter, TokenDrafter, generate_with_drafter,
};
use serde_json::{Value, json};
use std::process::{Command, Stdio};
use std::time::Instant;

/// Public prompts written for these benches (no third-party text), two per
/// category, from the agreement experiment's set. Each is sent in the Llama-2
/// chat form `[INST] ... [/INST]`.
const PROMPTS: [(&str, &str); 10] = [
    (
        "chat",
        "What are three simple habits that help people sleep better? Answer in a short numbered list.",
    ),
    (
        "chat",
        "I am visiting a new city for one day. What is a good way to plan the day?",
    ),
    (
        "code",
        "Write a Python function fib(n) that returns the n-th Fibonacci number iteratively. Include a docstring and two example calls.",
    ),
    (
        "code",
        "Write a Python function that counts how many times each word appears in a text and returns a dictionary.",
    ),
    (
        "math",
        "A train travels 180 kilometers in 2 hours and 15 minutes. What is its average speed in kilometers per hour?",
    ),
    (
        "math",
        "Find the derivative of f(x) = 4x^3 - 2x + 9 and explain the rules you used.",
    ),
    (
        "longform",
        "Describe the water cycle in detail, from evaporation to precipitation.",
    ),
    (
        "longform",
        "Explain how a web page travels from a server to your browser when you type an address.",
    ),
    (
        "copy",
        "Copy the following paragraph exactly, without changing a word: A deterministic engine computes every value with integer arithmetic, so the same prompt produces the same answer on every computer. Validators can therefore check a worker by running the same computation again and comparing the result.",
    ),
    (
        "copy",
        "Extract the product name, unit price and quantity from this order confirmation and present them as a table: Thank you for your order. You bought 2 Trail Runner backpacks at 59.99 dollars each, 1 Summit water bottle at 14.50 dollars, and 3 pairs of Alpine wool socks at 12.00 dollars per pair.",
    ),
];

struct Args {
    model: String,
    drafter: String,
    drafter_model: String,
    profile: String,
    kernel: String,
    max_new_tokens: u32,
    cap: usize,
    out: Option<String>,
    summary: Option<String>,
}

fn parse_args() -> Result<Args, String> {
    let mut args = Args {
        model: String::new(),
        drafter: String::new(),
        drafter_model: String::new(),
        profile: "legacy".into(),
        kernel: "simd".into(),
        max_new_tokens: 128,
        cap: DraftPolicy::MEASURED.cap,
        out: None,
        summary: None,
    };
    let mut iter = std::env::args().skip(1);
    while let Some(flag) = iter.next() {
        let mut value = || iter.next().ok_or_else(|| format!("{flag} needs a value"));
        match flag.as_str() {
            "--model" => args.model = value()?,
            "--drafter" => args.drafter = value()?,
            "--drafter-model" => args.drafter_model = value()?,
            "--profile" => args.profile = value()?,
            "--kernel" => args.kernel = value()?,
            "--max-new-tokens" => {
                args.max_new_tokens = value()?.parse().map_err(|e| format!("{e}"))?;
            }
            "--cap" => args.cap = value()?.parse().map_err(|e| format!("{e}"))?,
            "--out" => args.out = Some(value()?),
            "--summary" => args.summary = Some(value()?),
            other => return Err(format!("unknown argument {other}")),
        }
    }
    if args.model.is_empty() || args.drafter.is_empty() || args.drafter_model.is_empty() {
        return Err("--model, --drafter and --drafter-model are required".into());
    }
    if args.profile != "legacy" && args.profile != "interleaved" {
        return Err("--profile must be legacy or interleaved".into());
    }
    if args.kernel != "scalar" && args.kernel != "simd" {
        return Err("--kernel must be scalar or simd".into());
    }
    Ok(args)
}

fn seconds(nanos: u64) -> f64 {
    nanos as f64 / 1e9
}

fn rate(tokens: usize, nanos: u64) -> f64 {
    if nanos == 0 {
        0.0
    } else {
        tokens as f64 / seconds(nanos)
    }
}

fn stats_json(stats: &DraftStats) -> Value {
    json!({
        "passes": stats.passes,
        "plain_steps": stats.plain_steps,
        "drafted": stats.drafted,
        "accepted": stats.accepted,
        "rows_verified": stats.rows_verified,
        "rows_rolled_back": stats.rows_rolled_back,
        "drafter_errors": stats.drafter_errors,
        "draft_lengths": stats.draft_lengths,
        "prefill_s": seconds(stats.prefill_nanos),
        "draft_s": seconds(stats.draft_nanos),
        "verify_s": seconds(stats.verify_nanos),
    })
}

/// One run's report, and whether it equals the reference.
fn run_json(
    name: &str,
    output: &DraftOutput,
    reference: &(Vec<u32>, String),
    drafter_micros: u64,
) -> (Value, bool) {
    let hash = hex::encode(output.output_hash.0);
    let identical = output.tokens == reference.0 && hash == reference.1;
    let stats = &output.stats;
    let decode_nanos = stats.draft_nanos + stats.verify_nanos;
    let steps = stats.passes + stats.plain_steps;
    (
        json!({
            "run": name,
            "identical": identical,
            "tokens": output.tokens.len(),
            "output_hash": hash,
            "decode_tok_s": rate(output.tokens.len(), decode_nanos),
            "tokens_per_step": if steps == 0 { 0.0 } else { output.tokens.len() as f64 / steps as f64 },
            "acceptance": if stats.drafted == 0 { 0.0 } else { stats.accepted as f64 / stats.drafted as f64 },
            "drafter_reported_s": drafter_micros as f64 / 1e6,
            "stats": stats_json(stats),
        }),
        identical,
    )
}

fn load_model(path: &str, interleaved: bool) -> Result<CachedIntegerModel, String> {
    if interleaved {
        load_cached_model_canonical_i8_interleaved_rope(path)
    } else {
        load_cached_model_canonical_i8(path)
    }
    .map_err(|e| format!("load {path}: {e}"))
}

fn main() -> Result<(), String> {
    let args = parse_args()?;
    let started = Instant::now();
    let simd = args.kernel == "simd";
    if simd && !canonical_simd::dotprod_available() {
        return Err("the vectorised kernel is not available on this CPU".into());
    }
    canonical_simd::set_fast_canonical_kernel(simd);

    let load_started = Instant::now();
    let model = load_model(&args.model, args.profile == "interleaved")?;
    let load_s = load_started.elapsed().as_secs_f64();
    if !model.has_canonical_i8_profile() || !model.has_all_transformer_layers() {
        return Err("the model did not load as a complete canonical INT8 model".into());
    }
    let n_layers = model.config.n_layers;
    let eos = model.config.eos_tokens.clone();
    println!(
        "target: {n_layers} layers, vocab {}, profile {}, loaded in {load_s:.0} s, {} rayon threads, kernel {}",
        model.config.vocab_size,
        model.arithmetic_profile(),
        rayon::current_num_threads(),
        args.kernel
    );

    let mut command = Command::new(args.drafter.as_str());
    command
        .arg(args.drafter_model.as_str())
        .arg("4096")
        .env("OMP_WAIT_POLICY", "PASSIVE")
        .stderr(Stdio::inherit());
    let drafter_started = Instant::now();
    let mut drafter = ProcessDrafter::spawn(&mut command, "llama.cpp", model.config.vocab_size)
        .map_err(|e| format!("drafter: {e}"))?;
    let drafter_load_s = drafter_started.elapsed().as_secs_f64();
    println!("drafter {} ready in {drafter_load_s:.0} s", drafter.label());

    let policy = DraftPolicy {
        cap: args.cap,
        ..DraftPolicy::MEASURED
    };
    let whole = DraftVerifyConfig {
        semantics: ExactSemantics::Worker,
        stage_ends: vec![n_layers],
        policy,
    };
    let plain_config = DraftVerifyConfig {
        policy: DraftPolicy::OFF,
        ..whole.clone()
    };
    let splits = [
        vec![n_layers / 2, n_layers],
        vec![n_layers / 4, n_layers / 2, 3 * n_layers / 4, n_layers],
    ];

    let mut all_identical = true;
    let mut reports = Vec::new();
    let mut previous_category = "";
    for (kind, text) in PROMPTS {
        let prompt = model.encode(&format!("[INST] {text} [/INST]"));
        println!(
            "\n== {kind}: {} prompt tokens, up to {} new tokens",
            prompt.len(),
            args.max_new_tokens
        );
        let reference_started = Instant::now();
        let (tokens, hash) = model
            .try_generate(&prompt, args.max_new_tokens, &eos)
            .map_err(|e| e.to_string())?;
        let reference_s = reference_started.elapsed().as_secs_f64();
        let reference = (tokens, hex::encode(hash.0));
        println!(
            "reference try_generate: {} tokens in {reference_s:.1} s, hash {}",
            reference.0.len(),
            &reference.1[..12]
        );

        let mut runs = Vec::new();
        let plain = generate_with_drafter(
            &model,
            &prompt,
            args.max_new_tokens,
            &eos,
            &mut NoDrafter,
            &plain_config,
        )
        .map_err(|e| e.to_string())?;
        let (plain_report, same) = run_json("plain", &plain, &reference, 0);
        all_identical &= same;
        println!("plain: {plain_report}");
        runs.push(plain_report);

        let mut configs = vec![("drafted", whole.clone())];
        if kind != previous_category {
            for ends in &splits {
                configs.push((
                    if ends.len() == 2 {
                        "drafted, 2 stages"
                    } else {
                        "drafted, 4 stages"
                    },
                    DraftVerifyConfig {
                        stage_ends: ends.clone(),
                        ..whole.clone()
                    },
                ));
            }
        }
        previous_category = kind;
        for (name, config) in configs {
            let before = drafter.reported_micros();
            let output = generate_with_drafter(
                &model,
                &prompt,
                args.max_new_tokens,
                &eos,
                &mut drafter,
                &config,
            )
            .map_err(|e| e.to_string())?;
            let (report, same) = run_json(
                name,
                &output,
                &reference,
                drafter.reported_micros() - before,
            );
            all_identical &= same;
            println!("{name}: {report}");
            runs.push(report);
        }
        reports.push(json!({
            "category": kind,
            "prompt_tokens": prompt.len(),
            "reference_s": reference_s,
            "reference_tokens": reference.0.len(),
            "reference_hash": reference.1,
            "text": model.decode(&reference.0).chars().take(240).collect::<String>(),
            "runs": runs,
        }));
    }

    let report = json!({
        "label": "MEASURED on a CI runner",
        "profile": args.profile,
        "arithmetic_profile": model.arithmetic_profile(),
        "kernel": args.kernel,
        "rayon_threads": rayon::current_num_threads(),
        "max_new_tokens": args.max_new_tokens,
        "policy": { "min": policy.min, "start": policy.start, "cap": policy.cap, "probe_after": policy.probe_after },
        "load_s": load_s,
        "drafter_load_s": drafter_load_s,
        "all_identical": all_identical,
        "total_s": started.elapsed().as_secs_f64(),
        "prompts": reports,
    });
    if let Some(path) = &args.out {
        let text = serde_json::to_string_pretty(&report).map_err(|e| e.to_string())?;
        std::fs::write(path, text).map_err(|e| format!("write {path}: {e}"))?;
    }
    if let Some(path) = &args.summary {
        std::fs::write(path, markdown(&report)).map_err(|e| format!("write {path}: {e}"))?;
    }
    if !all_identical {
        return Err("a drafted run differs from plain exact decoding".into());
    }
    println!(
        "\nevery run identical to plain exact decoding; total {:.0} s",
        started.elapsed().as_secs_f64()
    );
    Ok(())
}

/// Per-category totals: tokens over decode seconds, so long outputs weigh more.
fn markdown(report: &Value) -> String {
    let mut out = format!(
        "{} profile, kernel {}, up to {} new tokens, draft policy {}. MEASURED on a CI runner.\n\n\
         | category | prompts | tokens | plain tok/s | drafted tok/s | speedup | tokens/step | acceptance | drafter s | verifier s | drafter share | identical |\n\
         |---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---|\n",
        report["profile"].as_str().unwrap_or("?"),
        report["kernel"].as_str().unwrap_or("?"),
        report["max_new_tokens"],
        report["policy"],
    );
    let prompts = report["prompts"].as_array().cloned().unwrap_or_default();
    let mut categories: Vec<&str> = Vec::new();
    for p in &prompts {
        let kind = p["category"].as_str().unwrap_or("?");
        if !categories.contains(&kind) {
            categories.push(kind);
        }
    }
    categories.push("all");
    for kind in categories {
        let mut n = 0;
        let (mut tokens, mut plain_s, mut drafted_s, mut draft_s, mut verify_s) =
            (0.0, 0.0, 0.0, 0.0, 0.0);
        let (mut steps, mut drafted, mut accepted) = (0.0, 0.0, 0.0);
        let mut identical = true;
        for p in prompts
            .iter()
            .filter(|p| kind == "all" || p["category"].as_str() == Some(kind))
        {
            n += 1;
            for run in p["runs"].as_array().into_iter().flatten() {
                identical &= run["identical"].as_bool().unwrap_or(false);
                let s = &run["stats"];
                let f = |v: &Value| v.as_f64().unwrap_or(0.0);
                match run["run"].as_str() {
                    Some("plain") => plain_s += f(&s["verify_s"]),
                    Some("drafted") => {
                        tokens += f(&run["tokens"]);
                        drafted_s += f(&s["draft_s"]) + f(&s["verify_s"]);
                        draft_s += f(&s["draft_s"]);
                        verify_s += f(&s["verify_s"]);
                        steps += f(&s["passes"]) + f(&s["plain_steps"]);
                        drafted += f(&s["drafted"]);
                        accepted += f(&s["accepted"]);
                    }
                    _ => {}
                }
            }
        }
        let ratio = |a: f64, b: f64| if b > 0.0 { a / b } else { 0.0 };
        out.push_str(&format!(
            "| {kind} | {n} | {tokens:.0} | {:.2} | {:.2} | {:.2}x | {:.2} | {:.0}% | {draft_s:.1} | {verify_s:.1} | {:.0}% | {identical} |\n",
            ratio(tokens, plain_s),
            ratio(tokens, drafted_s),
            ratio(plain_s, drafted_s),
            ratio(tokens, steps),
            100.0 * ratio(accepted, drafted),
            100.0 * ratio(draft_s, drafted_s),
        ));
    }
    out
}
