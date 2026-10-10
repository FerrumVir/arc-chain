//! Aggregate decode throughput when several users' sequences share every
//! weight read, on the real network model. An internal measurement, not a
//! product figure.
//!
//! The model is loaded as a community worker loads it
//! (`load_cached_model_canonical_i8`, the legacy split-half canonical INT8
//! profile). Every sequence has its own prompt (the lengths differ, so the
//! sequences sit at different positions) and its own K/V cache. For each
//! kernel and each number S of concurrent sequences, the driver runs `--steps`
//! decode steps from the prefilled prompts:
//!
//! * S = 1 is one user decoding as a worker does, one `forward_one_token` call
//!   per step;
//! * S >= 2 is one `forward_rows_multi` call per step, carrying the next row
//!   of every sequence.
//!
//! The report is the median time of a decode step and the aggregate decoded
//! tokens per second, S over that median.
//!
//! Every sequence follows the worker's own generation (`try_generate`): the
//! BOS and the prompt, then the last prompt token once more, and each step
//! picks the next token with the worker's selection,
//! `select_next_token_with_repetition_penalty`, over the sequence's own
//! generated history.
//!
//! Exactness on the real model. Every sequence first runs alone through
//! `try_generate`, the worker's own call. Every timed run must give every
//! sequence the same tokens and output hash, for every kernel and every S;
//! otherwise the driver exits non-zero.
//!
//! Kernels: `scalar`, or a multi-row kernel by its label (`neon-sdot-limb`,
//! `neon-i8mm-limb`, `avx2-limb`, `avx-vnni-limb`, `avx512-vnni-limb`); `simd`
//! is the architecture's base kernel, `best` the one chosen automatically,
//! and `all` (the default) is `scalar` plus every multi-row kernel this CPU
//! has.
//!
//! usage: multi_seq_bench --model GGUF [--kernel all|scalar|simd|best|LABEL,...]
//!        [--sequences 1,2,4,8,16,32] [--prompt N] [--steps N]
//!        [--cpu NAME] [--out FILE.json] [--summary FILE.md]

use arc_crypto::{Hash256, hash_bytes};
use arc_inference::cached_integer_model::{
    CANONICAL_REWARD_INFERENCE_PROFILE, CachedIntegerModel, KVCache, SeqRows, ShardRowsInput,
    ShardRowsOutput, load_cached_model_canonical_i8, select_next_token_with_repetition_penalty,
};
use arc_inference::canonical_simd::{self, BatchedKernel};
use serde_json::{Value, json};
use std::time::Instant;

/// `None` is the scalar kernel; `Some` pins a vectorised multi-row kernel.
type KernelChoice = Option<BatchedKernel>;

struct Args {
    model: String,
    kernels: Vec<KernelChoice>,
    sequences: Vec<usize>,
    prompt: usize,
    steps: usize,
    cpu: Option<String>,
    out: Option<String>,
    summary: Option<String>,
}

fn parse_count(text: &str) -> Result<usize, String> {
    text.parse()
        .map_err(|_| format!("expected a whole number, got {text}"))
}

/// The architecture's base multi-row kernel (SDOT or AVX2).
fn base_kernel() -> Result<BatchedKernel, String> {
    [BatchedKernel::NeonSdot, BatchedKernel::Avx2]
        .into_iter()
        .find(|kernel| kernel.available())
        .ok_or_else(|| "this CPU has no vectorised kernel".to_string())
}

fn parse_kernels(text: &str) -> Result<Vec<KernelChoice>, String> {
    let mut kernels: Vec<KernelChoice> = Vec::new();
    for name in text.split(',') {
        match name {
            "all" => {
                kernels.push(None);
                kernels.extend(
                    canonical_simd::available_batched_kernels()
                        .into_iter()
                        .map(Some),
                );
            }
            "scalar" => kernels.push(None),
            "simd" => kernels.push(Some(base_kernel()?)),
            "best" => kernels
                .push(Some(canonical_simd::best_batched_kernel().ok_or_else(
                    || "this CPU has no vectorised kernel".to_string(),
                )?)),
            label => {
                let kernel = BatchedKernel::from_label(label).ok_or_else(|| {
                    format!("--kernel takes all, scalar, simd, best or a kernel label, not {label}")
                })?;
                if !kernel.available() {
                    return Err(format!("this CPU cannot run {label}"));
                }
                kernels.push(Some(kernel));
            }
        }
    }
    let mut unique: Vec<KernelChoice> = Vec::new();
    for kernel in kernels {
        if !unique.contains(&kernel) {
            unique.push(kernel);
        }
    }
    Ok(unique)
}

