//! The dyadic profile on a GPU, through the portable WGSL kernels of
//! [`arc_gpu::modern`] (Vulkan, DX12, Metal).
//!
//! The GPU runs the forward pass; token selection, logits hashes and
//! tokenisation stay on the CPU and reuse [`super::arith`], so a GPU run
//! produces the same tokens, logits hashes and golden digests as the CPU
//! engine. This module also holds the traced CPU reference forward used to
//! name the first divergent layer and operation when a GPU disagrees, and the
//! operator known-answer self-test ([`kat`]). [`cli`] implements the
//! `arc-modern golden --gpu`, `gpu-info` and `gpu-check` commands.

pub mod backend;
pub mod cli;
pub mod kat;

use std::time::Instant;

use arc_gpu::modern::{
    AdapterReport, DyadicRef, EngineOptions, GpuEngine, GpuEngineBuilder, GpuModernError, LayerRef,
    ModelShape, TraceEntry,
};
use serde_json::{Value, json};

use super::ModernError;
use super::arith::{self, DyadicMatrix, HeadCache};
use super::model::{GenerationOutput, GenerationRequest, ModernConfig, ModernLayer, ModernModel};
use super::tables::{EXP_TABLE, attention_lambda};

/// GPU errors as the CPU engine's error kinds (a GPU domain refusal is the
/// CPU's refusal).
pub fn gpu_error(error: GpuModernError) -> ModernError {
    match error {
        GpuModernError::Domain(message) => ModernError::Domain(message),
        GpuModernError::Invalid(message) => ModernError::Invalid(message),
        other => ModernError::Invalid(format!("GPU: {other}")),
    }
}

/// Graphics APIs the Proof Kit's `runs[].adapter.backend` accepts.
pub const PROOF_GPU_APIS: [&str; 4] = ["vulkan", "metal", "dx12", "gl"];
/// Longest Proof Kit label, and the punctuation it may hold besides letters
/// and digits (`arc.proof-result.v1`).
const PROOF_LABEL_MAX: usize = 64;
const PROOF_LABEL_PUNCTUATION: &str = " ()@.,+/_-";

/// A name reduced to a Proof Kit label: other characters become spaces,
/// whitespace runs collapse, it starts with a letter or digit and holds at
/// most 64 bytes; `null` when nothing is left.
fn proof_label(raw: &str) -> Value {
    let mapped: String = raw
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || PROOF_LABEL_PUNCTUATION.contains(c) {
                c
            } else {
                ' '
            }
        })
        .collect();
    let joined = mapped.split_whitespace().collect::<Vec<_>>().join(" ");
    let Some(start) = joined.find(|c: char| c.is_ascii_alphanumeric()) else {
        return Value::Null;
    };
    let mut label = joined[start..].to_string();
    label.truncate(PROOF_LABEL_MAX);
    Value::from(label.trim_end())
}

/// The adapter as the Proof Kit's `runs[].adapter` reports it
/// (`arc.proof-result.v1`): `{vendor, device, backend, driver}`, each a
/// label or `null`, with the vendor and backend in lower case and the backend
/// one of [`PROOF_GPU_APIS`].
pub fn adapter_json(report: &AdapterReport) -> Value {
    let driver = [report.driver.as_str(), report.driver_info.as_str()]
        .iter()
        .filter(|part| !part.is_empty())
        .copied()
        .collect::<Vec<_>>()
        .join(" ");
    let backend = report.backend.to_lowercase();
    json!({
        "vendor": proof_label(&report.vendor.to_lowercase()),
        "device": proof_label(&report.name),
        "backend": if PROOF_GPU_APIS.contains(&backend.as_str()) {
            Value::from(backend)
        } else {
            Value::Null
        },
        "driver": proof_label(&driver),
    })
}

