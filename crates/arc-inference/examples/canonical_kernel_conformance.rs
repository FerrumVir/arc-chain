//! Bounded conformance and timing for the opt-in vectorised canonical kernel.
//!
//! Loads ONE real canonical-I8 model, drives a FIXED token sequence through it
//! twice - once on the existing scalar kernel, once on the vectorised one -
//! and compares EVERY logit at EVERY position exactly. The token sequence is
//! fixed in advance, so it cannot drift between the two runs.
//!
//! This is a conformance experiment. It changes no admission, execution or
//! consensus semantics: the vectorised kernel is opt-in and defaults to off.
//!
//! usage: canonical_kernel_conformance GGUF [n_positions] [repeats]

use arc_inference::cached_integer_model::{CachedIntegerModel, KVCache};
use arc_inference::canonical_simd;
use std::time::Instant;

fn max_rss_bytes() -> u64 {
    // SAFETY: getrusage with a zeroed, correctly sized out-parameter.
    unsafe {
        let mut u: libc::rusage = std::mem::zeroed();
        if libc::getrusage(libc::RUSAGE_SELF, &mut u) == 0 {
            u.ru_maxrss as u64 // bytes on macOS
        } else {
            0
        }
    }
}

fn run(model: &CachedIntegerModel, tokens: &[u32]) -> (Vec<Vec<i64>>, Vec<f64>) {
    let mut cache = KVCache::new(model.config.n_layers);
    let mut logits = Vec::with_capacity(tokens.len());
    let mut ms = Vec::with_capacity(tokens.len());
    for &t in tokens {
        let start = Instant::now();
        let l = model.forward_one_token(t, &mut cache);
        ms.push(start.elapsed().as_secs_f64() * 1e3);
        logits.push(l);
    }
    (logits, ms)
}

fn stats(v: &[f64]) -> (f64, f64, f64) {
    let mut s = v.to_vec();
    s.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let sum: f64 = s.iter().sum();
    (s[0], s[s.len() / 2], sum / s.len() as f64)
}

