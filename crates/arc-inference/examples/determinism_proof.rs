//! Cross-platform determinism proof for the canonical per-row INT8 engine.
//!
//! Loads the pinned Llama-2-7B-Chat Q4_K_M GGUF through the canonical
//! interleaved-RoPE INT8 profile, greedily decodes a fixed public prompt set
//! and writes a platform-independent transcript. For every forward position
//! the transcript records the BLAKE3 digest of the exact i64 logits and of the
//! KV rows appended at that position, then the generated token IDs. Two
//! machines agree byte-for-byte exactly when their transcripts have the same
//! SHA-256, which the caller computes with an independent tool.
//!
//! Nothing machine-specific (OS, CPU, kernel choice, timing) is written to the
//! transcript. That goes to the separate run JSON, so the scalar kernel and the
//! opt-in SIMD kernel must also produce the identical transcript.
//!
//! The generation loop is the engine's own greedy v2 loop
//! (`try_generate_v2_greedy`): one internal BOS forward, token-at-a-time
//! prefill, raw argmax with ties to the lowest index, EOS included in the
//! output, and the final non-EOS forward. The first prompt is replayed through
//! that public API as a cross-check unless `--no-engine-crosscheck` is given.
//!
//! ```text
//! determinism_proof --model GGUF --prompts PROMPTS.json --kernel scalar|simd
//!     --max-new-tokens N --transcript OUT.txt --run-json OUT.json
//!     [--prompt-limit N] [--shard K/COUNT] [--deadline-seconds S]
//!     [--no-engine-crosscheck]
//! ```
//!
//! `--shard K/COUNT` runs only the K-th of COUNT contiguous slices of the
//! prompt set (K counts from 0). Each prompt starts from an empty KV cache, so
//! a shard's blocks are exactly the blocks a single run writes for those
//! prompts; joining shard transcripts in order and ending with `end`
//! reproduces the single-run transcript byte for byte.

use arc_inference::cached_integer_model::{
    CachedIntegerModel, GGUF_INTERLEAVED_ROPE_I8_INFERENCE_PROFILE,
    GGUF_LLAMA_GREEDY_GENERATION_SEMANTICS_V1, KVCache,
    load_cached_model_canonical_i8_interleaved_rope,
};
use arc_inference::canonical_prefill;
use arc_inference::canonical_simd;
use arc_inference::integer_lut::argmax_i64;
use arc_inference::llama_spm_tokenizer::LlamaGgufSpmTokenizer;
use arc_inference::tensor_parallel::hash_i64;
use serde_json::{Value, json};
use std::io::{Read, Write};
use std::time::Instant;

const TRANSCRIPT_SCHEMA: &str = "arc-determinism-proof-transcript-v1";
const RUN_SCHEMA: &str = "arc-determinism-proof-run-v1";
const PROMPT_SCHEMA: &str = "arc-determinism-proof-prompts-v1";
const USAGE: &str = "usage: determinism_proof --model GGUF --prompts PROMPTS.json \
    --kernel scalar|simd --max-new-tokens N --transcript OUT.txt --run-json OUT.json \
    [--prompt-limit N] [--shard K/COUNT] [--deadline-seconds S] [--no-engine-crosscheck]";

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kernel {
    Scalar,
    Simd,
}

impl Kernel {
    fn requested_label(self) -> &'static str {
        match self {
            Kernel::Scalar => "scalar",
            Kernel::Simd => "simd",
        }
    }

    /// The projection kernel this build actually dispatches for the request.
    fn effective_label(self) -> &'static str {
        match self {
            Kernel::Scalar => "scalar-i8xi64",
            Kernel::Simd if cfg!(target_arch = "aarch64") => "neon-sdot-limb",
            Kernel::Simd if cfg!(target_arch = "x86_64") => "avx2-limb",
            Kernel::Simd => "unavailable",
        }
    }
}

struct Args {
    model: String,
    prompts: String,
    kernel: Kernel,
    max_new_tokens: u32,
    transcript: String,
    run_json: String,
    prompt_limit: Option<usize>,
    shard: (usize, usize),
    deadline_seconds: Option<f64>,
    engine_crosscheck: bool,
}

