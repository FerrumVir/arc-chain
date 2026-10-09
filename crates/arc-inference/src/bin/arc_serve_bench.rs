//! `arc-serve-bench`: CI measurements of the serving layer
//! (`arc_inference::modern::serving`) on a real integer package, with the
//! proof, on the same run, that it changes no byte.
//!
//! Every number it writes is a measurement of this process on this machine
//! (a CI runner unless stated otherwise); nothing is projected.
//!
//! * `golden`: the golden cases through continuous batching, chunked prefill
//!   and speculation, hashing every logits vector. The matrix digest must
//!   equal `--expect` (the `arc-modern golden` digest). The cases then run
//!   again through a warm prefix cache, and the tokens must not change.
//! * `batching`: aggregate and per-stream decode tok/s at each concurrency,
//!   with every stream's tokens compared across concurrencies.
//! * `prefix`: time to first token on a prompt-heavy agent replay, with and
//!   without the prefix cache, and the outputs compared.
//! * `speculative`: decode tok/s with and without prompt-lookup speculation,
//!   with the outputs compared.
//! * `all`: the four on one loaded model, one JSON report and a Markdown
//!   summary.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Instant;

use arc_inference::canonical_simd;
use arc_inference::modern::arith::{self, DyadicMatrix, Selection};
use arc_inference::modern::bpe::ByteLevelBpe;
use arc_inference::modern::chat::{ChatPrompt, render};
use arc_inference::modern::model::ModernModel;
use arc_inference::modern::serving::dense::DenseModel;
use arc_inference::modern::serving::gemm::{digit_kernel_enabled, exact_sums, project_rows};
use arc_inference::modern::serving::prefix::PrefixConfig;
use arc_inference::modern::serving::scheduler::{
    Completion, Generated, Request, Scheduler, SchedulerConfig, StepStats,
};
use arc_inference::modern::serving::spec::PromptLookup;
use arc_inference::modern::{ModernError, PROFILE, hex_lower, package};
use serde_json::{Map, Value, json};

const USAGE: &str = "usage: arc-serve-bench <golden|batching|prefix|speculative|all> \
--package PKG --tokenizer tokenizer.json --out REPORT.json
  [--summary REPORT.md] [--cases CASES.json] [--expect DIGEST] [--workload WORKLOAD.json]
  [--kernel scalar|simd] [--threads N] [--concurrency 1,2,4,8,16,32] [--gen N]
  [--prefix-gen N] [--spec-gen N] [--draft N]
       arc-serve-bench gemm --out REPORT.json [--summary REPORT.md] [--kernel scalar|simd]
  [--threads N] [--widths 1,2,4,...] [--repeats N]   (synthetic matrices, no model)";

const DEFAULT_TODAY: &str = "06 October 2026";
/// `<|im_end|>`, SmolLM3's end of turn.
const EOS_IM_END: u32 = 128_012;
/// Prefix cache used by the measurements: 16-position blocks, 2 GiB.
const PREFIX: PrefixConfig = PrefixConfig {
    block: 16,
    capacity_bytes: 1 << 31,
};
/// Prompt lookup as the measurements use it.
const LOOKUP: PromptLookup = PromptLookup {
    min_ngram: 2,
    max_ngram: 4,
};

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

    fn path(&self, name: &str) -> Result<PathBuf, ModernError> {
        self.value(name)
            .map(PathBuf::from)
            .ok_or_else(|| ModernError::Invalid(format!("missing {name}\n\n{USAGE}")))
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

fn ids_from(value: Option<&Value>) -> Result<Vec<u32>, ModernError> {
    let Some(list) = value else {
        return Ok(Vec::new());
    };
    list.as_array()
        .ok_or_else(|| ModernError::Invalid("expected a list of token ids".into()))?
        .iter()
        .map(|x| {
            x.as_u64()
                .and_then(|id| u32::try_from(id).ok())
                .ok_or_else(|| ModernError::Invalid("token ids must be u32".into()))
        })
        .collect()
}

fn strings<'a>(workload: &'a Value, key: &str) -> Result<Vec<&'a str>, ModernError> {
    workload
        .get(key)
        .and_then(Value::as_array)
        .and_then(|items| items.iter().map(Value::as_str).collect::<Option<Vec<_>>>())
        .ok_or_else(|| ModernError::Invalid(format!("workload has no string list {key}")))
}

fn mean(values: &[f64]) -> f64 {
    if values.is_empty() {
        0.0
    } else {
        values.iter().sum::<f64>() / values.len() as f64
    }
}

fn median(values: &[f64]) -> f64 {
    let mut sorted = values.to_vec();
    sorted.sort_by(f64::total_cmp);
    match sorted.len() {
        0 => 0.0,
        n if n.is_multiple_of(2) => (sorted[n / 2 - 1] + sorted[n / 2]) / 2.0,
        n => sorted[n / 2],
    }
}

