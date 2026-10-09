//! One command buffer per token for the canonical per-row INT8 profile.
//!
//! [`MetalForward`] runs [`CachedIntegerModel::forward_one_token`] and
//! [`CachedIntegerModel::forward_shard_token_with_history`] with every decoder
//! operation on the GPU ([`arc_gpu::metal_decoder`]): one command buffer and
//! one host round trip per token, where the per-projection hook of
//! [`crate::metal_gemv`] makes 225 for Llama-2-7B. Every value is
//! byte-identical to the CPU engine, including the KV-cache rows it appends to
//! the caller's [`KVCache`]; a token the GPU refuses (a status bit, a device
//! error, a cache it cannot mirror, a position past its cache) runs on the CPU
//! engine instead, so the caller always gets the CPU's answer.
//!
//! Opt-in: this module exists only with the `metal-exact` feature, and only
//! code that builds a [`MetalForward`] uses it. Nothing in the worker does.
//!
//! # Self-test
//!
//! The first [`MetalForward::new`] in a process runs a small random model with
//! grouped-query attention token by token on the GPU and on the CPU, and as a
//! two-way shard split, and compares every logit, hidden state, token and KV
//! row. A device that disagrees on one integer is never used.
//!
//! # KV-cache mirror
//!
//! The device keeps its own copy of the KV cache. Before each token, every
//! layer's device rows are reconciled with the caller's cache: rows the
//! device has not seen are uploaded (so CPU and GPU tokens can be mixed on one
//! cache), and if the last mirrored row no longer matches, the layer is
//! uploaded again from scratch. A cache whose layers do not hold exactly
//! `position` rows runs on the CPU, which then behaves exactly as it always
//! has.

use std::ops::Range;
use std::sync::{Arc, OnceLock};

use arc_gpu::metal_decoder::{
    DecoderLayerWeights, DecoderRefusal, DecoderShape, DecoderStep, DecoderWeights, MetalDecoder,
    Submission,
};
use arc_gpu::metal_exact::{ResidentMatrix, Storage};

use crate::cached_integer_model::{
    ArithmeticProfile, CachedIntegerModel, CachedLayer, I8Weights, KVCache, ModelConfig,
    ShardForwardError, ShardInput, ShardOutput, select_next_token_with_repetition_penalty,
};
use crate::integer_lut::{EXP_LUT, ONE};
use crate::metal_gemv::metal_engine;

static SELF_TEST: OnceLock<Result<(), String>> = OnceLock::new();

/// What ran where, for one [`MetalForward`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize)]
pub struct MetalForwardStats {
    /// Tokens (or shard steps) computed on the GPU.
    pub gpu_tokens: u64,
    /// Tokens computed by the CPU engine instead.
    pub cpu_tokens: u64,
    /// Every status bit a refused token set.
    pub refusal_bits: u32,
    /// Command buffers that did not complete.
    pub device_errors: u64,
    /// CPU KV-cache rows copied to the device mirror.
    pub kv_rows_uploaded: u64,
}

/// A model resident on the GPU, served one command buffer per token.
pub struct MetalForward<'a> {
    model: &'a CachedIntegerModel,
    decoder: MetalDecoder,
    mirrored: Vec<usize>,
    submission: Submission,
    stats: MetalForwardStats,
    last_gpu_seconds: f64,
}

/// The process-wide self-test: a small random model, GPU against CPU.
pub fn metal_forward_self_test() -> Result<(), String> {
    SELF_TEST.get_or_init(run_self_test).clone()
}

impl<'a> MetalForward<'a> {
    /// Upload `model` with a device KV cache of `kv_capacity` positions
    /// (at most the model's `max_seq`), after the process-wide self-test.
    pub fn new(model: &'a CachedIntegerModel, kv_capacity: usize) -> Result<Self, String> {
        metal_forward_self_test()?;
        Self::build(model, kv_capacity)
    }

    fn build(model: &'a CachedIntegerModel, kv_capacity: usize) -> Result<Self, String> {
        let cfg = &model.config;
        if !model.has_canonical_i8_profile() {
            return Err("the GPU decoder runs only the canonical per-row INT8 profile".into());
        }
        if model.layers.len() != cfg.n_layers || model.layers.iter().any(|l| !l.is_loaded()) {
            return Err("the GPU decoder needs every layer loaded".into());
        }
        let engine = metal_engine()?;
        let upload = |w: &I8Weights| -> Result<Arc<ResidentMatrix>, String> {
            engine
                .upload(&w.data, &w.scales, w.n_rows, w.n_cols, Storage::Shared)
                .map(Arc::new)
        };
        let mut layers = Vec::with_capacity(cfg.n_layers);
        for layer in &model.layers {
            layers.push(DecoderLayerWeights {
                wq: upload(&layer.wq)?,
                wk: upload(&layer.wk)?,
                wv: upload(&layer.wv)?,
                wo: upload(&layer.wo)?,
                w_gate: upload(&layer.w_gate)?,
                w_up: upload(&layer.w_up)?,
                w_down: upload(&layer.w_down)?,
                attn_norm: layer.attn_norm.clone(),
                ffn_norm: layer.ffn_norm.clone(),
            });
        }
        let output = upload(&model.output_weight)?;
        let shape = DecoderShape {
            n_layers: cfg.n_layers,
            d_model: cfg.d_model,
            n_heads: cfg.n_heads,
            n_kv_heads: cfg.n_kv_heads,
            d_head: cfg.d_head,
            d_kv: cfg.d_kv,
            d_ff: cfg.d_ff,
            vocab: cfg.vocab_size,
            attn_scale: cfg.attn_scale,
            max_seq: cfg.max_seq,
            kv_capacity: kv_capacity.min(cfg.max_seq),
        };
        let weights = DecoderWeights {
            layers,
            final_norm: model.final_norm.clone(),
            output,
            rope_cos: cfg.rope_cos.clone(),
            rope_sin: cfg.rope_sin.clone(),
            exp_lut: EXP_LUT.to_vec(),
        };
        let decoder = MetalDecoder::new(engine, shape, weights)?;
        Ok(Self {
            model,
            decoder,
            mirrored: vec![0; cfg.n_layers],
            submission: Submission::OneCommandBuffer,
            stats: MetalForwardStats::default(),
            last_gpu_seconds: 0.0,
        })
    }

    /// When the host waits for the GPU (the default is one command buffer per
    /// token). Changes speed only.
    pub fn set_submission(&mut self, submission: Submission) {
        self.submission = submission;
    }

    pub fn stats(&self) -> MetalForwardStats {
        self.stats
    }

    /// GPU time of the last token computed on the GPU, in seconds.
    pub fn last_gpu_seconds(&self) -> f64 {
        self.last_gpu_seconds
    }

    /// INT8 weight bytes one whole token reads.
    pub fn weight_bytes_per_token(&self) -> usize {
        self.decoder.weight_bytes_per_token()
    }

    /// Exactly [`CachedIntegerModel::forward_one_token`].
    pub fn forward_one_token(&mut self, token: u32, cache: &mut KVCache) -> Vec<i64> {
        let model = self.model;
        let cfg = &model.config;
        let d = cfg.d_model;
        // The CPU path's early return, before anything touches the cache.
        if model.embedding_q16.len() < (token as usize + 1) * d {
            return Vec::new();
        }
        let pos = cache.seq_len;
        let layers = 0..cfg.n_layers;
        if self.sync_mirror(cache, pos, layers.clone()) {
            let idx = (token as usize).min(cfg.vocab_size - 1);
            let hidden = &model.embedding_q16[idx * d..(idx + 1) * d];
            match self
                .decoder
                .step(hidden, pos, layers.clone(), true, self.submission)
            {
                Ok(step) => {
                    self.commit(cache, &step, layers, pos);
                    return step.logits.unwrap_or_default();
                }
                Err(refusal) => self.note_refusal(&refusal),
            }
        }
        self.stats.cpu_tokens += 1;
        model.forward_one_token(token, cache)
    }