fn parse_args() -> Result<Args, String> {
    let mut model = None;
    let mut prompts = None;
    let mut kernel = None;
    let mut max_new_tokens = None;
    let mut transcript = None;
    let mut run_json = None;
    let mut prompt_limit = None;
    let mut shard = (0, 1);
    let mut deadline_seconds = None;
    let mut engine_crosscheck = true;
    let mut args = std::env::args().skip(1);
    while let Some(flag) = args.next() {
        if flag == "--no-engine-crosscheck" {
            engine_crosscheck = false;
            continue;
        }
        let value = args
            .next()
            .ok_or_else(|| format!("{flag} needs a value\n{USAGE}"))?;
        match flag.as_str() {
            "--model" => model = Some(value),
            "--prompts" => prompts = Some(value),
            "--kernel" => {
                kernel = Some(match value.as_str() {
                    "scalar" => Kernel::Scalar,
                    "simd" => Kernel::Simd,
                    other => return Err(format!("--kernel must be scalar or simd, not {other}")),
                })
            }
            "--max-new-tokens" => {
                let parsed: u32 = value
                    .parse()
                    .map_err(|error| format!("--max-new-tokens {value}: {error}"))?;
                if parsed == 0 {
                    return Err("--max-new-tokens must be at least 1".into());
                }
                max_new_tokens = Some(parsed);
            }
            "--transcript" => transcript = Some(value),
            "--run-json" => run_json = Some(value),
            "--prompt-limit" => {
                let parsed: usize = value
                    .parse()
                    .map_err(|error| format!("--prompt-limit {value}: {error}"))?;
                if parsed == 0 {
                    return Err("--prompt-limit must be at least 1".into());
                }
                prompt_limit = Some(parsed);
            }
            "--shard" => {
                let (index, count) = value
                    .split_once('/')
                    .ok_or_else(|| format!("--shard {value}: expected K/COUNT"))?;
                let index: usize = index
                    .parse()
                    .map_err(|error| format!("--shard {value}: {error}"))?;
                let count: usize = count
                    .parse()
                    .map_err(|error| format!("--shard {value}: {error}"))?;
                if count == 0 || index >= count {
                    return Err(format!("--shard {value}: expected K/COUNT with K < COUNT"));
                }
                shard = (index, count);
            }
            "--deadline-seconds" => {
                let parsed: f64 = value
                    .parse()
                    .map_err(|error| format!("--deadline-seconds {value}: {error}"))?;
                if !parsed.is_finite() || parsed <= 0.0 {
                    return Err("--deadline-seconds must be a positive number".into());
                }
                deadline_seconds = Some(parsed);
            }
            other => return Err(format!("unknown argument {other}\n{USAGE}")),
        }
    }
    let missing = |name: &str| format!("missing {name}\n{USAGE}");
    Ok(Args {
        model: model.ok_or_else(|| missing("--model"))?,
        prompts: prompts.ok_or_else(|| missing("--prompts"))?,
        kernel: kernel.ok_or_else(|| missing("--kernel"))?,
        max_new_tokens: max_new_tokens.ok_or_else(|| missing("--max-new-tokens"))?,
        transcript: transcript.ok_or_else(|| missing("--transcript"))?,
        run_json: run_json.ok_or_else(|| missing("--run-json"))?,
        prompt_limit,
        shard,
        deadline_seconds,
        engine_crosscheck,
    })
}

struct Prompt {
    id: String,
    text: String,
}

fn load_prompts(path: &str) -> Result<Vec<Prompt>, String> {
    let text = std::fs::read_to_string(path).map_err(|error| format!("read {path}: {error}"))?;
    let document: Value =
        serde_json::from_str(&text).map_err(|error| format!("parse {path}: {error}"))?;
    if document["schema"] != PROMPT_SCHEMA {
        return Err(format!("{path} is not an {PROMPT_SCHEMA} document"));
    }
    let items = document["prompts"]
        .as_array()
        .ok_or("prompt file must contain a prompts array")?;
    let mut prompts = Vec::with_capacity(items.len());
    for item in items {
        let id = item["id"]
            .as_str()
            .ok_or("every prompt needs a string id")?;
        let text = item["text"]
            .as_str()
            .ok_or("every prompt needs a string text")?;
        if id.is_empty() || id.contains(char::is_whitespace) || text.is_empty() {
            return Err(format!(
                "prompt {id:?} has an empty or whitespace id or text"
            ));
        }
        prompts.push(Prompt {
            id: id.to_string(),
            text: text.to_string(),
        });
    }
    if prompts.is_empty() {
        return Err("prompt file has no prompts".into());
    }
    Ok(prompts)
}