fn ratio(numerator: f64, denominator: f64) -> f64 {
    if denominator > 0.0 {
        numerator / denominator
    } else {
        0.0
    }
}

/// The loaded model and what every measurement shares.
struct Bench {
    model: ModernModel,
    identity: [u8; 32],
    tokenizer: ByteLevelBpe,
    today: String,
}

impl Bench {
    fn dense(&self) -> DenseModel<'_> {
        DenseModel::new(&self.model, self.identity)
    }

    fn chat(&self, system: Option<&str>, user: &str) -> Result<Vec<u32>, ModernError> {
        self.tokenizer.encode(&render(&ChatPrompt {
            system,
            user,
            thinking: false,
            today: &self.today,
        }))
    }
}

/// Submit `requests`, run until idle, and return the completions by id.
fn run(
    scheduler: &mut Scheduler<'_, DenseModel<'_>>,
    requests: Vec<Request>,
) -> Result<HashMap<u64, Completion>, ModernError> {
    for request in requests {
        scheduler.submit(request)?;
    }
    Ok(scheduler.run().into_iter().map(|c| (c.id, c)).collect())
}

/// The output of a completion that must have succeeded.
fn generated(completions: &HashMap<u64, Completion>, id: u64) -> Result<&Generated, ModernError> {
    let completion = completions
        .get(&id)
        .ok_or_else(|| ModernError::Invalid(format!("request {id} never completed")))?;
    completion
        .result
        .as_ref()
        .map_err(|e| ModernError::Invalid(format!("request {id} failed: {e}")))
}

fn chat_request(id: usize, prompt: &[u32], max_tokens: usize, eos: &[u32]) -> Request {
    Request {
        id: id as u64,
        prompt: prompt.to_vec(),
        max_tokens,
        eos: eos.to_vec(),
        selection: Selection::Rp64Argmax,
    }
}

/// The golden cases: the matrix digest through batching, chunked prefill and
/// speculation, then the tokens again through a warm prefix cache.
fn bench_golden(bench: &Bench, cases: &Value, expect: Option<&str>) -> Result<Value, ModernError> {
    let list = cases
        .get("cases")
        .and_then(Value::as_array)
        .ok_or_else(|| ModernError::Invalid("cases file has no cases".into()))?;
    let mut ids = Vec::new();
    let mut requests = Vec::new();
    for (index, case) in list.iter().enumerate() {
        let id = case
            .get("id")
            .and_then(Value::as_str)
            .ok_or_else(|| ModernError::Invalid("every case needs an id".into()))?;
        let user = case
            .get("user")
            .and_then(Value::as_str)
            .ok_or_else(|| ModernError::Invalid(format!("case {id} has no user text")))?;
        let prompt = bench.tokenizer.encode(&render(&ChatPrompt {
            system: case.get("system").and_then(Value::as_str),
            user,
            thinking: case
                .get("thinking")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            today: case
                .get("today")
                .and_then(Value::as_str)
                .unwrap_or(&bench.today),
        }))?;
        let max_tokens = case
            .get("max_tokens")
            .and_then(Value::as_u64)
            .and_then(|v| usize::try_from(v).ok())
            .ok_or_else(|| ModernError::Invalid(format!("case {id} needs max_tokens")))?;
        let selection = Selection::parse(
            case.get("selection")
                .and_then(Value::as_str)
                .unwrap_or("rp64-argmax"),
        )?;
        ids.push(id.to_string());
        requests.push(Request {
            id: index as u64,
            prompt,
            max_tokens,
            eos: ids_from(case.get("eos"))?,
            selection,
        });
    }
    let dense = bench.dense();
    let batched_config = SchedulerConfig {
        max_running: requests.len().max(1),
        step_tokens: 512,
        prefill_chunk: 32,
        all_logits: true,
        draft_tokens: 6,
        prefix: None,
        kv_digests: false,
    };
    let mut scheduler = Scheduler::new(&dense, batched_config).with_drafter(Box::new(LOOKUP));
    let started = Instant::now();
    let batched = run(&mut scheduler, requests.clone())?;
    let batched_seconds = started.elapsed().as_secs_f64();
    let mut entries = Vec::new();
    let mut rows = Vec::new();
    for (index, (id, request)) in ids.iter().zip(&requests).enumerate() {
        let out = generated(&batched, index as u64)?;
        entries.push(json!({
            "id": id,
            "logits_digest": hex_lower(&arith::logits_digest(&out.logits_hashes)),
            "output_hash": hex_lower(&out.output_hash),
            "tokens": out.tokens,
        }));
        rows.push(json!({
            "id": id,
            "prompt_tokens": request.prompt.len(),
            "generated": out.tokens.len(),
            "drafted": out.drafted,
            "accepted": out.accepted,
            "decode_steps": out.decode_steps,
            "output_hash": hex_lower(&out.output_hash),
        }));
    }
    let matrix_text = arc_inference::model_package::canonical_json(&Value::from(entries))
        .map_err(|e| ModernError::Invalid(format!("canonical JSON: {e}")))?;
    let digest = blake3::hash(matrix_text.as_bytes()).to_hex().to_string();
    let cached_config = SchedulerConfig {
        max_running: 1,
        prefix: Some(PREFIX),
        ..SchedulerConfig::default()
    };
    let mut scheduler = Scheduler::new(&dense, cached_config);
    let mut cached_tokens = 0usize;
    let mut cache_identical = true;
    for (index, request) in requests.iter().enumerate() {
        let id = index as u64;
        let warm = run(&mut scheduler, vec![request.clone()])?;
        let out = generated(&warm, id)?;
        cached_tokens += out.cached_tokens;
        cache_identical &= out.tokens == generated(&batched, id)?.tokens;
    }
    let digest_matches = expect.is_none_or(|e| e == digest);
    Ok(json!({
        "matrix_digest": digest,
        "expected": expect,
        "digest_matches": digest_matches,
        "batched_config": {
            "max_running": batched_config.max_running,
            "prefill_chunk": batched_config.prefill_chunk,
            "draft_tokens": batched_config.draft_tokens,
            "all_logits": true,
        },
        "batched_seconds": batched_seconds,
        "cases": rows,
        "prefix_cache_tokens_identical": cache_identical,
        "prefix_cache_prompt_tokens_reused": cached_tokens,
        "pass": digest_matches && cache_identical,
    }))
}

