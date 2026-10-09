//! One command buffer per token for the canonical per-row INT8 profile.
//!
//! [`MetalForward`] runs [`CachedIntegerModel::forward_one_token`] and
//! [`CachedIntegerModel::forward_shard_token_with_history`] with every decoder
//! operation on the GPU ([`arc_gpu::metal_decoder`]): one command buffer and
//! one host round trip per token, where the per-projection hook of
//! [`crate::metal_gemv`] makes 225 for Llama-2-7B. Every value is
//! byte-identical to the CPU engine, including the KV-cache rows it appends to
//! the caller's cache; a token the GPU refuses (a status bit, a device error,
//! a cache it cannot mirror, a position past its cache) runs on the CPU engine
//! instead, so the caller always gets the CPU's answer.
//!
//! Opt-in: this module exists only with the `metal-exact` feature, and only
//! code that builds a [`MetalForward`] uses it. Nothing in the worker does.
//!
//! # Self-test
//!
//! The first [`MetalForward::new`] in a process runs a small random model with
//! grouped-query attention token by token on the GPU and on the CPU, as one
//! multi-row pass, and as a two-way shard split, and compares every logit,
//! hidden state, token and KV row. A device that disagrees on one integer is
//! never used.
//!
//! # KV-cache mirror
//!
//! The device keeps its own copy of the KV cache. The GPU entry points take a
//! [`MirroredKvCache`]: a [`KVCache`] that can be read freely (it derefs to
//! one) but changed only through methods that record, per layer, the first
//! row that may differ from the device copy. Before each token the device
//! uploads the rows from that mark on and nothing else, so on one cache the
//! cost per token does not grow with the context. The copy stays exact
//! because:
//! - every change goes through the wrapper: a CPU token or shard step marks
//!   rows from the old length, [`MirroredKvCache::truncate`] from the new
//!   length, and [`MirroredKvCache::edit`] (any other change) from row 0;
//! - each wrapper and each [`MetalForward`] has a unique id, and the device
//!   copy counts only for the pair that last synced, so switching caches, or
//!   syncing one cache from two decoders, uploads everything again;
//! - a token the GPU refuses leaves its row marked, so it is uploaded again.
//!
//! Test and debug builds also compare every mirrored row with the cache
//! before each token ([`MetalForward::set_verify_mirror`]) and count any
//! difference in [`MetalForwardStats::mirror_mismatches`], which the tests
//! require to be zero. A cache whose layers do not hold exactly `position`
//! rows runs on the CPU, which then behaves exactly as it always has.
//!
//! # Multi-row passes
//!
//! [`MetalForward::forward_rows_exact`] (the whole model, as #182's CPU call
//! of that name) and [`MetalForward::forward_shard_rows`] (one stage, as
//! #185's CPU call of that name) run up to [`MAX_ROWS`] consecutive tokens
//! per GPU pass, every weight read once for all of them
//! (`MetalDecoder::step_rows`): the verify step of speculative decoding.
//! They return exactly what the same one-row calls would, row for row, and
//! append the same KV rows. Rejected rows are dropped with
//! [`MetalForward::rollback_rows`] (#185) or [`MirroredKvCache::truncate`]
//! (#182), and the device keeps its copy of the rows below the new length.

use std::ops::{Deref, Range};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};

use arc_gpu::metal_decoder::{
    DecoderLayerWeights, DecoderRefusal, DecoderShape, DecoderStep, DecoderWeights, MAX_ROWS,
    MetalDecoder, Submission,
};
use arc_gpu::metal_exact::{ResidentMatrix, Storage};

use crate::cached_integer_model::{
    ArithmeticProfile, CachedIntegerModel, CachedLayer, I8Weights, KVCache, ModelConfig,
    ShardForwardError, ShardInput, ShardOutput, select_next_token_with_repetition_penalty,
};
use crate::integer_lut::{EXP_LUT, ONE};
use crate::metal_gemv::metal_engine;

static SELF_TEST: OnceLock<Result<(), String>> = OnceLock::new();

/// Ids of [`MirroredKvCache`] and [`MetalForward`] values.
static NEXT_ID: AtomicU64 = AtomicU64::new(1);

fn next_id() -> u64 {
    NEXT_ID.fetch_add(1, Ordering::Relaxed)
}

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
    /// Rows the mirror check compared with the cache (when it is on).
    pub kv_rows_verified: u64,
    /// Rows the check uploaded again, from the first one it found different
    /// from the cache. Zero unless a change escaped the wrapper's marks.
    pub mirror_mismatches: u64,
    /// Multi-row GPU passes. Their rows count in `gpu_tokens`.
    pub multi_row_passes: u64,
}

/// A [`KVCache`] for the GPU path. It records, per layer, the first row that
/// may differ from the device's copy, so a [`MetalForward`] uploads only rows
/// that changed.
///
/// It derefs to the cache for reading. Changes go through its methods, which
/// lower the mark: the CPU-token methods mark rows from each layer's old
/// length (the CPU engine only appends), [`Self::truncate`] from the new
/// length, and [`Self::edit`] (any other change) from row 0.
pub struct MirroredKvCache {
    cache: KVCache,
    id: u64,
    /// The [`MetalForward`] whose device copy `dirty_from` refers to.
    synced_by: Option<u64>,
    /// Per layer: rows from here on may differ from that device copy.
    dirty_from: Vec<usize>,
}

impl MirroredKvCache {
    /// An empty cache of `n_layers` layers.
    pub fn new(n_layers: usize) -> Self {
        Self::from_cache(KVCache::new(n_layers))
    }

    /// Wrap `cache`. Every row will be uploaded.
    pub fn from_cache(cache: KVCache) -> Self {
        let layers = cache.k_data.len();
        Self {
            cache,
            id: next_id(),
            synced_by: None,
            dirty_from: vec![0; layers],
        }
    }

    /// The cache itself.
    pub fn into_inner(self) -> KVCache {
        self.cache
    }

    /// Exactly [`CachedIntegerModel::forward_one_token`], on the CPU engine.
    pub fn cpu_forward_one_token(&mut self, model: &CachedIntegerModel, token: u32) -> Vec<i64> {
        self.mark_appended(model.config.d_kv, 0..self.cache.k_data.len());
        model.forward_one_token(token, &mut self.cache)
    }

    /// Exactly [`CachedIntegerModel::forward_shard_token_with_history`], on
    /// the CPU engine.
    pub fn cpu_forward_shard_token_with_history(
        &mut self,
        model: &CachedIntegerModel,
        input: ShardInput,
        start_layer: usize,
        end_layer: usize,
        position: usize,
        generated_tokens: &[u32],
    ) -> Result<ShardOutput, ShardForwardError> {
        self.mark_appended(model.config.d_kv, start_layer..end_layer);
        model.forward_shard_token_with_history(
            input,
            &mut self.cache,
            start_layer,
            end_layer,
            position,
            generated_tokens,
        )
    }

    /// Keep only the first `seq_len` positions, as `KVCache::truncate` does
    /// in #182: every position appended one K and one V row per layer and
    /// nothing rewrites an earlier row, so this restores exactly the cache
    /// that held `seq_len` positions. Speculative decoding uses it to drop the
    /// rows of rejected draft tokens; the device keeps its copy of the rows
    /// below `seq_len`. A `seq_len` at or past the current length changes
    /// nothing.
    pub fn truncate(&mut self, seq_len: usize) {
        let held = self.cache.seq_len;
        if seq_len >= held {
            return;
        }
        for rows in self
            .cache
            .k_data
            .iter_mut()
            .chain(self.cache.v_data.iter_mut())
        {
            debug_assert!(rows.len().is_multiple_of(held), "ragged KV cache layer");
            let width = rows.len() / held;
            rows.truncate(seq_len * width);
        }
        self.cache.seq_len = seq_len;
        for mark in &mut self.dirty_from {
            *mark = (*mark).min(seq_len);
        }
    }

    /// Keep `keep` positions of `width` values per layer:
    /// [`MetalForward::rollback_rows`]'s change, after its checks.
    fn keep_rows(&mut self, keep: usize, width: usize) {
        for rows in self
            .cache
            .k_data
            .iter_mut()
            .chain(self.cache.v_data.iter_mut())
        {
            rows.truncate(keep * width);
        }
        self.cache.seq_len = keep;
        for mark in &mut self.dirty_from {
            *mark = (*mark).min(keep);
        }
    }

    /// Any other change to the cache. Every row will be uploaded.
    pub fn edit<R>(&mut self, change: impl FnOnce(&mut KVCache) -> R) -> R {
        // Marked first, so a panic inside `change` cannot leave a stale mark.
        self.dirty_from.fill(0);
        let result = change(&mut self.cache);
        // `change` may have added or removed layers.
        self.dirty_from.resize(self.cache.k_data.len(), 0);
        result
    }

    /// Mark rows from each layer's current length on, in `layers`.
    fn mark_appended(&mut self, d_kv: usize, layers: Range<usize>) {
        for layer in layers {
            if let (Some(mark), Some(keys), Some(values)) = (
                self.dirty_from.get_mut(layer),
                self.cache.k_data.get(layer),
                self.cache.v_data.get(layer),
            ) {
                *mark = (*mark).min(keys.len().min(values.len()) / d_kv.max(1));
            }
        }
    }
}

impl Deref for MirroredKvCache {
    type Target = KVCache;