/// Digest of the selected prompts' content, independent of the file's line
/// endings or JSON formatting (Windows checkouts may convert newlines).
fn prompt_set_digest(prompts: &[Prompt]) -> String {
    let mut hasher = blake3::Hasher::new();
    hasher.update(PROMPT_SCHEMA.as_bytes());
    hasher.update(&(prompts.len() as u64).to_le_bytes());
    for prompt in prompts {
        for part in [&prompt.id, &prompt.text] {
            hasher.update(&(part.len() as u64).to_le_bytes());
            hasher.update(part.as_bytes());
        }
    }
    hasher.finalize().to_hex().to_string()
}

fn file_blake3(path: &str) -> Result<(String, u64), String> {
    let mut file = std::fs::File::open(path).map_err(|error| format!("open {path}: {error}"))?;
    let mut hasher = blake3::Hasher::new();
    let mut buffer = vec![0u8; 1 << 20];
    let mut total = 0u64;
    loop {
        let read = file
            .read(&mut buffer)
            .map_err(|error| format!("read {path}: {error}"))?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
        total += read as u64;
    }
    Ok((hasher.finalize().to_hex().to_string(), total))
}

/// Feed little-endian i64 bytes to `hasher` in bounded chunks. Equivalent to
/// hashing the whole little-endian byte string, without copying it whole.
fn update_i64(hasher: &mut blake3::Hasher, values: &[i64]) {
    let mut buffer = [0u8; 8 * 1024];
    for chunk in values.chunks(1024) {
        for (slot, value) in buffer.chunks_exact_mut(8).zip(chunk) {
            slot.copy_from_slice(&value.to_le_bytes());
        }
        hasher.update(&buffer[..chunk.len() * 8]);
    }
}

/// Embedding table and norm vectors: every resident Q16 value the forward pass
/// reads besides the I8 matrices (covered by the engine's `weight_hash`) and
/// the RoPE tables (digested separately because they come from libm).
fn resident_q16_digest(model: &CachedIntegerModel) -> String {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"arc-determinism-proof-resident-q16-v1");
    hasher.update(&(model.embedding_q16.len() as u64).to_le_bytes());
    update_i64(&mut hasher, &model.embedding_q16);
    hasher.update(&(model.layers.len() as u64).to_le_bytes());
    for layer in &model.layers {
        for norm in [&layer.attn_norm, &layer.ffn_norm] {
            hasher.update(&(norm.len() as u64).to_le_bytes());
            update_i64(&mut hasher, norm);
        }
    }
    hasher.update(&(model.final_norm.len() as u64).to_le_bytes());
    update_i64(&mut hasher, &model.final_norm);
    hasher.update(&model.config.attn_scale.to_le_bytes());
    hasher.finalize().to_hex().to_string()
}

fn rope_digest(model: &CachedIntegerModel) -> String {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"arc-determinism-proof-rope-q16-v1");
    for table in [&model.config.rope_cos, &model.config.rope_sin] {
        hasher.update(&(table.len() as u64).to_le_bytes());
        update_i64(&mut hasher, table);
    }
    hasher.finalize().to_hex().to_string()
}

/// K and V rows appended at `position`, for every layer, in layer order.
fn kv_append_digest(cache: &KVCache, position: usize, d_kv: usize) -> Result<String, String> {
    let start = position * d_kv;
    let end = start + d_kv;
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"arc-determinism-proof-kv-append-v1");
    hasher.update(&(position as u64).to_le_bytes());
    for (k, v) in cache.k_data.iter().zip(&cache.v_data) {
        let (Some(k_rows), Some(v_rows)) = (k.get(start..end), v.get(start..end)) else {
            return Err(format!("KV cache has no rows for position {position}"));
        };
        update_i64(&mut hasher, k_rows);
        update_i64(&mut hasher, v_rows);
    }
    Ok(hasher.finalize().to_hex().to_string())
}

/// Whole-cache digest with the same definition and domain string as
/// `low_residency_conformance`'s `kv_hash`.
fn kv_full_digest(cache: &KVCache) -> String {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"ARC-low-residency-conformance-kv-v1");
    hasher.update(&(cache.seq_len as u64).to_le_bytes());
    for layers in [&cache.k_data, &cache.v_data] {
        hasher.update(&(layers.len() as u64).to_le_bytes());
        for layer in layers {
            hasher.update(&(layer.len() as u64).to_le_bytes());
            update_i64(&mut hasher, layer);
        }
    }
    hasher.finalize().to_hex().to_string()
}

