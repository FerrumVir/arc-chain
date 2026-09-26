//! M8: latency of the serving path the native executor actually calls
//! (`CachedIntegerModel::try_generate_v2`) on the canonical profile.
//!
//! Measured, with repeated samples and distributions, never a single run:
//!   * cold load: artifact to a resident canonical model;
//!   * the first request after load, reported on its own and kept out of
//!     every distribution (page faults and cold caches belong to it);
//!   * time to first token: one-token generation (BOS + prompt prefill + one
//!     selection), per prompt length;
//!   * decode per token: (T(n tokens) - T(1 token)) / (n - 1);
//!   * full request latency for n tokens, and the sequential throughput it
//!     implies: the native worker executes one request at a time, so a
//!     node's serving throughput is n / T(n) and concurrent requests wait
//!     in its queue;
//!   * peak resident memory, swap in use (before load, after load, at the
//!     end) and the load average at the start and end, so a run that swapped
//!     or shared the host says so.
//!
//! Each configuration runs with batched prefill OFF (the serving default) and
//! ON (labelled). Every repeat must produce the same tokens, or the run
//! aborts: a faster wrong answer is not a measurement.
//!
//! This is the model half of the serving path. The protocol half (admission,
//! queueing, votes, certificate, settlement, retries) comes from a soak run's
//! workload records (`scripts/benchmarks/native_request_latency.py`): with
//! the deterministic executor it measures protocol overhead only, and the
//! real executor stays gated on reference qualification (M4).
//!
//!   cargo run -p arc-inference --example serving_latency --features candle --release -- \
//!     --model /path/model.gguf [--repeats 5] [--tokens 32] [--json-out out.json]

use arc_inference::cached_integer_model::{
    GGUF_INTERLEAVED_ROPE_I8_INFERENCE_PROFILE, load_cached_model_canonical_i8_interleaved_rope,
};
use arc_inference::canonical_prefill;
use arc_inference::model_artifact::ModelArtifactCommitment;
use serde_json::json;
use std::time::Instant;

fn arg(args: &[String], name: &str) -> Option<String> {
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1).cloned())
}

fn max_rss_bytes() -> u64 {
    // SAFETY: getrusage with a zeroed, correctly sized out-parameter.
    unsafe {
        let mut usage: libc::rusage = std::mem::zeroed();
        if libc::getrusage(libc::RUSAGE_SELF, &mut usage) != 0 {
            return 0;
        }
        // ru_maxrss is bytes on macOS and kilobytes on Linux.
        if cfg!(target_os = "macos") {
            usage.ru_maxrss as u64
        } else {
            usage.ru_maxrss as u64 * 1024
        }
    }
}

/// Swap in use, in bytes, or None when the platform does not say.
fn swap_used_bytes() -> Option<u64> {
    if cfg!(target_os = "macos") {
        // `sysctl -n vm.swapusage`: "total = 2048.00M  used = 1034.25M  free = …"
        let out = std::process::Command::new("sysctl")
            .args(["-n", "vm.swapusage"])
            .output()
            .ok()?;
        let text = String::from_utf8(out.stdout).ok()?;
        let used = text.split("used = ").nth(1)?.split_whitespace().next()?;
        let (number, unit) = used.split_at(used.len().checked_sub(1)?);
        let scale: u64 = match unit {
            "K" => 1 << 10,
            "M" => 1 << 20,
            "G" => 1 << 30,
            _ => return None,
        };
        let value: f64 = number.parse().ok()?;
        Some((value * scale as f64) as u64)
    } else {
        let text = std::fs::read_to_string("/proc/meminfo").ok()?;
        let kib = |name: &str| -> Option<u64> {
            text.lines()
                .find(|line| line.starts_with(name))?
                .split_whitespace()
                .nth(1)?
                .parse()
                .ok()
        };
        Some(kib("SwapTotal:")?.saturating_sub(kib("SwapFree:")?) * 1024)
    }
}