    fn deref(&self) -> &KVCache {
        &self.cache
    }
}

// ---- #185's multi-row stage types ---------------------------------------
//
// `CachedIntegerModel::forward_shard_rows` and `rollback_rows` (#185, stacked
// on #179) define these types in `cached_integer_model`, which this branch
// does not contain yet. They are mirrored here variant for variant, so that
// the GPU and CPU calls take and return the same values; once both pull
// requests are in, this module uses #185's types and drops these copies.
// `ShardRowsError::GpuRefused` is the one addition.

/// The rows a stage holder runs in one call: consecutive positions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ShardRowsInput {
    /// Token ids, for the stage that holds the embedding (`start_layer == 0`).
    Tokens(Vec<u32>),
    /// Hidden states from the previous stage, `d_model` values per row.
    Hidden(Vec<Vec<i64>>),
}

/// What a stage holder returns for the rows of one call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ShardRowsOutput {
    /// The residual stream leaving the stage, one row per input row.
    Hidden(Vec<Vec<i64>>),
    /// Raw logits of every row, before any repetition penalty, from the stage
    /// that holds the output head. The caller selects each row's token with
    /// that row's own generated history.
    Logits(Vec<Vec<i64>>),
}

/// Why a multi-row stage call or a rollback refused. A refusal leaves the
/// cache exactly as it was.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ShardRowsError {
    /// A check that [`CachedIntegerModel::forward_shard_token`] also makes:
    /// KV continuity and residency on every layer, the hidden width, and the
    /// RoPE table, which the last row must fit.
    Shard(ShardForwardError),
    /// The call carried no rows.
    NoRows,
    /// `[start_layer, end_layer)` is empty, or runs past the model or the cache.
    BadLayerRange {
        start_layer: usize,
        end_layer: usize,
        n_layers: usize,
    },
    /// Token ids sent to a stage without the embedding, or hidden rows sent
    /// to the stage that holds it.
    WrongInput { start_layer: usize },
    /// A token id this node holds no embedding row for.
    TokenNotEmbedded { token: u32 },
    /// The model is not the canonical per-row I8 profile.
    NotCanonicalProfile,
    /// A model dimension is zero, so no row can run.
    BadShape,
    /// The stage ends at the last layer but does not hold the final norm and
    /// output head.
    HeadNotLoaded,
    /// A rollback asked to keep more positions than the cache holds.
    RollbackGrows {
        keep: usize,
        cached_positions: usize,
    },
    /// A layer's K or V rows disagree with the cache's position count.
    RaggedCache {
        layer: usize,
        values: usize,
        expected: usize,
    },
    /// GPU only (not in #185): the GPU did not run the rows (a status bit,
    /// a device error, or more positions than the device KV cache holds).
    /// Nothing was appended; the caller runs the CPU's call instead.
    GpuRefused { reason: String },
}

impl ShardRowsError {
    /// Stable machine-readable tag, in the style of [`ShardForwardError::kind`].
    pub fn kind(&self) -> &'static str {
        match self {
            ShardRowsError::Shard(error) => error.kind(),
            ShardRowsError::NoRows => "no_rows",
            ShardRowsError::BadLayerRange { .. } => "bad_layer_range",
            ShardRowsError::WrongInput { .. } => "wrong_input",
            ShardRowsError::TokenNotEmbedded { .. } => "token_not_embedded",
            ShardRowsError::NotCanonicalProfile => "not_canonical_profile",
            ShardRowsError::BadShape => "bad_shape",
            ShardRowsError::HeadNotLoaded => "head_not_loaded",
            ShardRowsError::RollbackGrows { .. } => "rollback_grows",
            ShardRowsError::RaggedCache { .. } => "ragged_cache",
            ShardRowsError::GpuRefused { .. } => "gpu_refused",
        }
    }
}

impl From<ShardForwardError> for ShardRowsError {
    fn from(error: ShardForwardError) -> Self {
        ShardRowsError::Shard(error)
    }
}

impl std::fmt::Display for ShardRowsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ShardRowsError::Shard(error) => write!(f, "{error}"),
            ShardRowsError::GpuRefused { reason } => write!(f, "gpu_refused: {reason}"),
            other => write!(f, "{}: {other:?}", other.kind()),
        }
    }
}

impl std::error::Error for ShardRowsError {}

