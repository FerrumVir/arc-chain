//! Canonical INT8 worker benchmark on a 7B-shaped synthetic model.
//!
//! CI evidence for Stage 0 of the kernel plan: positions per second of the
//! forward pass a community worker runs, with the process's default projection
//! kernel and with the other kernel, in one process on the same machine. The
//! model has Llama-2-7B shapes (32 layers, d_model 4096, 32 heads, d_ff 11008,
//! a 32,000-row LM head) in the canonical per-row INT8 profile and the legacy
//! split-half layout workers run. Its weights come from a fixed SplitMix64
//! stream, so nothing is downloaded and every run computes the same integers.
//! Only the first 512 embedding rows are materialised; an embedding is one row
//! lookup per position, so this changes no per-position work.
//!
//! Each leg decodes the same tokens with `forward_one_token`, the call a worker
//! makes once per position, starting from an empty KV cache. The run fails if
//! the legs' logits or KV caches differ, or if a vectorised leg refused any
//! projection, which would otherwise report a scalar fallback as a vectorised
//! timing.
//!
//! Timings are wall clock on whatever machine runs this. On GitHub-hosted
//! runners they are CI-runner measurements, not product benchmarks.
//!
//! ```text
//! cargo run --release --locked -p arc-inference --example canonical_worker_bench -- \
//!     [--layers 32] [--positions 8] [--warmup 1] [--threads N] [--summary FILE]
//! ```
//!
//! `--summary` appends a Markdown table to an existing file, such as
//! `$GITHUB_STEP_SUMMARY`.

use arc_inference::cached_integer_model::{
    ArithmeticProfile, CachedIntegerModel, CachedLayer, I8Weights, KVCache, ModelConfig,
};
use arc_inference::canonical_simd::{self, ProjectionCensus};
use arc_inference::integer_lut::ONE;
use rayon::prelude::*;
use std::io::Write as _;
use std::time::Instant;

const D_MODEL: usize = 4096;
const N_HEADS: usize = 32;
const D_FF: usize = 11_008;
const VOCAB: usize = 32_000;
/// Embedding rows materialised. Every id in `TOKENS` is below this.
const EMBEDDED_TOKENS: usize = 512;
const MAX_SEQ: usize = 64;
/// Decoded in order, one forward per position, starting with Llama's BOS id.
const TOKENS: [u32; 16] = [
    1, 13, 29, 37, 101, 211, 307, 409, 503, 3, 7, 11, 17, 19, 23, 31,
];
/// Q16 samples of the unit circle at multiples of pi/8, as in the golden
/// fixture, so the RoPE tables need no platform math library.
const COS: [i64; 16] = [
    65_536, 60_547, 46_341, 25_080, 0, -25_080, -46_341, -60_547, -65_536, -60_547, -46_341,
    -25_080, 0, 25_080, 46_341, 60_547,
];
const SIN: [i64; 16] = [
    0, 25_080, 46_341, 60_547, 65_536, 60_547, 46_341, 25_080, 0, -25_080, -46_341, -60_547,
    -65_536, -60_547, -46_341, -25_080,
];
/// The kernel plan's Stage 0 target for a 4-vCPU runner (baseline 0.77).
const TARGET_POSITIONS_PER_SECOND: f64 = 1.4;
/// Bytes filled from one independently seeded stream, so weight generation
/// runs in parallel and still does not depend on the thread count.
const FILL_CHUNK: usize = 1 << 20;

struct Args {
    layers: usize,
    positions: usize,
    warmup: usize,
    threads: usize,
    summary: Option<String>,
}

fn number(flag: &str, value: &str) -> Result<usize, String> {
    value
        .parse()
        .map_err(|_| format!("{flag} expects a whole number, got {value:?}"))
}

fn parse_args() -> Result<Args, String> {
    let raw: Vec<String> = std::env::args().skip(1).collect();
    let mut args = Args {
        layers: 32,
        positions: 8,
        warmup: 1,
        threads: std::thread::available_parallelism().map_or(1, |n| n.get()),
        summary: None,
    };
    let mut index = 0;
    while index < raw.len() {
        let flag = raw[index].as_str();
        let value = raw
            .get(index + 1)
            .ok_or_else(|| format!("{flag} needs a value"))?;
        match flag {
            "--layers" => args.layers = number(flag, value)?,
            "--positions" => args.positions = number(flag, value)?,
            "--warmup" => args.warmup = number(flag, value)?,
            "--threads" => args.threads = number(flag, value)?,
            "--summary" => args.summary = Some(value.clone()),
            other => return Err(format!("unknown argument {other}; see the file header")),
        }
        index += 2;
    }
    if args.layers == 0 || args.positions == 0 || args.threads == 0 {
        return Err("--layers, --positions and --threads must be at least 1".into());
    }
    if args.warmup + args.positions > TOKENS.len() {
        return Err(format!(
            "--warmup plus --positions must be at most {}",
            TOKENS.len()
        ));
    }
    Ok(args)
}

