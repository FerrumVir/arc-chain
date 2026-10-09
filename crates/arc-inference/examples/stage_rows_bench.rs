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
//! usage: stage_rows_bench --model GGUF [--kernel scalar|simd|both]
//!        [--context N] [--repeats N] [--ks 1,2,4,8]
//!        [--out FILE.json] [--summary FILE.md]

use arc_crypto::{Hash256, hash_bytes};
use arc_inference::cached_integer_model::{
    CANONICAL_REWARD_INFERENCE_PROFILE, CachedIntegerModel, KVCache, ShardInput, ShardOutput,
    ShardRowsInput, ShardRowsOutput, load_cached_model_canonical_i8,
};
use arc_inference::canonical_simd;
use serde_json::{Value, json};
use std::time::Instant;

struct Args {
    model: String,
    kernels: Vec<bool>,
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

fn parse_args() -> Result<Args, String> {
    let mut args = Args {
        model: String::new(),
        kernels: vec![false, true],
        context: 32,
        repeats: 3,
        ks: vec![1, 2, 4, 8],
        out: None,
        summary: None,
    };
    let mut items = std::env::args().skip(1);
    while let Some(flag) = items.next() {
        let mut value = || items.next().ok_or_else(|| format!("{flag} needs a value"));
        match flag.as_str() {
            "--model" => args.model = value()?,
            "--kernel" => {
                args.kernels = match value()?.as_str() {
                    "scalar" => vec![false],
                    "simd" => vec![true],
                    "both" => vec![false, true],
                    other => {
                        return Err(format!(
                            "--kernel must be scalar, simd or both, not {other}"
                        ));
                    }
                }
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
    if n_layers < 4 || !n_layers.is_multiple_of(4) || vocab < 4 {
        return Err(format!(
            "{n_layers} layers and a {vocab}-token vocabulary do not split in four"
        ));
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
    let splits = [
        ("2 stages", vec![n_layers / 2, n_layers]),
        (
            "4 stages",
            vec![n_layers / 4, n_layers / 2, 3 * n_layers / 4, n_layers],
        ),
    ];

    let mut results: Vec<Value> = Vec::new();
    let mut table = String::from(
        "| Kernel | Split | k | One k-row call per stage, ms per row | k one-row calls per stage, ms per row | k-row cost / one-row cost |\n|---|---|---|---|---|---|\n",
    );
    for &vectorised in &args.kernels {
        if vectorised && !canonical_simd::dotprod_available() {
            return Err("this CPU has no vectorised kernel".into());
        }
        canonical_simd::set_fast_canonical_kernel(vectorised);
        let kernel = if vectorised { "vector" } else { "scalar" };
        for (split, ends) in &splits {
            let mut holders: Vec<KVCache> = ends.iter().map(|_| KVCache::new(n_layers)).collect();
            run_rows(&model, ends, &mut holders, 0, &context)?;
            for &k in &args.ks {
                let mut many = Vec::with_capacity(args.repeats);
                let mut one = Vec::with_capacity(args.repeats);
                for _ in 0..args.repeats {
                    let started = Instant::now();
                    let by_rows = run_rows(&model, ends, &mut holders, args.context, &rows[..k])?;
                    many.push(started.elapsed().as_secs_f64());
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
                let rows_per_call = k as f64;
                let many_ms = median(&many) * 1000.0 / rows_per_call;
                let one_ms = median(&one) * 1000.0 / rows_per_call;
                println!(
                    "{kernel}, {split}, k={k}: k-row call {many_ms:.1} ms/row, one-row calls {one_ms:.1} ms/row, ratio {:.3}",
                    many_ms / one_ms
                );
                table.push_str(&format!(
                    "| {kernel} | {split} | {k} | {many_ms:.1} | {one_ms:.1} | {:.3} |\n",
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
                }));
            }
        }
    }

    let report = json!({
        "model": args.model,
        "arch": std::env::consts::ARCH,
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