/// Aggregate and per-stream decode tok/s at each concurrency.
fn bench_batching(
    bench: &Bench,
    workload: &Value,
    levels: &[usize],
    gen_tokens: usize,
) -> Result<Value, ModernError> {
    let users = strings(workload, "batching_users")?;
    let prompts = users
        .iter()
        .map(|user| bench.chat(None, user))
        .collect::<Result<Vec<_>, _>>()?;
    if prompts.is_empty() {
        return Err(ModernError::Invalid("no batching prompts".into()));
    }
    let dense = bench.dense();
    let top = levels.iter().copied().max().unwrap_or(1);
    let config = SchedulerConfig {
        max_running: 1,
        step_tokens: 512.max(top + 64),
        prefill_chunk: 64,
        prefix: Some(PREFIX),
        ..SchedulerConfig::default()
    };
    let mut scheduler = Scheduler::new(&dense, config);
    // Warm the chat template's shared prefix once (not measured), so each
    // stream prefills only its own question.
    run(
        &mut scheduler,
        vec![chat_request(usize::MAX, &prompts[0], 1, &[])],
    )?;
    let mut first_seen: HashMap<usize, [u8; 32]> = HashMap::new();
    let mut invariant = true;
    let mut points = Vec::new();
    for &level in levels {
        scheduler.set_max_running(level);
        let first_step = scheduler.steps().len();
        let requests = (0..level)
            .map(|i| chat_request(i, &prompts[i % prompts.len()], gen_tokens, &[]))
            .collect();
        let started = Instant::now();
        let completions = run(&mut scheduler, requests)?;
        let wall_seconds = started.elapsed().as_secs_f64();
        let steps: &[StepStats] = &scheduler.steps()[first_step..];
        let decode: Vec<&StepStats> = steps
            .iter()
            .filter(|s| s.prefill_rows == 0 && s.decode_rows > 0)
            .collect();
        let decode_seconds: f64 = decode.iter().map(|s| s.seconds).sum();
        let decode_tokens: usize = decode.iter().map(|s| s.emitted).sum();
        let prefill_rows: usize = steps.iter().map(|s| s.prefill_rows).sum();
        let mut cached = 0usize;
        for i in 0..level {
            let out = generated(&completions, i as u64)?;
            cached += out.cached_tokens;
            let seen = *first_seen
                .entry(i % prompts.len())
                .or_insert(out.output_hash);
            invariant &= seen == out.output_hash;
        }
        points.push(json!({
            "concurrency": level,
            "aggregate_tok_s": ratio(decode_tokens as f64, decode_seconds),
            "per_stream_tok_s": ratio(decode.len() as f64, decode_seconds),
            "mean_step_seconds": ratio(decode_seconds, decode.len() as f64),
            "decode_steps": decode.len(),
            "decode_tokens": decode_tokens,
            "decode_seconds": decode_seconds,
            "prefill_tokens_computed": prefill_rows,
            "prompt_tokens_from_cache": cached,
            "wall_seconds": wall_seconds,
        }));
    }
    Ok(json!({
        "generated_per_stream": gen_tokens,
        "points": points,
        "streams_identical_across_concurrency": invariant,
        "pass": invariant,
    }))
}

