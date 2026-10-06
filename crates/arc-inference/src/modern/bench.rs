//! Speed measurement for the dyadic profile, with a determinism check.
//!
//! [`run`] measures, on one machine and one loaded model:
//!
//! * decode tok/s of every requested [`Spec`] after the same prefilled
//!   context, with the logits digest of every run (all specs must agree, or
//!   the result says so);
//! * decode tok/s of the fastest spec at several thread counts;
//! * where a decoded token's time goes ([`Engine`] phase profile, plus the
//!   logits hash and token selection);
//! * throughput of each kernel on the model's own matrices;
//! * the read bandwidth this machine sustains over the model's weights, the
//!   ceiling every decode figure is compared with.
//!
//! The numbers describe the machine they ran on (a CI runner unless stated
//! otherwise); projections to other devices are derived from them elsewhere
//! and labelled as projections.

use std::time::Instant;

use rayon::prelude::*;
use serde_json::{Value, json};

use super::ModernError;
use super::arith;
use super::engine::{Engine, Spec};
use super::kernels::{self, Kernel, MatrixRows, PreparedInput};
use super::model::{KvCache, ModernModel, TokenForward};

/// What [`run`] measures.
#[derive(Debug, Clone)]
pub struct BenchOptions {
    /// Specs whose decode speed is compared (the first fast spec also
    /// prefills, profiles and runs the thread scaling).
    pub specs: Vec<Spec>,
    /// Threads for the spec comparison (0 = rayon's default).
    pub threads: usize,
    /// Thread counts for the scaling runs of the first fast spec.
    pub scaling: Vec<usize>,
    /// Context lengths (prompt positions) before decoding.
    pub contexts: Vec<usize>,
    /// Tokens decoded per run.
    pub decode_tokens: usize,
    /// Token ids repeated to fill each context.
    pub source_tokens: Vec<u32>,
    /// Profile the phases of a decoded token.
    pub profile: bool,
    /// Measure each kernel on the model's matrices.
    pub kernel_micro: bool,
    /// Measure read bandwidth over the weights.
    pub bandwidth: bool,
}

fn pool(threads: usize) -> Result<rayon::ThreadPool, ModernError> {
    rayon::ThreadPoolBuilder::new()
        .num_threads(threads)
        .build()
        .map_err(|e| ModernError::Invalid(format!("thread pool: {e}")))
}

fn rate(count: f64, seconds: f64) -> f64 {
    if seconds > 0.0 { count / seconds } else { 0.0 }
}

/// INT8 weight bytes a decoded token streams: every matrix once (the tied
/// embedding is read as the LM head; the lookup reads one row).
pub fn weight_bytes(model: &ModernModel) -> usize {
    model.weight_count()
}

/// KV-cache bytes attention reads for one token at `positions` positions.
pub fn kv_bytes(model: &ModernModel, positions: usize) -> usize {
    model.config.kv_bytes_per_position() * positions
}

/// Prefill `tokens` with `spec`; returns the cache, the last logits and the
/// seconds taken.
fn prefill(
    model: &ModernModel,
    spec: Spec,
    tokens: &[u32],
    room: usize,
) -> Result<(KvCache, Vec<i64>, f64), ModernError> {
    spec.apply();
    let mut runner = spec.runner(model);
    runner.begin(room);
    let start = Instant::now();
    let mut last = Vec::new();
    for (index, &token) in tokens.iter().enumerate() {
        let logits = runner.forward_token(token)?;
        if index + 1 == tokens.len() {
            last = logits.to_vec();
        }
    }
    let seconds = start.elapsed().as_secs_f64();
    Ok((runner.kv_cache().clone(), last, seconds))
}

/// Greedy decode of `count` tokens from a prefilled cache: seconds, the
/// digest of the logits hashes, and the tokens.
fn decode(
    runner: &mut dyn TokenForward,
    model: &ModernModel,
    cache: &KvCache,
    last: &[i64],
    count: usize,
) -> Result<(f64, String, Vec<u32>), ModernError> {
    let mut cache = cache.clone();
    cache.reserve(cache.positions() + count, model.config.d_kv());
    runner.set_kv_cache(cache);
    let mut next = arith::argmax(last) as u32;
    let mut hashes = Vec::with_capacity(count);
    let mut tokens = Vec::with_capacity(count);
    let start = Instant::now();
    for _ in 0..count {
        tokens.push(next);
        let logits = runner.forward_token(next)?;
        hashes.push(arith::logits_hash(logits));
        next = arith::argmax(logits) as u32;
    }
    let seconds = start.elapsed().as_secs_f64();
    let digest = super::hex_lower(&arith::logits_digest(&hashes));
    Ok((seconds, digest, tokens))
}