fn kernel_name(kernel: KernelChoice) -> &'static str {
    kernel.map_or("scalar", BatchedKernel::label)
}

/// The CPU model: `--cpu` if given, else `/proc/cpuinfo`'s model name, else
/// Windows' `PROCESSOR_IDENTIFIER`.
fn cpu_model(given: Option<&str>) -> String {
    if let Some(name) = given {
        return name.to_string();
    }
    std::fs::read_to_string("/proc/cpuinfo")
        .ok()
        .and_then(|info| {
            info.lines()
                .find(|line| line.starts_with("model name"))
                .and_then(|line| line.split(':').nth(1))
                .map(|name| name.trim().to_string())
        })
        .or_else(|| std::env::var("PROCESSOR_IDENTIFIER").ok())
        .unwrap_or_else(|| "unknown".to_string())
}

fn parse_args() -> Result<Args, String> {
    let mut args = Args {
        model: String::new(),
        kernels: Vec::new(),
        sequences: vec![1, 2, 4, 8, 16, 32],
        prompt: 10,
        steps: 8,
        cpu: None,
        out: None,
        summary: None,
    };
    let mut items = std::env::args().skip(1);
    while let Some(flag) = items.next() {
        let mut value = || items.next().ok_or_else(|| format!("{flag} needs a value"));
        match flag.as_str() {
            "--model" => args.model = value()?,
            "--kernel" => args.kernels = parse_kernels(&value()?)?,
            "--sequences" => {
                args.sequences = value()?
                    .split(',')
                    .map(parse_count)
                    .collect::<Result<Vec<_>, _>>()?
            }
            "--prompt" => args.prompt = parse_count(&value()?)?,
            "--steps" => args.steps = parse_count(&value()?)?,
            "--cpu" => args.cpu = Some(value()?),
            "--out" => args.out = Some(value()?),
            "--summary" => args.summary = Some(value()?),
            other => return Err(format!("unknown argument {other}")),
        }
    }
    if args.model.is_empty() {
        return Err("--model is required".into());
    }
    if args.sequences.is_empty() || args.sequences.contains(&0) {
        return Err("every --sequences count must be at least 1".into());
    }
    if args.prompt == 0 || args.steps == 0 {
        return Err("--prompt and --steps must be at least 1".into());
    }
    if args.kernels.is_empty() {
        args.kernels = parse_kernels("all")?;
    }
    Ok(args)
}

/// One sequence's generation: its tokens and output hash, as `try_generate`
/// returns them.
#[derive(Clone, PartialEq, Eq)]
struct Generated {
    tokens: Vec<u32>,
    hash: Hash256,
}

/// The output hash `try_generate` returns: BLAKE3 of the little-endian tokens.
fn output_hash(tokens: &[u32]) -> Hash256 {
    let bytes: Vec<u8> = tokens
        .iter()
        .flat_map(|token| token.to_le_bytes())
        .collect();
    hash_bytes(&bytes)
}

/// One sequence alone, through the worker's own call.
fn generate_alone(
    model: &CachedIntegerModel,
    prompt: &[u32],
    steps: usize,
) -> Result<Generated, String> {
    let budget = u32::try_from(steps).map_err(|_| "--steps does not fit u32".to_string())?;
    let (tokens, hash) = model
        .try_generate(prompt, budget, &[])
        .map_err(|error| format!("try_generate: {error}"))?;
    Ok(Generated { tokens, hash })
}

/// A sequence after its BOS and prompt: its cache, and the token its first
/// step feeds, which is the last prompt token again, as `try_generate` feeds
/// it.
struct Prefilled {
    cache: KVCache,
    first_feed: u32,
}

fn clone_cache(cache: &KVCache) -> KVCache {
    KVCache {
        k_data: cache.k_data.clone(),
        v_data: cache.v_data.clone(),
        seq_len: cache.seq_len,
    }
}