/// SplitMix64: a fixed, platform-independent stream for the synthetic weights.
fn splitmix64(state: &mut u64) -> u64 {
    *state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut z = *state;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// A row-major INT8 matrix with weights in [-127, 127], the range per-row
/// quantization produces, and per-row Q16 scales in [12, 35]. These scales
/// keep activations at the magnitudes the real 7B model shows, so the limb
/// kernel works on three digit planes, as it does there.
fn matrix(rows: usize, cols: usize, seed: u64) -> I8Weights {
    let mut data = vec![0i8; rows * cols];
    data.par_chunks_mut(FILL_CHUNK)
        .enumerate()
        .for_each(|(chunk, weights)| {
            let mut state = seed ^ (chunk as u64).wrapping_mul(0xD1B5_4A32_D192_ED03);
            for group in weights.chunks_mut(8) {
                let draw = splitmix64(&mut state).to_le_bytes();
                for (weight, byte) in group.iter_mut().zip(draw) {
                    *weight = i8::from_le_bytes([byte]).max(-127);
                }
            }
        });
    let mut state = seed ^ 0x5CA1_E5CA_1E5C_A1E5;
    let scales = (0..rows)
        .map(|_| 12 + (splitmix64(&mut state) % 24) as i64)
        .collect();
    I8Weights {
        data,
        scales,
        n_rows: rows,
        n_cols: cols,
    }
}

/// RMSNorm gains near 1.0 in Q16, as in the golden fixture.
fn norm(len: usize, seed: u64) -> Vec<i64> {
    let mut state = seed;
    (0..len)
        .map(|_| ONE - 4096 + (splitmix64(&mut state) % 8193) as i64)
        .collect()
}

fn build_model(n_layers: usize) -> CachedIntegerModel {
    let d_head = D_MODEL / N_HEADS;
    let half = d_head / 2;
    let mut rope_cos = Vec::with_capacity(MAX_SEQ * half);
    let mut rope_sin = Vec::with_capacity(MAX_SEQ * half);
    for position in 0..MAX_SEQ {
        for pair in 0..half {
            let angle = (position * (pair + 1)) % COS.len();
            rope_cos.push(COS[angle]);
            rope_sin.push(SIN[angle]);
        }
    }
    let layers = (0..n_layers)
        .map(|layer| {
            let seed = 0xA2C0_0000_0000_0000_u64 ^ ((layer as u64) << 8);
            CachedLayer {
                wq: matrix(D_MODEL, D_MODEL, seed + 1),
                wk: matrix(D_MODEL, D_MODEL, seed + 2),
                wv: matrix(D_MODEL, D_MODEL, seed + 3),
                wo: matrix(D_MODEL, D_MODEL, seed + 4),
                w_gate: matrix(D_FF, D_MODEL, seed + 5),
                w_up: matrix(D_FF, D_MODEL, seed + 6),
                w_down: matrix(D_MODEL, D_FF, seed + 7),
                attn_norm: norm(D_MODEL, seed + 8),
                ffn_norm: norm(D_MODEL, seed + 9),
            }
        })
        .collect();
    let embedding_i8 = matrix(EMBEDDED_TOKENS, D_MODEL, 0xE3B0);
    let embedding_q16 = embedding_i8
        .data
        .chunks(D_MODEL)
        .zip(&embedding_i8.scales)
        .flat_map(|(row, &scale)| row.iter().map(move |&weight| i64::from(weight) * scale))
        .collect();
    CachedIntegerModel {
        config: ModelConfig {
            n_layers,
            d_model: D_MODEL,
            n_heads: N_HEADS,
            n_kv_heads: N_HEADS,
            d_ff: D_FF,
            d_head,
            d_kv: D_MODEL,
            vocab_size: VOCAB,
            // round(2^16 / sqrt(128))
            attn_scale: 5_793,
            rope_cos,
            rope_sin,
            max_seq: MAX_SEQ,
            eos_tokens: vec![2],
            bos_token: 1,
            chat_template: String::new(),
            arithmetic_profile: ArithmeticProfile::LegacySplitHalfV0,
        },
        embedding_q16,
        embedding_i8,
        layers,
        final_norm: norm(D_MODEL, 0xF1A1),
        output_weight: matrix(VOCAB, D_MODEL, 0x0B7E),
        vocab: Vec::new(),
        q4_layers: None,
        q4_output: None,
        i16_layers: None,
        i16_output: None,
        block_i8_layers: None,
        block_i8_output: None,
        ternary_layers: None,
        ternary_output: None,
        ternary_hybrid_layers: None,
        ternary_hybrid_output: None,
    }
}

fn weight_bytes(model: &CachedIntegerModel) -> usize {
    let layers: usize = model
        .layers
        .iter()
        .map(|layer| {
            [
                &layer.wq,
                &layer.wk,
                &layer.wv,
                &layer.wo,
                &layer.w_gate,
                &layer.w_up,
                &layer.w_down,
            ]
            .iter()
            .map(|weights| weights.data.len())
            .sum::<usize>()
        })
        .sum();
    layers + model.output_weight.data.len()
}

/// The projection kernel I8 matmuls use right now, named as the determinism
/// proof (PR #136) names them.
fn kernel_name() -> &'static str {
    if !canonical_simd::fast_canonical_kernel_enabled() {
        "scalar-i8xi64"
    } else if cfg!(target_arch = "x86_64") {
        "avx2-limb"
    } else {
        "neon-sdot-limb"
    }
}