struct PositionRecord {
    position: usize,
    input: u32,
    logits_blake3: String,
    kv_append_blake3: String,
}

struct PromptTrace {
    positions: Vec<PositionRecord>,
    forward_ms: Vec<f64>,
}

impl PromptTrace {
    /// One canonical forward, recorded exactly as the engine produced it.
    fn forward(
        &mut self,
        model: &CachedIntegerModel,
        token: u32,
        cache: &mut KVCache,
    ) -> Result<Vec<i64>, String> {
        let position = cache.seq_len;
        let started = Instant::now();
        let logits = model.forward_one_token(token, cache);
        self.forward_ms
            .push(started.elapsed().as_secs_f64() * 1000.0);
        if logits.len() != model.config.vocab_size || cache.seq_len != position + 1 {
            return Err(format!(
                "forward at position {position} returned {} logits and seq_len {}",
                logits.len(),
                cache.seq_len
            ));
        }
        self.positions.push(PositionRecord {
            position,
            input: token,
            logits_blake3: hash_i64(&logits).to_hex(),
            kv_append_blake3: kv_append_digest(cache, position, model.config.d_kv)?,
        });
        Ok(logits)
    }
}

struct PromptOutcome {
    prompt_ids: Vec<u32>,
    generated: Vec<u32>,
    stopped_on_eos: bool,
    output_hash: String,
    final_logits_blake3: String,
    kv_final_blake3: String,
    trace: PromptTrace,
    prefill_seconds: f64,
    decode_seconds: f64,
    decode_forwards: usize,
}

/// The engine's greedy v2 loop (`generate_preflighted_v2_with_sampling` with
/// sampling disabled), with every forward recorded.
fn run_prompt(
    model: &CachedIntegerModel,
    prompt_ids: &[u32],
    max_new_tokens: u32,
) -> Result<PromptOutcome, String> {
    let config = &model.config;
    let _admission = model
        .preflight_generation(prompt_ids.len(), max_new_tokens)
        .map_err(|error| error.to_string())?;
    let mut cache = KVCache::new(config.n_layers);
    let mut trace = PromptTrace {
        positions: Vec::new(),
        forward_ms: Vec::new(),
    };

    let mut logits = trace.forward(model, config.bos_token, &mut cache)?;
    for &token in prompt_ids {
        logits = trace.forward(model, token, &mut cache)?;
    }
    let prefill_forwards = trace.forward_ms.len();

    let mut generated = Vec::new();
    let mut stopped_on_eos = false;
    let mut decode_forwards = 0usize;
    for _ in 0..max_new_tokens {
        let next = argmax_i64(&logits) as u32;
        generated.push(next);
        if config.eos_tokens.contains(&next) {
            stopped_on_eos = true;
            break;
        }
        logits = trace.forward(model, next, &mut cache)?;
        decode_forwards += 1;
    }
    // Forward-pass time only: digesting and token selection are excluded.
    let (prefill_ms, decode_ms) = trace.forward_ms.split_at(prefill_forwards);
    let prefill_seconds = prefill_ms.iter().sum::<f64>() / 1000.0;
    let decode_seconds = decode_ms.iter().sum::<f64>() / 1000.0;

    let output_bytes: Vec<u8> = generated.iter().flat_map(|id| id.to_le_bytes()).collect();
    Ok(PromptOutcome {
        prompt_ids: prompt_ids.to_vec(),
        generated,
        stopped_on_eos,
        output_hash: arc_crypto::hash_bytes(&output_bytes).to_hex(),
        final_logits_blake3: hash_i64(&logits).to_hex(),
        kv_final_blake3: kv_full_digest(&cache),
        trace,
        prefill_seconds,
        decode_seconds,
        decode_forwards,
    })
}

fn join_ids(ids: &[u32]) -> String {
    ids.iter().map(u32::to_string).collect::<Vec<_>>().join(",")
}

fn transcript_block(index: usize, prompt: &Prompt, outcome: &PromptOutcome) -> String {
    let mut lines = vec![
        format!("prompt {index} id={}", prompt.id),
        format!("prompt_ids {}", join_ids(&outcome.prompt_ids)),
    ];
    for record in &outcome.trace.positions {
        lines.push(format!(
            "pos {} in={} logits={} kv={}",
            record.position, record.input, record.logits_blake3, record.kv_append_blake3
        ));
    }
    lines.push(format!("generated {}", join_ids(&outcome.generated)));
    let stop = if outcome.stopped_on_eos {
        "eos"
    } else {
        "max_new_tokens"
    };
    lines.push(format!("stop {stop}"));
    lines.push(format!("output_hash {}", outcome.output_hash));
    lines.push(format!(
        "final_logits_blake3 {}",
        outcome.final_logits_blake3
    ));
    lines.push(format!("kv_final_blake3 {}", outcome.kv_final_blake3));
    let mut block = lines.join("\n");
    block.push('\n');
    block
}