/// Time to first token on a prompt-heavy agent replay, with and without the
/// prefix cache.
fn bench_prefix(bench: &Bench, workload: &Value, gen_tokens: usize) -> Result<Value, ModernError> {
    let system = workload
        .get("agent_system")
        .and_then(Value::as_str)
        .ok_or_else(|| ModernError::Invalid("workload has no agent_system".into()))?;
    let turns = strings(workload, "agent_turns")?;
    let prompts = turns
        .iter()
        .map(|user| bench.chat(Some(system), user))
        .collect::<Result<Vec<_>, _>>()?;
    let dense = bench.dense();
    let mut arms = Vec::new();
    let mut outputs: Vec<Vec<Vec<u32>>> = Vec::new();
    let mut summaries = Vec::new();
    for (name, prefix) in [("no_cache", None), ("prefix_cache", Some(PREFIX))] {
        let config = SchedulerConfig {
            max_running: 1,
            step_tokens: 512,
            prefill_chunk: 256,
            prefix,
            ..SchedulerConfig::default()
        };
        let mut scheduler = Scheduler::new(&dense, config);
        let mut requests = Vec::new();
        let mut tokens = Vec::new();
        let mut ttft = Vec::new();
        let (mut input, mut output, mut cached) = (0usize, 0usize, 0usize);
        for (index, prompt) in prompts.iter().enumerate() {
            let id = index as u64;
            let completions = run(
                &mut scheduler,
                vec![chat_request(index, prompt, gen_tokens, &[EOS_IM_END])],
            )?;
            let out = generated(&completions, id)?;
            let timing = completions[&id].timing;
            let first = timing.first_token.as_secs_f64();
            ttft.push(first);
            input += prompt.len();
            output += out.tokens.len();
            cached += out.cached_tokens;
            tokens.push(out.tokens.clone());
            requests.push(json!({
                "prompt_tokens": prompt.len(),
                "prompt_tokens_from_cache": out.cached_tokens,
                "generated": out.tokens.len(),
                "ttft_seconds": first,
                "total_seconds": timing.total.as_secs_f64(),
            }));
        }
        let later = ttft.get(1..).unwrap_or(&[]);
        summaries.push((name, mean(later)));
        arms.push(json!({
            "arm": name,
            "requests": requests,
            "input_tokens": input,
            "output_tokens": output,
            "input_to_output": ratio(input as f64, output as f64),
            "prompt_tokens_from_cache": cached,
            "cache_hit_rate": ratio(cached as f64, input as f64),
            "ttft_first_request_seconds": ttft.first().copied().unwrap_or(0.0),
            "ttft_mean_seconds_requests_2_on": mean(later),
            "ttft_median_seconds_requests_2_on": median(later),
            "ttft_mean_seconds_all": mean(&ttft),
        }));
        outputs.push(tokens);
    }
    let identical = outputs.windows(2).all(|pair| pair[0] == pair[1]);
    let speedup = match summaries.as_slice() {
        [(_, cold), (_, warm)] => ratio(*cold, *warm),
        _ => 0.0,
    };
    Ok(json!({
        "shared_system_prompt": true,
        "generated_at_most": gen_tokens,
        "arms": arms,
        "ttft_speedup_requests_2_on": speedup,
        "outputs_identical": identical,
        "pass": identical,
    }))
}