fn hash_values(digest: &mut blake3::Hasher, values: &[i64]) {
    let bytes: Vec<u8> = values
        .iter()
        .flat_map(|value| value.to_le_bytes())
        .collect();
    digest.update(&bytes);
}

struct Leg {
    label: &'static str,
    kernel: &'static str,
    ms: Vec<f64>,
    digest: String,
    census: ProjectionCensus,
}

impl Leg {
    fn mean_ms(&self) -> f64 {
        self.ms.iter().sum::<f64>() / self.ms.len() as f64
    }

    fn min_ms(&self) -> f64 {
        self.ms.iter().copied().fold(f64::INFINITY, f64::min)
    }

    fn max_ms(&self) -> f64 {
        self.ms.iter().copied().fold(0.0, f64::max)
    }

    fn positions_per_second(&self) -> f64 {
        1000.0 / self.mean_ms()
    }
}

/// Decode `TOKENS[..warmup + positions]` on a `threads`-wide pool with the
/// current process-wide kernel, timing every position after the warm-up.
fn run_leg(model: &CachedIntegerModel, label: &'static str, args: &Args) -> Result<Leg, String> {
    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(args.threads)
        .build()
        .map_err(|error| format!("building a {}-thread pool: {error}", args.threads))?;
    let kernel = kernel_name();
    canonical_simd::reset_projection_census();
    canonical_simd::set_projection_census_enabled(true);
    let run = pool.install(|| {
        let mut cache = KVCache::new(model.config.n_layers);
        let mut digest = blake3::Hasher::new();
        let mut ms = Vec::with_capacity(args.positions);
        for (position, &token) in TOKENS[..args.warmup + args.positions].iter().enumerate() {
            let started = Instant::now();
            let logits = model.forward_one_token(token, &mut cache);
            let elapsed = started.elapsed().as_secs_f64() * 1000.0;
            if logits.len() != VOCAB {
                return Err(format!(
                    "position {position} returned {} logits, expected {VOCAB}",
                    logits.len()
                ));
            }
            if position >= args.warmup {
                ms.push(elapsed);
            }
            hash_values(&mut digest, &logits);
        }
        for (keys, values) in cache.k_data.iter().zip(&cache.v_data) {
            hash_values(&mut digest, keys);
            hash_values(&mut digest, values);
        }
        Ok((ms, digest.finalize().to_hex().to_string()))
    });
    let census = canonical_simd::projection_census();
    canonical_simd::set_projection_census_enabled(false);
    let (ms, digest) = run?;
    Ok(Leg {
        label,
        kernel,
        ms,
        digest,
        census,
    })
}