fn main() -> Result<(), String> {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 2 {
        return Err("usage: canonical_kernel_conformance GGUF [n_positions] [repeats]".into());
    }
    let n_pos: usize = args.get(2).map(|s| s.parse().unwrap_or(8)).unwrap_or(8);
    let repeats: usize = args.get(3).map(|s| s.parse().unwrap_or(3)).unwrap_or(3);

    // Fixed real Llama-2 token ids, independent of either kernel's output.
    let base: [u32; 12] = [
        1, 6324, 29892, 306, 29915, 29885, 263, 1243, 310, 278, 1904, 29889,
    ];
    let tokens: Vec<u32> = (0..n_pos).map(|i| base[i % base.len()]).collect();

    let t0 = Instant::now();
    let model =
        arc_inference::cached_integer_model::load_cached_model_canonical_i8_interleaved_rope(
            &args[1],
        )
        .map_err(|e| e.to_string())?;
    let load_ms = t0.elapsed().as_secs_f64() * 1e3;

    let profile = model
        .canonical_execution_profile()
        .ok_or("model is not complete canonical I8")?
        .to_string();
    eprintln!(
        "profile={profile} layers={} d_model={} vocab={} load_ms={load_ms:.0}",
        model.config.n_layers, model.config.d_model, model.config.vocab_size
    );
    eprintln!(
        "dotprod_available={} limb_bounds_ok={} limb_domain=[{}, {}] max_cols_for_i32={}",
        canonical_simd::dotprod_available(),
        canonical_simd::limb_bounds_match_formula(),
        canonical_simd::LIMB_MIN,
        canonical_simd::LIMB_MAX,
        canonical_simd::MAX_COLS_FOR_I32
    );
    if !canonical_simd::dotprod_available() {
        return Err("this CPU/build has no ARM dotprod; nothing to compare".into());
    }

    // Warmup on each path (discarded) so neither run pays first-touch costs.
    canonical_simd::set_projection_census_enabled(false);
    canonical_simd::set_fast_canonical_kernel(false);
    let _ = run(&model, &tokens[..1.min(tokens.len())]);
    canonical_simd::set_fast_canonical_kernel(true);
    let _ = run(&model, &tokens[..1.min(tokens.len())]);

    let mut compared_positions = 0usize;
    let mut compared_logits = 0usize;
    let mut max_abs_diff = 0i64;
    let mut first_mismatch: Option<(usize, usize, i64, i64)> = None;
    let mut reference: Option<Vec<Vec<i64>>> = None;
    let mut order_log: Vec<&'static str> = Vec::new();

    // One pass = `repeats` rounds; the kernel ORDER ALTERNATES between rounds,
    // so a fixed scalar-first ordering cannot be what produces the difference.
    let one_pass = |census: bool,
                    scalar_ms: &mut Vec<f64>,
                    fast_ms: &mut Vec<f64>,
                    compared_positions: &mut usize,
                    compared_logits: &mut usize,
                    max_abs_diff: &mut i64,
                    first_mismatch: &mut Option<(usize, usize, i64, i64)>,
                    reference: &mut Option<Vec<Vec<i64>>>,
                    order_log: &mut Vec<&'static str>|
     -> Result<(), String> {
        canonical_simd::set_projection_census_enabled(census);
        for r in 0..repeats {
            let scalar_first = r % 2 == 0;
            order_log.push(if scalar_first {
                "scalar-first"
            } else {
                "vectorised-first"
            });
            let (s_logits, s_ms, f_logits, f_ms) = if scalar_first {
                canonical_simd::set_fast_canonical_kernel(false);
                let (sl, sm) = run(&model, &tokens);
                canonical_simd::set_fast_canonical_kernel(true);
                let (fl, fm) = run(&model, &tokens);
                (sl, sm, fl, fm)
            } else {
                canonical_simd::set_fast_canonical_kernel(true);
                let (fl, fm) = run(&model, &tokens);
                canonical_simd::set_fast_canonical_kernel(false);
                let (sl, sm) = run(&model, &tokens);
                (sl, sm, fl, fm)
            };
            canonical_simd::set_fast_canonical_kernel(false);

            for (p, (a, b)) in s_logits.iter().zip(f_logits.iter()).enumerate() {
                if a.len() != b.len() {
                    return Err(format!("logit length mismatch at position {p}"));
                }
                *compared_positions += 1;
                *compared_logits += a.len();
                for (i, (x, y)) in a.iter().zip(b.iter()).enumerate() {
                    let d = (x - y).abs();
                    if d > *max_abs_diff {
                        *max_abs_diff = d;
                    }
                    if x != y && first_mismatch.is_none() {
                        *first_mismatch = Some((p, i, *x, *y));
                    }
                }
            }
            match reference {
                None => *reference = Some(s_logits.clone()),
                Some(ref0) => {
                    for (p, (a, b)) in ref0.iter().zip(s_logits.iter()).enumerate() {
                        if a != b {
                            return Err(format!(
                                "scalar run {r} diverged from run 0 at position {p}"
                            ));
                        }
                    }
                }
            }
            scalar_ms.extend(s_ms);
            fast_ms.extend(f_ms);
        }
        canonical_simd::set_projection_census_enabled(false);
        Ok(())
    };

    // Pass A: diagnostic counting ON, so the census describes a REAL
    // conformance run rather than a separate synthetic one.
    canonical_simd::reset_projection_census();
    let (mut sa_ms, mut fa_ms) = (Vec::new(), Vec::new());
    one_pass(
        true,
        &mut sa_ms,
        &mut fa_ms,
        &mut compared_positions,
        &mut compared_logits,
        &mut max_abs_diff,
        &mut first_mismatch,
        &mut reference,
        &mut order_log,
    )?;
    let census = canonical_simd::projection_census();

    // Pass B: counting OFF. These are the headline timings; the difference
    // between the two passes IS the measured cost of the diagnostics.
    let (mut scalar_ms, mut fast_ms) = (Vec::new(), Vec::new());
    one_pass(
        false,
        &mut scalar_ms,
        &mut fast_ms,
        &mut compared_positions,
        &mut compared_logits,
        &mut max_abs_diff,
        &mut first_mismatch,
        &mut reference,
        &mut order_log,
    )?;

    let (sa_min, sa_med, _) = stats(&sa_ms);
    let (fa_min, fa_med, _) = stats(&fa_ms);

    let (s_min, s_med, s_mean) = stats(&scalar_ms);
    let (f_min, f_med, f_mean) = stats(&fast_ms);
    let exact = max_abs_diff == 0;

    println!(
        "{}",
        serde_json::json!({
            "type": "canonical_kernel_conformance",
            "profile": profile,
            "gguf": args[1],
            "positions_per_run": tokens.len(),
            "repeats_per_pass": repeats,
            "passes": 2,
            "compared_positions_formula": "positions_per_run * repeats_per_pass * passes",
            "token_ids": tokens,
            "exact_all_logits": exact,
            "max_abs_logit_diff": max_abs_diff,
            "first_mismatch": first_mismatch.map(|(p,i,x,y)| serde_json::json!({
                "position": p, "index": i, "scalar": x, "vectorised": y })),
            "compared_positions": compared_positions,
            "compared_logits": compared_logits,
            "scalar_forward_ms": {"min": s_min, "median": s_med, "mean": s_mean},
            "vectorised_forward_ms": {"min": f_min, "median": f_med, "mean": f_mean},
            "forward_speedup_median": s_med / f_med,
            "forward_speedup_min": s_min / f_min,
            "kernel_order_per_round": order_log,
            "projection_census_during_conformance": {
                "attempted": census.attempted,
                "accepted": census.accepted,
                "refused_total": census.refused_total(),
                "refused_unavailable": census.refused_unavailable,
                "refused_shape": census.refused_shape,
                "refused_inner_dim_above_i32_bound": census.refused_inner_dim_above_i32_bound,
                "refused_activation_out_of_domain": census.refused_activation_out_of_domain,
                "refused_scale_multiply_would_overflow": census.refused_scale_multiply_would_overflow,
                "note": "counted only while the vectorised kernel was enabled, over pass A"
            },
            "census_overhead": {
                "scalar_median_ms_census_on": sa_med,
                "scalar_min_ms_census_on": sa_min,
                "vectorised_median_ms_census_on": fa_med,
                "vectorised_min_ms_census_on": fa_min,
                "vectorised_median_delta_ms": fa_med - f_med,
                "note": "pass A counted, pass B did not; headline timings are pass B"
            },
            "scalar_forward_ms_all": scalar_ms,
            "vectorised_forward_ms_all": fast_ms,
            "model_load_ms": load_ms,
            "max_rss_bytes": max_rss_bytes(),
            "dotprod": true,
            "note": "single process, one resident model, kernel toggled between runs; \
                     token sequence fixed in advance so it cannot drift between kernels; \
                     kernel order alternates between rounds"
        })
    );
    eprintln!(
        "exact={exact} max_abs_logit_diff={max_abs_diff} logits_compared={compared_logits} \
         scalar_median={s_med:.1}ms vectorised_median={f_med:.1}ms speedup={:.3}x",
        s_med / f_med
    );
    if !exact {
        return Err("LOGIT MISMATCH - the vectorised kernel is not conformant".into());
    }
    Ok(())
}