struct TranscriptWriter {
    file: std::fs::File,
    hasher: blake3::Hasher,
    bytes: u64,
}

impl TranscriptWriter {
    fn create(path: &str) -> Result<Self, String> {
        let file =
            std::fs::File::create(path).map_err(|error| format!("create {path}: {error}"))?;
        Ok(Self {
            file,
            hasher: blake3::Hasher::new(),
            bytes: 0,
        })
    }

    /// Append and flush, so a run stopped early still leaves its evidence.
    fn append(&mut self, text: &str) -> Result<(), String> {
        self.file
            .write_all(text.as_bytes())
            .and_then(|()| self.file.flush())
            .map_err(|error| format!("write transcript: {error}"))?;
        self.hasher.update(text.as_bytes());
        self.bytes += text.len() as u64;
        Ok(())
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn max_rss_bytes() -> Option<u64> {
    // SAFETY: getrusage receives a zeroed, correctly sized out-parameter.
    unsafe {
        let mut u: libc::rusage = std::mem::zeroed();
        if libc::getrusage(libc::RUSAGE_SELF, &mut u) != 0 {
            return None;
        }
        #[cfg(target_os = "macos")]
        let bytes = u.ru_maxrss as u64;
        #[cfg(target_os = "linux")]
        let bytes = (u.ru_maxrss as u64).saturating_mul(1024);
        Some(bytes)
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn max_rss_bytes() -> Option<u64> {
    None
}

fn summarize_ms(samples: &[f64]) -> Value {
    if samples.is_empty() {
        return Value::Null;
    }
    let mut sorted = samples.to_vec();
    sorted.sort_by(f64::total_cmp);
    let sum: f64 = sorted.iter().sum();
    json!({
        "count": sorted.len(),
        "min": sorted[0],
        "median": sorted[sorted.len() / 2],
        "mean": sum / sorted.len() as f64,
        "max": sorted[sorted.len() - 1],
    })
}

fn rate(count: usize, seconds: f64) -> Value {
    if count == 0 || seconds <= 0.0 {
        Value::Null
    } else {
        json!(count as f64 / seconds)
    }
}

fn main() -> Result<(), String> {
    let args = parse_args()?;
    let started = Instant::now();
    let mut prompts = load_prompts(&args.prompts)?;
    let prompts_in_file = prompts.len();
    if let Some(limit) = args.prompt_limit {
        if limit > prompts.len() {
            return Err(format!(
                "--prompt-limit {limit} exceeds the {prompts_in_file} prompts in the file"
            ));
        }
        prompts.truncate(limit);
    }
    let (shard_index, shard_count) = args.shard;
    let shard_start = shard_index * prompts.len() / shard_count;
    let shard_end = (shard_index + 1) * prompts.len() / shard_count;
    if shard_start == shard_end {
        return Err(format!(
            "shard {shard_index}/{shard_count} of {} prompts is empty",
            prompts.len()
        ));
    }

    // Force each switch's one-time environment initialisation before setting
    // it, so an inherited ARC_FAST_CANONICAL_KERNEL or ARC_BATCHED_PREFILL
    // cannot change the path after this point.
    let want_simd = args.kernel == Kernel::Simd;
    let _ = canonical_simd::fast_canonical_kernel_enabled();
    canonical_simd::set_fast_canonical_kernel(want_simd);
    if want_simd && !canonical_simd::dotprod_available() {
        return Err(format!(
            "--kernel simd requested, but this {} CPU/build has no exact SIMD backend \
             (NEON dotprod on aarch64, AVX2 on x86_64)",
            std::env::consts::ARCH
        ));
    }
    if canonical_simd::fast_canonical_kernel_enabled() != want_simd {
        return Err("the canonical kernel switch did not take effect".into());
    }
    let _ = canonical_prefill::batched_prefill_enabled();
    canonical_prefill::set_batched_prefill_enabled(false);
    canonical_simd::reset_projection_census();
    canonical_simd::set_projection_census_enabled(true);

    eprintln!(
        "determinism_proof: kernel={} ({}) arch={} os={} prompts={} shard={shard_index}/{shard_count} \
         (prompts {shard_start}..{shard_end}) max_new_tokens={}",
        args.kernel.requested_label(),
        args.kernel.effective_label(),
        std::env::consts::ARCH,
        std::env::consts::OS,
        prompts.len(),
        args.max_new_tokens
    );

    let hash_started = Instant::now();
    let (model_blake3, model_bytes) = file_blake3(&args.model)?;
    let model_hash_seconds = hash_started.elapsed().as_secs_f64();
    eprintln!("model file blake3={model_blake3} bytes={model_bytes} ({model_hash_seconds:.1} s)");

    let tokenizer =
        LlamaGgufSpmTokenizer::from_gguf(&args.model).map_err(|error| error.to_string())?;
    let load_started = Instant::now();
    let model = load_cached_model_canonical_i8_interleaved_rope(&args.model)
        .map_err(|error| error.to_string())?;
    let load_seconds = load_started.elapsed().as_secs_f64();
    let profile = model
        .canonical_execution_profile()
        .ok_or("the loaded model is not a complete canonical I8 profile")?;
    if profile != GGUF_INTERLEAVED_ROPE_I8_INFERENCE_PROFILE || !model.has_all_transformer_layers()
    {
        return Err(format!("unexpected execution profile {profile}"));
    }
    let config = &model.config;
    if tokenizer.bos_token() != config.bos_token
        || tokenizer.eos_tokens() != config.eos_tokens.as_slice()
    {
        return Err("the GGUF tokenizer and model disagree on BOS/EOS ids".into());
    }
    eprintln!(
        "loaded {profile} in {load_seconds:.1} s: layers={} d_model={} vocab={} resident={} MiB",
        config.n_layers,
        config.d_model,
        config.vocab_size,
        model.memory_bytes() / (1024 * 1024)
    );

    let digest_started = Instant::now();
    let weight_blake3 = model.weight_hash().to_hex();
    let resident_blake3 = resident_q16_digest(&model);
    let rope_blake3 = rope_digest(&model);
    let digest_seconds = digest_started.elapsed().as_secs_f64();
    let prompt_set_blake3 = prompt_set_digest(&prompts);
    eprintln!(
        "weight_blake3={weight_blake3} resident_q16_blake3={resident_blake3} \
         rope_tables_blake3={rope_blake3} ({digest_seconds:.1} s)"
    );

    let mut prompt_ids = Vec::with_capacity(prompts.len());
    for prompt in &prompts {
        let ids = tokenizer
            .encode_prompt(&prompt.text)
            .map_err(|error| format!("tokenize {}: {error}", prompt.id))?;
        // The tokenizer prepends BOS; generation owns the one BOS forward.
        if ids.len() < 2
            || ids[0] != config.bos_token
            || ids[1..]
                .iter()
                .any(|&id| id == config.bos_token || id as usize >= config.vocab_size)
        {
            return Err(format!(
                "prompt {} tokenized to an invalid sequence",
                prompt.id
            ));
        }
        prompt_ids.push(ids[1..].to_vec());
    }

    let eos = config
        .eos_tokens
        .iter()
        .map(u32::to_string)
        .collect::<Vec<_>>()
        .join(",");
    let header = [
        TRANSCRIPT_SCHEMA.to_string(),
        format!("model_file_blake3 {model_blake3}"),
        format!("model_file_bytes {model_bytes}"),
        format!("execution_profile {profile}"),
        format!("generation_semantics {GGUF_LLAMA_GREEDY_GENERATION_SEMANTICS_V1}"),
        format!("tokenizer_profile {}", tokenizer.profile()),
        format!(
            "tokenizer_vocab_blake3 {}",
            hex::encode(tokenizer.vocabulary_digest())
        ),
        format!(
            "config layers={} d_model={} heads={} kv_heads={} d_head={} d_kv={} d_ff={} \
             vocab={} max_seq={} bos={} eos={eos}",
            config.n_layers,
            config.d_model,
            config.n_heads,
            config.n_kv_heads,
            config.d_head,
            config.d_kv,
            config.d_ff,
            config.vocab_size,
            config.max_seq,
            config.bos_token
        ),
        format!("weight_blake3 {weight_blake3}"),
        format!("resident_q16_blake3 {resident_blake3}"),
        format!("rope_tables_blake3 {rope_blake3}"),
        format!("prompt_set_blake3 {prompt_set_blake3}"),
        format!("prompt_count {}", prompts.len()),
        format!("max_new_tokens {}", args.max_new_tokens),
    ];
    let mut transcript = TranscriptWriter::create(&args.transcript)?;
    transcript.append(&(header.join("\n") + "\n"))?;

    let mut outcomes: Vec<PromptOutcome> = Vec::with_capacity(shard_end - shard_start);
    let mut prompt_reports = Vec::with_capacity(shard_end - shard_start);
    let mut stopped_by_deadline = false;
    let selected = prompts
        .iter()
        .zip(&prompt_ids)
        .enumerate()
        .skip(shard_start)
        .take(shard_end - shard_start);
    for (index, (prompt, ids)) in selected {
        if let Some(deadline) = args.deadline_seconds
            && started.elapsed().as_secs_f64() > deadline
        {
            eprintln!("deadline of {deadline} s reached before prompt {index}; stopping early");
            stopped_by_deadline = true;
            break;
        }
        let outcome = run_prompt(&model, ids, args.max_new_tokens)?;
        transcript.append(&transcript_block(index, prompt, &outcome))?;
        let text = tokenizer.decode_generated_content(&outcome.generated);
        eprintln!(
            "[{}/{}] {}: {} prompt + {} new tokens; prefill {:.1} s, decode {:.1} s; {text:?}",
            index + 1,
            prompts.len(),
            prompt.id,
            outcome.prompt_ids.len(),
            outcome.generated.len(),
            outcome.prefill_seconds,
            outcome.decode_seconds,
        );
        prompt_reports.push(json!({
            "index": index,
            "id": prompt.id,
            "prompt_tokens": outcome.prompt_ids.len(),
            "generated_tokens": outcome.generated.len(),
            "stop": if outcome.stopped_on_eos { "eos" } else { "max_new_tokens" },
            "output_hash": outcome.output_hash,
            "final_logits_blake3": outcome.final_logits_blake3,
            "forwards": outcome.trace.positions.len(),
            "prefill_seconds": outcome.prefill_seconds,
            "decode_seconds": outcome.decode_seconds,
            "decode_forwards": outcome.decode_forwards,
            "text": text,
        }));
        outcomes.push(outcome);
    }
    let complete = outcomes.len() == shard_end - shard_start;
    let terminator = match (complete, shard_count) {
        (false, _) => "incomplete".to_string(),
        (true, 1) => "end".to_string(),
        (true, _) => format!("end-of-shard {shard_index}/{shard_count}"),
    };
    transcript.append(&(terminator + "\n"))?;
    let census = canonical_simd::projection_census();

    // Replay the first prompt through the engine's public greedy API. Its
    // tokens and output hash must equal the recorded loop exactly.
    let mut crosscheck_ok = true;
    let crosscheck = match outcomes.first() {
        Some(first) if args.engine_crosscheck && shard_start == 0 => {
            let crosscheck_started = Instant::now();
            let (engine_tokens, engine_hash) = model
                .try_generate_v2_greedy(&first.prompt_ids, args.max_new_tokens, &config.eos_tokens)
                .map_err(|error| error.to_string())?;
            let engine_hash = engine_hash.to_hex();
            let tokens_match = engine_tokens == first.generated;
            let output_hash_match = engine_hash == first.output_hash;
            crosscheck_ok = tokens_match && output_hash_match;
            json!({
                "api": "CachedIntegerModel::try_generate_v2_greedy",
                "prompt_index": 0,
                "tokens_match": tokens_match,
                "output_hash_match": output_hash_match,
                "engine_output_hash": engine_hash,
                "seconds": crosscheck_started.elapsed().as_secs_f64(),
            })
        }
        _ => Value::Null,
    };

    let forward_ms: Vec<f64> = outcomes
        .iter()
        .flat_map(|outcome| outcome.trace.forward_ms.iter().copied())
        .collect();
    let prefill_forwards: usize = outcomes
        .iter()
        .map(|outcome| outcome.trace.positions.len() - outcome.decode_forwards)
        .sum();
    let decode_forwards: usize = outcomes.iter().map(|outcome| outcome.decode_forwards).sum();
    let prefill_seconds: f64 = outcomes.iter().map(|outcome| outcome.prefill_seconds).sum();
    let decode_seconds: f64 = outcomes.iter().map(|outcome| outcome.decode_seconds).sum();
    let generated_tokens: usize = outcomes.iter().map(|outcome| outcome.generated.len()).sum();
    let transcript_blake3 = transcript.hasher.finalize().to_hex().to_string();
    let env = |name: &str| std::env::var(name).ok();

    let run = json!({
        "schema": RUN_SCHEMA,
        "complete": complete,
        "stopped_by_deadline": stopped_by_deadline,
        "engine_crosscheck_ok": crosscheck_ok,
        "transcript": {
            "path": args.transcript,
            "bytes": transcript.bytes,
            "blake3": transcript_blake3,
            "note": "the combined SHA-256 is computed over these exact bytes by an independent tool",
        },
        "kernel": {
            "requested": args.kernel.requested_label(),
            "effective": args.kernel.effective_label(),
            "simd_available": canonical_simd::dotprod_available(),
            "fast_kernel_enabled": canonical_simd::fast_canonical_kernel_enabled(),
            "batched_prefill_enabled": canonical_prefill::batched_prefill_enabled(),
            "projection_census": serde_json::to_value(census).unwrap_or(Value::Null),
            "attention_dot": if cfg!(target_arch = "aarch64") {
                "exact NEON i32-lane path when every operand fits in i32, else exact i64 (aarch64, both kernels)"
            } else {
                "exact scalar i64"
            },
        },
        "platform": {
            "os": std::env::consts::OS,
            "arch": std::env::consts::ARCH,
            "family": std::env::consts::FAMILY,
            "available_parallelism": std::thread::available_parallelism().map(|n| n.get()).ok(),
            "rayon_threads": rayon::current_num_threads(),
        },
        "environment": {
            "ARC_FAST_CANONICAL_KERNEL": env("ARC_FAST_CANONICAL_KERNEL"),
            "ARC_BATCHED_PREFILL": env("ARC_BATCHED_PREFILL"),
            "RAYON_NUM_THREADS": env("RAYON_NUM_THREADS"),
        },
        "model": {
            "path": args.model,
            "file_blake3": model_blake3,
            "file_bytes": model_bytes,
            "file_hash_seconds": model_hash_seconds,
            "load_seconds": load_seconds,
            "digest_seconds": digest_seconds,
            "execution_profile": profile,
            "resident_bytes": model.memory_bytes(),
            "weight_blake3": weight_blake3,
            "resident_q16_blake3": resident_blake3,
            "rope_tables_blake3": rope_blake3,
        },
        "workload": {
            "prompt_file": args.prompts,
            "prompts_in_file": prompts_in_file,
            "prompt_count": prompts.len(),
            "shard": {
                "index": shard_index,
                "count": shard_count,
                "start": shard_start,
                "end": shard_end,
            },
            "prompts_completed": outcomes.len(),
            "prompt_set_blake3": prompt_set_blake3,
            "max_new_tokens": args.max_new_tokens,
            "generation_semantics": GGUF_LLAMA_GREEDY_GENERATION_SEMANTICS_V1,
            "tokenizer_profile": tokenizer.profile(),
        },
        "timing": {
            "total_seconds": started.elapsed().as_secs_f64(),
            "forwards": forward_ms.len(),
            "prefill_forwards": prefill_forwards,
            "decode_forwards": decode_forwards,
            "generated_tokens": generated_tokens,
            "prefill_seconds": prefill_seconds,
            "decode_seconds": decode_seconds,
            "prefill_tokens_per_second": rate(prefill_forwards, prefill_seconds),
            "decode_tokens_per_second": rate(decode_forwards, decode_seconds),
            "forward_ms": summarize_ms(&forward_ms),
            "note": "single process, one resident model; seconds are summed forward passes, excluding digests and token selection",
        },
        "max_rss_bytes": max_rss_bytes(),
        "engine_crosscheck": crosscheck,
        "prompts": prompt_reports,
    });
    let rendered = serde_json::to_string_pretty(&run).map_err(|error| error.to_string())?;
    std::fs::write(&args.run_json, rendered + "\n")
        .map_err(|error| format!("write {}: {error}", args.run_json))?;

    eprintln!(
        "transcript {} ({} bytes, blake3 {transcript_blake3}); run json {}",
        args.transcript, transcript.bytes, args.run_json
    );
    if !crosscheck_ok {
        return Err("the engine's own greedy API disagreed with the recorded loop".into());
    }
    if !complete {
        return Err(format!(
            "only {} of {} prompts completed before the deadline",
            outcomes.len(),
            shard_end - shard_start
        ));
    }
    Ok(())
}