/// Read every weight byte once with `threads` threads; returns GB/s.
fn read_bandwidth(model: &ModernModel, threads: usize) -> Result<f64, ModernError> {
    let mut slices: Vec<&[i8]> = vec![model.embed.q.as_slice()];
    for layer in &model.layers {
        for m in [
            &layer.wq,
            &layer.wk,
            &layer.wv,
            &layer.wo,
            &layer.w_gate,
            &layer.w_up,
            &layer.w_down,
        ] {
            slices.push(m.q.as_slice());
        }
    }
    let bytes: usize = slices.iter().map(|s| s.len()).sum();
    let pool = pool(threads)?;
    let mut best = f64::MAX;
    for _ in 0..3 {
        let start = Instant::now();
        let sum = pool.install(|| {
            slices
                .par_iter()
                .flat_map(|s| s.par_chunks(1 << 20))
                .map(|chunk| {
                    // SAFETY: every bit pattern is a valid u64; `align_to`
                    // returns an aligned middle part and the unaligned ends.
                    let (head, words, tail) = unsafe { chunk.align_to::<u64>() };
                    let ends = head
                        .iter()
                        .chain(tail)
                        .fold(0u64, |acc, &b| acc.wrapping_add(u64::from(b as u8)));
                    words.iter().fold(ends, |acc, &w| acc.wrapping_add(w))
                })
                .reduce(|| 0u64, u64::wrapping_add)
        });
        std::hint::black_box(sum);
        best = best.min(start.elapsed().as_secs_f64());
    }
    Ok(rate(bytes as f64, best) / 1e9)
}

/// Each available kernel on the model's own matrices: seconds per call and
/// GB/s of weights streamed, with the default thread pool.
fn kernel_micro(model: &ModernModel) -> Result<Vec<Value>, ModernError> {
    let layer = &model.layers[0];
    let matrices = [
        ("q_proj", &layer.wq),
        ("k_proj", &layer.wk),
        ("o_proj", &layer.wo),
        ("gate_proj", &layer.w_gate),
        ("down_proj", &layer.w_down),
        ("lm_head", &model.embed),
    ];
    let mut rows = Vec::new();
    for kernel in Kernel::available_kernels() {
        for (name, m) in matrices {
            // A deterministic input with RMS-normalised magnitudes (|x| up to
            // 8.0 in Q16), the common case for projection inputs.
            let x: Vec<i64> = (0..m.cols)
                .map(|j| ((j as i64 * 7919) % (16 << 16)) - (8 << 16))
                .collect();
            let mut input = PreparedInput::new();
            input.prepare(&x, kernel)?;
            let mut out = vec![0i64; m.rows];
            kernels::project(MatrixRows::of(m), &input, &mut out)?;
            let mut calls = 0usize;
            let start = Instant::now();
            while calls < 3 || (start.elapsed().as_secs_f64() < 0.3 && calls < 1000) {
                kernels::project(MatrixRows::of(m), &input, &mut out)?;
                calls += 1;
            }
            let seconds = start.elapsed().as_secs_f64() / calls as f64;
            let weights = (m.rows * m.cols) as f64;
            rows.push(json!({
                "kernel": kernel.name(),
                "matrix": name,
                "rows": m.rows,
                "cols": m.cols,
                "limbs": input.limbs(),
                "calls": calls,
                "seconds_per_call": seconds,
                "gb_s": rate(weights, seconds) / 1e9,
            }));
        }
    }
    Ok(rows)
}