    /// Exactly [`CachedIntegerModel::forward_shard_token`].
    pub fn forward_shard_token(
        &mut self,
        input: ShardInput,
        cache: &mut KVCache,
        start_layer: usize,
        end_layer: usize,
        position: usize,
    ) -> Result<ShardOutput, ShardForwardError> {
        self.forward_shard_token_with_history(input, cache, start_layer, end_layer, position, &[])
    }

    /// Exactly [`CachedIntegerModel::forward_shard_token_with_history`].
    pub fn forward_shard_token_with_history(
        &mut self,
        input: ShardInput,
        cache: &mut KVCache,
        start_layer: usize,
        end_layer: usize,
        position: usize,
        generated_tokens: &[u32],
    ) -> Result<ShardOutput, ShardForwardError> {
        let model = self.model;
        let cfg = &model.config;
        let d = cfg.d_model;
        let is_last = end_layer == cfg.n_layers;
        let end = end_layer.min(model.layers.len());

        // The CPU path's preflight, in its order and with its errors.
        if position >= cfg.max_seq {
            return Err(ShardForwardError::PositionOutOfRange {
                position,
                max_seq: cfg.max_seq,
            });
        }
        for layer_idx in start_layer..end {
            let cached = cache.k_data[layer_idx].len() / cfg.d_kv.max(1);
            if cached != position {
                return Err(ShardForwardError::KvCacheOutOfSync {
                    layer: layer_idx,
                    expected_positions: position,
                    cached_positions: cached,
                });
            }
        }
        for layer_idx in start_layer..end {
            if !model.layers[layer_idx].is_loaded() {
                return Err(ShardForwardError::LayerNotLoaded { layer: layer_idx });
            }
        }
        let (hidden, token) = match input {
            ShardInput::Token(token_id) => {
                let idx = (token_id as usize).min(cfg.vocab_size - 1);
                match model.embedding_q16.get(idx * d..idx * d + d) {
                    Some(row) => (row.to_vec(), Some(token_id)),
                    // Out of range: let the CPU path fail exactly as it does.
                    None => {
                        self.stats.cpu_tokens += 1;
                        return model.forward_shard_token_with_history(
                            ShardInput::Token(token_id),
                            cache,
                            start_layer,
                            end_layer,
                            position,
                            generated_tokens,
                        );
                    }
                }
            }
            ShardInput::Hidden(state) => {
                if state.len() != d {
                    return Err(ShardForwardError::BadHiddenDim {
                        got: state.len(),
                        expected: d,
                    });
                }
                (state, None)
            }
        };

        let layers = start_layer..end;
        if self.sync_mirror(cache, position, layers.clone()) {
            match self
                .decoder
                .step(&hidden, position, layers.clone(), is_last, self.submission)
            {
                Ok(step) => {
                    self.commit(cache, &step, layers, position);
                    return Ok(if is_last {
                        let mut logits = step.logits.unwrap_or_default();
                        let id = select_next_token_with_repetition_penalty(
                            &mut logits,
                            generated_tokens,
                        );
                        let bytes: Vec<u8> = logits.iter().flat_map(|v| v.to_le_bytes()).collect();
                        ShardOutput::Token {
                            id,
                            logits_hash: arc_crypto::hash_bytes(&bytes),
                        }
                    } else {
                        ShardOutput::Hidden(step.hidden)
                    });
                }
                Err(refusal) => self.note_refusal(&refusal),
            }
        }
        self.stats.cpu_tokens += 1;
        let input = match token {
            Some(token_id) => ShardInput::Token(token_id),
            None => ShardInput::Hidden(hidden),
        };
        model.forward_shard_token_with_history(
            input,
            cache,
            start_layer,
            end_layer,
            position,
            generated_tokens,
        )
    }

    /// Append the step's KV rows to the caller's cache, as the CPU does.
    fn commit(
        &mut self,
        cache: &mut KVCache,
        step: &DecoderStep,
        layers: Range<usize>,
        pos: usize,
    ) {
        for ((layer, k), v) in layers.zip(&step.k_rows).zip(&step.v_rows) {
            cache.push_k(layer, k);
            cache.push_v(layer, v);
            self.mirrored[layer] = pos + 1;
        }
        cache.seq_len = pos + 1;
        self.stats.gpu_tokens += 1;
        self.last_gpu_seconds = step.gpu_seconds;
    }

    fn note_refusal(&mut self, refusal: &DecoderRefusal) {
        match refusal {
            DecoderRefusal::Status(bits) => self.stats.refusal_bits |= *bits,
            DecoderRefusal::Device => self.stats.device_errors += 1,
            DecoderRefusal::Input(_) => {}
        }
    }

    /// Reconcile the device KV cache with `cache` for `layers`. Returns false
    /// when the GPU cannot run position `pos` from this cache; the CPU then
    /// computes the token.
    fn sync_mirror(&mut self, cache: &KVCache, pos: usize, layers: Range<usize>) -> bool {
        let shape = *self.decoder.shape();
        if pos >= shape.kv_capacity || pos >= shape.max_seq {
            return false;
        }
        let d_kv = shape.d_kv;
        for layer in layers {
            let (Some(keys), Some(values)) = (cache.k_data.get(layer), cache.v_data.get(layer))
            else {
                return false;
            };
            if keys.len() != pos * d_kv || values.len() != pos * d_kv {
                return false;
            }
            let mut mirrored = self.mirrored[layer].min(pos);
            if mirrored > 0 {
                let last = mirrored - 1;
                let row = last * d_kv..(last + 1) * d_kv;
                let same = matches!(
                    self.decoder.read_kv(layer, last),
                    Ok((k, v)) if k[..] == keys[row.clone()] && v[..] == values[row.clone()]
                );
                if !same {
                    mirrored = 0;
                }
            }
            for p in mirrored..pos {
                let row = p * d_kv..(p + 1) * d_kv;
                if self
                    .decoder
                    .write_kv(layer, p, &keys[row.clone()], &values[row])
                    .is_err()
                {
                    return false;
                }
                self.stats.kv_rows_uploaded += 1;
            }
            self.mirrored[layer] = pos;
        }
        true
    }
}

// ---- synthetic models: the self-test, the tests and the benchmark ---------

/// Dimensions of a synthetic Llama-shaped model.
#[derive(Debug, Clone, Copy)]
pub(crate) struct SyntheticShape {
    pub(crate) n_layers: usize,
    pub(crate) d_model: usize,
    pub(crate) n_heads: usize,
    pub(crate) n_kv_heads: usize,
    pub(crate) d_ff: usize,
    pub(crate) vocab: usize,
    pub(crate) max_seq: usize,
    /// Embedding rows filled with random values; the rest stay zero, so a
    /// 32,000-row embedding costs no memory until it is touched.
    pub(crate) embedding_rows: usize,
}

/// SplitMix64.
pub(crate) struct SynthRng(pub(crate) u64);

impl SynthRng {
    pub(crate) fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    fn below(&mut self, n: u64) -> i64 {
        (self.next_u64() % n) as i64
    }