/// The GPU engine's view of a model configuration.
pub fn shape_of(config: &ModernConfig) -> ModelShape {
    ModelShape {
        n_layers: config.n_layers,
        d_model: config.d_model,
        n_heads: config.n_heads,
        n_kv_heads: config.n_kv_heads,
        d_head: config.d_head,
        d_ff: config.d_ff,
        vocab_size: config.vocab_size,
        max_seq: config.max_seq,
        rms_eps_q32: config.rms_eps_q32,
        rope_layers: config.rope_layers.clone(),
        attention_lambda: attention_lambda(config.d_head),
    }
}

fn dyadic(m: &DyadicMatrix) -> DyadicRef<'_> {
    DyadicRef {
        rows: m.rows,
        cols: m.cols,
        q: &m.q,
        mu: &m.mu,
        k: &m.k,
    }
}

fn layer_ref(layer: &ModernLayer) -> LayerRef<'_> {
    LayerRef {
        attn_norm: &layer.attn_norm,
        wq: dyadic(&layer.wq),
        wk: dyadic(&layer.wk),
        wv: dyadic(&layer.wv),
        wo: dyadic(&layer.wo),
        ffn_norm: &layer.ffn_norm,
        w_gate: dyadic(&layer.w_gate),
        w_up: dyadic(&layer.w_up),
        w_down: dyadic(&layer.w_down),
    }
}

/// Upload a model to a GPU, keeping the host copy (tests, traces).
pub fn engine_for(model: &ModernModel, options: &EngineOptions) -> Result<GpuEngine, ModernError> {
    let mut builder = GpuEngineBuilder::new(
        &shape_of(&model.config),
        &EXP_TABLE,
        &model.rope_cos,
        &model.rope_sin,
        options,
    )
    .map_err(gpu_error)?;
    builder
        .embed(&dyadic(&model.embed), &model.final_norm)
        .map_err(gpu_error)?;
    for layer in &model.layers {
        builder.layer(&layer_ref(layer)).map_err(gpu_error)?;
    }
    builder.finish().map_err(gpu_error)
}

/// Upload a model to a GPU, freeing each host tensor right after its upload,
/// so peak host memory stays near one copy of the model.
pub fn engine_from(model: ModernModel, options: &EngineOptions) -> Result<GpuEngine, ModernError> {
    let ModernModel {
        config,
        embed,
        final_norm,
        rope_cos,
        rope_sin,
        layers,
        ..
    } = model;
    let mut builder = GpuEngineBuilder::new(
        &shape_of(&config),
        &EXP_TABLE,
        &rope_cos,
        &rope_sin,
        options,
    )
    .map_err(gpu_error)?;
    drop(rope_cos);
    drop(rope_sin);
    builder
        .embed(&dyadic(&embed), &final_norm)
        .map_err(gpu_error)?;
    drop(embed);
    for layer in layers {
        builder.layer(&layer_ref(&layer)).map_err(gpu_error)?;
    }
    builder.finish().map_err(gpu_error)
}