/// Decode tok/s with and without prompt-lookup speculation, alternating the
/// two arms request by request.
fn bench_speculative(
    bench: &Bench,
    workload: &Value,
    cases: Option<&Value>,
    draft: usize,
    gen_tokens: usize,
) -> Result<Value, ModernError> {
    let mut sets: Vec<(&str, Vec<Vec<u32>>)> = Vec::new();
    let echo = strings(workload, "echo_tasks")?;
    sets.push((
        "quotes-the-prompt",
        echo.iter()
            .map(|user| bench.chat(None, user))
            .collect::<Result<Vec<_>, _>>()?,
    ));
    if let Some(cases) = cases {
        let users: Vec<&str> = cases
            .get("cases")
            .and_then(Value::as_array)
            .map(|list| {
                list.iter()
                    .filter_map(|case| case.get("user").and_then(Value::as_str))
                    .collect()
            })
            .unwrap_or_default();
        sets.push((
            "general-chat",
            users
                .iter()
                .map(|user| bench.chat(None, user))
                .collect::<Result<Vec<_>, _>>()?,
        ));
    }
    let dense = bench.dense();
    // One scheduler with a prefix cache: the plain run of a prompt fills it, so
    // the speculative run starts decoding at once. Decode speed is measured
    // from each request's first token to its last.
    let config = SchedulerConfig {
        max_running: 1,
        step_tokens: 512,
        prefill_chunk: 256,
        prefix: Some(PREFIX),
        ..SchedulerConfig::default()
    };
    let mut scheduler = Scheduler::new(&dense, config).with_drafter(Box::new(LOOKUP));
    let mut results = Vec::new();
    let mut identical = true;
    for (set, prompts) in &sets {
        // Per arm: decode tokens, decode seconds, drafted, accepted, passes.
        let mut totals = [(0usize, 0f64, 0usize, 0usize, 0usize); 2];
        let mut records = Vec::new();
        for (index, prompt) in prompts.iter().enumerate() {
            let mut plain_tokens = Vec::new();
            for (arm, k) in [0usize, draft].into_iter().enumerate() {
                scheduler.set_draft_tokens(k);
                let id = index as u64;
                let completions = run(
                    &mut scheduler,
                    vec![chat_request(index, prompt, gen_tokens, &[EOS_IM_END])],
                )?;
                let out = generated(&completions, id)?;
                let timing = completions[&id].timing;
                let total = &mut totals[arm];
                total.0 += out.tokens.len().saturating_sub(1);
                total.1 += timing
                    .total
                    .saturating_sub(timing.first_token)
                    .as_secs_f64();
                total.2 += out.drafted;
                total.3 += out.accepted;
                total.4 += out.decode_steps;
                if arm == 0 {
                    plain_tokens = out.tokens.clone();
                } else {
                    identical &= plain_tokens == out.tokens;
                    records.push(json!({
                        "prompt_tokens": prompt.len(),
                        "generated": out.tokens.len(),
                        "drafted": out.drafted,
                        "accepted": out.accepted,
                        "passes": out.decode_steps,
                        "text": bench.tokenizer.decode(&out.tokens, true),
                    }));
                }
            }
        }
        let arms: Vec<Value> = totals
            .iter()
            .zip([0usize, draft])
            .map(|(&(tokens, seconds, drafted, accepted, passes), k)| {
                json!({
                    "draft_tokens": k,
                    "decode_tokens": tokens,
                    "decode_seconds": seconds,
                    "decode_tok_s": ratio(tokens as f64, seconds),
                    "drafted": drafted,
                    "accepted": accepted,
                    "acceptance": ratio(accepted as f64, drafted as f64),
                    "tokens_per_pass": ratio(tokens as f64, passes as f64),
                })
            })
            .collect();
        let plain = ratio(totals[0].0 as f64, totals[0].1);
        let spec = ratio(totals[1].0 as f64, totals[1].1);
        results.push(json!({
            "set": set,
            "prompts": prompts.len(),
            "arms": arms,
            "speedup": ratio(spec, plain),
            "requests": records,
        }));
    }
    Ok(json!({
        "drafter": {"kind": "prompt-lookup", "min_ngram": LOOKUP.min_ngram, "max_ngram": LOOKUP.max_ngram},
        "generated_at_most": gen_tokens,
        "concurrency": 1,
        "sets": results,
        "outputs_identical": identical,
        "pass": identical,
    }))
}

/// Batched projection against one-row-at-a-time projection on synthetic
/// matrices shaped like SmolLM3's (`d_model` 2048, `d_ff` 11008): time per row
/// and weights per second at each width, and the outputs compared. Needs no
/// model, so it gives kernel feedback in minutes.
fn bench_gemm(widths: &[usize], repeats: usize) -> Result<Value, ModernError> {
    let mut state = 0x2545_f491_4f6c_dd1d_u64;
    let mut next = move || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state
    };
    let top = widths.iter().copied().max().unwrap_or(1);
    let mut shapes = Vec::new();
    let mut identical = true;
    for (rows, cols) in [(2048usize, 2048usize), (2048, 11008)] {
        let m = DyadicMatrix {
            rows,
            cols,
            q: (0..rows * cols)
                .map(|_| ((next() % 255) as i64 - 127) as i8)
                .collect(),
            mu: (0..rows)
                .map(|_| ((1u64 << 30) + next() % (1 << 30)) as i32)
                .collect(),
            k: (0..rows).map(|_| 40 + (next() % 3) as u8).collect(),
        };
        // Activations like normalised Q16 inputs: |x| below 4.0.
        let all: Vec<i64> = (0..top * cols)
            .map(|_| (next() % (1 << 19)) as i64 - (1 << 18))
            .collect();
        let mut points = Vec::new();
        for &width in widths {
            let xs = &all[..width * cols];
            let mut gemv = vec![0i64; width * rows];
            let mut gemv_seconds = f64::MAX;
            for _ in 0..repeats {
                let started = Instant::now();
                for (x, y) in xs.chunks(cols).zip(gemv.chunks_mut(rows)) {
                    arith::project(&m, x, y)?;
                }
                gemv_seconds = gemv_seconds.min(started.elapsed().as_secs_f64());
            }
            let mut gemm = vec![0i64; width * rows];
            let mut gemm_seconds = f64::MAX;
            for _ in 0..repeats {
                let mut errors: Vec<Option<ModernError>> = (0..width).map(|_| None).collect();
                let started = Instant::now();
                project_rows(&m, xs, &mut gemm, &mut errors);
                gemm_seconds = gemm_seconds.min(started.elapsed().as_secs_f64());
                if let Some(error) = errors.into_iter().flatten().next() {
                    return Err(error);
                }
            }
            // The digit kernel alone (raw sums, no epilogue), also at width 1,
            // where project_rows uses the single-row path instead.
            let live: Vec<usize> = (0..width).collect();
            let mut sums_seconds = f64::MAX;
            let mut sums = Vec::new();
            for _ in 0..repeats {
                let started = Instant::now();
                sums = exact_sums(&m, xs, &live);
                sums_seconds = sums_seconds.min(started.elapsed().as_secs_f64());
            }
            // Raw sums against independent scalar dot products, element by
            // element; the projected outputs against one row at a time.
            identical &= sums.len() == rows * width
                && (0..rows).all(|i| {
                    let row = &m.q[i * cols..(i + 1) * cols];
                    (0..width).all(|t| {
                        sums[i * width + t] == arith::dot_i8_i64(row, &xs[t * cols..(t + 1) * cols])
                    })
                });
            identical &= gemv == gemm;
            let weights = (rows * cols * width) as f64;
            let per_row = |seconds: f64| 1e3 * seconds / width as f64;
            points.push(json!({
                "width": width,
                "gemv_ms_per_row": per_row(gemv_seconds),
                "gemm_ms_per_row": per_row(gemm_seconds),
                "kernel_ms_per_row": per_row(sums_seconds),
                "gemv_gweights_s": weights / gemv_seconds / 1e9,
                "gemm_gweights_s": weights / gemm_seconds / 1e9,
                "gemm_speedup_per_row": ratio(gemv_seconds, gemm_seconds),
            }));
        }
        shapes.push(json!({"rows": rows, "cols": cols, "points": points}));
    }
    Ok(json!({
        "repeats": repeats,
        "shapes": shapes,
        "outputs_identical": identical,
        "pass": identical,
    }))
}