fn main() -> Result<(), String> {
    let args = parse_args()?;
    // Read the process default before anything chooses a kernel, so the first
    // leg runs what a worker started without an override runs.
    let default_is_vectorised = canonical_simd::fast_canonical_kernel_enabled();
    let simd_available = canonical_simd::dotprod_available();
    println!(
        "host: os={} arch={} logical_cpus={} threads={} simd_available={simd_available} \
         default_kernel={}",
        std::env::consts::OS,
        std::env::consts::ARCH,
        std::thread::available_parallelism().map_or(0, |n| n.get()),
        args.threads,
        kernel_name()
    );

    let started = Instant::now();
    let model = build_model(args.layers);
    let build_ms = started.elapsed().as_secs_f64() * 1000.0;
    let gigabytes = weight_bytes(&model) as f64 / 1e9;
    println!(
        "model: {} layers, d_model {D_MODEL}, d_ff {D_FF}, vocab {VOCAB}; {gigabytes:.2} GB of \
         INT8 weights built in {build_ms:.0} ms",
        args.layers
    );

    let mut legs = vec![run_leg(&model, "default", &args)?];
    if default_is_vectorised {
        canonical_simd::set_fast_canonical_kernel(false);
        legs.push(run_leg(&model, "scalar (forced)", &args)?);
    } else if simd_available {
        canonical_simd::set_fast_canonical_kernel(true);
        legs.push(run_leg(&model, "vectorised (forced)", &args)?);
    }

    // Every I8 projection of a forward reaches the kernel switch: seven per
    // layer plus the LM head.
    let forwards = (args.warmup + args.positions) as u64;
    let expected_attempts = forwards * (7 * args.layers as u64 + 1);
    let mut failures = Vec::new();
    for leg in &legs {
        let c = leg.census;
        println!(
            "leg {}: kernel {}, {:.1} ms per position (min {:.1}, max {:.1}), {:.2} positions/s; \
             projections attempted {} accepted {} refused {}; digest {}",
            leg.label,
            leg.kernel,
            leg.mean_ms(),
            leg.min_ms(),
            leg.max_ms(),
            leg.positions_per_second(),
            c.attempted,
            c.accepted,
            c.refused_total(),
            leg.digest
        );
        if leg.digest != legs[0].digest {
            failures.push(format!(
                "leg {} computed different logits or KV cache than leg {}",
                leg.label, legs[0].label
            ));
        }
        let vectorised = leg.kernel != "scalar-i8xi64";
        if vectorised
            && (c.attempted != expected_attempts
                || c.accepted != c.attempted
                || c.refused_total() != 0)
        {
            failures.push(format!(
                "leg {} did not run every projection on the vectorised kernel \
                 (expected {expected_attempts} accepted): {c:?}",
                leg.label
            ));
        }
        if !vectorised && c.attempted != 0 {
            failures.push(format!(
                "leg {} is scalar but reached the vectorised kernel: {c:?}",
                leg.label
            ));
        }
    }

    let scalar = legs.iter().find(|leg| leg.kernel == "scalar-i8xi64");
    let vectorised = legs.iter().find(|leg| leg.kernel != "scalar-i8xi64");
    let speedup = match (scalar, vectorised) {
        (Some(scalar), Some(vectorised)) => Some(scalar.mean_ms() / vectorised.mean_ms()),
        _ => None,
    };
    if let Some(speedup) = speedup {
        println!("vectorised over scalar: {speedup:.2}x");
    }
    let default_rate = legs[0].positions_per_second();
    let target = if default_rate >= TARGET_POSITIONS_PER_SECOND {
        "met"
    } else {
        "not met"
    };
    println!(
        "default kernel: {default_rate:.2} positions/s; Stage 0 target for a 4-vCPU runner \
         (>= {TARGET_POSITIONS_PER_SECOND} positions/s): {target}"
    );

    if let Some(path) = &args.summary {
        let mut text = String::new();
        text.push_str(
            "### Canonical INT8 worker forward, 7B-shaped (CI-runner measurement)\n\n\
             | Leg | Kernel | Threads | ms per position (mean) | min | max | positions/s |\n\
             |---|---|---|---|---|---|---|\n",
        );
        for leg in &legs {
            text.push_str(&format!(
                "| {} | `{}` | {} | {:.1} | {:.1} | {:.1} | {:.2} |\n",
                leg.label,
                leg.kernel,
                args.threads,
                leg.mean_ms(),
                leg.min_ms(),
                leg.max_ms(),
                leg.positions_per_second()
            ));
        }
        text.push_str(&format!(
            "\n{} on {} {}, {} logical CPUs. {} layers of Llama-2-7B shape with synthetic \
             weights ({gigabytes:.2} GB); {} warm-up and {} timed positions per leg.\n\n",
            if failures.is_empty() {
                "All legs computed identical logits and KV caches"
            } else {
                "**FAILED**"
            },
            std::env::consts::OS,
            std::env::consts::ARCH,
            std::thread::available_parallelism().map_or(0, |n| n.get()),
            args.layers,
            args.warmup,
            args.positions
        ));
        if let Some(speedup) = speedup {
            text.push_str(&format!("Vectorised over scalar: {speedup:.2}x. "));
        }
        text.push_str(&format!(
            "Default kernel: {default_rate:.2} positions/s (Stage 0 target for a 4-vCPU \
             runner: >= {TARGET_POSITIONS_PER_SECOND}, {target}). Logits and KV digest: `{}`.\n",
            legs[0].digest
        ));
        std::fs::OpenOptions::new()
            .append(true)
            .open(path)
            .and_then(|mut file| file.write_all(text.as_bytes()))
            .map_err(|error| format!("appending the summary to {path}: {error}"))?;
    }

    if failures.is_empty() {
        Ok(())
    } else {
        Err(failures.join("; "))
    }
}