/// The generation loop of `ModernModel::generate` (spec §6.2) over any
/// forward pass that takes up to `batch` tokens at consecutive positions and
/// returns each token's logits. The prompt is fed in batches; the decode loop
/// one token at a time; the last selected token is never forwarded.
pub fn generate_with<F>(
    config: &ModernConfig,
    request: &GenerationRequest<'_>,
    batch: usize,
    mut forward: F,
) -> Result<GenerationOutput, ModernError>
where
    F: FnMut(&[u32]) -> Result<Vec<Vec<i64>>, ModernError>,
{
    if request.prompt.is_empty() || request.max_tokens == 0 {
        return Err(ModernError::Invalid(
            "generation needs a non-empty prompt and max_tokens >= 1".into(),
        ));
    }
    if request.prompt.len() + request.max_tokens > config.max_seq {
        return Err(ModernError::Domain(format!(
            "{} prompt tokens + {} generated tokens exceed the {}-position context",
            request.prompt.len(),
            request.max_tokens,
            config.max_seq
        )));
    }
    if let Some(&bad) = request
        .prompt
        .iter()
        .find(|&&t| t as usize >= config.vocab_size)
    {
        return Err(ModernError::Domain(format!(
            "prompt token {bad} is outside the vocabulary"
        )));
    }
    let wrong_count =
        || ModernError::Invalid("forward pass returned the wrong number of logits".into());
    let mut hashes = Vec::with_capacity(request.prompt.len() + request.max_tokens);
    let prefill_start = Instant::now();
    let mut logits = Vec::new();
    for chunk in request.prompt.chunks(batch.max(1)) {
        let outputs = forward(chunk)?;
        if outputs.len() != chunk.len() {
            return Err(wrong_count());
        }
        for output in outputs {
            hashes.push(arith::logits_hash(&output));
            logits = output;
        }
    }
    let prefill_seconds = prefill_start.elapsed().as_secs_f64();
    let decode_start = Instant::now();
    let mut tokens: Vec<u32> = Vec::new();
    loop {
        let next = arith::select(&logits, &tokens, request.selection)?;
        tokens.push(next);
        if request.eos.contains(&next) || tokens.len() == request.max_tokens {
            break;
        }
        let mut outputs = forward(&[next])?;
        if outputs.len() != 1 {
            return Err(wrong_count());
        }
        logits = outputs.remove(0);
        hashes.push(arith::logits_hash(&logits));
    }
    let decode_seconds = decode_start.elapsed().as_secs_f64();
    Ok(GenerationOutput {
        output_hash: arith::tokens_hash(&tokens),
        logits_digest: arith::logits_digest(&hashes),
        decode_forwards: tokens.len() - 1,
        tokens,
        logits_hashes: hashes,
        prefill_seconds,
        decode_seconds,
    })
}

/// Generation on the GPU, starting from an empty KV cache. Identical output to
/// `ModernModel::generate` for every request the CPU accepts.
pub fn generate_gpu(
    engine: &mut GpuEngine,
    config: &ModernConfig,
    request: &GenerationRequest<'_>,
) -> Result<GenerationOutput, ModernError> {
    if request.prompt.len() + request.max_tokens > engine.capacity() {
        return Err(ModernError::Invalid(format!(
            "{} prompt tokens + {} generated tokens exceed the GPU KV capacity of {} positions",
            request.prompt.len(),
            request.max_tokens,
            engine.capacity()
        )));
    }
    engine.reset();
    let batch = engine.batch();
    generate_with(config, request, batch, |tokens| {
        engine.forward_batch(tokens).map_err(gpu_error)
    })
}

/// A KV cache for [`trace_forward_cpu`], laid out like the CPU engine's.
#[derive(Debug, Clone)]
pub struct TraceCache {
    keys: Vec<Vec<i32>>,
    values: Vec<Vec<i32>>,
    positions: usize,
}

impl TraceCache {
    pub fn new(config: &ModernConfig) -> Self {
        Self {
            keys: vec![Vec::new(); config.n_layers],
            values: vec![Vec::new(); config.n_layers],
            positions: 0,
        }
    }

    pub fn positions(&self) -> usize {
        self.positions
    }

    fn push(&mut self, layer: usize, k: &[i64], v: &[i64]) -> Result<(), ModernError> {
        let narrow = |x: &i64| {
            i32::try_from(*x)
                .map_err(|_| ModernError::Domain("KV value outside i32 (|v| >= 2^31)".into()))
        };
        let k32 = k.iter().map(narrow).collect::<Result<Vec<i32>, _>>()?;
        let v32 = v.iter().map(narrow).collect::<Result<Vec<i32>, _>>()?;
        self.keys[layer].extend_from_slice(&k32);
        self.values[layer].extend_from_slice(&v32);
        Ok(())
    }

    /// The same digest as the CPU engine's `KvCache::digest`.
    pub fn digest(&self) -> [u8; 32] {
        let mut hasher = blake3::Hasher::new();
        for (keys, values) in self.keys.iter().zip(&self.values) {
            for x in keys.iter().chain(values) {
                hasher.update(&x.to_le_bytes());
            }
        }
        *hasher.finalize().as_bytes()
    }
}

