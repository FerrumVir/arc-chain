//! Per-row cost of a multi-row stage call against one-row stage calls, on the
//! real network model. An internal measurement, not a product figure.
//!
//! The model is loaded as a community worker loads it
//! (`load_cached_model_canonical_i8`, the legacy split-half canonical INT8
//! profile). For a 2-stage and a 4-stage split of its layers, every stage
//! holder first takes the same context, and then, for each k, the driver runs
//! the next k positions through every stage two ways:
//!
//! * one `forward_shard_rows` call per stage carrying all k rows;
//! * k `forward_shard_token` calls per stage, one row each.
//!
//! Both must hand on bit-identical hidden rows and logits, or the driver exits
//! non-zero. After each run every holder is rolled back to the context with
//! `rollback_rows`, so every run starts from the same caches. The report is
//! the median, over `--repeats` runs, of the time per row summed over the
//! stages.
//!
//! Kernels. `scalar` is the scalar kernel. Every other kernel turns the
//! vectorised path on and pins the multi-row (batched) kernel by its label:
//! `neon-sdot-limb` and `neon-i8mm-limb` on arm64, `avx2-limb`,
//! `avx-vnni-limb` and `avx512-vnni-limb` on x86-64. `simd` is the base
//! kernel of the architecture (SDOT or AVX2), `best` the one chosen
//! automatically, and `all` (the default) is `scalar` plus every multi-row
//! kernel this CPU has. One-row calls always run the one-row kernel (scalar,
//! or SDOT/AVX2), so for every vectorised kernel the baseline is the same
//! one-row pass. The report also records which multi-row kernel ran: a
//! one-row k-row call on `neon-i8mm-limb` runs SDOT, because SMMLA pairs rows.
//!
//! usage: stage_rows_bench --model GGUF [--kernel all|scalar|simd|best|LABEL,...]
//!        [--splits 2,4] [--context N] [--repeats N] [--ks 1,2,4,8,16,32,64]
//!        [--out FILE.json] [--summary FILE.md]

use arc_crypto::{Hash256, hash_bytes};
use arc_inference::cached_integer_model::{
    CANONICAL_REWARD_INFERENCE_PROFILE, CachedIntegerModel, KVCache, ShardInput, ShardOutput,
    ShardRowsInput, ShardRowsOutput, load_cached_model_canonical_i8,
};
use arc_inference::canonical_simd::{self, BatchedKernel};
use serde_json::{Value, json};
use std::time::Instant;

/// `None` is the scalar kernel; `Some` pins a multi-row vectorised kernel.
type KernelChoice = Option<BatchedKernel>;