/// Run the measurements described by `options`.
pub fn run(model: &ModernModel, options: &BenchOptions) -> Result<Value, ModernError> {
    let c = &model.config;
    if options.specs.is_empty() || options.source_tokens.is_empty() {
        return Err(ModernError::Invalid(
            "bench needs at least one spec and source tokens".into(),
        ));
    }
    if options.decode_tokens == 0 {
        return Err(ModernError::Invalid("bench needs decode tokens".into()));
    }
    let lead = options
        .specs
        .iter()
        .copied()
        .find(|s| matches!(s, Spec::Fast(_)))
        .unwrap_or(options.specs[0]);
    let threads = if options.threads == 0 {
        rayon::current_num_threads()
    } else {
        options.threads
    };
    let main_pool = pool(threads)?;
    let mut bandwidth = Vec::new();
    if options.bandwidth {
        let mut counts = options.scaling.clone();
        counts.push(threads);
        counts.sort_unstable();
        counts.dedup();
        for count in counts {
            bandwidth.push(json!({
                "threads": count,
                "read_gb_s": read_bandwidth(model, count)?,
            }));
        }
    }
    let mut contexts = Vec::new();
    for &context in &options.contexts {
        if context == 0 || context + options.decode_tokens > c.max_seq {
            return Err(ModernError::Invalid(format!(
                "context {context} + {} decode tokens does not fit {} positions",
                options.decode_tokens, c.max_seq
            )));
        }
        let tokens: Vec<u32> = options
            .source_tokens
            .iter()
            .copied()
            .cycle()
            .take(context)
            .collect();
        let room = context + options.decode_tokens;
        let (cache, last, prefill_seconds) =
            main_pool.install(|| prefill(model, lead, &tokens, room))?;
        let position_bytes =
            weight_bytes(model) + kv_bytes(model, context + options.decode_tokens / 2);
        let mut runs = Vec::new();
        let mut digests = Vec::new();
        let mut measure = |spec: Spec, count: usize| -> Result<(), ModernError> {
            let run_pool = pool(count)?;
            let (seconds, digest, tokens) = run_pool.install(|| {
                spec.apply();
                Spec::start_census();
                let mut runner = spec.runner(model);
                decode(runner.as_mut(), model, &cache, &last, options.decode_tokens)
            })?;
            let tok_s = rate(options.decode_tokens as f64, seconds);
            digests.push(digest.clone());
            runs.push(json!({
                "spec": spec.name(),
                "threads": count,
                "tokens": options.decode_tokens,
                "seconds": seconds,
                "tok_s": tok_s,
                "bytes_per_token": position_bytes,
                "effective_gb_s": tok_s * position_bytes as f64 / 1e9,
                "logits_digest": digest,
                "generated": tokens,
                "census": serde_json::to_value(spec.census()).unwrap_or(Value::Null),
            }));
            Ok(())
        };
        for &spec in &options.specs {
            measure(spec, threads)?;
        }
        for &count in &options.scaling {
            if count != threads {
                measure(lead, count)?;
            }
        }
        let digests_equal = digests.windows(2).all(|w| w[0] == w[1]);
        let mut profile = Value::Null;
        if let (true, Spec::Fast(kernel)) = (options.profile, lead) {
            profile = main_pool.install(|| -> Result<Value, ModernError> {
                lead.apply();
                let mut engine = Engine::new(model, kernel);
                let mut snapshot = cache.clone();
                snapshot.reserve(room, c.d_kv());
                engine.set_kv_cache(snapshot);
                engine.set_profiling(true);
                let mut next = arith::argmax(&last) as u32;
                let mut select_hash = 0f64;
                let mut scratch = Vec::new();
                let start = Instant::now();
                for _ in 0..options.decode_tokens {
                    let logits = engine.forward(next)?;
                    let mark = Instant::now();
                    std::hint::black_box(arith::logits_hash(logits));
                    next = arith::select_into(
                        logits,
                        &[next],
                        arith::Selection::Rp64Argmax,
                        &mut scratch,
                    )?;
                    select_hash += mark.elapsed().as_secs_f64();
                }
                let total = start.elapsed().as_secs_f64();
                let per_token = options.decode_tokens as f64;
                let mut phases = serde_json::Map::new();
                for (name, seconds) in engine.phase_seconds() {
                    phases.insert(name.to_string(), json!(seconds / per_token));
                }
                phases.insert("hash_and_select".into(), json!(select_hash / per_token));
                Ok(json!({
                    "spec": lead.name(),
                    "threads": threads,
                    "per_token_seconds": phases,
                    "total_per_token_seconds": total / per_token,
                }))
            })?;
        }
        contexts.push(json!({
            "context": context,
            "decode_tokens": options.decode_tokens,
            "prefill": {
                "spec": lead.name(),
                "threads": threads,
                "tokens": context,
                "seconds": prefill_seconds,
                "tok_s": rate(context as f64, prefill_seconds),
            },
            "decode": runs,
            "digests_equal": digests_equal,
            "profile": profile,
        }));
    }
    let micro = if options.kernel_micro {
        main_pool.install(|| kernel_micro(model))?
    } else {
        Vec::new()
    };
    Ok(json!({
        "schema": "arc.modern-bench.v1",
        "profile": super::PROFILE,
        "platform": {
            "os": std::env::consts::OS,
            "arch": std::env::consts::ARCH,
            "logical_cpus": std::thread::available_parallelism().map(|n| n.get()).unwrap_or(0),
            "kernels_available": Kernel::available_kernels().iter().map(|k| k.name()).collect::<Vec<_>>(),
        },
        "model": {
            "architecture": c.architecture,
            "n_layers": c.n_layers,
            "d_model": c.d_model,
            "d_ff": c.d_ff,
            "vocab_size": c.vocab_size,
            "weight_bytes": weight_bytes(model),
            "kv_bytes_per_position": c.kv_bytes_per_position(),
        },
        "threads": threads,
        "bandwidth": bandwidth,
        "contexts": contexts,
        "kernel_micro": micro,
        "limb_histogram": kernels::limb_histogram(),
    }))
}