fn probe(trace: &mut Vec<TraceEntry>, name: String, values: &[i64]) {
    trace.push(TraceEntry {
        name,
        blake3: arith::logits_hash(values),
    });
}

/// `ModernModel::forward` (spec §5.8) step by step, with the BLAKE3 of every
/// intermediate under the names the GPU engine traces. Tests check that it
/// returns the engine's logits and KV cache exactly.
pub fn trace_forward_cpu(
    model: &ModernModel,
    token: u32,
    cache: &mut TraceCache,
) -> Result<(Vec<i64>, Vec<TraceEntry>), ModernError> {
    let c = &model.config;
    let position = cache.positions;
    if position >= c.max_seq {
        return Err(ModernError::Domain(format!(
            "position {position} is outside the {}-position context",
            c.max_seq
        )));
    }
    let mut trace = Vec::new();
    let mut hidden = arith::embed_row(&model.embed, token as usize)?;
    probe(&mut trace, "embed".into(), &hidden);
    let half = c.d_head / 2;
    let cos = &model.rope_cos[position * half..(position + 1) * half];
    let sin = &model.rope_sin[position * half..(position + 1) * half];
    let lambda = attention_lambda(c.d_head);
    let group = c.n_heads / c.n_kv_heads;
    let mut q = vec![0i64; c.d_q()];
    let mut k = vec![0i64; c.d_kv()];
    let mut v = vec![0i64; c.d_kv()];
    let mut attended = vec![0i64; c.d_q()];
    let mut projected = vec![0i64; c.d_model];
    let mut gate = vec![0i64; c.d_ff];
    let mut up = vec![0i64; c.d_ff];
    for (l, layer) in model.layers.iter().enumerate() {
        let name = |op: &str| format!("layer{l}.{op}");
        let normed = arith::rms_norm(&hidden, &layer.attn_norm, c.rms_eps_q32)?;
        probe(&mut trace, name("attn_norm"), &normed);
        arith::project(&layer.wq, &normed, &mut q)?;
        probe(&mut trace, name("q"), &q);
        arith::project(&layer.wk, &normed, &mut k)?;
        probe(&mut trace, name("k"), &k);
        arith::project(&layer.wv, &normed, &mut v)?;
        probe(&mut trace, name("v"), &v);
        if c.rope_layers[l] {
            for head in q.chunks_exact_mut(c.d_head) {
                arith::rope_split_half(head, cos, sin)?;
            }
            probe(&mut trace, name("q_rope"), &q);
            for head in k.chunks_exact_mut(c.d_head) {
                arith::rope_split_half(head, cos, sin)?;
            }
            probe(&mut trace, name("k_rope"), &k);
        }
        cache.push(l, &k, &v)?;
        for (head, (out, q_head)) in attended
            .chunks_mut(c.d_head)
            .zip(q.chunks(c.d_head))
            .enumerate()
        {
            let view = HeadCache {
                keys: &cache.keys[l],
                values: &cache.values[l],
                positions: position + 1,
                stride: c.d_kv(),
                offset: (head / group) * c.d_head,
            };
            arith::attention_head(q_head, view, lambda, out)?;
        }
        probe(&mut trace, name("attention"), &attended);
        arith::project(&layer.wo, &attended, &mut projected)?;
        probe(&mut trace, name("o_proj"), &projected);
        arith::add_residual(&mut hidden, &projected)?;
        probe(&mut trace, name("attn_residual"), &hidden);
        let normed = arith::rms_norm(&hidden, &layer.ffn_norm, c.rms_eps_q32)?;
        probe(&mut trace, name("ffn_norm"), &normed);
        arith::project(&layer.w_gate, &normed, &mut gate)?;
        probe(&mut trace, name("gate"), &gate);
        arith::project(&layer.w_up, &normed, &mut up)?;
        probe(&mut trace, name("up"), &up);
        for (g, &u) in gate.iter_mut().zip(&up) {
            *g = arith::gated_silu(*g, u)?;
        }
        probe(&mut trace, name("silu"), &gate);
        arith::project(&layer.w_down, &gate, &mut projected)?;
        probe(&mut trace, name("down"), &projected);
        arith::add_residual(&mut hidden, &projected)?;
        probe(&mut trace, name("ffn_residual"), &hidden);
    }
    cache.positions = position + 1;
    let normed = arith::rms_norm(&hidden, &model.final_norm, c.rms_eps_q32)?;
    probe(&mut trace, "final_norm".into(), &normed);
    let mut logits = vec![0i64; c.vocab_size];
    arith::project(&model.embed, &normed, &mut logits)?;
    probe(&mut trace, "logits".into(), &logits);
    Ok((logits, trace))
}