fn gemm_summary(report: &Value) -> String {
    let p = &report["platform"];
    let g = &report["gemm"];
    let mut md = format!(
        "### Batched projection vs one row at a time (synthetic, {} {}, {} threads, kernel {}; outputs identical: {})\n\n",
        p["os"].as_str().unwrap_or("?"),
        p["cpu"]
            .as_str()
            .or_else(|| p["arch"].as_str())
            .unwrap_or("?"),
        p["rayon_threads"],
        report["kernel"].as_str().unwrap_or("?"),
        yes(&g["outputs_identical"]),
    );
    for shape in g["shapes"].as_array().into_iter().flatten() {
        md += &format!(
            "**{} x {} matrix**\n\n| rows in the step | one row at a time (ms/row) | batched (ms/row) | digit kernel only (ms/row) | batched Gweights/s | speedup per row |\n|---|---|---|---|---|---|\n",
            shape["rows"], shape["cols"]
        );
        for point in shape["points"].as_array().into_iter().flatten() {
            md += &format!(
                "| {} | {} | {} | {} | {} | {}x |\n",
                point["width"],
                fmt(&point["gemv_ms_per_row"], 3),
                fmt(&point["gemm_ms_per_row"], 3),
                fmt(&point["kernel_ms_per_row"], 3),
                fmt(&point["gemm_gweights_s"], 2),
                fmt(&point["gemm_speedup_per_row"], 2),
            );
        }
        md += "\n";
    }
    md
}

fn configure(args: &Args) -> Result<(usize, String), ModernError> {
    let threads = args.number("--threads", 0)?;
    if threads > 0 {
        rayon::ThreadPoolBuilder::new()
            .num_threads(threads)
            .build_global()
            .map_err(|e| ModernError::Invalid(format!("thread pool: {e}")))?;
    }
    let kernel = args.value("--kernel").unwrap_or_else(|| "simd".to_string());
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
    Ok((rayon::current_num_threads(), kernel))
}

fn cpu_model() -> Value {
    std::fs::read_to_string("/proc/cpuinfo")
        .ok()
        .and_then(|text| {
            text.lines()
                .find(|line| line.starts_with("model name"))
                .and_then(|line| line.split(':').nth(1))
                .map(|name| Value::from(name.trim()))
        })
        .unwrap_or(Value::Null)
}

fn fmt(value: &Value, digits: usize) -> String {
    value
        .as_f64()
        .map(|v| format!("{v:.digits$}"))
        .unwrap_or_else(|| "n/a".to_string())
}

fn yes(value: &Value) -> &'static str {
    if value.as_bool() == Some(true) {
        "yes"
    } else {
        "**NO**"
    }
}