/// Runs every sequence's BOS and prompt in multi-sequence calls of at most
/// the stage's row cap, so the prompts share weight reads too.
fn prefill_all(model: &CachedIntegerModel, prompts: &[Vec<u32>]) -> Result<Vec<Prefilled>, String> {
    let n_layers = model.config.n_layers;
    let cap = model.max_shard_rows(n_layers);
    let fed: Vec<Vec<u32>> = prompts
        .iter()
        .map(|prompt| {
            let mut tokens = vec![model.config.bos_token];
            tokens.extend_from_slice(prompt);
            tokens
        })
        .collect();
    let mut done: Vec<Prefilled> = Vec::with_capacity(prompts.len());
    let mut start = 0usize;
    while start < fed.len() {
        let mut end = start;
        let mut rows = 0usize;
        while end < fed.len() && rows + fed[end].len() <= cap {
            rows += fed[end].len();
            end += 1;
        }
        if end == start {
            return Err(format!(
                "a {}-token prompt exceeds the {cap}-row cap",
                fed[start].len()
            ));
        }
        let mut caches: Vec<KVCache> = (start..end).map(|_| KVCache::new(n_layers)).collect();
        let mut batch: Vec<SeqRows<'_>> = caches
            .iter_mut()
            .zip(&fed[start..end])
            .map(|(cache, tokens)| SeqRows {
                cache,
                position: 0,
                input: ShardRowsInput::Tokens(tokens.clone()),
            })
            .collect();
        model
            .forward_rows_multi(&mut batch, 0, n_layers)
            .map_err(|error| format!("prefill: {error}"))?;
        drop(batch);
        for (cache, prompt) in caches.into_iter().zip(&prompts[start..end]) {
            let first_feed = *prompt.last().ok_or("an empty prompt")?;
            done.push(Prefilled { cache, first_feed });
        }
        start = end;
    }
    Ok(done)
}

/// Runs `steps` decode steps of the first `count` prefilled sequences, all in
/// one call per step (one `forward_one_token` per step for a single
/// sequence), each sequence selecting its next token with the worker's
/// selection over its own history. Returns each sequence's generation and
/// each step's forward time.
fn decode_together(
    model: &CachedIntegerModel,
    prefilled: &[Prefilled],
    count: usize,
    steps: usize,
) -> Result<(Vec<Generated>, Vec<f64>), String> {
    let n_layers = model.config.n_layers;
    let mut caches: Vec<KVCache> = prefilled[..count]
        .iter()
        .map(|sequence| clone_cache(&sequence.cache))
        .collect();
    let mut generated: Vec<Vec<u32>> = vec![Vec::with_capacity(steps); count];
    let mut seconds = Vec::with_capacity(steps);
    for _ in 0..steps {
        let feed: Vec<u32> = generated
            .iter()
            .zip(&prefilled[..count])
            .map(|(tokens, sequence)| tokens.last().copied().unwrap_or(sequence.first_feed))
            .collect();
        let started = Instant::now();
        let mut logits: Vec<Vec<i64>> = if count == 1 {
            vec![model.forward_one_token(feed[0], &mut caches[0])]
        } else {
            let mut batch: Vec<SeqRows<'_>> = caches
                .iter_mut()
                .zip(&feed)
                .map(|(cache, &token)| {
                    let position = cache.seq_len;
                    SeqRows {
                        cache,
                        position,
                        input: ShardRowsInput::Tokens(vec![token]),
                    }
                })
                .collect();
            let outputs = model
                .forward_rows_multi(&mut batch, 0, n_layers)
                .map_err(|error| format!("decode: {error}"))?;
            drop(batch);
            let mut rows = Vec::with_capacity(count);
            for output in outputs {
                let ShardRowsOutput::Logits(mut row) = output else {
                    return Err("a whole-model call returned hidden rows".into());
                };
                rows.push(row.pop().ok_or("a decode step returned no logits")?);
            }
            rows
        };
        seconds.push(started.elapsed().as_secs_f64());
        for (tokens, row) in generated.iter_mut().zip(logits.iter_mut()) {
            let next = select_next_token_with_repetition_penalty(row, tokens);
            tokens.push(next);
        }
    }
    let decoded = generated
        .into_iter()
        .map(|tokens| Generated {
            hash: output_hash(&tokens),
            tokens,
        })
        .collect();
    Ok((decoded, seconds))
}

fn median(samples: &[f64]) -> f64 {
    let mut sorted = samples.to_vec();
    sorted.sort_by(f64::total_cmp);
    sorted[sorted.len() / 2]
}