/// The first traced operation whose value differs (the CPU trace is
/// `expected`), or `None` when every hash agrees.
pub fn first_divergence(expected: &[TraceEntry], actual: &[TraceEntry]) -> Option<String> {
    for (e, a) in expected.iter().zip(actual) {
        if e.name != a.name {
            return Some(format!(
                "trace layout ({} on the CPU, {} on the GPU)",
                e.name, a.name
            ));
        }
        if e.blake3 != a.blake3 {
            return Some(e.name.clone());
        }
    }
    (expected.len() != actual.len()).then(|| {
        format!(
            "trace length ({} on the CPU, {} on the GPU)",
            expected.len(),
            actual.len()
        )
    })
}

/// Where a GPU first disagreed with the CPU while replaying a token sequence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Divergence {
    /// Forward call index (0 = the first prompt token).
    pub forward: usize,
    /// The first divergent operation, e.g. `layer12.gate`.
    pub op: String,
}

/// Replay `tokens` (a prompt followed by generated tokens) through the traced
/// CPU forward and the traced GPU forward, `forwards` calls deep, and return
/// the first operation whose hash differs. Also returns how many operation
/// hashes were compared.
pub fn localize(
    model: &ModernModel,
    engine: &mut GpuEngine,
    tokens: &[u32],
    forwards: usize,
) -> Result<(Option<Divergence>, usize), ModernError> {
    engine.reset();
    let mut cache = TraceCache::new(&model.config);
    let mut compared = 0;
    for (index, &token) in tokens.iter().enumerate().take(forwards) {
        let (cpu_logits, cpu_trace) = trace_forward_cpu(model, token, &mut cache)?;
        let (gpu_logits, gpu_trace) = match engine.forward_traced(token) {
            Ok(result) => result,
            Err(GpuModernError::Domain(message)) => {
                engine.reset();
                return Ok((
                    Some(Divergence {
                        forward: index,
                        op: format!("GPU refused: {message}"),
                    }),
                    compared,
                ));
            }
            Err(other) => return Err(gpu_error(other)),
        };
        compared += cpu_trace.len();
        if let Some(op) = first_divergence(&cpu_trace, &gpu_trace) {
            engine.reset();
            return Ok((Some(Divergence { forward: index, op }), compared));
        }
        if cpu_logits != gpu_logits {
            engine.reset();
            return Ok((
                Some(Divergence {
                    forward: index,
                    op: "logits (hash collision?)".into(),
                }),
                compared,
            ));
        }
    }
    engine.reset();
    Ok((None, compared))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::modern::arith::Selection;
    use crate::modern::model::tests::tiny_model;

    /// The GPU engine for `model`, or `None` (test skipped) when this machine
    /// has no adapter and `ARC_GPU_REQUIRE` is not `1`.
    pub(crate) fn gpu_or_skip(model: &ModernModel, batch: usize) -> Option<GpuEngine> {
        let options = EngineOptions {
            adapter: std::env::var("ARC_GPU_ADAPTER").ok(),
            batch: Some(batch),
            ..EngineOptions::default()
        };
        match engine_for(model, &options) {
            Ok(engine) => {
                eprintln!(
                    "GPU: {} ({}, {}), dot: {}",
                    engine.report().name,
                    engine.report().backend,
                    engine.report().driver,
                    engine.dot_path()
                );
                Some(engine)
            }
            Err(error) => {
                let message = error.to_string();
                let required = std::env::var("ARC_GPU_REQUIRE").as_deref() == Ok("1");
                assert!(
                    !required && message.contains("no usable GPU adapter"),
                    "GPU engine unavailable: {message}"
                );
                eprintln!("SKIP (no GPU adapter): {message}");
                None
            }
        }
    }

    fn cpu_logits(model: &ModernModel, tokens: &[u32]) -> (Vec<Vec<i64>>, [u8; 32]) {
        let mut cache = model.new_cache();
        let logits = tokens
            .iter()
            .map(|&t| model.forward(t, &mut cache).unwrap())
            .collect();
        (logits, cache.digest())
    }

    #[test]
    fn adapter_json_has_the_proof_kit_shape() {
        let report = AdapterReport {
            index: 0,
            name: "llvmpipe (LLVM 20.1.2, 256 bits)".into(),
            vendor_id: 0x10005,
            vendor: "Mesa".into(),
            device_id: 0,
            device_type: "Cpu".into(),
            backend: "Vulkan".into(),
            driver: "llvmpipe".into(),
            driver_info: "Mesa 25.2.8".into(),
            software: true,
        };
        assert_eq!(
            adapter_json(&report),
            json!({
                "vendor": "mesa",
                "device": "llvmpipe (LLVM 20.1.2, 256 bits)",
                "backend": "vulkan",
                "driver": "llvmpipe Mesa 25.2.8",
            })
        );
        // Metal reports no driver: `null`, never an empty label. Names keep
        // only the Proof Kit's label characters; an API the kit does not
        // list is `null`.
        let metal = AdapterReport {
            name: "Apple M2 Ultra".into(),
            vendor: "Apple".into(),
            backend: "Metal".into(),
            driver: String::new(),
            driver_info: String::new(),
            ..report.clone()
        };
        assert_eq!(
            adapter_json(&metal),
            json!({"vendor": "apple", "device": "Apple M2 Ultra", "backend": "metal", "driver": null})
        );
        let odd = AdapterReport {
            name: "  ~Radeon™ RX 7900 XTX\u{7}".into(),
            backend: "BrowserWebGpu".into(),
            driver: "x".repeat(80),
            ..report
        };
        let value = adapter_json(&odd);
        assert_eq!(value["device"], "Radeon RX 7900 XTX");
        assert_eq!(value["backend"], Value::Null);
        assert_eq!(value["driver"].as_str().unwrap().len(), 64);
    }

    #[test]
    fn generic_generation_loop_matches_the_engine() {
        let model = tiny_model();
        for selection in [Selection::Rp64Argmax, Selection::Argmax] {
            for batch in [1, 2, 5] {
                let request = GenerationRequest {
                    prompt: &[1, 2, 3, 7, 11],
                    max_tokens: 9,
                    eos: &[],
                    selection,
                };
                let expected = model.generate(&request).unwrap();
                let mut cache = model.new_cache();
                let got = generate_with(&model.config, &request, batch, |tokens| {
                    tokens
                        .iter()
                        .map(|&t| model.forward(t, &mut cache))
                        .collect()
                })
                .unwrap();
                assert_eq!(got.tokens, expected.tokens);
                assert_eq!(got.logits_hashes, expected.logits_hashes);
                assert_eq!(got.logits_digest, expected.logits_digest);
                assert_eq!(got.output_hash, expected.output_hash);
                assert_eq!(got.decode_forwards, expected.decode_forwards);
            }
        }
    }

    #[test]
    fn traced_cpu_forward_is_the_engine_forward() {
        let model = tiny_model();
        let tokens = [3u32, 17, 5, 39, 0, 21];
        let (expected, kv) = cpu_logits(&model, &tokens);
        let mut cache = TraceCache::new(&model.config);
        for (token, want) in tokens.iter().zip(&expected) {
            let (logits, trace) = trace_forward_cpu(&model, *token, &mut cache).unwrap();
            assert_eq!(&logits, want);
            // embed + 15 per RoPE layer + 13 per NoPE layer + final norm + logits.
            assert_eq!(trace.len(), 1 + 15 + 13 + 2);
            assert_eq!(trace.last().unwrap().blake3, arith::logits_hash(&logits));
        }
        assert_eq!(cache.digest(), kv);
        assert_eq!(cache.positions(), tokens.len());
        assert_eq!(first_divergence(&[], &[]), None);
    }

    #[test]
    fn gpu_forward_matches_cpu_forward_and_kv_cache() {
        let model = tiny_model();
        let Some(mut engine) = gpu_or_skip(&model, 1) else {
            return;
        };
        let tokens = [3u32, 17, 5, 39, 0, 21, 21, 8];
        let (expected, kv) = cpu_logits(&model, &tokens);
        for (index, (&token, want)) in tokens.iter().zip(&expected).enumerate() {
            let got = engine.forward(token).unwrap();
            assert_eq!(&got, want, "logits differ at position {index}");
        }
        assert_eq!(engine.kv_digest().unwrap(), kv);
        // Batched prefill computes the same logits and KV cache.
        let Some(mut batched) = gpu_or_skip(&model, 3) else {
            return;
        };
        let mut logits = Vec::new();
        for chunk in tokens.chunks(3) {
            logits.extend(batched.forward_batch(chunk).unwrap());
        }
        assert_eq!(logits, expected);
        assert_eq!(batched.kv_digest().unwrap(), kv);
    }

    #[test]
    fn gpu_generation_matches_cpu_generation() {
        let model = tiny_model();
        let Some(mut engine) = gpu_or_skip(&model, 4) else {
            return;
        };
        for selection in [Selection::Rp64Argmax, Selection::Argmax] {
            let prompts: [&[u32]; 3] = [&[1], &[1, 2, 3], &[5, 9, 2, 33, 7, 0, 12, 4]];
            for prompt in prompts {
                let request = GenerationRequest {
                    prompt,
                    max_tokens: 12,
                    eos: &[],
                    selection,
                };
                let cpu = model.generate(&request).unwrap();
                let gpu = generate_gpu(&mut engine, &model.config, &request).unwrap();
                assert_eq!(gpu.tokens, cpu.tokens);
                assert_eq!(gpu.logits_hashes, cpu.logits_hashes);
                assert_eq!(gpu.logits_digest, cpu.logits_digest);
                assert_eq!(gpu.output_hash, cpu.output_hash);
            }
        }
    }

    #[test]
    fn localisation_names_the_first_divergent_operation() {
        let model = tiny_model();
        // Change one weight of layer 1's up projection in the GPU's copy only:
        // every operation before layer1.up agrees, and layer1.up differs.
        let mut altered = model.clone();
        let weight = &mut altered.layers[1].w_up.q[0];
        *weight = if *weight > 0 { -127 } else { 127 };
        let Some(mut engine) = gpu_or_skip(&altered, 1) else {
            return;
        };
        let tokens = [4u32, 4, 31];
        let (divergence, compared) = localize(&model, &mut engine, &tokens, tokens.len()).unwrap();
        assert_eq!(
            divergence,
            Some(Divergence {
                forward: 0,
                op: "layer1.up".into(),
            })
        );
        // The whole first forward was compared (31 operations) before naming it.
        assert_eq!(compared, 31);
    }

    #[test]
    fn gpu_trace_matches_cpu_trace() {
        let model = tiny_model();
        let Some(mut engine) = gpu_or_skip(&model, 1) else {
            return;
        };
        let tokens = [4u32, 4, 31, 0, 19];
        let (divergence, compared) = localize(&model, &mut engine, &tokens, tokens.len()).unwrap();
        assert_eq!(divergence, None);
        assert_eq!(compared, tokens.len() * 31);
    }
}