/// A Markdown summary of the report.
fn summary(report: &Value) -> String {
    let p = &report["platform"];
    let mut md = format!(
        "### Serving layer on SmolLM3-3B (CI runner: {} {}, {} logical CPUs, {} threads, kernel {})\n\n\
         Measured by `arc-serve-bench` in this job. CI-runner numbers, not device benchmarks.\n\n",
        p["os"].as_str().unwrap_or("?"),
        p["cpu"]
            .as_str()
            .or_else(|| p["arch"].as_str())
            .unwrap_or("?"),
        p["logical_cpus"],
        p["rayon_threads"],
        report["kernel"].as_str().unwrap_or("?"),
    );
    let g = &report["golden"];
    if !g.is_null() {
        md += &format!(
            "**Golden digest through batching + chunked prefill + speculation:** `{}` \
             (expected `{}`): match {}. Same tokens through a warm prefix cache: {}.\n\n",
            g["matrix_digest"].as_str().unwrap_or("?"),
            g["expected"].as_str().unwrap_or("not given"),
            yes(&g["digest_matches"]),
            yes(&g["prefix_cache_tokens_identical"]),
        );
    }
    let b = &report["batching"];
    if let Some(points) = b["points"].as_array() {
        md += "**Continuous batching, decode only** (each stream generates ";
        md += &format!(
            "{} tokens; streams identical across concurrency: {})\n\n",
            b["generated_per_stream"],
            yes(&b["streams_identical_across_concurrency"])
        );
        md += "| concurrent streams | aggregate tok/s | per-stream tok/s | mean step (s) |\n|---|---|---|---|\n";
        for point in points {
            md += &format!(
                "| {} | {} | {} | {} |\n",
                point["concurrency"],
                fmt(&point["aggregate_tok_s"], 2),
                fmt(&point["per_stream_tok_s"], 2),
                fmt(&point["mean_step_seconds"], 3),
            );
        }
        md += "\n";
    }
    let x = &report["prefix"];
    if let Some(arms) = x["arms"].as_array() {
        md += &format!(
            "**Prefix cache, prompt-heavy agent replay** (shared system prompt; outputs identical: {}; \
             TTFT speedup on requests 2+: {}x)\n\n",
            yes(&x["outputs_identical"]),
            fmt(&x["ttft_speedup_requests_2_on"], 2),
        );
        md += "| arm | input:output | cache hit rate | TTFT request 1 (s) | mean TTFT requests 2+ (s) |\n|---|---|---|---|---|\n";
        for arm in arms {
            md += &format!(
                "| {} | {}:1 | {} | {} | {} |\n",
                arm["arm"].as_str().unwrap_or("?"),
                fmt(&arm["input_to_output"], 1),
                fmt(&arm["cache_hit_rate"], 3),
                fmt(&arm["ttft_first_request_seconds"], 2),
                fmt(&arm["ttft_mean_seconds_requests_2_on"], 2),
            );
        }
        md += "\n";
    }
    let s = &report["speculative"];
    if let Some(sets) = s["sets"].as_array() {
        md += &format!(
            "**Speculative decoding, prompt lookup, concurrency 1** (outputs identical to plain greedy: {})\n\n",
            yes(&s["outputs_identical"]),
        );
        md += "| prompts | plain tok/s | speculative tok/s | speedup | acceptance | tokens per pass |\n|---|---|---|---|---|---|\n";
        for set in sets {
            let arms = &set["arms"];
            md += &format!(
                "| {} | {} | {} | {}x | {} | {} |\n",
                set["set"].as_str().unwrap_or("?"),
                fmt(&arms[0]["decode_tok_s"], 2),
                fmt(&arms[1]["decode_tok_s"], 2),
                fmt(&set["speedup"], 2),
                fmt(&arms[1]["acceptance"], 2),
                fmt(&arms[1]["tokens_per_pass"], 2),
            );
        }
        md += "\n";
    }
    md
}