    /// Weights in [-127, 127] (the profile's range) with per-row scales in
    /// [32, 96], which keeps random-model activations near 2^20.
    pub(crate) fn matrix(&mut self, rows: usize, cols: usize) -> I8Weights {
        let mut data = Vec::with_capacity(rows * cols + 8);
        while data.len() < rows * cols {
            data.extend(self.next_u64().to_le_bytes().map(|b| (b as i8).max(-127)));
        }
        data.truncate(rows * cols);
        I8Weights {
            data,
            scales: (0..rows).map(|_| 32 + self.below(65)).collect(),
            n_rows: rows,
            n_cols: cols,
        }
    }

    fn gains(&mut self, n: usize) -> Vec<i64> {
        (0..n).map(|_| ONE + self.below(8193) - 4096).collect()
    }
}

/// Q16 samples of the unit circle at multiples of pi/8 (as in the golden
/// fixture): integer RoPE tables without platform math.
pub(crate) fn synthetic_rope(d_head: usize, max_seq: usize) -> (Vec<i64>, Vec<i64>) {
    const COS: [i64; 16] = [
        65_536, 60_547, 46_341, 25_080, 0, -25_080, -46_341, -60_547, -65_536, -60_547, -46_341,
        -25_080, 0, 25_080, 46_341, 60_547,
    ];
    const SIN: [i64; 16] = [
        0, 25_080, 46_341, 60_547, 65_536, 60_547, 46_341, 25_080, 0, -25_080, -46_341, -60_547,
        -65_536, -60_547, -46_341, -25_080,
    ];
    let pairs = d_head / 2;
    let mut cos = Vec::with_capacity(max_seq * pairs);
    let mut sin = Vec::with_capacity(max_seq * pairs);
    for position in 0..max_seq {
        for pair in 0..pairs {
            let angle = (position * (pair + 1)) % COS.len();
            cos.push(COS[angle]);
            sin.push(SIN[angle]);
        }
    }
    (cos, sin)
}

/// round(2^16 / sqrt(d_head)), the loader's attention scale, for the head
/// widths the synthetic models use (no platform math in tests).
fn synthetic_attn_scale(d_head: usize) -> i64 {
    match d_head {
        4 => 32_768,
        8 => 23_170,
        16 => 16_384,
        32 => 11_585,
        64 => 8_192,
        128 => 5_793,
        _ => 4_096,
    }
}