/// The 1, 5 and 15 minute load averages.
fn load_average() -> Option<[f64; 3]> {
    let mut loads = [0f64; 3];
    // SAFETY: getloadavg writes at most `nelem` (3) doubles into the buffer.
    let written = unsafe { libc::getloadavg(loads.as_mut_ptr(), 3) };
    (written == 3).then_some(loads)
}

/// Memory the OS can hand out now, without evicting anything that is in use:
/// free + inactive + speculative pages on macOS, `MemAvailable` on Linux.
fn available_memory_bytes() -> Option<u64> {
    if cfg!(target_os = "macos") {
        let out = std::process::Command::new("vm_stat").output().ok()?;
        let text = String::from_utf8(out.stdout).ok()?;
        let page: u64 = text
            .lines()
            .next()?
            .split("page size of ")
            .nth(1)?
            .split_whitespace()
            .next()?
            .parse()
            .ok()?;
        let pages = |name: &str| -> u64 {
            text.lines()
                .find(|line| line.starts_with(name))
                .and_then(|line| line.split(':').nth(1))
                .map(|value| value.trim().trim_end_matches('.'))
                .and_then(|value| value.parse::<u64>().ok())
                .unwrap_or(0)
        };
        Some((pages("Pages free") + pages("Pages inactive") + pages("Pages speculative")) * page)
    } else {
        let text = std::fs::read_to_string("/proc/meminfo").ok()?;
        let line = text
            .lines()
            .find(|line| line.starts_with("MemAvailable:"))?;
        Some(line.split_whitespace().nth(1)?.parse::<u64>().ok()? * 1024)
    }
}

/// A resident canonical model measured 6.7 GB for a 3.8 GB artifact (1.76x),
/// so twice the artifact leaves room for that plus the KV cache and the
/// process itself. Below this the run pages, and paging is what the previous
/// attempt measured instead of the model.
const REQUIRED_MEMORY_PER_ARTIFACT_BYTE: u64 = 2;

/// The contaminated run started at a 1-minute load average of 7.08 on ten
/// threads. Half the core count is the line between "this host is mine" and
/// "something else is already using it".
const MAX_START_LOAD_PER_CORE: f64 = 0.5;

/// Swap that grows during the run means the model is being paged, which is
/// exactly what invalidated the previous attempt (+5.24 GB). A clean run grows
/// none; this allows a little for unrelated system activity.
const MAX_SWAP_GROWTH_BYTES: u64 = 512 << 20;

/// Why the host is not fit to measure on, or `None` if it is.
fn contended(
    artifact_bytes: u64,
    cores: usize,
    swap_before: Option<u64>,
    started: bool,
) -> Option<String> {
    if let (Some(load), true) = (load_average(), !started) {
        let limit = cores as f64 * MAX_START_LOAD_PER_CORE;
        if load[0] > limit {
            return Some(format!(
                "start load average {:.2} exceeds {limit:.2} ({cores} cores): the host is \
                 already busy, so these timings would measure contention",
                load[0]
            ));
        }
    }
    if !started {
        let required = artifact_bytes.saturating_mul(REQUIRED_MEMORY_PER_ARTIFACT_BYTE);
        match available_memory_bytes() {
            Some(available) if available < required => {
                return Some(format!(
                    "{:.1} GB available, {:.1} GB required (2x the {:.1} GB artifact): the model \
                     would page instead of resident",
                    available as f64 / (1 << 30) as f64,
                    required as f64 / (1 << 30) as f64,
                    artifact_bytes as f64 / (1 << 30) as f64,
                ));
            }
            _ => {}
        }
    }
    if let (Some(before), Some(now)) = (swap_before, swap_used_bytes())
        && now.saturating_sub(before) > MAX_SWAP_GROWTH_BYTES
    {
        return Some(format!(
            "swap grew {:.2} GB during the run (limit {:.2} GB): the model is being paged, so \
             every timing after this point is paging, not compute",
            now.saturating_sub(before) as f64 / (1 << 30) as f64,
            MAX_SWAP_GROWTH_BYTES as f64 / (1 << 30) as f64,
        ));
    }
    None
}