fn execute(command: &str, args: &Args) -> Result<bool, ModernError> {
    if !matches!(
        command,
        "golden" | "batching" | "prefix" | "speculative" | "all" | "gemm"
    ) {
        return Err(ModernError::Invalid(USAGE.into()));
    }
    let (threads, kernel) = configure(args)?;
    let platform = json!({
        "os": std::env::consts::OS,
        "arch": std::env::consts::ARCH,
        "cpu": cpu_model(),
        "logical_cpus": std::thread::available_parallelism().map(|n| n.get()).unwrap_or(0),
        "rayon_threads": threads,
        "simd_available": canonical_simd::dotprod_available(),
        "batched_digit_kernel": digit_kernel_enabled(),
    });
    if command == "gemm" {
        let widths = parse_list(args.value("--widths"), &[1, 2, 4, 8, 16, 32, 64, 128, 256])?;
        let result = bench_gemm(&widths, args.number("--repeats", 3)?)?;
        let pass = result["pass"].as_bool() == Some(true);
        let report = json!({
            "schema": "arc.serving-gemm-bench.v1",
            "kernel": kernel,
            "platform": platform,
            "gemm": result,
            "pass": pass,
        });
        write_report(args, &report, gemm_summary(&report))?;
        return Ok(pass);
    }
    let package_path = args.path("--package")?;
    let digest = package::digest_file(&package_path)?;
    let started = Instant::now();
    let model = package::load_package(&package_path)?;
    let load_seconds = started.elapsed().as_secs_f64();
    let tokenizer_path = args.path("--tokenizer")?;
    let tokenizer_bytes = std::fs::read(&tokenizer_path)
        .map_err(|e| ModernError::Io(format!("{}: {e}", tokenizer_path.display())))?;
    let workload = match args.value("--workload") {
        Some(path) => Some(read_json(Path::new(&path))?),
        None => None,
    };
    let cases = match args.value("--cases") {
        Some(path) => Some(read_json(Path::new(&path))?),
        None => None,
    };
    let today = workload
        .as_ref()
        .and_then(|w| w.get("today"))
        .and_then(Value::as_str)
        .unwrap_or(DEFAULT_TODAY)
        .to_string();
    let mut identity = blake3::Hasher::new();
    identity.update(PROFILE.as_bytes());
    identity.update(digest.sha256.as_bytes());
    let bench = Bench {
        model,
        identity: *identity.finalize().as_bytes(),
        tokenizer: ByteLevelBpe::from_json(&tokenizer_bytes)?,
        today,
    };
    let mut report = Map::new();
    report.insert("schema".into(), json!("arc.serving-bench.v1"));
    report.insert("profile".into(), json!(PROFILE));
    report.insert("package".into(), digest.to_json());
    report.insert("kernel".into(), json!(kernel));
    report.insert("load_seconds".into(), json!(load_seconds));
    report.insert("platform".into(), platform);
    let all = command == "all";
    let need_workload = || {
        workload
            .as_ref()
            .ok_or_else(|| ModernError::Invalid(format!("{command} needs --workload")))
    };
    let mut pass = true;
    if all || command == "golden" {
        let cases = cases
            .as_ref()
            .ok_or_else(|| ModernError::Invalid("golden needs --cases".into()))?;
        let expect = args.value("--expect");
        let result = bench_golden(&bench, cases, expect.as_deref())?;
        eprintln!("golden: {}", result["matrix_digest"]);
        pass &= result["pass"].as_bool() == Some(true);
        report.insert("golden".into(), result);
    }
    if all || command == "batching" {
        let levels = parse_list(args.value("--concurrency"), &[1, 2, 4, 8, 16, 32])?;
        let result = bench_batching(&bench, need_workload()?, &levels, args.number("--gen", 16)?)?;
        eprintln!("batching: {}", result["points"]);
        pass &= result["pass"].as_bool() == Some(true);
        report.insert("batching".into(), result);
    }
    if all || command == "prefix" {
        let result = bench_prefix(&bench, need_workload()?, args.number("--prefix-gen", 24)?)?;
        eprintln!("prefix: {}", result["ttft_speedup_requests_2_on"]);
        pass &= result["pass"].as_bool() == Some(true);
        report.insert("prefix".into(), result);
    }
    if all || command == "speculative" {
        let result = bench_speculative(
            &bench,
            need_workload()?,
            cases.as_ref(),
            args.number("--draft", 8)?,
            args.number("--spec-gen", 96)?,
        )?;
        eprintln!("speculative: {}", result["sets"]);
        pass &= result["pass"].as_bool() == Some(true);
        report.insert("speculative".into(), result);
    }
    report.insert("pass".into(), json!(pass));
    let report = Value::Object(report);
    write_report(args, &report, summary(&report))?;
    Ok(pass)
}

/// A comma-separated list of counts, or `default`.
fn parse_list(text: Option<String>, default: &[usize]) -> Result<Vec<usize>, ModernError> {
    match text {
        None => Ok(default.to_vec()),
        Some(list) => list
            .split(',')
            .map(|item| item.trim().parse::<usize>())
            .collect::<Result<Vec<_>, _>>()
            .map_err(|_| ModernError::Invalid(format!("bad list {list}"))),
    }
}

/// Write the JSON report to `--out` and the Markdown summary to `--summary`.
fn write_report(args: &Args, report: &Value, markdown: String) -> Result<(), ModernError> {
    let out = args.path("--out")?;
    let text = serde_json::to_string_pretty(report)
        .map_err(|e| ModernError::Invalid(format!("JSON: {e}")))?;
    std::fs::write(&out, text + "\n")
        .map_err(|e| ModernError::Io(format!("{}: {e}", out.display())))?;
    if let Some(path) = args.value("--summary") {
        std::fs::write(&path, markdown).map_err(|e| ModernError::Io(format!("{path}: {e}")))?;
    }
    Ok(())
}

fn main() -> ExitCode {
    let items: Vec<String> = std::env::args().skip(1).collect();
    let Some(command) = items.first().cloned() else {
        eprintln!("{USAGE}");
        return ExitCode::from(2);
    };
    match execute(&command, &Args { items }) {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => {
            eprintln!("arc-serve-bench {command}: a byte-identity check failed; see the report");
            ExitCode::FAILURE
        }
        Err(error) => {
            eprintln!("arc-serve-bench {command}: {error}");
            ExitCode::FAILURE
        }
    }
}
