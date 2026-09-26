//! E1(b): batched multi-token prefill, measured against token-at-a-time on the
//! real canonical-I8 artifact.
//!
//! Four configurations are measured so batching and vectorisation can be told
//! apart, which is the whole point of giving them separate switches:
//!
//!   scalar / token-at-a-time    (the original path)
//!   scalar / batched
//!   SIMD   / token-at-a-time    (**the primary baseline**)
//!   SIMD   / batched            (the candidate)
//!
//! Exactness is checked before any timing is reported: every logit at every
//! position of the target prompt, plus a continuation decode afterwards so the
//! persisted KV state is validated and not just the prefill's internal
//! consistency. Whether the optimised paths were actually used is counted, so a
//! silent fallback can never be reported as a speedup.
//!
//! usage: batched_prefill_experiment GGUF [prompt_len] [chunk] [repeats]

use arc_inference::cached_integer_model::{CachedIntegerModel, KVCache};
use arc_inference::{canonical_prefill, canonical_simd};
use std::time::Instant;

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn max_rss_bytes() -> Option<u64> {
    // SAFETY: getrusage with a zeroed, correctly sized out-parameter.
    unsafe {
        let mut u: libc::rusage = std::mem::zeroed();
        if libc::getrusage(libc::RUSAGE_SELF, &mut u) == 0 {
            #[cfg(target_os = "macos")]
            let bytes = u.ru_maxrss as u64;
            #[cfg(target_os = "linux")]
            let bytes = (u.ru_maxrss as u64).saturating_mul(1024);
            Some(bytes)
        } else {
            None
        }
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn max_rss_bytes() -> Option<u64> {
    None
}

fn fresh(model: &CachedIntegerModel) -> KVCache {
    KVCache::new(model.config.n_layers)
}

/// Token-at-a-time prefill. Returns logits for every position, which is what
/// `forward_one_token` produces whether or not the caller wants them.
fn taat(model: &CachedIntegerModel, tokens: &[u32], cache: &mut KVCache) -> Vec<Vec<i64>> {
    tokens
        .iter()
        .map(|t| model.forward_one_token(*t, cache))
        .collect()
}

fn median(v: &[f64]) -> f64 {
    let mut s = v.to_vec();
    s.sort_by(|a, b| a.partial_cmp(b).unwrap());
    s[s.len() / 2]
}

fn set_mode(simd: bool, batched: bool) {
    canonical_simd::set_fast_canonical_kernel(simd);
    canonical_prefill::set_batched_prefill_enabled(batched);
}

fn main() -> Result<(), String> {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 2 {
        return Err("usage: batched_prefill_experiment GGUF [prompt_len] [chunk] [repeats]".into());
    }
    let prompt_len: usize = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(500);
    let chunk: usize = args.get(3).and_then(|s| s.parse().ok()).unwrap_or(64);
    let repeats: usize = args.get(4).and_then(|s| s.parse().ok()).unwrap_or(2);

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
    let cfg = &model.config;
    eprintln!(
        "profile={profile} layers={} d={} vocab={} max_seq={} load_ms={load_ms:.0} dotprod={}",
        cfg.n_layers,
        cfg.d_model,
        cfg.vocab_size,
        cfg.max_seq,
        canonical_simd::dotprod_available()
    );
    if !canonical_simd::dotprod_available() {
        return Err("no ARM dotprod on this host; the primary baseline is unavailable".into());
    }
    if prompt_len == 0 || prompt_len + 8 > cfg.max_seq {
        return Err(format!(
            "prompt_len {prompt_len} (+8 decode) does not fit max_seq {}",
            cfg.max_seq
        ));
    }

    // Fixed real Llama-2 ids, independent of any kernel's output.
    let base: [u32; 16] = [
        1, 6324, 29892, 306, 29915, 29885, 263, 1243, 310, 278, 1904, 29889, 887, 508, 1065, 372,
    ];
    let prompt: Vec<u32> = (0..prompt_len).map(|i| base[i % base.len()]).collect();
    let decode_ids: [u32; 4] = [306, 29915, 29885, 263];

    // ── Phase A: exactness at every position, plus continuation decode ──────
    //
    // Memory-lean by construction. The naive version held two full KV caches
    // (1.05 GB each at 500 positions, because the cache is i64) plus two full
    // logit sets (128 MB each) and was OOM-killed on this 16 GB host. Instead:
    // each pass keeps ONE cache, digests each position's logits as it goes, and
    // the batched pass is driven in slices so its logits never accumulate. A
    // BLAKE3 digest over all 32,000 i64 values of a position is a full
    // comparison of that position; the final slice is additionally compared
    // element by element so an exact-value claim is also directly evidenced.
    fn digest(v: &[i64]) -> [u8; 32] {
        let mut h = blake3::Hasher::new();
        for x in v {
            h.update(&x.to_le_bytes());
        }
        *h.finalize().as_bytes()
    }
    fn cache_digest(c: &KVCache) -> [u8; 32] {
        let mut h = blake3::Hasher::new();
        h.update(&(c.seq_len as u64).to_le_bytes());
        for layer in c.k_data.iter().chain(c.v_data.iter()) {
            h.update(&(layer.len() as u64).to_le_bytes());
            for x in layer {
                h.update(&x.to_le_bytes());
            }
        }
        *h.finalize().as_bytes()
    }

    set_mode(true, false);
    let mut ref_digests: Vec<[u8; 32]> = Vec::with_capacity(prompt_len);
    let mut ref_tail: Vec<Vec<i64>> = Vec::new();
    let tail_from = prompt_len.saturating_sub(chunk.min(prompt_len));
    let mut ref_cache = fresh(&model);
    let a0 = Instant::now();
    for (p, t) in prompt.iter().enumerate() {
        let l = model.forward_one_token(*t, &mut ref_cache);
        ref_digests.push(digest(&l));
        if p >= tail_from {
            ref_tail.push(l);
        }
    }
    let taat_simd_ms = a0.elapsed().as_secs_f64() * 1e3;
    let ref_prefill_cache = cache_digest(&ref_cache);
    let mut ref_cont: Vec<[u8; 32]> = Vec::new();
    for t in decode_ids.iter() {
        ref_cont.push(digest(&model.forward_one_token(*t, &mut ref_cache)));
    }
    let ref_final_cache = cache_digest(&ref_cache);
    drop(ref_cache);

    canonical_prefill::reset_prefill_census();
    canonical_simd::set_projection_census_enabled(true);
    set_mode(true, true);
    let mut bat_cache = fresh(&model);
    let mut compared = 0usize;
    let mut max_abs = 0i64;
    let mut first_bad: Option<(usize, usize, i64, i64)> = None;
    let mut digest_mismatch: Option<usize> = None;
    let mut pos = 0usize;
    let a1 = Instant::now();
    while pos < prompt_len {
        let take = chunk.min(prompt_len - pos);
        let slice = &prompt[pos..pos + take];
        let got = model
            .prefill_canonical_i8_batched(slice, &mut bat_cache, chunk, true)
            .ok_or("batched prefill refused a slice of the target prompt")?;
        if got.len() != take {
            return Err(format!("slice at {pos} returned {} positions", got.len()));
        }
        for (i, l) in got.iter().enumerate() {
            let p = pos + i;
            if digest(l) != ref_digests[p] && digest_mismatch.is_none() {
                digest_mismatch = Some(p);
            }
            compared += l.len();
            if p >= tail_from {
                let r = &ref_tail[p - tail_from];
                for (idx, (x, y)) in r.iter().zip(l.iter()).enumerate() {
                    let d = (x - y).abs();
                    if d > max_abs {
                        max_abs = d;
                    }
                    if x != y && first_bad.is_none() {
                        first_bad = Some((p, idx, *x, *y));
                    }
                }
            }
        }
        pos += take;
    }
    let batched_all_ms = a1.elapsed().as_secs_f64() * 1e3;
    canonical_simd::set_projection_census_enabled(false);
    let pc = canonical_prefill::prefill_census();
    let jc = canonical_simd::projection_census();
    let cache_same = cache_digest(&bat_cache) == ref_prefill_cache;

    set_mode(true, false);
    let mut cont_ok = true;
    let mut cont_steps = 0usize;
    for (n, t) in decode_ids.iter().enumerate() {
        let d = digest(&model.forward_one_token(*t, &mut bat_cache));
        cont_steps = n + 1;
        if d != ref_cont[n] {
            cont_ok = false;
            break;
        }
    }
    let cont_cache_same = cache_digest(&bat_cache) == ref_final_cache;
    drop(bat_cache);

    let exact =
        digest_mismatch.is_none() && max_abs == 0 && cache_same && cont_ok && cont_cache_same;
    eprintln!(
        "exactness: digests_all_positions={} tail_elementwise_max_diff={} kv={} continuation={} ({cont_steps} steps)",
        digest_mismatch.is_none(),
        max_abs,
        cache_same,
        cont_ok
    );
    if !exact {
        eprintln!(
            "first digest mismatch at position {digest_mismatch:?}; first value mismatch {first_bad:?}"
        );
        return Err("EXACTNESS FAILURE - batched prefill is not conformant".into());
    }

    // ── Phase B: timing, production shape ───────────────────────────────────
    //
    // Measurement environment caveat, recorded because it dominates the
    // numbers: this host runs the 6.9 GB resident model on 16 GB of RAM while
    // already deep in swap, so wall-clock samples are heavily contended. The
    // protocol is therefore built for that: the two SIMD configurations (the
    // primary comparison Codex specified) are interleaved A/B within every
    // repeat so contention hits both alike, and `min` is reported alongside the
    // median as the least-contended estimator. The two scalar configurations
    // are sampled once each and reported separately, as secondary context.
    let mut obs: Vec<(&str, Vec<f64>)> = vec![
        ("scalar_token_at_a_time", vec![]),
        ("scalar_batched", vec![]),
        ("simd_token_at_a_time", vec![]),
        ("simd_batched", vec![]),
    ];
    let mut order_log: Vec<&'static str> = Vec::new();

    let sample = |i: usize, obs: &mut Vec<(&str, Vec<f64>)>| -> Result<(), String> {
        let (simd, batched) = match i {
            0 => (false, false),
            1 => (false, true),
            2 => (true, false),
            _ => (true, true),
        };
        set_mode(simd, batched);
        let mut c = fresh(&model);
        let t = Instant::now();
        if batched {
            model
                .prefill_canonical_i8_batched(&prompt, &mut c, chunk, false)
                .ok_or("batched prefill refused during timing")?;
        } else {
            let _ = taat(&model, &prompt, &mut c);
        }
        let ms = t.elapsed().as_secs_f64() * 1e3;
        obs[i].1.push(ms);
        eprintln!("  sample {}: {:.0} ms", obs[i].0, ms);
        Ok(())
    };

    // One discarded warm cycle per SIMD configuration.
    sample(2, &mut obs)?;
    sample(3, &mut obs)?;
    obs[2].1.clear();
    obs[3].1.clear();

    for r in 0..repeats {
        let taat_first = r % 2 == 0;
        order_log.push(if taat_first {
            "taat-first"
        } else {
            "batched-first"
        });
        if taat_first {
            sample(2, &mut obs)?;
            sample(3, &mut obs)?;
        } else {
            sample(3, &mut obs)?;
            sample(2, &mut obs)?;
        }
    }
    // Secondary: the original scalar path, one sample each.
    sample(0, &mut obs)?;
    sample(1, &mut obs)?;
    set_mode(false, false);

    let med: Vec<f64> = obs.iter().map(|(_, v)| median(v)).collect();
    let mins: Vec<f64> = obs
        .iter()
        .map(|(_, v)| v.iter().cloned().fold(f64::INFINITY, f64::min))
        .collect();
    let (s_taat, s_bat) = (med[0], med[1]);
    // Primary ratio from the least-contended sample of each SIMD config.
    let (x_taat, x_bat) = (mins[2], mins[3]);

    println!(
        "{}",
        serde_json::json!({
            "type": "batched_prefill_experiment",
            "profile": profile,
            "gguf": args[1],
            "prompt_len": prompt_len,
            "chunk": chunk,
            "repeats": repeats,
            "order_per_repeat": order_log,
            "exact": exact,
            "max_abs_logit_diff": max_abs,
            "logits_compared_by_digest": compared,
            "logits_compared_elementwise": ref_tail.iter().map(|v| v.len()).sum::<usize>(),
            "digest_mismatch_position": digest_mismatch,
            "kv_cache_identical": cache_same,
            "continuation_decode_identical": cont_ok,
            "continuation_steps": cont_steps,
            "continuation_cache_identical": cont_cache_same,
            "prefill_census": {
                "chunks": pc.chunks, "tokens": pc.tokens,
                "batched_projections": pc.batched_projections,
                "refused_total": pc.refused_total()
            },
            "projection_census_during_batched_pass": {
                "attempted": jc.attempted, "accepted": jc.accepted,
                "refused_total": jc.refused_total()
            },
            "primary_estimator": "min of the interleaved SIMD samples; see the environment caveat",
            "prefill_ms_min": {
                "scalar_token_at_a_time": mins[0],
                "scalar_batched": mins[1],
                "simd_token_at_a_time": mins[2],
                "simd_batched": mins[3]
            },
            "prefill_ms_median": {
                "scalar_token_at_a_time": s_taat,
                "scalar_batched": s_bat,
                "simd_token_at_a_time": x_taat,
                "simd_batched": x_bat
            },
            "prefill_ms_all": obs.iter().map(|(k,v)| (k.to_string(), v.clone()))
                                  .collect::<std::collections::BTreeMap<_,_>>(),
            "speedups": {
                "PRIMARY_simd_batched_over_simd_token_at_a_time": x_taat / x_bat,
                "batching_alone_scalar": s_taat / s_bat,
                "simd_alone_token_at_a_time": s_taat / x_taat,
                "combined_over_original_scalar_taat": s_taat / x_bat
            },
            "ms_per_prompt_token": {
                "simd_token_at_a_time": x_taat / prompt_len as f64,
                "simd_batched": x_bat / prompt_len as f64
            },
            "verification_shape_ms": {
                "simd_token_at_a_time_all_logits": taat_simd_ms,
                "simd_batched_all_logits": batched_all_ms,
                "note": "all-position logits; the timing block above uses the production shape (final position only)"
            },
            "model_load_ms": load_ms,
            "max_rss_bytes": max_rss_bytes(),
        })
    );
    eprintln!(
        "PRIMARY simd batched vs simd token-at-a-time: {:.3}x  ({:.0} ms -> {:.0} ms for {prompt_len} tokens)",
        x_taat / x_bat,
        x_taat,
        x_bat
    );
    eprintln!(
        "batching alone (scalar): {:.3}x | simd alone: {:.3}x | combined vs original: {:.3}x",
        s_taat / s_bat,
        s_taat / x_taat,
        s_taat / x_bat
    );
    Ok(())
}