fn summary(mut samples: Vec<f64>) -> serde_json::Value {
    samples.sort_by(|a, b| a.total_cmp(b));
    let n = samples.len();
    json!({
        "n": n,
        "min": samples.first(),
        "median": samples.get(n / 2),
        "max": samples.last(),
        "samples": samples,
    })
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let model_path = arg(&args, "--model").expect("--model PATH");
    let repeats: usize = arg(&args, "--repeats").map_or(5, |v| v.parse().expect("--repeats"));
    let tokens: u32 = arg(&args, "--tokens").map_or(32, |v| v.parse().expect("--tokens"));
    assert!(repeats >= 3, "distributions need at least 3 samples");
    assert!(tokens >= 2, "decode needs at least two generated tokens");

    // A contended host produces numbers that look like measurements and are
    // not. The previous attempt ran ninety minutes and had to be thrown away:
    // swap grew 5.24 GB and it started at a load average of 7.08 on ten
    // threads. Check before spending that time, and again as it is spent.
    let allow_contended = args.iter().any(|a| a == "--allow-contended");
    let cores = std::thread::available_parallelism().map_or(1, |n| n.get());
    let artifact_bytes = std::fs::metadata(&model_path).map(|m| m.len()).unwrap_or(0);
    if let Some(reason) = contended(artifact_bytes, cores, None, false) {
        if allow_contended {
            eprintln!(
                "serving_latency: PROCEEDING ON A CONTENDED HOST ({reason}). Its timings \
                       are not a measurement."
            );
        } else {
            eprintln!("serving_latency: refusing to start - {reason}.");
            eprintln!(
                "Free the host and try again, or pass --allow-contended to record a run that is \
                 explicitly not a measurement."
            );
            std::process::exit(2);
        }
    }

    let load_average_start = load_average();
    let swap_before_load = swap_used_bytes();
    let artifact = ModelArtifactCommitment::from_path(&model_path).expect("hash artifact");
    let load = Instant::now();
    let model = load_cached_model_canonical_i8_interleaved_rope(&model_path).expect("load");
    let cold_load_s = load.elapsed().as_secs_f64();
    assert_eq!(
        model.canonical_execution_profile(),
        Some(GGUF_INTERLEAVED_ROPE_I8_INFERENCE_PROFILE)
    );
    let rss_after_load = max_rss_bytes();
    let swap_after_load = swap_used_bytes();
    let eos = model.config.eos_tokens.clone();
    // Deterministic synthetic prompts of fixed lengths: ids 100.. avoid BOS/EOS.
    let prompt =
        |len: usize| -> Vec<u32> { (0..len as u32).map(|i| 100 + (i * 37) % 30_000).collect() };

    // The first request after load, on the serving default, kept out of the
    // distributions below.
    canonical_prefill::set_batched_prefill_enabled(false);
    let first_request = Instant::now();
    model.try_generate_v2(&prompt(16), 1, &eos).expect("fits");
    let first_request_after_load_s = first_request.elapsed().as_secs_f64();

    // Prompt lengths. The default is the full set; `--prompt-lens 16,128`
    // exists because the 512-token configuration adds about 1 GB of KV cache
    // on top of a ~6.7 GB resident model, which is what the OS killed this
    // benchmark for on a 16 GB host. A run that measures fewer lengths says so
    // in its own JSON (`prompt_lens`), so a shorter run can never be mistaken
    // for a full one.
    let prompt_lens: Vec<usize> = match arg(&args, "--prompt-lens") {
        Some(list) => {
            let lens: Vec<usize> = list
                .split(',')
                .map(|item| item.trim().parse().expect("--prompt-lens takes integers"))
                .collect();
            assert!(!lens.is_empty(), "--prompt-lens needs at least one length");
            assert!(
                lens.iter().all(|len| *len >= 2),
                "a prompt shorter than 2 tokens cannot measure prefill"
            );
            lens
        }
        None => vec![16, 128, 512],
    };

    let mut results = Vec::new();
    for batched in [false, true] {
        canonical_prefill::set_batched_prefill_enabled(batched);
        for len in prompt_lens.iter().copied() {
            let p = prompt(len);
            let (mut ttft, mut full, mut per_token, mut throughput) =
                (Vec::new(), Vec::new(), Vec::new(), Vec::new());
            let mut reference: Option<(Vec<u32>, Vec<u32>)> = None;
            for _ in 0..repeats {
                let start = Instant::now();
                let (first, _) = model.try_generate_v2(&p, 1, &eos).expect("fits");
                let t1 = start.elapsed().as_secs_f64();
                let start = Instant::now();
                // EOS disabled: every run must produce exactly `tokens` tokens,
                // so per-token decode is comparable across repeats.
                let (all, _) = model.try_generate_v2(&p, tokens, &[]).expect("fits");
                let tn = start.elapsed().as_secs_f64();
                assert_eq!(
                    all.len(),
                    tokens as usize,
                    "generation stopped early without EOS"
                );
                match &reference {
                    None => reference = Some((first.clone(), all.clone())),
                    Some((f, a)) => assert!(
                        f == &first && a == &all,
                        "repeats disagree: the serving path is not deterministic"
                    ),
                }
                ttft.push(t1);
                full.push(tn);
                per_token.push((tn - t1).max(0.0) / f64::from(tokens - 1));
                throughput.push(f64::from(tokens) / tn);
            }
            eprintln!("batched={batched} prompt={len}: done");
            // Re-check before paying for the next configuration rather than
            // discovering at the end that everything since was paging.
            if let Some(reason) = contended(artifact_bytes, cores, swap_before_load, true)
                && !allow_contended
            {
                eprintln!(
                    "serving_latency: ABORTING after batched={batched} prompt={len} - {reason}."
                );
                eprintln!("Partial results are discarded: a contended sample is not a sample.");
                std::process::exit(3);
            }
            results.push(json!({
                "batched_prefill": batched,
                "prompt_tokens": len,
                "generated_tokens": tokens,
                "time_to_first_token_s": summary(ttft),
                "full_request_s": summary(full),
                "decode_s_per_token": summary(per_token),
                "sequential_tokens_per_s": summary(throughput),
            }));
        }
    }
    canonical_prefill::set_batched_prefill_enabled(false);
    let report = json!({
        "schema": "arc.m8.serving-latency.v1",
        "artifact_blake3": artifact.model_id().to_hex(),
        "profile": GGUF_INTERLEAVED_ROPE_I8_INFERENCE_PROFILE,
        "threads": rayon::current_num_threads(),
        "repeats": repeats,
        "prompt_lens": prompt_lens,
        "isolation": {
            "checked": true,
            "allow_contended": allow_contended,
            "cores": cores,
            "required_memory_bytes": artifact_bytes * REQUIRED_MEMORY_PER_ARTIFACT_BYTE,
            "max_start_load": cores as f64 * MAX_START_LOAD_PER_CORE,
            "max_swap_growth_bytes": MAX_SWAP_GROWTH_BYTES,
        },
        "cold_load_s": cold_load_s,
        "first_request_after_load_s": first_request_after_load_s,
        "max_rss_bytes_after_load": rss_after_load,
        "max_rss_bytes_end": max_rss_bytes(),
        "swap_used_bytes": {
            "before_load": swap_before_load,
            "after_load": swap_after_load,
            "end": swap_used_bytes(),
        },
        "load_average_1_5_15": { "start": load_average_start, "end": load_average() },
        "results": results,
        "note": "model serving path only; one host; no other load allowed during the run. \
                 Swap that grew or a start load average near the core count means the run \
                 was not isolated.",
    });
    let text = serde_json::to_string_pretty(&report).expect("json");
    if let Some(path) = arg(&args, "--json-out") {
        std::fs::write(path, &text).expect("--json-out");
    }
    println!("{text}");
}