struct Args {
    model: String,
    kernels: Vec<KernelChoice>,
    splits: Vec<usize>,
    context: usize,
    repeats: usize,
    ks: Vec<usize>,
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
            // #185's spelling: the scalar and the base vectorised kernel.
            "both" => kernels.extend([None, Some(base_kernel()?)]),
            "best" => kernels
                .push(Some(canonical_simd::best_batched_kernel().ok_or_else(
                    || "this CPU has no vectorised kernel".to_string(),
                )?)),
            label => {
                let kernel = BatchedKernel::from_label(label).ok_or_else(|| {
                    format!("--kernel takes all, scalar, simd, best, both or a kernel label, not {label}")
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

/// Multi-row projections per kernel so far, in `BatchedKernel::ALL` order.
fn batched_runs() -> [u64; 5] {
    BatchedKernel::ALL.map(canonical_simd::batched_kernel_runs)
}

/// The multi-row kernels that ran between two `batched_runs` snapshots.
fn kernels_that_ran(before: [u64; 5], after: [u64; 5]) -> Vec<&'static str> {
    BatchedKernel::ALL
        .into_iter()
        .zip(before.into_iter().zip(after))
        .filter(|(_, (before, after))| after > before)
        .map(|(kernel, _)| kernel.label())
        .collect()
}

/// The CPU model, from `/proc/cpuinfo` where there is one.
fn cpu_model() -> String {
    std::fs::read_to_string("/proc/cpuinfo")
        .ok()
        .and_then(|info| {
            info.lines()
                .find(|line| line.starts_with("model name"))
                .and_then(|line| line.split(':').nth(1))
                .map(|name| name.trim().to_string())
        })
        .unwrap_or_else(|| "unknown".to_string())
}

fn parse_args() -> Result<Args, String> {
    let mut args = Args {
        model: String::new(),
        kernels: Vec::new(),
        splits: vec![2, 4],
        context: 32,
        repeats: 3,
        ks: vec![1, 2, 4, 8, 16, 32, 64],
        out: None,
        summary: None,
    };
    let mut items = std::env::args().skip(1);
    while let Some(flag) = items.next() {
        let mut value = || items.next().ok_or_else(|| format!("{flag} needs a value"));
        match flag.as_str() {
            "--model" => args.model = value()?,
            "--kernel" => args.kernels = parse_kernels(&value()?)?,
            "--splits" => {
                args.splits = value()?
                    .split(',')
                    .map(parse_count)
                    .collect::<Result<Vec<_>, _>>()?
            }
            "--context" => args.context = parse_count(&value()?)?,
            "--repeats" => args.repeats = parse_count(&value()?)?,
            "--ks" => {
                args.ks = value()?
                    .split(',')
                    .map(parse_count)
                    .collect::<Result<Vec<_>, _>>()?
            }
            "--out" => args.out = Some(value()?),
            "--summary" => args.summary = Some(value()?),
            other => return Err(format!("unknown argument {other}")),
        }
    }
    if args.model.is_empty() {
        return Err("--model is required".into());
    }
    if args.repeats == 0 || args.ks.is_empty() || args.ks.contains(&0) {
        return Err("--repeats and every k must be at least 1".into());
    }
    if args.splits.is_empty() || args.splits.contains(&0) {
        return Err("every split needs at least one stage".into());
    }
    if args.kernels.is_empty() {
        args.kernels = parse_kernels("all")?;
    }
    Ok(args)
}

fn hash_row(row: &[i64]) -> Hash256 {
    let bytes: Vec<u8> = row.iter().flat_map(|value| value.to_le_bytes()).collect();
    hash_bytes(&bytes)
}

/// Hashes of what every stage hands on, indexed `[stage][row]`: hidden rows
/// at the cuts and logits on the last stage.
type Handoffs = Vec<Vec<Hash256>>;

/// One `forward_shard_rows` call per stage, carrying every row.
fn run_rows(
    model: &CachedIntegerModel,
    ends: &[usize],
    holders: &mut [KVCache],
    position: usize,
    tokens: &[u32],
) -> Result<Handoffs, String> {
    let mut input = ShardRowsInput::Tokens(tokens.to_vec());
    let mut start = 0;
    let mut handoffs = Vec::with_capacity(ends.len());
    for (&end, holder) in ends.iter().zip(holders.iter_mut()) {
        let output = model
            .forward_shard_rows(input, holder, start, end, position)
            .map_err(|error| format!("stage [{start}, {end}): {error}"))?;
        match output {
            ShardRowsOutput::Hidden(rows) => {
                handoffs.push(rows.iter().map(Vec::as_slice).map(hash_row).collect());
                input = ShardRowsInput::Hidden(rows);
            }
            ShardRowsOutput::Logits(rows) => {
                handoffs.push(rows.iter().map(Vec::as_slice).map(hash_row).collect());
                return Ok(handoffs);
            }
        }
        start = end;
    }
    Err("the last stage returned hidden rows".into())
}

/// One `forward_shard_token` call per stage and row.
fn run_one_row_at_a_time(
    model: &CachedIntegerModel,
    ends: &[usize],
    holders: &mut [KVCache],
    position: usize,
    tokens: &[u32],
) -> Result<Handoffs, String> {
    let mut handoffs: Handoffs = vec![Vec::with_capacity(tokens.len()); ends.len()];
    for (offset, &token) in tokens.iter().enumerate() {
        let mut input = ShardInput::Token(token);
        let mut start = 0;
        for (stage, (&end, holder)) in ends.iter().zip(holders.iter_mut()).enumerate() {
            let output = model
                .forward_shard_token(input, holder, start, end, position + offset)
                .map_err(|error| format!("stage [{start}, {end}): {error}"))?;
            match output {
                ShardOutput::Hidden(hidden) => {
                    handoffs[stage].push(hash_row(&hidden));
                    input = ShardInput::Hidden(hidden);
                }
                ShardOutput::Token { logits_hash, .. } => {
                    handoffs[stage].push(logits_hash);
                    break;
                }
            }
            start = end;
        }
    }
    Ok(handoffs)
}

fn roll_back(
    model: &CachedIntegerModel,
    holders: &mut [KVCache],
    keep: usize,
) -> Result<(), String> {
    for holder in holders {
        model
            .rollback_rows(holder, keep)
            .map_err(|error| error.to_string())?;
    }
    Ok(())
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
    let n_layers = model.config.n_layers;
    let vocab = model.config.vocab_size;
    let max_k = args.ks.iter().copied().max().unwrap_or(1);
    if vocab < 4 {
        return Err(format!("a {vocab}-token vocabulary is too small"));
    }
    for &stages in &args.splits {
        if stages > n_layers || !n_layers.is_multiple_of(stages) {
            return Err(format!("{n_layers} layers do not split in {stages}"));
        }
    }
    if args.context + max_k > model.config.max_seq {
        return Err("--context plus the largest k exceeds the context window".into());
    }
    println!(
        "loaded {n_layers} layers in {:.1} s",
        load_started.elapsed().as_secs_f64()
    );

    // Fixed token ids that avoid the special tokens 0, 1 and 2.
    let token = |index: usize| 3 + u32::try_from(index * 7_919 % (vocab - 3)).unwrap();
    let context: Vec<u32> = (0..args.context).map(token).collect();
    let rows: Vec<u32> = (args.context..args.context + max_k).map(token).collect();
    let splits: Vec<(String, Vec<usize>)> = args
        .splits
        .iter()
        .map(|&stages| {
            let ends: Vec<usize> = (1..=stages).map(|s| s * n_layers / stages).collect();
            (format!("{stages} stages"), ends)
        })
        .collect();
    let cpu = cpu_model();
    let features = canonical_simd::detected_cpu_features();
    println!("cpu: {cpu}; detected features: {features:?}");

    let mut results: Vec<Value> = Vec::new();
    let mut table = String::from(
        "| Kernel | Split | k | One k-row call per stage, ms per row | k one-row calls per stage, ms per row | k-row cost / one-row cost | Multi-row kernel that ran |\n|---|---|---|---|---|---|---|\n",
    );
    for &choice in &args.kernels {
        canonical_simd::set_fast_canonical_kernel(choice.is_some());
        canonical_simd::set_batched_kernel_preference(choice);
        if choice.is_some() && canonical_simd::selected_batched_kernel() != choice {
            return Err(format!("{} did not take effect", kernel_name(choice)));
        }
        let kernel = kernel_name(choice);
        for (split, ends) in &splits {
            let mut holders: Vec<KVCache> = ends.iter().map(|_| KVCache::new(n_layers)).collect();
            run_rows(&model, ends, &mut holders, 0, &context)?;
            for &k in &args.ks {
                let mut many = Vec::with_capacity(args.repeats);
                let mut one = Vec::with_capacity(args.repeats);
                let mut ran: Vec<&str> = Vec::new();
                for _ in 0..args.repeats {
                    let before = batched_runs();
                    let started = Instant::now();
                    let by_rows = run_rows(&model, ends, &mut holders, args.context, &rows[..k])?;
                    many.push(started.elapsed().as_secs_f64());
                    for label in kernels_that_ran(before, batched_runs()) {
                        if !ran.contains(&label) {
                            ran.push(label);
                        }
                    }
                    roll_back(&model, &mut holders, args.context)?;
                    let started = Instant::now();
                    let by_token = run_one_row_at_a_time(
                        &model,
                        ends,
                        &mut holders,
                        args.context,
                        &rows[..k],
                    )?;
                    one.push(started.elapsed().as_secs_f64());
                    roll_back(&model, &mut holders, args.context)?;
                    if by_rows != by_token {
                        return Err(format!(
                            "{kernel}, {split}, k={k}: the k-row and one-row stage calls handed on different rows"
                        ));
                    }
                }
                // The scalar kernel never enters the vectorised path.
                let ran = if ran.is_empty() {
                    "scalar".to_string()
                } else {
                    ran.join(" + ")
                };
                if let Some(pinned) = choice {
                    let expected = if pinned == BatchedKernel::NeonI8mm && k == 1 {
                        BatchedKernel::NeonSdot.label()
                    } else {
                        pinned.label()
                    };
                    if ran != expected {
                        return Err(format!(
                            "{kernel}, {split}, k={k}: expected {expected} to run, but {ran} ran"
                        ));
                    }
                }
                let rows_per_call = k as f64;
                let many_ms = median(&many) * 1000.0 / rows_per_call;
                let one_ms = median(&one) * 1000.0 / rows_per_call;
                println!(
                    "{kernel}, {split}, k={k}: k-row call {many_ms:.1} ms/row, one-row calls {one_ms:.1} ms/row, ratio {:.3}, ran {ran}",
                    many_ms / one_ms
                );
                table.push_str(&format!(
                    "| {kernel} | {split} | {k} | {many_ms:.1} | {one_ms:.1} | {:.3} | {ran} |\n",
                    many_ms / one_ms
                ));
                results.push(json!({
                    "kernel": kernel,
                    "split": split,
                    "stage_ends": ends,
                    "k": k,
                    "k_row_ms_per_row": many_ms,
                    "one_row_ms_per_row": one_ms,
                    "k_row_seconds": many,
                    "one_row_seconds": one,
                    "multi_row_kernel_ran": ran,
                }));
            }
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
        "context": args.context,
        "repeats": args.repeats,
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