/// A model resident on the GPU, served one command buffer per token.
pub struct MetalForward<'a> {
    model: &'a CachedIntegerModel,
    decoder: MetalDecoder,
    id: u64,
    /// The [`MirroredKvCache`] the device rows belong to.
    mirror_of: Option<u64>,
    /// Per layer: device rows written for that cache.
    mirrored: Vec<usize>,
    verify_mirror: bool,
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
            id: next_id(),
            mirror_of: None,
            mirrored: vec![0; cfg.n_layers],
            verify_mirror: cfg!(any(test, debug_assertions)),
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

    /// Also compare every mirrored row with the cache before each token, and
    /// upload any row that differs, counting it in
    /// [`MetalForwardStats::mirror_mismatches`]. On by default in test and
    /// debug builds. It reads the whole cache every token, so release builds
    /// leave it off.
    pub fn set_verify_mirror(&mut self, verify: bool) {
        self.verify_mirror = verify;
    }

    /// Which multi-row projection the multi-row passes use (see
    /// `MetalDecoder::set_staged_gemm`). Changes speed only.
    pub fn set_staged_gemm(&mut self, staged: bool) {
        self.decoder.set_staged_gemm(staged);
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
    pub fn forward_one_token(&mut self, token: u32, cache: &mut MirroredKvCache) -> Vec<i64> {
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
        cache.cpu_forward_one_token(model, token)
    }

    /// Exactly [`CachedIntegerModel::forward_shard_token`].
    pub fn forward_shard_token(
        &mut self,
        input: ShardInput,
        cache: &mut MirroredKvCache,
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
        cache: &mut MirroredKvCache,
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
                        return cache.cpu_forward_shard_token_with_history(
                            model,
                            ShardInput::Token(token_id),
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
        cache.cpu_forward_shard_token_with_history(
            model,
            input,
            start_layer,
            end_layer,
            position,
            generated_tokens,
        )
    }

    /// Exactly `rows.len()` calls of [`Self::forward_one_token`], in order,
    /// returning each call's logits. Up to [`MAX_ROWS`] tokens run in one GPU
    /// pass, every weight read once for all of them; a pass the GPU refuses
    /// runs on the CPU engine row by row. Provisional name: the GPU
    /// counterpart of `CachedIntegerModel::forward_rows_exact` in #182.
    pub fn forward_rows_exact(
        &mut self,
        rows: &[u32],
        cache: &mut MirroredKvCache,
    ) -> Vec<Vec<i64>> {
        let mut logits = Vec::with_capacity(rows.len());
        for chunk in rows.chunks(MAX_ROWS) {
            logits.extend(self.forward_rows_pass(chunk, cache));
        }
        logits
    }

    /// One pass of 1 to [`MAX_ROWS`] rows.
    fn forward_rows_pass(&mut self, rows: &[u32], cache: &mut MirroredKvCache) -> Vec<Vec<i64>> {
        let model = self.model;
        let cfg = &model.config;
        let d = cfg.d_model;
        let count = rows.len();
        // A token past the embedding returns no logits and leaves the cache
        // alone, which moves every later row: such rows run one at a time,
        // exactly as the single-row calls would.
        if rows
            .iter()
            .any(|&token| model.embedding_q16.len() < (token as usize + 1) * d)
        {
            return rows
                .iter()
                .map(|&token| self.forward_one_token(token, cache))
                .collect();
        }
        let pos = cache.seq_len;
        let layers = 0..cfg.n_layers;
        let shape = *self.decoder.shape();
        let fits = pos + count <= shape.kv_capacity && pos + count <= shape.max_seq;
        if fits && self.sync_mirror(cache, pos, layers.clone()) {
            let mut hidden = Vec::with_capacity(count * d);
            for &token in rows {
                let idx = (token as usize).min(cfg.vocab_size - 1);
                hidden.extend_from_slice(&model.embedding_q16[idx * d..(idx + 1) * d]);
            }
            match self
                .decoder
                .step_rows(&hidden, pos, layers.clone(), true, self.submission)
            {
                Ok(step) => {
                    self.commit_rows(cache, &step, layers, pos, count);
                    return step
                        .logits
                        .unwrap_or_default()
                        .chunks_exact(cfg.vocab_size)
                        .map(<[i64]>::to_vec)
                        .collect();
                }
                Err(refusal) => self.note_refusal(&refusal),
            }
        }
        self.stats.cpu_tokens += count as u64;
        rows.iter()
            .map(|&token| cache.cpu_forward_one_token(model, token))
            .collect()
    }

    /// Runs `k` consecutive positions through layers `[start_layer,
    /// end_layer)` in one pass over the weights: the GPU twin of #185's
    /// `CachedIntegerModel::forward_shard_rows`, with its checks, in its
    /// order, and its errors. The rows produce exactly the hidden states,
    /// raw logits and K/V rows that `k` one-row calls would. Up to
    /// [`MAX_ROWS`] rows run per GPU pass, and passes follow one another
    /// on the device; their rows reach `cache` only when every pass
    /// succeeded. Any refusal, including [`ShardRowsError::GpuRefused`],
    /// leaves `cache` unchanged.
    pub fn forward_shard_rows(
        &mut self,
        input: ShardRowsInput,
        cache: &mut MirroredKvCache,
        start_layer: usize,
        end_layer: usize,
        position: usize,
    ) -> Result<ShardRowsOutput, ShardRowsError> {
        let model = self.model;
        let cfg = &model.config;
        let d = cfg.d_model;
        let rows = match &input {
            ShardRowsInput::Tokens(tokens) => tokens.len(),
            ShardRowsInput::Hidden(states) => states.len(),
        };
        if rows == 0 {
            return Err(ShardRowsError::NoRows);
        }
        if !model.has_canonical_i8_profile() {
            return Err(ShardRowsError::NotCanonicalProfile);
        }
        if d == 0
            || cfg.d_head == 0
            || cfg.n_heads == 0
            || cfg.n_kv_heads == 0
            || cfg.d_kv == 0
            || cfg.vocab_size == 0
        {
            return Err(ShardRowsError::BadShape);
        }
        if start_layer >= end_layer
            || end_layer > cfg.n_layers
            || end_layer > model.layers.len()
            || end_layer > cache.k_data.len()
            || end_layer > cache.v_data.len()
        {
            return Err(ShardRowsError::BadLayerRange {
                start_layer,
                end_layer,
                n_layers: cfg.n_layers,
            });
        }
        // Every row must fit the RoPE table, as each one-row call requires.
        let last = position.saturating_add(rows - 1);
        if last >= cfg.max_seq {
            return Err(ShardForwardError::PositionOutOfRange {
                position: last,
                max_seq: cfg.max_seq,
            }
            .into());
        }
        for layer in start_layer..end_layer {
            let cached = cache.k_data[layer].len() / cfg.d_kv;
            if cached != position {
                return Err(ShardForwardError::KvCacheOutOfSync {
                    layer,
                    expected_positions: position,
                    cached_positions: cached,
                }
                .into());
            }
        }
        for layer in start_layer..end_layer {
            if !model.layers[layer].is_loaded() {
                return Err(ShardForwardError::LayerNotLoaded { layer }.into());
            }
        }
        let terminal = end_layer == cfg.n_layers;
        if terminal
            && (model.final_norm.len() != d
                || model.output_weight.n_rows != cfg.vocab_size
                || model.output_weight.n_cols != d
                || model.output_weight.scales.len() != cfg.vocab_size
                || model.output_weight.data.len() != cfg.vocab_size * d)
        {
            return Err(ShardRowsError::HeadNotLoaded);
        }
        let mut hidden = Vec::with_capacity(rows * d);
        match input {
            ShardRowsInput::Tokens(tokens) => {
                if start_layer != 0 {
                    return Err(ShardRowsError::WrongInput { start_layer });
                }
                for token in tokens {
                    // The same clamp as `forward_shard_token`.
                    let idx = (token as usize).min(cfg.vocab_size - 1);
                    let row = model
                        .embedding_q16
                        .get(idx * d..(idx + 1) * d)
                        .ok_or(ShardRowsError::TokenNotEmbedded { token })?;
                    hidden.extend_from_slice(row);
                }
            }
            ShardRowsInput::Hidden(states) => {
                if start_layer == 0 {
                    return Err(ShardRowsError::WrongInput { start_layer });
                }
                for state in &states {
                    if state.len() != d {
                        return Err(ShardForwardError::BadHiddenDim {
                            got: state.len(),
                            expected: d,
                        }
                        .into());
                    }
                    hidden.extend_from_slice(state);
                }
            }
        }

        let refused = |reason: String| ShardRowsError::GpuRefused { reason };
        let layers = start_layer..end_layer;
        let shape = *self.decoder.shape();
        if position + rows > shape.kv_capacity {
            return Err(refused(format!(
                "positions {position}..{} past the device KV cache of {}",
                position + rows,
                shape.kv_capacity
            )));
        }
        if !self.sync_mirror(cache, position, layers.clone()) {
            return Err(refused("the device cannot mirror this cache".to_string()));
        }
        // Every pass first; the device keeps each pass's K/V rows for the
        // next. Nothing reaches `cache` unless they all succeed.
        let mut steps = Vec::with_capacity(rows.div_ceil(MAX_ROWS));
        for (index, chunk) in hidden.chunks(MAX_ROWS * d).enumerate() {
            let at = position + index * MAX_ROWS;
            match self
                .decoder
                .step_rows(chunk, at, layers.clone(), terminal, self.submission)
            {
                Ok(step) => steps.push((at, chunk.len() / d, step)),
                Err(refusal) => {
                    self.note_refusal(&refusal);
                    return Err(refused(refusal.to_string()));
                }
            }
        }
        let width = if terminal { cfg.vocab_size } else { d };
        let mut out = Vec::with_capacity(rows);
        for (at, count, step) in steps {
            self.commit_rows(cache, &step, layers.clone(), at, count);
            let values = if terminal {
                step.logits.unwrap_or_default()
            } else {
                step.hidden
            };
            out.extend(values.chunks_exact(width).map(<[i64]>::to_vec));
        }
        Ok(if terminal {
            ShardRowsOutput::Logits(out)
        } else {
            ShardRowsOutput::Hidden(out)
        })
    }

    /// Keeps the first `keep` positions of `cache` and drops the rest, as
    /// #185's `CachedIntegerModel::rollback_rows` does on a stage holder
    /// (with its checks and errors): the K/V rows of rejected drafts go and
    /// every earlier row stays byte for byte. Keeping as many positions as
    /// the cache holds is a no-op. The device keeps its copy of the kept rows.
    pub fn rollback_rows(
        &self,
        cache: &mut MirroredKvCache,
        keep: usize,
    ) -> Result<(), ShardRowsError> {
        let held = cache.seq_len;
        if keep > held {
            return Err(ShardRowsError::RollbackGrows {
                keep,
                cached_positions: held,
            });
        }
        let width = self.model.config.d_kv;
        let expected = held.saturating_mul(width);
        if cache.k_data.len() != cache.v_data.len() {
            return Err(ShardRowsError::RaggedCache {
                layer: cache.k_data.len().min(cache.v_data.len()),
                values: 0,
                expected,
            });
        }
        for (layer, (keys, values)) in cache.k_data.iter().zip(&cache.v_data).enumerate() {
            for held_values in [keys.len(), values.len()] {
                if held_values != 0 && held_values != expected {
                    return Err(ShardRowsError::RaggedCache {
                        layer,
                        values: held_values,
                        expected,
                    });
                }
            }
        }
        cache.keep_rows(keep, width);
        Ok(())
    }

    /// Append a multi-row step's KV rows to the caller's cache, as the
    /// single-row calls would, one row after another.
    fn commit_rows(
        &mut self,
        cache: &mut MirroredKvCache,
        step: &DecoderStep,
        layers: Range<usize>,
        pos: usize,
        count: usize,
    ) {
        for ((layer, k), v) in layers.zip(&step.k_rows).zip(&step.v_rows) {
            cache.cache.push_k(layer, k);
            cache.cache.push_v(layer, v);
            // The appended rows are the device's own rows.
            self.mirrored[layer] = pos + count;
            if let Some(mark) = cache.dirty_from.get_mut(layer) {
                *mark = pos + count;
            }
        }
        cache.cache.seq_len = pos + count;
        self.stats.gpu_tokens += count as u64;
        self.stats.multi_row_passes += 1;
        self.last_gpu_seconds = step.gpu_seconds;
    }

    /// Append the step's KV rows to the caller's cache, as the CPU does.
    fn commit(
        &mut self,
        cache: &mut MirroredKvCache,
        step: &DecoderStep,
        layers: Range<usize>,
        pos: usize,
    ) {
        for ((layer, k), v) in layers.zip(&step.k_rows).zip(&step.v_rows) {
            cache.cache.push_k(layer, k);
            cache.cache.push_v(layer, v);
            // The appended row is the device's own row: both copies agree on
            // one more row.
            self.mirrored[layer] = pos + 1;
            if let Some(mark) = cache.dirty_from.get_mut(layer) {
                *mark = pos + 1;
            }
        }
        cache.cache.seq_len = pos + 1;
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

    /// Bring the device KV cache of `layers` up to date with `cache`. Returns
    /// false when the GPU cannot run position `pos` from this cache; the CPU
    /// then computes the token.
    ///
    /// Rows below both this decoder's count and the cache's mark are equal on
    /// both sides (see the module documentation); the rest are uploaded.
    fn sync_mirror(
        &mut self,
        cache: &mut MirroredKvCache,
        pos: usize,
        layers: Range<usize>,
    ) -> bool {
        let shape = *self.decoder.shape();
        if pos >= shape.kv_capacity || pos >= shape.max_seq {
            return false;
        }
        let d_kv = shape.d_kv;
        if self.mirror_of != Some(cache.id) || cache.synced_by != Some(self.id) {
            // Another cache, or another decoder synced this one since: no
            // device row counts.
            self.mirrored.fill(0);
            cache.dirty_from.fill(0);
            self.mirror_of = Some(cache.id);
            cache.synced_by = Some(self.id);
        }
        for layer in layers {
            let (Some(keys), Some(values), Some(&dirty)) = (
                cache.cache.k_data.get(layer),
                cache.cache.v_data.get(layer),
                cache.dirty_from.get(layer),
            ) else {
                return false;
            };
            if keys.len() != pos * d_kv || values.len() != pos * d_kv {
                return false;
            }
            let clean = self.mirrored[layer].min(dirty).min(pos);
            if !self.upload_rows(layer, clean..pos, keys, values) {
                return false;
            }
            if self.verify_mirror && !self.verify_rows(layer, pos, keys, values) {
                return false;
            }
            self.mirrored[layer] = pos;
            cache.dirty_from[layer] = pos;
        }
        true
    }

    /// Copy rows `rows` of one layer from the cache to the device.
    fn upload_rows(
        &mut self,
        layer: usize,
        rows: Range<usize>,
        keys: &[i64],
        values: &[i64],
    ) -> bool {
        let d_kv = self.decoder.shape().d_kv;
        for p in rows {
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
        true
    }

    /// Compare the first `pos` device rows of one layer with the cache, and
    /// upload the rows from the first difference on, counting them as
    /// mismatches.
    fn verify_rows(&mut self, layer: usize, pos: usize, keys: &[i64], values: &[i64]) -> bool {
        let Ok(equal) = self.decoder.kv_rows_matching(layer, keys, values) else {
            return false;
        };
        self.stats.kv_rows_verified += pos as u64;
        if equal < pos {
            self.stats.mirror_mismatches += (pos - equal) as u64;
            return self.upload_rows(layer, equal..pos, keys, values);
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
    let mut gpu_cache = MirroredKvCache::new(shape.n_layers);
    let mut cpu_logits = Vec::with_capacity(tokens.len());
    for (index, &token) in tokens.iter().enumerate() {
        let want = model.forward_one_token(token, &mut cpu_cache);
        let got = gpu.forward_one_token(token, &mut gpu_cache);
        if got != want {
            return Err(format!(
                "self-test token {index}: GPU logits differ from the CPU"
            ));
        }
        cpu_logits.push(want);
    }
    if gpu_cache.k_data != cpu_cache.k_data
        || gpu_cache.v_data != cpu_cache.v_data
        || gpu_cache.seq_len != cpu_cache.seq_len
    {
        return Err("self-test: the GPU KV cache differs from the CPU".to_string());
    }
    // The same tokens as one multi-row pass.
    let mut rows_cache = MirroredKvCache::new(shape.n_layers);
    if gpu.forward_rows_exact(&tokens, &mut rows_cache) != cpu_logits
        || rows_cache.k_data != cpu_cache.k_data
        || rows_cache.v_data != cpu_cache.v_data
        || gpu.stats().multi_row_passes != 1
    {
        return Err(format!(
            "self-test: a multi-row pass differs from the CPU ({:?})",
            gpu.stats()
        ));
    }
    if gpu.stats().gpu_tokens != 2 * tokens.len() as u64 {
        return Err(format!(
            "self-test: in-domain tokens fell back to the CPU ({:?})",
            gpu.stats()
        ));
    }
    // A two-way shard split, token after token, with a penalty history.
    let mut cpu_cache = KVCache::new(shape.n_layers);
    let mut gpu_cache = MirroredKvCache::new(shape.n_layers);
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
    if gpu.stats().mirror_mismatches != 0 {
        return Err(format!(
            "self-test: the device KV copy missed a change ({:?})",
            gpu.stats()
        ));
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
        let (mut cpu_cache, mut gpu_cache) = (KVCache::new(3), MirroredKvCache::new(3));
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
        assert_eq!(stats.mirror_mismatches, 0, "{stats:?}");
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
        let mut reference = KVCache::new(3);
        let mut mixed = MirroredKvCache::new(3);
        for (index, token) in [3u32, 9, 27, 81, 243, 2, 6, 18].into_iter().enumerate() {
            let want = model.forward_one_token(token, &mut reference);
            let got = if index % 3 == 1 {
                mixed.cpu_forward_one_token(&model, token)
            } else {
                gpu.forward_one_token(token, &mut mixed)
            };
            assert_eq!(got, want, "token {index}");
        }
        assert_eq!(mixed.k_data, reference.k_data);
        assert_eq!(mixed.v_data, reference.v_data);
        assert!(gpu.stats().kv_rows_uploaded > 0, "{:?}", gpu.stats());

        /// The same CPU tokens into a wrapped cache and a plain one.
        fn filled(model: &CachedIntegerModel, tokens: &[u32]) -> (MirroredKvCache, KVCache) {
            let mut wrapped = MirroredKvCache::new(model.config.n_layers);
            let mut plain = KVCache::new(model.config.n_layers);
            for &token in tokens {
                wrapped.cpu_forward_one_token(model, token);
                model.forward_one_token(token, &mut plain);
            }
            (wrapped, plain)
        }
        let before = gpu.stats();
        // Switch to a different cache whose row at the device's last mirrored
        // position (6) comes from the mixed session's token there. At layer 0
        // that row is then identical while rows 0 to 5 differ, so a check of
        // the last row alone would keep them (review ARC-76, finding 3).
        let (mut other, mut other_reference) = filled(&model, &[5, 4, 3, 2, 1, 0, 6, 7]);
        let want = model.forward_one_token(11, &mut other_reference);
        assert_eq!(gpu.forward_one_token(11, &mut other), want);
        assert_eq!(other.k_data, other_reference.k_data);
        assert_eq!(other.v_data, other_reference.v_data);

        // Another cache that shares rows 0 to 5 with the one just mirrored.
        let (mut other, mut other_reference) = filled(&model, &[5, 4, 3, 2, 1, 0, 8, 7]);
        let want = model.forward_one_token(12, &mut other_reference);
        assert_eq!(gpu.forward_one_token(12, &mut other), want);
        assert_eq!(other.k_data, other_reference.k_data);
        assert_eq!(other.v_data, other_reference.v_data);

        // Rows changed in place, through the wrapper's escape hatch, in the
        // cache the device holds: row 3 of every layer becomes the mixed
        // session's row 3.
        let row = 3 * model.config.d_kv..4 * model.config.d_kv;
        let replace = |cache: &mut KVCache| {
            let layers = cache.k_data.iter_mut().zip(cache.v_data.iter_mut());
            for ((keys, values), (from_keys, from_values)) in
                layers.zip(mixed.k_data.iter().zip(&mixed.v_data))
            {
                keys[row.clone()].copy_from_slice(&from_keys[row.clone()]);
                values[row.clone()].copy_from_slice(&from_values[row.clone()]);
            }
        };
        other.edit(replace);
        replace(&mut other_reference);
        let want = model.forward_one_token(13, &mut other_reference);
        assert_eq!(gpu.forward_one_token(13, &mut other), want);
        assert_eq!(other.k_data, other_reference.k_data);
        assert_eq!(other.v_data, other_reference.v_data);

        // All three ran on the GPU, so none of these answers is a CPU
        // fallback, and the full compare of every mirrored row found none
        // stale.
        let after = gpu.stats();
        assert_eq!(
            (
                after.gpu_tokens - before.gpu_tokens,
                after.cpu_tokens - before.cpu_tokens,
                after.mirror_mismatches
            ),
            (3, 0, 0),
            "{before:?} {after:?}"
        );
        assert!(
            after.kv_rows_verified > before.kv_rows_verified,
            "{after:?}"
        );
    }

    /// On one cache the mirror uploads only rows that changed: none for GPU
    /// tokens, one per layer for a CPU token, none after a truncation, and
    /// every row after an edit or after another decoder synced the cache.
    #[test]
    fn the_mirror_uploads_only_rows_that_changed() {
        let _guard = kernel_switch_guard();
        let shape = small_shape();
        let model = synthetic_model(0x5EED_0000_0000_000B, shape);
        let mut gpu = MetalForward::new(&model, shape.max_seq).expect("GPU decoder");
        let mut second = MetalForward::new(&model, shape.max_seq).expect("GPU decoder");
        let layers = shape.n_layers as u64;
        let d_kv = model.config.d_kv;
        let mut reference = KVCache::new(shape.n_layers);
        let mut cache = MirroredKvCache::new(shape.n_layers);

        /// Run `token` on `gpu` and on the CPU; the rows `gpu` uploaded.
        fn uploads(
            gpu: &mut MetalForward<'_>,
            model: &CachedIntegerModel,
            cache: &mut MirroredKvCache,
            reference: &mut KVCache,
            token: u32,
        ) -> u64 {
            let before = gpu.stats();
            let want = model.forward_one_token(token, reference);
            assert_eq!(gpu.forward_one_token(token, cache), want, "token {token}");
            let after = gpu.stats();
            assert_eq!(
                after.gpu_tokens,
                before.gpu_tokens + 1,
                "token {token}: {after:?}"
            );
            after.kv_rows_uploaded - before.kv_rows_uploaded
        }

        // GPU tokens on one cache (positions 0 to 3): nothing to upload.
        for token in [1u32, 2, 3, 4] {
            assert_eq!(
                uploads(&mut gpu, &model, &mut cache, &mut reference, token),
                0
            );
        }
        // A CPU token at position 4: its one row per layer.
        let want = model.forward_one_token(5, &mut reference);
        assert_eq!(cache.cpu_forward_one_token(&model, 5), want);
        assert_eq!(
            uploads(&mut gpu, &model, &mut cache, &mut reference, 6),
            layers
        );
        // Back to 3 positions: the rows kept are still on the device.
        cache.truncate(3);
        for (keys, values) in reference.k_data.iter_mut().zip(&mut reference.v_data) {
            keys.truncate(3 * d_kv);
            values.truncate(3 * d_kv);
        }
        reference.seq_len = 3;
        assert_eq!(uploads(&mut gpu, &model, &mut cache, &mut reference, 7), 0);
        // Any other change: every row (4 positions) again.
        cache.edit(|c| c.k_data[0][0] = c.k_data[0][0].wrapping_add(1));
        reference.k_data[0][0] = reference.k_data[0][0].wrapping_add(1);
        assert_eq!(
            uploads(&mut gpu, &model, &mut cache, &mut reference, 8),
            layers * 4
        );
        // Another decoder syncs this cache, then the first one again: each
        // uploads every row (5, then 6 positions).
        assert_eq!(
            uploads(&mut second, &model, &mut cache, &mut reference, 9),
            layers * 5
        );
        assert_eq!(
            uploads(&mut gpu, &model, &mut cache, &mut reference, 10),
            layers * 6
        );
        assert_eq!(cache.k_data, reference.k_data);
        assert_eq!(cache.v_data, reference.v_data);
        assert_eq!(gpu.stats().mirror_mismatches, 0, "{:?}", gpu.stats());
        assert_eq!(second.stats().mirror_mismatches, 0, "{:?}", second.stats());

        // A change that escapes the marks (written past the wrapper, which
        // only this module can do) is found by the full compare and repaired:
        // layer 1 from row 1 on, 6 of its 7 rows.
        cache.cache.k_data[1][d_kv] = cache.cache.k_data[1][d_kv].wrapping_add(1);
        reference.k_data[1][d_kv] = reference.k_data[1][d_kv].wrapping_add(1);
        assert_eq!(uploads(&mut gpu, &model, &mut cache, &mut reference, 11), 6);
        assert_eq!(gpu.stats().mirror_mismatches, 6, "{:?}", gpu.stats());
        assert_eq!(cache.k_data, reference.k_data);
    }

    #[test]
    fn shard_steps_match_the_cpu_shards() {
        let _guard = kernel_switch_guard();
        let shape = small_shape();
        let model = synthetic_model(0x5EED_0000_0000_0008, shape);
        let mut gpu = MetalForward::new(&model, shape.max_seq).expect("GPU decoder");
        let (mut cpu_cache, mut gpu_cache) = (KVCache::new(3), MirroredKvCache::new(3));
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
        assert_eq!(gpu.stats().mirror_mismatches, 0, "{:?}", gpu.stats());
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
        let (mut cpu_cache, mut gpu_cache) = (KVCache::new(3), MirroredKvCache::new(3));
        for token in [1u32, 2, 3, 4] {
            let want = model.forward_one_token(token, &mut cpu_cache);
            assert_eq!(gpu.forward_one_token(token, &mut gpu_cache), want);
        }
        assert_eq!(gpu_cache.k_data, cpu_cache.k_data);
        let stats = gpu.stats();
        assert_eq!((stats.gpu_tokens, stats.cpu_tokens), (0, 4), "{stats:?}");
        assert_eq!(stats.mirror_mismatches, 0, "{stats:?}");
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
        let (mut cpu_cache, mut gpu_cache) = (KVCache::new(2), MirroredKvCache::new(2));
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
        let stats = gpu.stats();
        assert_eq!(
            (stats.gpu_tokens, stats.mirror_mismatches),
            (3, 0),
            "{stats:?}"
        );
    }

    // ---- multi-row passes ------------------------------------------------

    /// `small_shape` with room for several multi-row passes.
    fn rows_shape() -> SyntheticShape {
        SyntheticShape {
            max_seq: 40,
            ..small_shape()
        }
    }

    /// A shard output as plain data, to compare: the hidden state, or the
    /// token id and the logits hash.
    type Plain = (Vec<i64>, Option<(u32, [u8; 32])>);

    fn plain(output: &ShardOutput) -> Plain {
        match output {
            ShardOutput::Hidden(hidden) => (hidden.clone(), None),
            ShardOutput::Token { id, logits_hash } => (Vec::new(), Some((*id, logits_hash.0))),
        }
    }

    /// The next stage's input for a stage's output.
    fn next_input(output: &ShardOutput) -> ShardInput {
        match output {
            ShardOutput::Hidden(hidden) => ShardInput::Hidden(hidden.clone()),
            ShardOutput::Token { .. } => ShardInput::Token(0),
        }
    }

    /// Every stage's outputs for `tokens` at `position..`, one row at a time
    /// on the CPU (all rows through a stage before the next, which changes
    /// nothing: a layer's rows depend only on that layer's inputs).
    fn cpu_stage_rows(
        model: &CachedIntegerModel,
        cache: &mut KVCache,
        stages: &[(usize, usize)],
        tokens: &[u32],
        histories: &[Vec<u32>],
        position: usize,
    ) -> Vec<Vec<Plain>> {
        let mut inputs: Vec<ShardInput> = tokens.iter().map(|&t| ShardInput::Token(t)).collect();
        let mut per_stage = Vec::new();
        for &(start, end) in stages {
            let outputs: Vec<ShardOutput> = inputs
                .into_iter()
                .zip(histories)
                .enumerate()
                .map(|(r, (input, history))| {
                    model
                        .forward_shard_token_with_history(
                            input,
                            cache,
                            start,
                            end,
                            position + r,
                            history,
                        )
                        .expect("CPU stage")
                })
                .collect();
            inputs = outputs.iter().map(next_input).collect();
            per_stage.push(outputs.iter().map(plain).collect());
        }
        per_stage
    }

    /// The same stages as multi-row GPU calls ([`MetalForward::forward_shard_rows`]).
    /// The last stage returns raw logits; each row then gets the selection the
    /// one-row call makes, with its own history, so the two compare as plain
    /// data.
    fn gpu_stage_rows(
        gpu: &mut MetalForward<'_>,
        cache: &mut MirroredKvCache,
        stages: &[(usize, usize)],
        tokens: &[u32],
        histories: &[Vec<u32>],
        position: usize,
    ) -> Vec<Vec<Plain>> {
        let mut input = ShardRowsInput::Tokens(tokens.to_vec());
        let mut per_stage = Vec::new();
        for &(start, end) in stages {
            match gpu
                .forward_shard_rows(input, cache, start, end, position)
                .expect("GPU stage")
            {
                ShardRowsOutput::Hidden(rows) => {
                    per_stage.push(rows.iter().map(|row| (row.clone(), None)).collect());
                    input = ShardRowsInput::Hidden(rows);
                }
                ShardRowsOutput::Logits(rows) => {
                    per_stage.push(
                        rows.into_iter()
                            .zip(histories)
                            .map(|(mut logits, history)| {
                                let id =
                                    select_next_token_with_repetition_penalty(&mut logits, history);
                                let bytes: Vec<u8> =
                                    logits.iter().flat_map(|v| v.to_le_bytes()).collect();
                                (Vec::new(), Some((id, arc_crypto::hash_bytes(&bytes).0)))
                            })
                            .collect(),
                    );
                    input = ShardRowsInput::Tokens(Vec::new());
                }
            }
        }
        per_stage
    }

    /// A k-row pass equals k single-row CPU passes: logits, every KV row and
    /// `seq_len`, for k in {1, 2, 3, 4, 8}, after single-row tokens and
    /// twice in a row.
    #[test]
    fn multi_row_passes_match_single_row_cpu_passes() {
        let _guard = kernel_switch_guard();
        let shape = rows_shape();
        let model = synthetic_model(0x5EED_0000_0000_000C, shape);
        let mut gpu = MetalForward::new(&model, shape.max_seq).expect("GPU decoder");
        for k in [1usize, 2, 3, 4, 8] {
            let before = gpu.stats();
            let mut reference = KVCache::new(shape.n_layers);
            let mut cache = MirroredKvCache::new(shape.n_layers);
            for token in [7u32, 70, 170] {
                let want = model.forward_one_token(token, &mut reference);
                assert_eq!(gpu.forward_one_token(token, &mut cache), want, "k {k}");
            }
            for pass in 0..2u32 {
                let rows: Vec<u32> = (0..k as u32)
                    .map(|i| (pass * 97 + i * 31 + 11) % 280)
                    .collect();
                let want: Vec<Vec<i64>> = rows
                    .iter()
                    .map(|&t| model.forward_one_token(t, &mut reference))
                    .collect();
                assert_eq!(
                    gpu.forward_rows_exact(&rows, &mut cache),
                    want,
                    "k {k}, pass {pass}"
                );
            }
            assert_eq!(cache.k_data, reference.k_data, "k {k}");
            assert_eq!(cache.v_data, reference.v_data, "k {k}");
            assert_eq!(cache.seq_len, reference.seq_len, "k {k}");
            let after = gpu.stats();
            assert_eq!(
                (
                    after.gpu_tokens - before.gpu_tokens,
                    after.cpu_tokens - before.cpu_tokens,
                    after.multi_row_passes - before.multi_row_passes,
                    after.mirror_mismatches,
                ),
                (3 + 2 * k as u64, 0, 2, 0),
                "k {k}: {after:?}"
            );
        }
    }

    /// Speculative rollback on the whole model: of a 4-row pass the first 2
    /// rows are accepted, `truncate` drops the other 2, and a 3-row pass and
    /// a single row continue. Everything equals the CPU forwarding only the
    /// accepted rows.
    #[test]
    fn rejected_rows_truncate_and_the_sequence_continues() {
        let _guard = kernel_switch_guard();
        let shape = rows_shape();
        let model = synthetic_model(0x5EED_0000_0000_000D, shape);
        let mut gpu = MetalForward::new(&model, shape.max_seq).expect("GPU decoder");
        let mut reference = KVCache::new(shape.n_layers);
        let mut cache = MirroredKvCache::new(shape.n_layers);
        let prefix = [3u32, 141, 59];
        let want: Vec<Vec<i64>> = prefix
            .iter()
            .map(|&t| model.forward_one_token(t, &mut reference))
            .collect();
        assert_eq!(gpu.forward_rows_exact(&prefix, &mut cache), want);
        let draft = [26u32, 53, 58, 97];
        let got = gpu.forward_rows_exact(&draft, &mut cache);
        let accepted: Vec<Vec<i64>> = draft[..2]
            .iter()
            .map(|&t| model.forward_one_token(t, &mut reference))
            .collect();
        assert_eq!(got[..2], accepted[..]);
        cache.truncate(prefix.len() + 2);
        assert_eq!(cache.k_data, reference.k_data);
        assert_eq!(cache.v_data, reference.v_data);
        assert_eq!(cache.seq_len, reference.seq_len);
        let next = [23u32, 84, 62];
        let want: Vec<Vec<i64>> = next
            .iter()
            .map(|&t| model.forward_one_token(t, &mut reference))
            .collect();
        assert_eq!(gpu.forward_rows_exact(&next, &mut cache), want);
        let want = model.forward_one_token(64, &mut reference);
        assert_eq!(gpu.forward_one_token(64, &mut cache), want);
        assert_eq!(cache.k_data, reference.k_data);
        assert_eq!(cache.v_data, reference.v_data);
        let stats = gpu.stats();
        assert_eq!(
            (
                stats.gpu_tokens,
                stats.cpu_tokens,
                stats.multi_row_passes,
                stats.mirror_mismatches,
            ),
            (3 + 4 + 3 + 1, 0, 3, 0),
            "{stats:?}"
        );
    }

    /// The stage variant ([`MetalForward::forward_shard_rows`]) on 1-, 2- and
    /// 3-way splits, k in {1, 2, 3, 4, 8}, two passes each: equal to one-row
    /// CPU stage calls at every boundary (hidden states between stages; the
    /// token and logits hash each row's raw logits give at the end) and in
    /// every KV row. Then a rollback: 2 of 4 drafted rows accepted,
    /// [`MetalForward::rollback_rows`], and a 3-row pass continues.
    #[test]
    fn multi_row_shard_steps_match_single_row_cpu_shards() {
        let _guard = kernel_switch_guard();
        let shape = rows_shape();
        let model = synthetic_model(0x5EED_0000_0000_000E, shape);
        let mut gpu = MetalForward::new(&model, shape.max_seq).expect("GPU decoder");
        let splits: [&[(usize, usize)]; 3] =
            [&[(0, 3)], &[(0, 1), (1, 3)], &[(0, 1), (1, 2), (2, 3)]];
        for stages in splits {
            for k in [1usize, 2, 3, 4, 8] {
                let before = gpu.stats();
                let mut reference = KVCache::new(shape.n_layers);
                let mut cache = MirroredKvCache::new(shape.n_layers);
                let mut generated: Vec<u32> = Vec::new();
                for pass in 0..2u32 {
                    let tokens: Vec<u32> = (0..k as u32)
                        .map(|i| (pass * 89 + i * 41 + 5) % 280)
                        .collect();
                    let histories: Vec<Vec<u32>> = (0..k)
                        .map(|r| [generated.as_slice(), &tokens[..r]].concat())
                        .collect();
                    let position = generated.len();
                    let want = cpu_stage_rows(
                        &model,
                        &mut reference,
                        stages,
                        &tokens,
                        &histories,
                        position,
                    );
                    let got =
                        gpu_stage_rows(&mut gpu, &mut cache, stages, &tokens, &histories, position);
                    assert_eq!(got, want, "{stages:?}, k {k}, pass {pass}");
                    generated.extend(&tokens);
                }
                assert_eq!(cache.k_data, reference.k_data, "{stages:?}, k {k}");
                assert_eq!(cache.v_data, reference.v_data, "{stages:?}, k {k}");
                let after = gpu.stats();
                let calls = 2 * stages.len() as u64;
                assert_eq!(
                    (
                        after.gpu_tokens - before.gpu_tokens,
                        after.cpu_tokens - before.cpu_tokens,
                        after.multi_row_passes - before.multi_row_passes,
                        after.mirror_mismatches,
                    ),
                    (calls * k as u64, 0, calls, 0),
                    "{stages:?}, k {k}: {after:?}"
                );
            }
        }

        // Rollback through two stages.
        let stages = [(0usize, 1usize), (1, 3)];
        let mut reference = KVCache::new(shape.n_layers);
        let mut cache = MirroredKvCache::new(shape.n_layers);
        let prefix = [9u32, 99, 199];
        let histories: Vec<Vec<u32>> = (0..3).map(|r| prefix[..r].to_vec()).collect();
        let want = cpu_stage_rows(&model, &mut reference, &stages, &prefix, &histories, 0);
        let got = gpu_stage_rows(&mut gpu, &mut cache, &stages, &prefix, &histories, 0);
        assert_eq!(got, want);
        let draft = [17u32, 34, 51, 68];
        let histories: Vec<Vec<u32>> = (0..4)
            .map(|r| [prefix.as_slice(), &draft[..r]].concat())
            .collect();
        let got = gpu_stage_rows(&mut gpu, &mut cache, &stages, &draft, &histories, 3);
        let want = cpu_stage_rows(
            &model,
            &mut reference,
            &stages,
            &draft[..2],
            &histories[..2],
            3,
        );
        let accepted: Vec<Vec<_>> = got.iter().map(|stage| stage[..2].to_vec()).collect();
        assert_eq!(accepted, want);
        gpu.rollback_rows(&mut cache, 5).expect("rollback");
        assert_eq!(cache.k_data, reference.k_data);
        assert_eq!(cache.seq_len, 5);
        let next = [85u32, 102, 119];
        let history: Vec<u32> = [prefix.as_slice(), &draft[..2]].concat();
        let histories: Vec<Vec<u32>> = (0..3)
            .map(|r| [history.as_slice(), &next[..r]].concat())
            .collect();
        let want = cpu_stage_rows(&model, &mut reference, &stages, &next, &histories, 5);
        let got = gpu_stage_rows(&mut gpu, &mut cache, &stages, &next, &histories, 5);
        assert_eq!(got, want);
        assert_eq!(cache.k_data, reference.k_data);
        assert_eq!(cache.v_data, reference.v_data);
        let stats = gpu.stats();
        assert_eq!(
            (stats.cpu_tokens, stats.mirror_mismatches),
            (0, 0),
            "{stats:?}"
        );
    }

    /// The stage call refuses what #185's CPU call refuses, with the same
    /// errors in the same order, and a refusal leaves the cache unchanged; so
    /// does a rollback that would grow the cache.
    #[test]
    fn multi_row_shard_calls_refuse_like_the_cpu_stage_call() {
        let _guard = kernel_switch_guard();
        let shape = rows_shape();
        let model = synthetic_model(0x5EED_0000_0000_0011, shape);
        let mut gpu = MetalForward::new(&model, shape.max_seq).expect("GPU decoder");
        let mut cache = MirroredKvCache::new(shape.n_layers);
        let first = gpu
            .forward_shard_rows(ShardRowsInput::Tokens(vec![1, 2]), &mut cache, 0, 1, 0)
            .expect("stage 0");
        let ShardRowsOutput::Hidden(hidden) = first else {
            panic!("the first stage returns hidden rows");
        };
        gpu.forward_shard_rows(ShardRowsInput::Hidden(hidden.clone()), &mut cache, 1, 3, 0)
            .expect("stage 1");
        let (keys, values, seq_len) = (cache.k_data.clone(), cache.v_data.clone(), cache.seq_len);
        let cases = [
            (
                ShardRowsInput::Tokens(vec![]),
                0,
                1,
                2,
                ShardRowsError::NoRows,
            ),
            (
                ShardRowsInput::Tokens(vec![3]),
                1,
                1,
                2,
                ShardRowsError::BadLayerRange {
                    start_layer: 1,
                    end_layer: 1,
                    n_layers: 3,
                },
            ),
            (
                ShardRowsInput::Tokens(vec![3]),
                0,
                4,
                2,
                ShardRowsError::BadLayerRange {
                    start_layer: 0,
                    end_layer: 4,
                    n_layers: 3,
                },
            ),
            (
                ShardRowsInput::Tokens(vec![3; 39]),
                0,
                1,
                2,
                ShardRowsError::Shard(ShardForwardError::PositionOutOfRange {
                    position: 40,
                    max_seq: 40,
                }),
            ),
            (
                ShardRowsInput::Tokens(vec![3]),
                0,
                1,
                1,
                ShardRowsError::Shard(ShardForwardError::KvCacheOutOfSync {
                    layer: 0,
                    expected_positions: 1,
                    cached_positions: 2,
                }),
            ),
            (
                ShardRowsInput::Tokens(vec![3]),
                1,
                3,
                2,
                ShardRowsError::WrongInput { start_layer: 1 },
            ),
            (
                ShardRowsInput::Hidden(vec![hidden[0].clone()]),
                0,
                1,
                2,
                ShardRowsError::WrongInput { start_layer: 0 },
            ),
            (
                ShardRowsInput::Hidden(vec![vec![0; 3]]),
                1,
                3,
                2,
                ShardRowsError::Shard(ShardForwardError::BadHiddenDim {
                    got: 3,
                    expected: 64,
                }),
            ),
        ];
        for (input, start, end, position, want) in cases {
            let kind = want.kind();
            assert_eq!(
                gpu.forward_shard_rows(input, &mut cache, start, end, position),
                Err(want),
                "{kind}"
            );
            assert_eq!(cache.k_data, keys, "{kind}: a refusal changed the cache");
            assert_eq!(cache.v_data, values, "{kind}: a refusal changed the cache");
            assert_eq!(
                cache.seq_len, seq_len,
                "{kind}: a refusal changed the cache"
            );
        }
        assert_eq!(
            gpu.rollback_rows(&mut cache, 3),
            Err(ShardRowsError::RollbackGrows {
                keep: 3,
                cached_positions: 2,
            })
        );
        gpu.rollback_rows(&mut cache, 2)
            .expect("keeping every position is a no-op");
        assert_eq!(cache.k_data, keys);
        assert_eq!(gpu.stats().cpu_tokens, 0, "{:?}", gpu.stats());
    }

    /// A pass the GPU refuses runs on the CPU engine row by row, with the
    /// same answer and the same cache.
    #[test]
    fn refused_rows_run_on_the_cpu_with_the_same_answer() {
        let _guard = kernel_switch_guard();
        let shape = rows_shape();
        let mut model = synthetic_model(0x5EED_0000_0000_000F, shape);
        // Gains of 2^40 push the normed vector far outside four digits.
        model.layers[1].ffn_norm = vec![1 << 40; shape.d_model];
        let mut gpu = MetalForward::new(&model, shape.max_seq).expect("GPU decoder");
        let mut reference = KVCache::new(shape.n_layers);
        let mut cache = MirroredKvCache::new(shape.n_layers);
        let rows = [1u32, 2, 3, 4, 5];
        let want: Vec<Vec<i64>> = rows
            .iter()
            .map(|&t| model.forward_one_token(t, &mut reference))
            .collect();
        assert_eq!(gpu.forward_rows_exact(&rows, &mut cache), want);
        assert_eq!(cache.k_data, reference.k_data);
        assert_eq!(cache.v_data, reference.v_data);
        let stats = gpu.stats();
        assert_eq!(
            (stats.gpu_tokens, stats.cpu_tokens, stats.multi_row_passes),
            (0, 5, 0),
            "{stats:?}"
        );
        assert_ne!(stats.refusal_bits, 0, "{stats:?}");
    }

    /// Real-width rows: two Llama-2-7B-width layers, 3 and then 8 rows in one
    /// pass each, against the CPU.
    #[test]
    fn llama_7b_width_rows_match_the_cpu() {
        let _guard = kernel_switch_guard();
        let shape = SyntheticShape {
            n_layers: 2,
            d_model: 4096,
            n_heads: 32,
            n_kv_heads: 32,
            d_ff: 11008,
            vocab: 4096,
            max_seq: 16,
            embedding_rows: 16,
        };
        let model = synthetic_model(0x5EED_0000_0000_0010, shape);
        let mut gpu = MetalForward::new(&model, shape.max_seq).expect("GPU decoder");
        let mut reference = KVCache::new(2);
        let mut cache = MirroredKvCache::new(2);
        let passes: [&[u32]; 2] = [&[3, 1, 4], &[1, 5, 9, 2, 6, 5, 3, 5]];
        for rows in passes {
            let want: Vec<Vec<i64>> = rows
                .iter()
                .map(|&t| model.forward_one_token(t, &mut reference))
                .collect();
            assert_eq!(gpu.forward_rows_exact(rows, &mut cache), want);
        }
        assert_eq!(cache.k_data, reference.k_data);
        assert_eq!(cache.v_data, reference.v_data);
        let stats = gpu.stats();
        assert_eq!(
            (
                stats.gpu_tokens,
                stats.cpu_tokens,
                stats.multi_row_passes,
                stats.mirror_mismatches,
            ),
            (11, 0, 2, 0),
            "{stats:?}"
        );
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

    /// Median wall time per token of `forward` over TOKENS tokens on the
    /// fresh cache `make` returns, after WARM_UP untimed tokens.
    fn per_token<C>(
        make: impl FnOnce() -> C,
        mut forward: impl FnMut(u32, &mut C) -> Vec<i64>,
    ) -> f64 {
        let mut cache = make();
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
            let plain = || KVCache::new(n_layers);
            let scalar = per_token(plain, |t, c| model.forward_one_token(t, c));
            points[0].push((n_layers, scalar));
            canonical_simd::set_fast_canonical_kernel(true);
            let simd = per_token(plain, |t, c| model.forward_one_token(t, c));
            points[1].push((n_layers, simd));
            canonical_simd::set_fast_canonical_kernel(false);
            {
                let metal = MetalModel::new(&model).expect("resident model");
                let _switch = SwitchGuard::set(true);
                let hooked = per_token(plain, |t, c| metal.run(|| model.forward_one_token(t, c)));
                points[2].push((n_layers, hooked));
            }
            let mut fused = MetalForward::new(&model, 16).expect("GPU decoder");
            fused.set_verify_mirror(false);
            let one_buffer = per_token(
                || MirroredKvCache::new(n_layers),
                |t, c| fused.forward_one_token(t, c),
            );
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

    /// Cached positions for the KV-mirror benchmark.
    const CONTEXTS: [usize; 3] = [128, 1024, 4096];

    /// How the device copy of the KV cache is checked before each token.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum MirrorCheck {
        /// Each layer's last mirrored row only, as at 0fc7dda7 (not exact).
        LastRow,
        /// Every mirrored row, as at 205c48ec.
        EveryRow,
        /// The wrapper's marks: only rows that changed (this change).
        Marks,
    }

    /// What the device-copy check costs per token as the context grows: the
    /// last-row check of 0fc7dda7 (emulated), the every-row compare of
    /// 205c48ec (the mirror check switched on), and the wrapper's marks.
    #[test]
    #[ignore = "benchmark: run explicitly in release with --ignored --nocapture"]
    fn kv_mirror_cost_by_context() {
        let _guard = kernel_switch_guard();
        metal_forward_self_test().expect("self-test");
        let device = metal_engine().expect("Metal device").report();
        // Two real-width Llama-2-7B layers (d_kv 4096: 64 KiB of K and V per
        // position and layer) and a small head, so the layers dominate.
        let max_seq = CONTEXTS[CONTEXTS.len() - 1] + 64;
        let shape = SyntheticShape {
            n_layers: 2,
            d_model: 4096,
            n_heads: 32,
            n_kv_heads: 32,
            d_ff: 11008,
            vocab: 4096,
            max_seq,
            embedding_rows: EMBEDDING_ROWS,
        };
        let model = synthetic_model(0x0007_B70C_E400_00C0, shape);
        let d_kv = model.config.d_kv;
        let mut fused = MetalForward::new(&model, max_seq).expect("GPU decoder");
        let mut rng = SynthRng(0x0007_B70C_E400_00C1);
        let checks = [
            MirrorCheck::LastRow,
            MirrorCheck::EveryRow,
            MirrorCheck::Marks,
        ];
        let mut results: Vec<(usize, Vec<f64>)> = Vec::new();
        for context in CONTEXTS {
            // `context` cached positions of KV values at a real cache's
            // magnitude (about 2^18).
            let mut random_rows = || -> Vec<Vec<i64>> {
                (0..shape.n_layers)
                    .map(|_| {
                        (0..context * d_kv)
                            .map(|_| rng.below(1 << 19) - (1 << 18))
                            .collect()
                    })
                    .collect()
            };
            let (keys, values) = (random_rows(), random_rows());
            let mut ms = Vec::with_capacity(checks.len());
            for check in checks {
                fused.set_verify_mirror(check == MirrorCheck::EveryRow);
                let mut cache = MirroredKvCache::from_cache(KVCache {
                    k_data: keys.clone(),
                    v_data: values.clone(),
                    seq_len: context,
                });
                let before = fused.stats();
                let mut samples = Vec::with_capacity(TOKENS);
                for step in 0..WARM_UP + TOKENS {
                    let token = (step % EMBEDDING_ROWS) as u32;
                    let start = Instant::now();
                    if check == MirrorCheck::LastRow {
                        let last = cache.seq_len - 1;
                        let row = last * d_kv..(last + 1) * d_kv;
                        for (layer, (k, v)) in cache.k_data.iter().zip(&cache.v_data).enumerate() {
                            let (device_k, device_v) =
                                fused.decoder.read_kv(layer, last).expect("device row");
                            let _ = std::hint::black_box(
                                device_k[..] == k[row.clone()] && device_v[..] == v[row.clone()],
                            );
                        }
                    }
                    let logits = fused.forward_one_token(token, &mut cache);
                    let seconds = start.elapsed().as_secs_f64();
                    assert!(!logits.is_empty());
                    if step >= WARM_UP {
                        samples.push(seconds);
                    }
                }
                let after = fused.stats();
                assert_eq!(
                    after.gpu_tokens - before.gpu_tokens,
                    (WARM_UP + TOKENS) as u64,
                    "{check:?} at {context}: {after:?}"
                );
                assert_eq!(after.mirror_mismatches, 0, "{after:?}");
                ms.push(median(samples) * 1e3);
            }
            results.push((context, ms));
        }

        let mut md = String::new();
        md.push_str("### KV-cache mirror cost by context\n\n");
        md.push_str(
            "Virtualized-runner measurement: GitHub-hosted macOS VM (Apple M1, virtual), \
             paravirtual Metal GPU. Not Apple GPU hardware numbers.\n\n",
        );
        md.push_str(&format!(
            "Device `{}`. Two real-width Llama-2-7B layers (d_kv 4096: 64 KiB of K and V per \
             cached position and layer) and a 4,096-token head, so the layers dominate. Each \
             time is the median of {TOKENS} tokens after {WARM_UP} warm-up tokens; the first \
             uploads the whole cache.\n\n",
            device.name,
        ));
        md.push_str(
            "| Cached positions | Last row only (as 0fc7dda7, not exact) | Every row (as \
             205c48ec) | Marks (this change) |\n|---|---|---|---|\n",
        );
        for (context, ms) in &results {
            let (last, every, marks) = (ms[0], ms[1], ms[2]);
            md.push_str(&format!(
                "| {context} | {last:.1} ms, {:.2} tok/s | {every:.1} ms, {:.2} tok/s, {:+.1}% | \
                 {marks:.1} ms, {:.2} tok/s, {:+.1}% |\n",
                1e3 / last,
                1e3 / every,
                100.0 * (every / last - 1.0),
                1e3 / marks,
                100.0 * (marks / last - 1.0),
            ));
        }
        md.push_str(
            "\nPercentages are against the last-row column. DERIVED for a 32-layer 7B token: \
             the every-row compare adds 16 times the two-layer difference per token.\n",
        );
        println!("{md}");
        let json = serde_json::json!({
            "label": "virtualized-runner measurement",
            "device": device,
            "layers": shape.n_layers,
            "d_kv": d_kv,
            "results": results
                .iter()
                .map(|(context, ms)| serde_json::json!({
                    "cached_positions": context,
                    "last_row_ms": ms[0],
                    "every_row_ms": ms[1],
                    "marks_ms": ms[2],
                }))
                .collect::<Vec<_>>(),
        });
        println!("METAL_KV_MIRROR_BENCH {json}");
        if let Ok(path) = std::env::var("ARC_METAL_KV_BENCH_MD") {
            std::fs::write(path, md).expect("write the benchmark summary");
        }
    }

    /// Rows per pass for the multi-row benchmark.
    const ROW_COUNTS: [usize; 4] = [1, 2, 4, 8];
    /// Cached positions before every measured pass.
    const ROWS_CONTEXT: usize = 128;

    /// What verifying k rows costs: one k-row pass against k single-row
    /// passes, from the same cached context (each measurement is rolled back
    /// with `truncate`, the speculative-decoding pattern).
    #[test]
    #[ignore = "benchmark: run explicitly in release with --ignored --nocapture"]
    fn multi_row_cost_by_k() {
        let _guard = kernel_switch_guard();
        metal_forward_self_test().expect("self-test");
        let device = metal_engine().expect("Metal device").report();
        let max_seq = ROWS_CONTEXT + 2 * MAX_ROWS + 16;
        let shape = SyntheticShape {
            n_layers: 2,
            d_model: 4096,
            n_heads: 32,
            n_kv_heads: 32,
            d_ff: 11008,
            vocab: 4096,
            max_seq,
            embedding_rows: EMBEDDING_ROWS,
        };
        let model = synthetic_model(0x0007_B70C_E400_00D0, shape);
        let d_kv = model.config.d_kv;
        let mut fused = MetalForward::new(&model, max_seq).expect("GPU decoder");
        fused.set_verify_mirror(false);
        let mut rng = SynthRng(0x0007_B70C_E400_00D1);
        let mut random_rows = || -> Vec<Vec<i64>> {
            (0..shape.n_layers)
                .map(|_| {
                    (0..ROWS_CONTEXT * d_kv)
                        .map(|_| rng.below(1 << 19) - (1 << 18))
                        .collect()
                })
                .collect()
        };
        let (keys, values) = (random_rows(), random_rows());
        let mut cache = MirroredKvCache::from_cache(KVCache {
            k_data: keys,
            v_data: values,
            seq_len: ROWS_CONTEXT,
        });
        let tokens: Vec<u32> = (0..MAX_ROWS).map(|t| (t % EMBEDDING_ROWS) as u32).collect();
        // (k, staged pass wall, staged pass GPU, unstaged pass wall, unstaged
        // pass GPU, k one-row passes wall), seconds.
        let mut results: Vec<(usize, [f64; 5])> = Vec::new();
        for k in ROW_COUNTS {
            let mut samples: [Vec<f64>; 5] = Default::default();
            for step in 0..WARM_UP + TOKENS {
                let single_start = Instant::now();
                let single: Vec<Vec<i64>> = tokens[..k]
                    .iter()
                    .map(|&token| fused.forward_one_token(token, &mut cache))
                    .collect();
                let single_wall = single_start.elapsed().as_secs_f64();
                cache.truncate(ROWS_CONTEXT);
                let mut times = [0.0; 5];
                times[4] = single_wall;
                for (slot, staged) in [(0usize, true), (2, false)] {
                    fused.set_staged_gemm(staged);
                    let start = Instant::now();
                    let rows = fused.forward_rows_exact(&tokens[..k], &mut cache);
                    times[slot] = start.elapsed().as_secs_f64();
                    times[slot + 1] = fused.last_gpu_seconds();
                    cache.truncate(ROWS_CONTEXT);
                    assert_eq!(
                        rows, single,
                        "k {k}, staged {staged}: the pass and the one-row passes differ"
                    );
                }
                fused.set_staged_gemm(false);
                if step >= WARM_UP {
                    for (series, time) in samples.iter_mut().zip(times) {
                        series.push(time);
                    }
                }
            }
            results.push((k, samples.map(median)));
        }
        let stats = fused.stats();
        assert_eq!(stats.cpu_tokens, 0, "{stats:?}");

        let one_row = results[0].1[4];
        let mut md = String::new();
        md.push_str("### Verifying k rows: one multi-row pass against k one-row passes\n\n");
        md.push_str(
            "Hosted-VM CI measurement: GitHub-hosted macOS VM (Apple M1, virtual), paravirtual \
             Metal GPU. Not Apple GPU hardware numbers.\n\n",
        );
        md.push_str(&format!(
            "Device `{}`. Two real-width Llama-2-7B layers (d_kv 4096) and a 4,096-token head, \
             {ROWS_CONTEXT} cached positions before every pass; each pass is rolled back with \
             `truncate`. Every time is the median of {TOKENS} passes after {WARM_UP} warm-up \
             passes, and every pass's logits equal the one-row passes'. Two multi-row \
             projections: digit planes staged in threadgroup memory, and read from device \
             memory by every simdgroup (the default).\n\n",
            device.name,
        ));
        md.push_str(
            "| k | Staged pass: wall ms (GPU ms) | Per row | Unstaged pass: wall ms (GPU ms) | \
             Per row | k one-row passes: wall ms | Per row | Staged pass / one one-row pass |\n\
             |---|---|---|---|---|---|---|---|\n",
        );
        for &(k, [staged, staged_gpu, unstaged, unstaged_gpu, singles]) in &results {
            let rows = k as f64;
            md.push_str(&format!(
                "| {k} | {:.1} ({:.1}) | {:.1} | {:.1} ({:.1}) | {:.1} | {:.1} | {:.1} | {:.2}x |\n",
                staged * 1e3,
                staged_gpu * 1e3,
                staged * 1e3 / rows,
                unstaged * 1e3,
                unstaged_gpu * 1e3,
                unstaged * 1e3 / rows,
                singles * 1e3,
                singles * 1e3 / rows,
                staged / one_row,
            ));
        }
        println!("{md}");
        let json = serde_json::json!({
            "label": "hosted-VM CI measurement",
            "device": device,
            "layers": shape.n_layers,
            "cached_positions": ROWS_CONTEXT,
            "results": results
                .iter()
                .map(|&(k, [staged, staged_gpu, unstaged, unstaged_gpu, singles])| serde_json::json!({
                    "rows": k,
                    "staged_pass_wall_ms": staged * 1e3,
                    "staged_pass_gpu_ms": staged_gpu * 1e3,
                    "unstaged_pass_wall_ms": unstaged * 1e3,
                    "unstaged_pass_gpu_ms": unstaged_gpu * 1e3,
                    "single_rows_wall_ms": singles * 1e3,
                    "staged_pass_over_one_row": staged / one_row,
                }))
                .collect::<Vec<_>>(),
        });
        println!("METAL_ROWS_BENCH {json}");
        if let Ok(path) = std::env::var("ARC_METAL_ROWS_BENCH_MD") {
            std::fs::write(path, md).expect("write the benchmark summary");
        }
    }
}