fn main() -> Result<(), String> {
    let args = parse_args()?;
    let load_started = Instant::now();
    let model = load_cached_model_canonical_i8(&args.model)
        .map_err(|error| format!("could not load {}: {error}", args.model))?;
    if model.canonical_execution_profile() != Some(CANONICAL_REWARD_INFERENCE_PROFILE) {
        return Err("the model did not load as the canonical legacy INT8 profile".into());
    }
    let vocab = model.config.vocab_size;
    let most = args.sequences.iter().copied().max().unwrap_or(1);
    if vocab < 4 {
        return Err(format!("a {vocab}-token vocabulary is too small"));
    }
    if 1 + args.prompt + 3 + args.steps > model.config.max_seq {
        return Err("--prompt plus --steps exceeds the context window".into());
    }
    println!(
        "loaded {} layers in {:.1} s",
        model.config.n_layers,
        load_started.elapsed().as_secs_f64()
    );
    let cpu = cpu_model(args.cpu.as_deref());
    let features = canonical_simd::detected_cpu_features();
    println!("cpu: {cpu}; detected features: {features:?}");

    // Prompts of four lengths, so the sequences sit at different positions.
    // Fixed token ids that avoid the special tokens 0, 1 and 2.
    let prompts: Vec<Vec<u32>> = (0..most)
        .map(|sequence| {
            (0..args.prompt + sequence % 4)
                .map(|i| {
                    3 + u32::try_from((sequence * 7_919 + i * 104_729 + 17) % (vocab - 3))
                        .expect("a token id fits u32")
                })
                .collect()
        })
        .collect();

    // Every sequence alone through the worker's own call, with the widest
    // kernel, as the reference.
    canonical_simd::set_fast_canonical_kernel(true);
    canonical_simd::set_batched_kernel_preference(None);
    let alone_started = Instant::now();
    let alone: Vec<Generated> = prompts
        .iter()
        .map(|prompt| generate_alone(&model, prompt, args.steps))
        .collect::<Result<_, _>>()?;
    let prefilled = prefill_all(&model, &prompts)?;
    println!(
        "reference: {most} sequences alone (try_generate) and prefilled together in {:.1} s",
        alone_started.elapsed().as_secs_f64()
    );

    let mut results: Vec<Value> = Vec::new();
    let mut table = String::from(
        "| Kernel | Sequences | Rows per call | Decode step, ms (median) | Aggregate tokens per second | Against one sequence |\n|---|---|---|---|---|---|\n",
    );
    for &choice in &args.kernels {
        canonical_simd::set_fast_canonical_kernel(choice.is_some());
        canonical_simd::set_batched_kernel_preference(choice);
        if choice.is_some() && canonical_simd::selected_batched_kernel() != choice {
            return Err(format!("{} did not take effect", kernel_name(choice)));
        }
        let kernel = kernel_name(choice);
        let mut single_rate = None;
        for &count in &args.sequences {
            let (decoded, seconds) = decode_together(&model, &prefilled, count, args.steps)?;
            for (sequence, (got, reference)) in decoded.iter().zip(&alone).enumerate() {
                if got != reference {
                    return Err(format!(
                        "{kernel}, {count} sequences: sequence {sequence} generated other tokens \
                         or another output hash than its own try_generate"
                    ));
                }
            }
            let step = median(&seconds);
            let rate = count as f64 / step;
            let single = *single_rate.get_or_insert(rate);
            let path = if count == 1 {
                "one row (forward_one_token)"
            } else {
                "forward_rows_multi"
            };
            println!(
                "{kernel}, {count} sequences: {:.1} ms per step, {rate:.2} tokens/s, {:.2}x one sequence ({path})",
                step * 1000.0,
                rate / single
            );
            table.push_str(&format!(
                "| {kernel} | {count} | {count} | {:.1} | {rate:.2} | {:.2}x |\n",
                step * 1000.0,
                rate / single
            ));
            results.push(json!({
                "kernel": kernel,
                "sequences": count,
                "path": path,
                "step_seconds": seconds,
                "median_step_ms": step * 1000.0,
                "aggregate_tokens_per_second": rate,
                "against_one_sequence": rate / single,
            }));
        }
    }

    let report = json!({
        "model": args.model,
        "arch": std::env::consts::ARCH,
        "cpu": cpu,
        "cpu_features": features
            .iter()
            .map(|(name, present)| json!({ "feature": name, "detected": present }))
            .collect::<Vec<_>>(),
        "threads": std::thread::available_parallelism().map_or(0, |n| n.get()),
        "prompt_tokens": args.prompt,
        "steps": args.steps,
        "results": results,
    });
    if let Some(path) = &args.out {
        let text = serde_json::to_string_pretty(&report).map_err(|error| error.to_string())?;
        std::fs::write(path, text).map_err(|error| format!("{path}: {error}"))?;
    }
    if let Some(path) = &args.summary {
        std::fs::write(path, &table).map_err(|error| format!("{path}: {error}"))?;
    }
    print!("{table}");
    Ok(())
}