pub(crate) fn synthetic_model(seed: u64, shape: SyntheticShape) -> CachedIntegerModel {
    let mut rng = SynthRng(seed);
    let d = shape.d_model;
    let d_head = d / shape.n_heads;
    let d_kv = d_head * shape.n_kv_heads;
    let mut embedding_q16 = vec![0i64; shape.vocab * d];
    for row in 0..shape.embedding_rows.min(shape.vocab) {
        let scale = 32 + rng.below(65);
        for value in &mut embedding_q16[row * d..(row + 1) * d] {
            *value = (rng.below(255) - 127) * scale;
        }
    }
    let embedding_i8 = I8Weights {
        data: vec![0; shape.vocab * d],
        scales: vec![1; shape.vocab],
        n_rows: shape.vocab,
        n_cols: d,
    };
    let layers = (0..shape.n_layers)
        .map(|_| CachedLayer {
            wq: rng.matrix(d, d),
            wk: rng.matrix(d_kv, d),
            wv: rng.matrix(d_kv, d),
            wo: rng.matrix(d, d),
            w_gate: rng.matrix(shape.d_ff, d),
            w_up: rng.matrix(shape.d_ff, d),
            w_down: rng.matrix(d, shape.d_ff),
            attn_norm: rng.gains(d),
            ffn_norm: rng.gains(d),
        })
        .collect();
    let final_norm = rng.gains(d);
    let output_weight = rng.matrix(shape.vocab, d);
    let (rope_cos, rope_sin) = synthetic_rope(d_head, shape.max_seq);
    CachedIntegerModel {
        config: ModelConfig {
            n_layers: shape.n_layers,
            d_model: d,
            n_heads: shape.n_heads,
            n_kv_heads: shape.n_kv_heads,
            d_ff: shape.d_ff,
            d_head,
            d_kv,
            vocab_size: shape.vocab,
            attn_scale: synthetic_attn_scale(d_head),
            rope_cos,
            rope_sin,
            max_seq: shape.max_seq,
            eos_tokens: Vec::new(),
            bos_token: 1,
            chat_template: String::new(),
            arithmetic_profile: ArithmeticProfile::LegacySplitHalfV0,
        },
        embedding_q16,
        embedding_i8,
        layers,
        final_norm,
        output_weight,
        vocab: (0..shape.vocab).map(|t| format!("t{t}")).collect(),
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

fn run_self_test() -> Result<(), String> {
    let shape = SyntheticShape {
        n_layers: 2,
        d_model: 64,
        n_heads: 4,
        n_kv_heads: 2,
        d_ff: 88,
        vocab: 300,
        max_seq: 12,
        embedding_rows: 300,
    };
    let model = synthetic_model(0x5E1F_7E57_0000_0001, shape);
    let tokens = [1u32, 7, 42, 3, 299, 150, 7, 9];
    let mut gpu = MetalForward::build(&model, shape.max_seq)?;
    let mut cpu_cache = KVCache::new(shape.n_layers);
    let mut gpu_cache = KVCache::new(shape.n_layers);
    for (index, &token) in tokens.iter().enumerate() {
        let want = model.forward_one_token(token, &mut cpu_cache);
        let got = gpu.forward_one_token(token, &mut gpu_cache);
        if got != want {
            return Err(format!(
                "self-test token {index}: GPU logits differ from the CPU"
            ));
        }
    }
    if gpu_cache.k_data != cpu_cache.k_data
        || gpu_cache.v_data != cpu_cache.v_data
        || gpu_cache.seq_len != cpu_cache.seq_len
    {
        return Err("self-test: the GPU KV cache differs from the CPU".to_string());
    }
    if gpu.stats().gpu_tokens != tokens.len() as u64 {
        return Err(format!(
            "self-test: in-domain tokens fell back to the CPU ({:?})",
            gpu.stats()
        ));
    }
    // A two-way shard split, token after token, with a penalty history.
    let mut cpu_cache = KVCache::new(shape.n_layers);
    let mut gpu_cache = KVCache::new(shape.n_layers);
    for (position, &token) in tokens.iter().take(4).enumerate() {
        let history = &tokens[..position];
        let cpu_hidden = model
            .forward_shard_token(ShardInput::Token(token), &mut cpu_cache, 0, 1, position)
            .map_err(|e| e.to_string())?;
        let gpu_hidden = gpu
            .forward_shard_token(ShardInput::Token(token), &mut gpu_cache, 0, 1, position)
            .map_err(|e| e.to_string())?;
        let (ShardOutput::Hidden(cpu_state), ShardOutput::Hidden(gpu_state)) =
            (cpu_hidden, gpu_hidden)
        else {
            return Err("self-test: the first shard must return a hidden state".to_string());
        };
        if cpu_state != gpu_state {
            return Err(format!(
                "self-test shard position {position}: hidden states differ"
            ));
        }
        let cpu_out = model
            .forward_shard_token_with_history(
                ShardInput::Hidden(cpu_state),
                &mut cpu_cache,
                1,
                shape.n_layers,
                position,
                history,
            )
            .map_err(|e| e.to_string())?;
        let gpu_out = gpu
            .forward_shard_token_with_history(
                ShardInput::Hidden(gpu_state),
                &mut gpu_cache,
                1,
                shape.n_layers,
                position,
                history,
            )
            .map_err(|e| e.to_string())?;
        match (cpu_out, gpu_out) {
            (
                ShardOutput::Token {
                    id: cpu_id,
                    logits_hash: cpu_hash,
                },
                ShardOutput::Token {
                    id: gpu_id,
                    logits_hash: gpu_hash,
                },
            ) if cpu_id == gpu_id && cpu_hash == gpu_hash => {}
            _ => {
                return Err(format!(
                    "self-test shard position {position}: tokens or logits differ"
                ));
            }
        }
    }
    if gpu_cache.k_data != cpu_cache.k_data || gpu_cache.v_data != cpu_cache.v_data {
        return Err("self-test: the shard KV caches differ".to_string());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cached_integer_model::{apply_rope, flash_attention_i64, layernorm, silu_i64};
    use crate::canonical_simd::{self, kernel_switch_guard};
    use crate::integer_lut::FRAC_BITS;
    use arc_gpu::metal_decoder::{STATUS_NORM_DOMAIN, STATUS_SCALE_BOUND, STATUS_SPLIT_DOMAIN};
    use arc_gpu::metal_exact::{MAX_PLANES, PLANE_MAX};

    fn ints(value: &serde_json::Value) -> Vec<i64> {
        value
            .as_array()
            .expect("array")
            .iter()
            .map(|v| v.as_i64().expect("integer"))
            .collect()
    }

    fn operator_kat() -> serde_json::Value {
        serde_json::from_str(include_str!("../tests/fixtures/integer_operator_kat.json"))
            .expect("operator KAT")
    }

    fn symmetric(rng: &mut SynthRng, limit: i64) -> i64 {
        (rng.next_u64() % (2 * limit as u64 + 1)) as i64 - limit
    }

    #[test]
    fn rms_norm_kernel_matches_the_cpu_and_the_operator_kat() {
        let _guard = kernel_switch_guard();
        let engine = metal_engine().expect("Metal device");
        let doc = operator_kat();
        for (index, case) in doc["rms_norm"]
            .as_array()
            .expect("cases")
            .iter()
            .enumerate()
        {
            let (input, gain) = (ints(&case["input"]), ints(&case["gamma"]));
            let (got, status) = engine.lab_rms_norm(&input, &gain).expect("GPU RMSNorm");
            assert_eq!(status, 0, "KAT case {index}");
            assert_eq!(got, ints(&case["output"]), "KAT case {index}");
        }
        let mut rng = SynthRng(0x0000_0000_4E04_0001);
        for (len, limit) in [
            (1usize, 1i64 << 20),
            (7, 1 << 40),
            (64, 1 << 30),
            (4096, 1 << 24),
            (4096, 1 << 45),
            (11008, 1 << 33),
        ] {
            let input: Vec<i64> = (0..len).map(|_| symmetric(&mut rng, limit)).collect();
            let gain: Vec<i64> = (0..len / 2)
                .map(|_| ONE + symmetric(&mut rng, 4096))
                .collect();
            let (got, status) = engine.lab_rms_norm(&input, &gain).expect("GPU RMSNorm");
            assert_eq!(status, 0, "len {len}, |x| <= {limit}");
            assert_eq!(got, layernorm(&input, &gain), "len {len}, |x| <= {limit}");
        }
        // Past 2^56 the GPU refuses (the CPU's i128 sum is still exact there).
        let (_, status) = engine
            .lab_rms_norm(&[1 << 56, 5], &[])
            .expect("GPU RMSNorm");
        assert_eq!(status, STATUS_NORM_DOMAIN);
    }

    #[test]
    fn silu_kernel_matches_the_cpu_and_the_operator_kat() {
        let _guard = kernel_switch_guard();
        let engine = metal_engine().expect("Metal device");
        let doc = operator_kat();
        let pairs: Vec<Vec<i64>> = doc["silu"]
            .as_array()
            .expect("pairs")
            .iter()
            .map(ints)
            .collect();
        let gate: Vec<i64> = pairs.iter().map(|p| p[0]).collect();
        let want: Vec<i64> = pairs.iter().map(|p| p[1]).collect();
        let got = engine
            .lab_silu_mul(&gate, &vec![ONE; gate.len()], &EXP_LUT[..])
            .expect("GPU SiLU");
        assert_eq!(got, want, "silu(x) * ONE >> 16 must be the KAT's silu(x)");
        let mut rng = SynthRng(0x0000_0000_5117_0002);
        let gate: Vec<i64> = (0..20_000).map(|_| symmetric(&mut rng, 1 << 30)).collect();
        let up: Vec<i64> = (0..20_000).map(|_| symmetric(&mut rng, 1 << 30)).collect();
        let got = engine
            .lab_silu_mul(&gate, &up, &EXP_LUT[..])
            .expect("GPU SiLU");
        let want: Vec<i64> = gate
            .iter()
            .zip(&up)
            .map(|(&g, &u)| (silu_i64(g) * u) >> FRAC_BITS)
            .collect();
        assert_eq!(got, want);
    }

    #[test]
    fn rope_kernel_matches_the_cpu_and_the_operator_kat() {
        let _guard = kernel_switch_guard();
        let engine = metal_engine().expect("Metal device");
        let doc = operator_kat();
        let rope = &doc["rope"];
        let d_head = rope["d_head"].as_u64().expect("d_head") as usize;
        let (cos, sin) = (ints(&rope["cos"]), ints(&rope["sin"]));
        for (index, case) in rope["cases"].as_array().expect("cases").iter().enumerate() {
            if case["layout"].as_str() != Some("split_half") {
                continue;
            }
            let input = ints(&case["input"]);
            let pos = case["pos"].as_u64().expect("pos") as usize;
            let (q, k, v) = engine
                .lab_rope(&input, &input, &input, 1, 1, d_head, pos, (&cos, &sin))
                .expect("GPU RoPE");
            assert_eq!(q, ints(&case["output"]), "KAT case {index} (query)");
            assert_eq!(k, ints(&case["output"]), "KAT case {index} (key)");
            assert_eq!(v, input, "KAT case {index} (value copy)");
        }
        let mut rng = SynthRng(0x0000_0000_2093_0003);
        let (n_heads, n_kv_heads, d_head, max_seq) = (8usize, 2usize, 128usize, 9usize);
        let (cos, sin) = synthetic_rope(d_head, max_seq);
        for pos in [0usize, 3, 8] {
            let q: Vec<i64> = (0..n_heads * d_head)
                .map(|_| symmetric(&mut rng, 1 << 40))
                .collect();
            let k: Vec<i64> = (0..n_kv_heads * d_head)
                .map(|_| symmetric(&mut rng, 1 << 40))
                .collect();
            let v: Vec<i64> = (0..n_kv_heads * d_head)
                .map(|_| symmetric(&mut rng, 1 << 40))
                .collect();
            let (got_q, got_k, got_v) = engine
                .lab_rope(&q, &k, &v, n_heads, n_kv_heads, d_head, pos, (&cos, &sin))
                .expect("GPU RoPE");
            let (mut want_q, mut want_k) = (q.clone(), k.clone());
            for head in want_q.chunks_exact_mut(d_head) {
                apply_rope(head, pos, d_head, &cos, &sin);
            }
            for head in want_k.chunks_exact_mut(d_head) {
                apply_rope(head, pos, d_head, &cos, &sin);
            }
            assert_eq!((got_q, got_k, got_v), (want_q, want_k, v), "position {pos}");
        }
    }

    #[test]
    fn attention_kernel_matches_the_cpu_and_the_operator_kat() {
        let _guard = kernel_switch_guard();
        let engine = metal_engine().expect("Metal device");
        let doc = operator_kat();
        for case in doc["attention_head"].as_array().expect("cases") {
            let q = ints(&case["q"]);
            let keys: Vec<i64> = case["keys"]
                .as_array()
                .expect("keys")
                .iter()
                .flat_map(ints)
                .collect();
            let values: Vec<i64> = case["values"]
                .as_array()
                .expect("values")
                .iter()
                .flat_map(ints)
                .collect();
            let got = engine
                .lab_attention(
                    &q,
                    &keys,
                    &values,
                    1,
                    1,
                    q.len(),
                    case["attn_scale"].as_i64().expect("scale"),
                    &EXP_LUT[..],
                )
                .expect("GPU attention");
            assert_eq!(got, ints(&case["output"]), "attention: {}", case["note"]);
        }
        let mut rng = SynthRng(0x0000_0000_A77E_0004);
        for (n_heads, n_kv_heads, d_head, positions, limit) in [
            (4usize, 2usize, 16usize, 9usize, 1i64 << 22),
            (8, 8, 128, 33, 1 << 20),
            (6, 3, 40, 17, 1 << 34),
            (32, 32, 128, 64, 1 << 21),
        ] {
            let d_kv = n_kv_heads * d_head;
            let q: Vec<i64> = (0..n_heads * d_head)
                .map(|_| symmetric(&mut rng, limit))
                .collect();
            let keys: Vec<i64> = (0..positions * d_kv)
                .map(|_| symmetric(&mut rng, limit))
                .collect();
            let values: Vec<i64> = (0..positions * d_kv)
                .map(|_| symmetric(&mut rng, limit))
                .collect();
            let scale = synthetic_attn_scale(d_head);
            let got = engine
                .lab_attention(
                    &q,
                    &keys,
                    &values,
                    n_heads,
                    n_kv_heads,
                    d_head,
                    scale,
                    &EXP_LUT[..],
                )
                .expect("GPU attention");
            let mut want = Vec::with_capacity(q.len());
            for head in 0..n_heads {
                let kv_h = head * n_kv_heads / n_heads;
                want.extend(flash_attention_i64(
                    &q[head * d_head..(head + 1) * d_head],
                    &keys,
                    &values,
                    d_kv,
                    kv_h,
                    d_head,
                    positions,
                    scale,
                ));
            }
            assert_eq!(
                got, want,
                "{n_heads}/{n_kv_heads} heads of {d_head}, {positions} positions"
            );
        }
    }

    #[test]
    fn split_kernel_matches_the_cpu_limb_digits_and_flags_its_bounds() {
        let _guard = kernel_switch_guard();
        let engine = metal_engine().expect("Metal device");
        let mut rng = SynthRng(0x0000_0000_5B17_0005);
        for (len, limit) in [
            (1usize, 100i64),
            (17, 1 << 15),
            (4096, 1 << 22),
            (11008, PLANE_MAX),
        ] {
            let input: Vec<i64> = (0..len).map(|_| symmetric(&mut rng, limit)).collect();
            let (planes, used, status) = engine.lab_split(&input, 64).expect("GPU split");
            assert_eq!(status, 0, "len {len}");
            let mut cpu = vec![0i8; MAX_PLANES * len];
            let cpu_used = canonical_simd::split_limbs(&input, &mut cpu).expect("in domain");
            assert_eq!(used as usize, cpu_used, "len {len}");
            let stride = len.div_ceil(16) * 16;
            for (d, cpu_plane) in cpu.chunks_exact(len).enumerate() {
                assert_eq!(
                    &planes[d * stride..d * stride + len],
                    cpu_plane,
                    "len {len} plane {d}"
                );
                assert!(
                    planes[d * stride + len..(d + 1) * stride]
                        .iter()
                        .all(|&c| c == 0)
                );
            }
        }
        let (_, _, status) = engine.lab_split(&[1, PLANE_MAX + 1], 1).expect("GPU split");
        assert_ne!(status & STATUS_SPLIT_DOMAIN, 0);
        // 128 * sum|x| * max|s| one past i64::MAX.
        let (_, _, status) = engine
            .lab_split(&[1; 32], (i64::MAX / (128 * 32)) as u64 + 1)
            .expect("GPU split");
        assert_eq!(status, STATUS_SCALE_BOUND);
        let (_, _, status) = engine
            .lab_split(&[1; 32], (i64::MAX / (128 * 32)) as u64)
            .expect("GPU split");
        assert_eq!(status, 0);
    }

    fn small_shape() -> SyntheticShape {
        SyntheticShape {
            n_layers: 3,
            d_model: 64,
            n_heads: 8,
            n_kv_heads: 2,
            d_ff: 144,
            vocab: 280,
            max_seq: 16,
            embedding_rows: 280,
        }
    }

    #[test]
    fn whole_model_tokens_match_the_cpu_logits_and_kv_cache() {
        let _guard = kernel_switch_guard();
        let shape = small_shape();
        let model = synthetic_model(0x5EED_0000_0000_0006, shape);
        // A device cache of 12 positions: tokens 12 to 15 run on the CPU.
        let mut gpu = MetalForward::new(&model, 12).expect("GPU decoder");
        let (mut cpu_cache, mut gpu_cache) = (KVCache::new(3), KVCache::new(3));
        let tokens: Vec<u32> = (0..shape.max_seq as u32)
            .map(|i| (i * 37 + 5) % 280)
            .collect();
        for (index, &token) in tokens.iter().enumerate() {
            let want = model.forward_one_token(token, &mut cpu_cache);
            let got = gpu.forward_one_token(token, &mut gpu_cache);
            assert_eq!(got, want, "token {index}");
        }
        assert_eq!(gpu_cache.k_data, cpu_cache.k_data);
        assert_eq!(gpu_cache.v_data, cpu_cache.v_data);
        assert_eq!(gpu_cache.seq_len, cpu_cache.seq_len);
        let stats = gpu.stats();
        assert_eq!((stats.gpu_tokens, stats.cpu_tokens), (12, 4), "{stats:?}");
        // An out-of-range token returns empty logits without touching the cache.
        let before = gpu_cache.seq_len;
        assert!(gpu.forward_one_token(10_000, &mut gpu_cache).is_empty());
        assert_eq!(gpu_cache.seq_len, before);
    }

    #[test]
    fn cpu_and_gpu_tokens_mix_on_one_cache() {
        let _guard = kernel_switch_guard();
        let shape = small_shape();
        let model = synthetic_model(0x5EED_0000_0000_0007, shape);
        let mut gpu = MetalForward::new(&model, shape.max_seq).expect("GPU decoder");
        let (mut reference, mut mixed) = (KVCache::new(3), KVCache::new(3));
        for (index, token) in [3u32, 9, 27, 81, 243, 2, 6, 18].into_iter().enumerate() {
            let want = model.forward_one_token(token, &mut reference);
            let got = if index % 3 == 1 {
                model.forward_one_token(token, &mut mixed)
            } else {
                gpu.forward_one_token(token, &mut mixed)
            };
            assert_eq!(got, want, "token {index}");
        }
        assert_eq!(mixed.k_data, reference.k_data);
        assert_eq!(mixed.v_data, reference.v_data);
        assert!(gpu.stats().kv_rows_uploaded > 0, "{:?}", gpu.stats());
        // A different cache of the same length is detected and re-uploaded.
        let mut other = KVCache::new(3);
        for token in [5u32, 4, 3, 2, 1, 0, 8, 7] {
            model.forward_one_token(token, &mut other);
        }
        let mut other_reference = KVCache::new(3);
        for token in [5u32, 4, 3, 2, 1, 0, 8, 7] {
            model.forward_one_token(token, &mut other_reference);
        }
        let want = model.forward_one_token(11, &mut other_reference);
        assert_eq!(gpu.forward_one_token(11, &mut other), want);
        assert_eq!(other.k_data, other_reference.k_data);
    }

    #[test]
    fn shard_steps_match_the_cpu_shards() {
        let _guard = kernel_switch_guard();
        let shape = small_shape();
        let model = synthetic_model(0x5EED_0000_0000_0008, shape);
        let mut gpu = MetalForward::new(&model, shape.max_seq).expect("GPU decoder");
        let (mut cpu_cache, mut gpu_cache) = (KVCache::new(3), KVCache::new(3));
        let tokens = [11u32, 22, 33, 44, 55];
        for (position, &token) in tokens.iter().enumerate() {
            let history = &tokens[..position];
            let mut cpu_state = ShardInput::Token(token);
            let mut gpu_state = ShardInput::Token(token);
            for (start, end) in [(0usize, 1usize), (1, 3)] {
                let cpu = model
                    .forward_shard_token_with_history(
                        cpu_state,
                        &mut cpu_cache,
                        start,
                        end,
                        position,
                        history,
                    )
                    .expect("CPU shard");
                let gpu_out = gpu
                    .forward_shard_token_with_history(
                        gpu_state,
                        &mut gpu_cache,
                        start,
                        end,
                        position,
                        history,
                    )
                    .expect("GPU shard");
                match (cpu, gpu_out) {
                    (ShardOutput::Hidden(a), ShardOutput::Hidden(b)) => {
                        assert_eq!(a, b, "hidden at position {position}");
                        cpu_state = ShardInput::Hidden(a);
                        gpu_state = ShardInput::Hidden(b);
                    }
                    (
                        ShardOutput::Token {
                            id: a,
                            logits_hash: ha,
                        },
                        ShardOutput::Token {
                            id: b,
                            logits_hash: hb,
                        },
                    ) => {
                        assert_eq!((a, ha), (b, hb), "token at position {position}");
                        cpu_state = ShardInput::Token(0);
                        gpu_state = ShardInput::Token(0);
                    }
                    _ => panic!("shard outputs differ in kind at position {position}"),
                }
            }
        }
        assert_eq!(gpu_cache.k_data, cpu_cache.k_data);
        assert_eq!(gpu_cache.v_data, cpu_cache.v_data);
        // The CPU's typed refusals come back unchanged.
        let error = gpu
            .forward_shard_token(
                ShardInput::Hidden(vec![0; 3]),
                &mut gpu_cache,
                1,
                3,
                tokens.len(),
            )
            .expect_err("bad hidden dimension");
        assert_eq!(
            error,
            ShardForwardError::BadHiddenDim {
                got: 3,
                expected: 64
            }
        );
        let error = gpu
            .forward_shard_token(ShardInput::Token(1), &mut gpu_cache, 0, 1, 2)
            .expect_err("replayed position");
        assert_eq!(error.kind(), "kv_cache_out_of_sync");
    }

    #[test]
    fn refused_tokens_run_on_the_cpu_with_the_same_answer() {
        let _guard = kernel_switch_guard();
        let shape = small_shape();
        let mut model = synthetic_model(0x5EED_0000_0000_0009, shape);
        // Gains of 2^40 push the normed vector far outside four digits.
        model.layers[1].ffn_norm = vec![1 << 40; shape.d_model];
        let mut gpu = MetalForward::new(&model, shape.max_seq).expect("GPU decoder");
        let (mut cpu_cache, mut gpu_cache) = (KVCache::new(3), KVCache::new(3));
        for token in [1u32, 2, 3, 4] {
            let want = model.forward_one_token(token, &mut cpu_cache);
            assert_eq!(gpu.forward_one_token(token, &mut gpu_cache), want);
        }
        assert_eq!(gpu_cache.k_data, cpu_cache.k_data);
        let stats = gpu.stats();
        assert_eq!((stats.gpu_tokens, stats.cpu_tokens), (0, 4), "{stats:?}");
        assert_ne!(
            stats.refusal_bits & (STATUS_SPLIT_DOMAIN | STATUS_SCALE_BOUND),
            0,
            "{stats:?}"
        );
    }

    #[test]
    fn llama_7b_width_layers_match_the_cpu() {
        let _guard = kernel_switch_guard();
        let shape = SyntheticShape {
            n_layers: 2,
            d_model: 4096,
            n_heads: 32,
            n_kv_heads: 32,
            d_ff: 11008,
            vocab: 4096,
            max_seq: 8,
            embedding_rows: 16,
        };
        let model = synthetic_model(0x5EED_0000_0000_000A, shape);
        let mut gpu = MetalForward::new(&model, shape.max_seq).expect("GPU decoder");
        let (mut cpu_cache, mut gpu_cache) = (KVCache::new(2), KVCache::new(2));
        for (index, token) in [3u32, 1, 4].into_iter().enumerate() {
            let want = model.forward_one_token(token, &mut cpu_cache);
            assert_eq!(
                gpu.forward_one_token(token, &mut gpu_cache),
                want,
                "token {index}"
            );
        }
        assert_eq!(gpu_cache.k_data, cpu_cache.k_data);
        assert_eq!(gpu_cache.v_data, cpu_cache.v_data);
        assert_eq!(gpu.stats().gpu_tokens, 3, "{:?}", gpu.stats());
    }
}

/// Benchmark: a Llama-2-7B token on the hosted VM. Run explicitly, in
/// release: `cargo test --release -p arc-inference --features metal-exact
/// --lib metal_forward::bench -- --ignored --nocapture --test-threads=1`.
#[cfg(test)]
mod bench {
    use super::*;
    use crate::canonical_simd::{self, kernel_switch_guard};
    use crate::metal_gemv::MetalModel;
    use crate::metal_gemv::test_support::SwitchGuard;
    use std::time::Instant;

    const READ_PROBE_BYTES: usize = 256 << 20;
    /// Untimed tokens before every measurement, then the timed tokens whose
    /// median is reported.
    const WARM_UP: usize = 2;
    const TOKENS: usize = 10;
    /// Depths of the real-width models, which have distinct weights in every
    /// layer. Six layers, the head and one device copy fit in the VM's 7 GB.
    const DEPTHS: [usize; 4] = [1, 2, 4, 6];
    /// Embedding rows the synthetic models fill; token ids cycle through them.
    const EMBEDDING_ROWS: usize = 16;

    fn median(mut samples: Vec<f64>) -> f64 {
        samples.sort_by(f64::total_cmp);
        samples[samples.len() / 2]
    }

    /// Median wall time per token of `forward` over TOKENS tokens on a fresh
    /// cache, after WARM_UP untimed tokens.
    fn per_token(n_layers: usize, mut forward: impl FnMut(u32, &mut KVCache) -> Vec<i64>) -> f64 {
        let mut cache = KVCache::new(n_layers);
        let mut samples = Vec::with_capacity(TOKENS);
        for step in 0..WARM_UP + TOKENS {
            let token = (step % EMBEDDING_ROWS) as u32;
            let start = Instant::now();
            let logits = forward(token, &mut cache);
            let seconds = start.elapsed().as_secs_f64();
            assert!(!logits.is_empty());
            if step >= WARM_UP {
                samples.push(seconds);
            }
        }
        median(samples)
    }

    /// Least-squares line through (layers, seconds): (fixed seconds per
    /// token, seconds per layer).
    fn fit(points: &[(usize, f64)]) -> (f64, f64) {
        let n = points.len() as f64;
        let mean_x = points.iter().map(|&(x, _)| x as f64).sum::<f64>() / n;
        let mean_y = points.iter().map(|&(_, y)| y).sum::<f64>() / n;
        let (mut sxx, mut sxy) = (0.0, 0.0);
        for &(x, y) in points {
            let dx = x as f64 - mean_x;
            sxx += dx * dx;
            sxy += dx * (y - mean_y);
        }
        let per_layer = sxy / sxx;
        (mean_y - per_layer * mean_x, per_layer)
    }

    /// A fitted line's time for a 32-layer token.
    fn at_32((fixed, per_layer): (f64, f64)) -> f64 {
        fixed + 32.0 * per_layer
    }

    /// One path's measurements and fitted line, in milliseconds.
    fn line_json(measured: &[(usize, f64)], line: (f64, f64)) -> serde_json::Value {
        serde_json::json!({
            "ms_by_depth": measured
                .iter()
                .map(|&(depth, seconds)| serde_json::json!([depth, seconds * 1e3]))
                .collect::<Vec<_>>(),
            "ms_per_layer": line.1 * 1e3,
            "fixed_ms": line.0 * 1e3,
            "derived_32_layers_ms": at_32(line) * 1e3,
        })
    }

    #[test]
    #[ignore = "benchmark: run explicitly in release with --ignored --nocapture"]
    fn llama_7b_one_command_buffer_per_token() {
        let _guard = kernel_switch_guard();
        let engine = metal_engine().expect("Metal device");
        metal_forward_self_test().expect("self-test");
        let device = engine.report();
        let read = |storage| {
            let times = engine
                .measure_read_bandwidth(READ_PROBE_BYTES, 10, storage)
                .expect("read probe");
            READ_PROBE_BYTES as f64 / times[0] / 1e9
        };
        let read_gbps = read(Storage::Shared).max(read(Storage::Private));

        // ---- After, measured directly: a whole 7B-shaped token on the GPU.
        // The 32 layers share one layer's weights (the VM cannot hold 6.6 GB
        // twice); every layer still streams its 202 MB from memory, far past
        // any cache.
        let (d, d_ff, vocab, heads, d_head) =
            (4096usize, 11008usize, 32000usize, 32usize, 128usize);
        let mut rng = SynthRng(0x0007_B70C_E400_0001);
        let up = |w: I8Weights| {
            Arc::new(
                engine
                    .upload(&w.data, &w.scales, w.n_rows, w.n_cols, Storage::Shared)
                    .expect("upload"),
            )
        };
        let shared_layer = [
            up(rng.matrix(d, d)),
            up(rng.matrix(d, d)),
            up(rng.matrix(d, d)),
            up(rng.matrix(d, d)),
            up(rng.matrix(d_ff, d)),
            up(rng.matrix(d_ff, d)),
            up(rng.matrix(d, d_ff)),
        ];
        let output = up(rng.matrix(vocab, d));
        let gains: Vec<i64> = (0..d).map(|_| ONE + rng.below(8193) - 4096).collect();
        let layers = (0..32)
            .map(|_| DecoderLayerWeights {
                wq: shared_layer[0].clone(),
                wk: shared_layer[1].clone(),
                wv: shared_layer[2].clone(),
                wo: shared_layer[3].clone(),
                w_gate: shared_layer[4].clone(),
                w_up: shared_layer[5].clone(),
                w_down: shared_layer[6].clone(),
                attn_norm: gains.clone(),
                ffn_norm: gains.clone(),
            })
            .collect();
        let (rope_cos, rope_sin) = synthetic_rope(d_head, 64);
        let shape = DecoderShape {
            n_layers: 32,
            d_model: d,
            n_heads: heads,
            n_kv_heads: heads,
            d_head,
            d_kv: d,
            d_ff,
            vocab,
            attn_scale: 5_793,
            max_seq: 64,
            kv_capacity: 16,
        };
        let mut decoder = MetalDecoder::new(
            engine.clone(),
            shape,
            DecoderWeights {
                layers,
                final_norm: gains.clone(),
                output,
                rope_cos,
                rope_sin,
                exp_lut: EXP_LUT.to_vec(),
            },
        )
        .expect("decoder");
        let token_bytes = decoder.weight_bytes_per_token();
        let hidden: Vec<i64> = (0..d).map(|_| (rng.below(255) - 127) * 64).collect();
        // Median wall and GPU seconds, command buffers and dispatches of a
        // token through layers 0..depth and the head.
        let mut run = |depth: usize, submission: Submission| {
            let (mut walls, mut gpus) = (Vec::new(), Vec::new());
            let (mut buffers, mut dispatches) = (0, 0);
            for pos in 0..WARM_UP + TOKENS {
                let start = Instant::now();
                let step = decoder
                    .step(&hidden, pos, 0..depth, true, submission)
                    .expect("in-domain token");
                if pos >= WARM_UP {
                    walls.push(start.elapsed().as_secs_f64());
                    gpus.push(step.gpu_seconds);
                }
                buffers = step.command_buffers;
                dispatches = step.dispatches;
            }
            (median(walls), median(gpus), buffers, dispatches)
        };
        let modes: Vec<(Submission, (f64, f64, usize, usize))> = [
            Submission::OneCommandBuffer,
            Submission::PerLayer,
            Submission::PerDispatch,
        ]
        .into_iter()
        .map(|submission| (submission, run(32, submission)))
        .collect();
        // The same decoder at the real-width depths, to check the 32-layer
        // extrapolation that the before figures rely on.
        let shared_points: Vec<(usize, f64)> = DEPTHS
            .iter()
            .map(|&depth| (depth, run(depth, Submission::OneCommandBuffer).0))
            .collect();
        drop(decoder);
        drop(shared_layer);

        // ---- Before and after on real-width models (distinct weights in
        // every layer) of each depth. A least-squares line per path gives its
        // cost per layer and its fixed cost per token; 32 layers is DERIVED.
        let names = [
            "CPU scalar",
            "CPU SIMD",
            "GPU per projection (#176)",
            "GPU one command buffer per token",
        ];
        let mut points: Vec<Vec<(usize, f64)>> = vec![Vec::new(); names.len()];
        for n_layers in DEPTHS {
            let model = synthetic_model(
                0x0007_B70C_E400_0010 + n_layers as u64,
                SyntheticShape {
                    n_layers,
                    d_model: d,
                    n_heads: heads,
                    n_kv_heads: heads,
                    d_ff,
                    vocab,
                    max_seq: 64,
                    embedding_rows: EMBEDDING_ROWS,
                },
            );
            let scalar = per_token(n_layers, |t, c| model.forward_one_token(t, c));
            points[0].push((n_layers, scalar));
            canonical_simd::set_fast_canonical_kernel(true);
            let simd = per_token(n_layers, |t, c| model.forward_one_token(t, c));
            points[1].push((n_layers, simd));
            canonical_simd::set_fast_canonical_kernel(false);
            {
                let metal = MetalModel::new(&model).expect("resident model");
                let _switch = SwitchGuard::set(true);
                let hooked =
                    per_token(n_layers, |t, c| metal.run(|| model.forward_one_token(t, c)));
                points[2].push((n_layers, hooked));
            }
            let mut fused = MetalForward::new(&model, 16).expect("GPU decoder");
            let one_buffer = per_token(n_layers, |t, c| fused.forward_one_token(t, c));
            points[3].push((n_layers, one_buffer));
            assert_eq!(fused.stats().cpu_tokens, 0, "{:?}", fused.stats());
        }

        // ---- Report.
        let (one_wall, one_gpu, _, _) = modes[0].1;
        let gbps = token_bytes as f64 / one_gpu / 1e9;
        let fraction = gbps / read_gbps;
        let fits: Vec<(f64, f64)> = points.iter().map(Vec::as_slice).map(fit).collect();
        let shared_fit = fit(&shared_points);
        let shared_check = at_32(shared_fit) / one_wall - 1.0;
        let before = at_32(fits[2]);
        let after_derived = at_32(fits[3]);
        let simd = at_32(fits[1]);

        let mut md = String::new();
        md.push_str("### One command buffer per token, Llama-2-7B shape\n\n");
        md.push_str(
            "Virtualized-runner measurement: GitHub-hosted macOS VM (Apple M1, virtual), \
             paravirtual Metal GPU. Not Apple GPU hardware numbers.\n\n",
        );
        md.push_str(&format!(
            "Device `{}`. Measured read bandwidth {read_gbps:.1} GB/s (256 MiB, best of 10, \
             best of shared and private). A token reads {token_bytes} weight bytes. Every \
             time is the median of {TOKENS} tokens after {WARM_UP} warm-up tokens.\n\n",
            device.name,
        ));
        md.push_str(
            "**A whole 32-layer token on the GPU, measured.** The 32 layers share one layer's \
             weights, which still stream from memory in every layer.\n\n",
        );
        md.push_str("| Submission | Host round trips | Dispatches | Wall ms per token | GPU ms per token | tok/s (wall) |\n");
        md.push_str("|---|---|---|---|---|---|\n");
        for (submission, (wall, gpu, buffers, dispatches)) in &modes {
            md.push_str(&format!(
                "| {submission:?} | {buffers} | {dispatches} | {:.2} | {:.2} | {:.2} |\n",
                wall * 1e3,
                gpu * 1e3,
                1.0 / wall,
            ));
        }
        let (per_dispatch_wall, _, per_dispatch_buffers, _) = modes[2].1;
        let round_trip_ms =
            (per_dispatch_wall - one_wall) / (per_dispatch_buffers.max(2) - 1) as f64 * 1e3;
        md.push_str(&format!(
            "\nOne command buffer per token: {gbps:.1} GB/s of weights over GPU time, {:.0}% \
             of the measured read bandwidth. Each removed round trip saved about \
             {round_trip_ms:.3} ms of wall time (per-dispatch minus one-buffer wall time over \
             the extra round trips).\n\n",
            100.0 * fraction,
        ));
        md.push_str(
            "**Before and after on real-width models** (distinct weights in every layer). Wall \
             ms per token at each depth; a least-squares line gives the cost per layer and the \
             fixed cost per token (embedding, final norm, LM head, host overhead), extrapolated \
             to 32 layers (DERIVED).\n\n",
        );
        md.push_str("| Path |");
        for depth in DEPTHS {
            md.push_str(&format!(
                " {depth} layer{} |",
                if depth == 1 { "" } else { "s" }
            ));
        }
        md.push_str(" ms per layer | fixed ms | DERIVED 32 layers, ms | DERIVED tok/s |\n|---|");
        md.push_str(&"---|".repeat(DEPTHS.len() + 4));
        md.push('\n');
        let rows = names
            .iter()
            .copied()
            .zip(points.iter().zip(&fits))
            .chain(std::iter::once((
                "GPU one command buffer, shared weights (decoder only)",
                (&shared_points, &shared_fit),
            )));
        for (name, (measured, &(fixed, per_layer))) in rows {
            md.push_str(&format!("| {name} |"));
            for (_, seconds) in measured {
                md.push_str(&format!(" {:.1} |", seconds * 1e3));
            }
            let total = at_32((fixed, per_layer));
            md.push_str(&format!(
                " {:.2} | {:.1} | {:.0} | {:.2} |\n",
                per_layer * 1e3,
                fixed * 1e3,
                total * 1e3,
                1.0 / total,
            ));
        }
        md.push_str(&format!(
            "\nCheck on the extrapolation: the shared-weight decoder's line through 1 to {} \
             layers gives {:.0} ms at 32 layers; measured directly, {:.0} ms ({:+.0}%).\n\n",
            DEPTHS[DEPTHS.len() - 1],
            at_32(shared_fit) * 1e3,
            one_wall * 1e3,
            100.0 * shared_check,
        ));
        md.push_str(&format!(
            "**Before and after, a 7B token on this VM:** GPU per projection (#176) {:.2} tok/s \
             (DERIVED, {:.0} ms); one command buffer per token {:.2} tok/s measured at 32 \
             layers ({:.0} ms), and {:.2} tok/s DERIVED from the real-width line, the before \
             figure's method ({:.0} ms). CPU SIMD: {:.2} tok/s (DERIVED).\n\n",
            1.0 / before,
            before * 1e3,
            1.0 / one_wall,
            one_wall * 1e3,
            1.0 / after_derived,
            after_derived * 1e3,
            1.0 / simd,
        ));
        let ultra_ms = token_bytes as f64 / (fraction * 800e9) * 1e3;
        md.push_str(&format!(
            "M2 Ultra (DERIVED, ASSUMED 800 GB/s read): at the same {:.0}% of read \
             bandwidth, a token's weights take {ultra_ms:.1} ms, i.e. at most {:.0} tok/s \
             before per-token host and dispatch overhead; the VM measured {:.2} ms of wall time \
             above GPU time per token.\n",
            100.0 * fraction,
            1e3 / ultra_ms,
            (one_wall - one_gpu) * 1e3,
        ));
        println!("{md}");
        let json = serde_json::json!({
            "label": "virtualized-runner measurement",
            "device": device,
            "read_gbps": read_gbps,
            "token_weight_bytes": token_bytes,
            "warm_up_tokens": WARM_UP,
            "timed_tokens": TOKENS,
            "modes_32_layers_shared_weights": modes
                .iter()
                .map(|(s, (wall, gpu, buffers, dispatches))| serde_json::json!({
                    "submission": format!("{s:?}"),
                    "wall_ms": wall * 1e3,
                    "gpu_ms": gpu * 1e3,
                    "command_buffers": buffers,
                    "dispatches": dispatches,
                }))
                .collect::<Vec<_>>(),
            "gbps_one_command_buffer": gbps,
            "fraction_of_read_bandwidth": fraction,
            "round_trip_ms": round_trip_ms,
            "real_width_paths": names
                .iter()
                .zip(points.iter().zip(&fits))
                .map(|(name, (measured, &line_fit))| serde_json::json!({
                    "path": name,
                    "line": line_json(measured, line_fit),
                }))
                .collect::<Vec<_>>(),
            "shared_weight_decoder_line": line_json(&shared_points, shared_fit),
            "derived_m2_ultra_ms": ultra_ms,
        });
        println!("METAL_TOKEN_BENCH {json}");
        if let Ok(path) = std::env::var("ARC_METAL_TOKEN_BENCH_MD") {
            std::fs::write(path, md).expect("write the benchmark summary");
        }
    }
}
