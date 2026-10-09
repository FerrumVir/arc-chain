//! Lossless speculative decoding for whole-model integer generation.
//!
//! A [`Drafter`] guesses the next few tokens. The target model then forwards
//! the pending token and every guess as consecutive rows of one pass
//! ([`CachedIntegerModel::forward_rows_exact`]), chooses a token from each
//! row's logits exactly as plain decoding would, keeps the longest run of
//! guesses it agrees with plus the one token it chose itself, and deletes the
//! K/V rows of the guesses it rejected ([`KVCache::truncate`]). One pass emits
//! between one and `k + 1` tokens.
//!
//! # Why the output cannot change
//!
//! 1. **Rows are exact.** Row `j` of a multi-row pass has bit-identical logits
//!    and K/V bytes to `forward_one_token` called on the same tokens one at a
//!    time. The canonical profile has no cross-row reduction and no
//!    batch-dependent quantisation; `prefill_canonical_i8_batched` is the
//!    permitted strategy listed in the integer profile contract (§6), and its
//!    conformance tests compare every position, every chunk size, the KV bytes
//!    and a continuation decode. Anything else runs the rows one at a time.
//! 2. **Only correct rows are read.** The logits of row `j + 1` are used only
//!    when its input token equals the token the target itself chose from row
//!    `j`, so every logits vector that selects a token is one plain decoding
//!    computes at that step. Selection uses the same function and the same
//!    generated history, so it returns the same token.
//! 3. **Rejected rows leave no trace.** K/V rows are the only per-request
//!    state, they are append-only, and the rows of rejected guesses are cut
//!    off before the next pass. After every pass the cache holds exactly the
//!    rows plain decoding would hold (the tests compare the bytes).
//! 4. **Stopping is identical.** EOS and `max_tokens` are checked after every
//!    emitted token, in plain decoding's order, and a pass never drafts past
//!    the remaining budget or the admitted context window.
//!
//! A drafter therefore decides only how many rows a pass carries, never a
//! token. It may be wrong, empty, out of vocabulary, longer than asked or
//! adversarial: that costs time, not correctness. The output hash, which is
//! BLAKE3 of the little-endian token ids, is identical with speculation on or
//! off, so validators that recompute without speculation agree exactly.
//!
//! # Scope
//!
//! Selection is deterministic: ARC's penalised argmax (the community worker's
//! contract) or raw greedy argmax. Any other rule that is a deterministic
//! function of a position's target logits, the generated history and the
//! position itself (for example seeded Gumbel-max sampling keyed by position)
//! is drafter-invariant under the same loop; no such rule exists in the
//! engine today, so none is implemented here.
//!
//! Speed depends on how a multi-row pass costs against one row. The batched
//! kernel works in four-row quads, so `k = 3` (one quad) is the default, and
//! a deterministic back-off halves the draft length and then pauses drafting
//! when guesses keep missing.

use crate::cached_integer_model::{
    CachedIntegerModel, CachedLayer, GenerationError, I8Weights, KVCache, ModelConfig,
    select_next_token_with_repetition_penalty,
};
use crate::integer_lut::{ONE, argmax_i64, integer_isqrt};
use arc_crypto::Hash256;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

/// Default number of drafted tokens a pass verifies (`k`): one four-row quad
/// of the batched canonical kernel, so every row of the pass is paid for.
pub const DEFAULT_MAX_DRAFT: usize = 3;
/// Upper bound on `k`. Each row's logits are a full vocabulary vector, so the
/// bound also bounds a pass's transient memory.
pub const MAX_DRAFT_LIMIT: usize = 32;
/// Shortest suffix the n-gram drafter will match.
pub const DEFAULT_MIN_NGRAM: usize = 2;
/// Longest suffix the n-gram drafter will look up.
pub const DEFAULT_MAX_NGRAM: usize = 4;
/// Longest n-gram a [`NgramDrafter`] can index.
pub const NGRAM_KEY_LEN: usize = 8;
/// Drafted tokens allowed per matched token (SuffixDecoding's `alpha`): a
/// longer match is more likely to continue, so it may propose more.
pub const NGRAM_DRAFT_PER_MATCHED_TOKEN: usize = 2;
/// The n-gram drafter stops extending a match (its confidence) here.
pub const NGRAM_MAX_MATCH_LEN: usize = 32;
/// Consecutive passes with no accepted guess before drafting pauses.
pub const GOVERNOR_MISSES_BEFORE_PAUSE: u32 = 2;
/// The pause doubles per further miss, up to `2^GOVERNOR_MAX_PAUSE_EXPONENT`
/// passes.
pub const GOVERNOR_MAX_PAUSE_EXPONENT: u32 = 4;

/// Which whole-model generation contract the speculative loop reproduces.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GenerationSemantics {
    /// [`CachedIntegerModel::try_generate`], the community worker's contract:
    /// one BOS forward, the prompt one token at a time, then the final prompt
    /// token forwarded again as the first decode step, with ARC's repetition
    /// penalty selecting every token.
    LegacyV1,
    /// [`CachedIntegerModel::try_generate_v2`]: the final prompt logits select
    /// the first token, with the repetition penalty.
    V2,
    /// [`CachedIntegerModel::try_generate_v2_greedy`]: v2 with raw argmax.
    V2Greedy,
}

impl GenerationSemantics {
    /// The plain path's token choice for this contract.
    pub fn select(self, logits: &mut [i64], generated: &[u32]) -> u32 {
        match self {
            Self::LegacyV1 | Self::V2 => {
                select_next_token_with_repetition_penalty(logits, generated)
            }
            Self::V2Greedy => argmax_i64(logits) as u32,
        }
    }

    /// Run the plain (non-speculative) function this contract names.
    pub fn generate_plain(
        self,
        model: &CachedIntegerModel,
        prompt: &[u32],
        max_tokens: u32,
        eos_tokens: &[u32],
    ) -> Result<(Vec<u32>, Hash256), GenerationError> {
        match self {
            Self::LegacyV1 => model.try_generate(prompt, max_tokens, eos_tokens),
            Self::V2 => model.try_generate_v2(prompt, max_tokens, eos_tokens),
            Self::V2Greedy => model.try_generate_v2_greedy(prompt, max_tokens, eos_tokens),
        }
    }
}

/// Speculation settings. They change how many rows a pass carries, never a
/// token.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SpeculativeConfig {
    /// Most drafted tokens one pass verifies (`k`); `0` is plain decoding.
    /// Values above [`MAX_DRAFT_LIMIT`] are clamped.
    pub max_draft: usize,
    /// Halve the draft length after a pass that accepted nothing and pause
    /// drafting after repeated misses, so a drafter that keeps guessing wrong
    /// costs little.
    pub adaptive: bool,
}

impl Default for SpeculativeConfig {
    fn default() -> Self {
        Self {
            max_draft: DEFAULT_MAX_DRAFT,
            adaptive: true,
        }
    }
}

impl SpeculativeConfig {
    /// The default policy with `k = max_draft`.
    pub fn with_max_draft(max_draft: usize) -> Self {
        Self {
            max_draft,
            ..Self::default()
        }
    }
}

/// Counters for one speculative generation. Timing fields are wall-clock and
/// are never part of any result or hash.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SpeculativeStats {
    /// Target passes in the decode phase. Each forwards one pending row plus
    /// any drafted rows.
    pub passes: u64,
    /// Passes that verified at least one drafted token.
    pub drafted_passes: u64,
    /// Drafted tokens sent to verification.
    pub drafted_tokens: u64,
    /// Drafted tokens the target confirmed.
    pub accepted_tokens: u64,
    /// Rows the target forwarded in the decode phase, rejected ones included.
    pub target_rows: u64,
    /// Tokens emitted by decode passes (each kept row emits exactly one).
    pub emitted_tokens: u64,
    /// All generated tokens, including a v2 first token chosen from the
    /// prompt logits.
    pub generated_tokens: u64,
    /// BOS and prompt forwards (and the v2 first selection).
    pub prefill_nanos: u64,
    /// Time spent inside [`Drafter::propose`].
    pub draft_nanos: u64,
    /// Time spent in target passes.
    pub verify_nanos: u64,
    /// The whole call.
    pub total_nanos: u64,
}

impl SpeculativeStats {
    /// Mean tokens emitted per target decode pass (often written as tau).
    pub fn tokens_per_pass(&self) -> f64 {
        ratio(self.emitted_tokens, self.passes)
    }

    /// Share of drafted tokens the target confirmed.
    pub fn acceptance_rate(&self) -> f64 {
        ratio(self.accepted_tokens, self.drafted_tokens)
    }

    /// Wall-clock time after prefill.
    pub fn decode_nanos(&self) -> u64 {
        self.total_nanos.saturating_sub(self.prefill_nanos)
    }
}

fn ratio(numerator: u64, denominator: u64) -> f64 {
    if denominator == 0 {
        0.0
    } else {
        numerator as f64 / denominator as f64
    }
}

/// The result of [`CachedIntegerModel::try_generate_speculative`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SpeculativeOutput {
    /// Exactly the tokens the plain function returns.
    pub tokens: Vec<u32>,
    /// BLAKE3 of the little-endian token ids, as the plain function hashes it.
    pub output_hash: Hash256,
    pub stats: SpeculativeStats,
}

/// Guesses the tokens a target model will emit next.
///
/// Nothing a drafter returns can change an output: the target re-derives
/// every token and discards guesses it disagrees with. A wrong, short, empty,
/// overlong or out-of-vocabulary proposal costs time only.
pub trait Drafter: Send {
    /// Short stable name for logs and benchmark reports.
    fn label(&self) -> &str;

    /// Propose up to `max_draft` tokens to follow `stream`.
    ///
    /// `stream` is every token the target has consumed, in order, followed by
    /// the pending token it consumes next; the guesses are for the tokens it
    /// emits after that. `generated` is the tokens generated so far, which is
    /// the repetition-penalty history. Within one generation `stream` only
    /// grows, but a drafter must not rely on that for correctness.
    fn propose(&mut self, stream: &[u32], generated: &[u32], max_draft: usize) -> Vec<u32>;
}

/// Never drafts: every pass forwards one row, which is plain decoding.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct NoDrafter;

impl Drafter for NoDrafter {
    fn label(&self) -> &str {
        "none"
    }

    fn propose(&mut self, _stream: &[u32], _generated: &[u32], _max_draft: usize) -> Vec<u32> {
        Vec::new()
    }
}

/// Prompt-lookup / suffix-match drafter. No weights and no model calls.
///
/// It finds the most recent earlier occurrence of the stream's longest suffix
/// of `min_ngram..=max_ngram` tokens and proposes the tokens that followed
/// that occurrence. The copy is LZ77-style, so a periodic stream continues
/// past its own end. A match is extended backwards to measure its length, and
/// at most [`NGRAM_DRAFT_PER_MATCHED_TOKEN`] tokens are proposed per matched
/// token. Lookup is one hash probe per n-gram length; indexing is incremental.
#[derive(Clone, Debug)]
pub struct NgramDrafter {
    min_ngram: usize,
    max_ngram: usize,
    /// `tables[n - min_ngram]` maps an n-gram to the index of the token that
    /// followed its most recent occurrence.
    tables: Vec<HashMap<[u32; NGRAM_KEY_LEN], usize>>,
    /// The stream prefix already indexed. A stream that does not extend it is
    /// re-indexed from scratch.
    indexed: Vec<u32>,
}

impl Default for NgramDrafter {
    fn default() -> Self {
        Self::new(DEFAULT_MIN_NGRAM, DEFAULT_MAX_NGRAM)
    }
}

impl NgramDrafter {
    /// Match suffixes of `min_ngram..=max_ngram` tokens. Lengths are clamped
    /// to `1..=NGRAM_KEY_LEN`, and `max_ngram` to at least `min_ngram`.
    pub fn new(min_ngram: usize, max_ngram: usize) -> Self {
        let min_ngram = min_ngram.clamp(1, NGRAM_KEY_LEN);
        let max_ngram = max_ngram.clamp(min_ngram, NGRAM_KEY_LEN);
        Self {
            min_ngram,
            max_ngram,
            tables: vec![HashMap::new(); max_ngram - min_ngram + 1],
            indexed: Vec::new(),
        }
    }

    fn key(window: &[u32]) -> [u32; NGRAM_KEY_LEN] {
        let mut key = [0u32; NGRAM_KEY_LEN];
        key[..window.len()].copy_from_slice(window);
        key
    }

    fn sync(&mut self, stream: &[u32]) {
        if !stream.starts_with(&self.indexed) {
            self.indexed.clear();
            for table in &mut self.tables {
                table.clear();
            }
        }
        let start = self.indexed.len();
        self.indexed.extend_from_slice(&stream[start..]);
        for next in start..self.indexed.len() {
            for n in self.min_ngram..=self.max_ngram {
                if next >= n {
                    let key = Self::key(&self.indexed[next - n..next]);
                    self.tables[n - self.min_ngram].insert(key, next);
                }
            }
        }
    }
}

impl Drafter for NgramDrafter {
    fn label(&self) -> &str {
        "ngram"
    }

    fn propose(&mut self, stream: &[u32], _generated: &[u32], max_draft: usize) -> Vec<u32> {
        self.sync(stream);
        let len = stream.len();
        if max_draft == 0 {
            return Vec::new();
        }
        let mut found = None;
        for n in (self.min_ngram..=self.max_ngram).rev() {
            if len < n {
                continue;
            }
            let key = Self::key(&stream[len - n..]);
            if let Some(&next) = self.tables[n - self.min_ngram].get(&key) {
                found = Some((n, next));
                break;
            }
        }
        let Some((n, next)) = found else {
            return Vec::new();
        };
        // `next < len`: the occurrence `stream[next - n..next]` ends before
        // the suffix does. Extend it backwards to measure the match.
        let mut matched = n;
        while matched < NGRAM_MAX_MATCH_LEN
            && next > matched
            && stream[next - matched - 1] == stream[len - matched - 1]
        {
            matched += 1;
        }
        let draft_len = max_draft.min(matched.saturating_mul(NGRAM_DRAFT_PER_MATCHED_TOKEN));
        let mut drafts: Vec<u32> = Vec::with_capacity(draft_len);
        for offset in 0..draft_len {
            let source = next + offset;
            // `source - len < offset`, so a copy past the stream's end reads a
            // token this loop already proposed.
            let token = if source < len {
                stream[source]
            } else {
                drafts[source - len]
            };
            drafts.push(token);
        }
        drafts
    }
}

/// Drafts with a small model that shares the target's tokenizer, for example
/// TinyLlama-1.1B for Llama-2-7B (both use the same 32,000-token vocabulary).
///
/// The drafter keeps its own KV cache in step with the target's stream: each
/// call rolls back to the longest prefix it already holds and forwards only
/// the new tokens. It mirrors the target's selection rule (repetition penalty
/// on by default), which raises agreement; any choice is still exact.
pub struct DraftModelDrafter {
    model: Arc<CachedIntegerModel>,
    cache: KVCache,
    /// Tokens whose K/V rows `cache` holds, in order.
    fed: Vec<u32>,
    repetition_penalty: bool,
}

impl DraftModelDrafter {
    pub fn new(model: Arc<CachedIntegerModel>) -> Self {
        let cache = KVCache::new(model.config.n_layers);
        Self {
            model,
            cache,
            fed: Vec::new(),
            repetition_penalty: true,
        }
    }

    /// Choose drafts with ARC's repetition penalty (`true`, the default, which
    /// mirrors the worker contract) or with raw argmax.
    pub fn with_repetition_penalty(mut self, enabled: bool) -> Self {
        self.repetition_penalty = enabled;
        self
    }

    /// Logits after the whole `stream`, reusing every cached row that still
    /// matches it.
    fn logits_after(&mut self, stream: &[u32]) -> Vec<i64> {
        let mut keep = self
            .fed
            .iter()
            .zip(stream)
            .take_while(|(held, wanted)| held == wanted)
            .count();
        if keep == stream.len() {
            // The final token's logits are not stored; recompute that row.
            keep = keep.saturating_sub(1);
        }
        self.cache.truncate(keep);
        self.fed.truncate(keep);
        let fresh = &stream[keep..];
        let model = &self.model;
        let batched = if fresh.len() >= crate::canonical_prefill::MIN_PROFITABLE_BATCH_TOKENS
            && model.has_canonical_i8_profile()
        {
            let chunk = crate::canonical_prefill::SERVING_PREFILL_CHUNK.min(
                crate::canonical_prefill::max_chunk_within_scratch_budget(
                    model.config.d_model,
                    model.config.d_kv,
                    model.config.d_ff,
                ),
            );
            model.prefill_canonical_i8_batched(fresh, &mut self.cache, chunk, false)
        } else {
            None
        };
        let logits = match batched {
            Some(mut last) => last.pop().unwrap_or_default(),
            None => {
                let mut last = Vec::new();
                for &token in fresh {
                    last = model.forward_one_token(token, &mut self.cache);
                }
                last
            }
        };
        self.fed.extend_from_slice(fresh);
        logits
    }
}

impl Drafter for DraftModelDrafter {
    fn label(&self) -> &str {
        "draft-model"
    }

    fn propose(&mut self, stream: &[u32], generated: &[u32], max_draft: usize) -> Vec<u32> {
        let vocab = self.model.config.vocab_size;
        let max_seq = self.model.config.max_seq;
        if max_draft == 0
            || stream.is_empty()
            || stream.len() > max_seq
            || stream
                .iter()
                .any(|&token| token as usize >= vocab || !can_embed(&self.model, token))
        {
            return Vec::new();
        }
        // The drafter forwards every guess but the last, so the stream plus
        // `draft_len - 1` positions must fit its context window.
        let draft_len = max_draft.min(max_seq - stream.len() + 1);
        let mut logits = self.logits_after(stream);
        let mut history = generated.to_vec();
        let mut drafts = Vec::with_capacity(draft_len);
        for index in 0..draft_len {
            let token = if self.repetition_penalty {
                select_next_token_with_repetition_penalty(&mut logits, &history)
            } else {
                argmax_i64(&logits) as u32
            };
            drafts.push(token);
            history.push(token);
            if index + 1 == draft_len || !can_embed(&self.model, token) {
                break;
            }
            logits = self.model.forward_one_token(token, &mut self.cache);
            self.fed.push(token);
        }
        drafts
    }
}

/// Check that `draft` can draft for `target`: every layer present, the same
/// vocabulary size, BOS and token strings, and a full embedding table.
pub fn check_draft_compatible(
    target: &CachedIntegerModel,
    draft: &CachedIntegerModel,
) -> Result<(), String> {
    if !draft.has_all_transformer_layers() {
        return Err("the draft model is missing transformer layers".into());
    }
    if draft.config.vocab_size != target.config.vocab_size {
        return Err(format!(
            "the draft vocabulary has {} tokens and the target's has {}",
            draft.config.vocab_size, target.config.vocab_size
        ));
    }
    if draft.config.bos_token != target.config.bos_token {
        return Err(format!(
            "the draft BOS token is {} and the target's is {}",
            draft.config.bos_token, target.config.bos_token
        ));
    }
    if !draft.vocab.is_empty() && !target.vocab.is_empty() && draft.vocab != target.vocab {
        return Err(
            "the draft and target token strings differ; a draft model must share the target's tokenizer"
                .into(),
        );
    }
    let embedding = draft
        .config
        .vocab_size
        .checked_mul(draft.config.d_model)
        .unwrap_or(usize::MAX);
    if draft.embedding_q16.len() < embedding {
        return Err("the draft model has no complete embedding table".into());
    }
    Ok(())
}

/// Mirrors `forward_one_token`'s own guard: it returns no logits, and leaves
/// the cache untouched, for a token past the embedding table.
fn can_embed(model: &CachedIntegerModel, token: u32) -> bool {
    (token as usize)
        .checked_add(1)
        .and_then(|rows| rows.checked_mul(model.config.d_model))
        .is_some_and(|needed| needed <= model.embedding_q16.len())
}

/// Deterministic draft-length control. It only changes how many rows a pass
/// carries.
#[derive(Clone, Debug)]
struct AcceptanceGovernor {
    max_draft: usize,
    adaptive: bool,
    budget: usize,
    misses: u32,
    pause: u32,
}

impl AcceptanceGovernor {
    fn new(config: SpeculativeConfig) -> Self {
        let max_draft = config.max_draft.min(MAX_DRAFT_LIMIT);
        Self {
            max_draft,
            adaptive: config.adaptive,
            budget: max_draft,
            misses: 0,
            pause: 0,
        }
    }

    /// Drafted tokens the next pass may carry.
    fn budget(&mut self) -> usize {
        if let Some(rest) = self.pause.checked_sub(1) {
            self.pause = rest;
            return 0;
        }
        self.budget
    }

    fn record(&mut self, drafted: usize, accepted: usize) {
        if !self.adaptive || drafted == 0 {
            return;
        }
        if accepted == 0 {
            self.misses = self.misses.saturating_add(1);
            self.budget = (self.budget / 2).max(1);
            if self.misses >= GOVERNOR_MISSES_BEFORE_PAUSE {
                let exponent =
                    (self.misses - GOVERNOR_MISSES_BEFORE_PAUSE).min(GOVERNOR_MAX_PAUSE_EXPONENT);
                self.pause = 1u32 << exponent;
            }
        } else {
            self.misses = 0;
            if accepted == drafted {
                self.budget = self.budget.saturating_mul(2).min(self.max_draft);
            }
        }
    }
}

impl CachedIntegerModel {
    /// Forward `rows` as consecutive positions and return every row's logits.
    ///
    /// Bit-identical to calling [`Self::forward_one_token`] once per row, in
    /// order: the same logits for every row and the same K/V bytes appended to
    /// `cache`. Two or more rows of a complete canonical-I8 model run as one
    /// batched pass ([`Self::prefill_canonical_i8_batched`]); any other case,
    /// and any refusal, runs the rows one at a time.
    pub fn forward_rows_exact(&self, rows: &[u32], cache: &mut KVCache) -> Vec<Vec<i64>> {
        if rows.len() > 1 && self.has_canonical_i8_profile() {
            let base = cache.seq_len;
            if let Some(all) = self.prefill_canonical_i8_batched(rows, cache, rows.len(), true) {
                if all.len() == rows.len() {
                    return all;
                }
                // `all_positions` always yields one vector per row; should it
                // not, undo the pass and take the reference path.
                cache.truncate(base);
            }
        }
        rows.iter()
            .map(|&token| self.forward_one_token(token, cache))
            .collect()
    }

    /// Speculative twin of `semantics.generate_plain(..)`.
    ///
    /// Returns exactly the plain function's tokens and output hash (and its
    /// error for a request that does not fit the context window), whatever
    /// `drafter` proposes. Only speed and [`SpeculativeStats`] differ.
    pub fn try_generate_speculative(
        &self,
        prompt: &[u32],
        max_tokens: u32,
        eos_tokens: &[u32],
        semantics: GenerationSemantics,
        drafter: &mut dyn Drafter,
        config: SpeculativeConfig,
    ) -> Result<SpeculativeOutput, GenerationError> {
        let _admission = self.preflight_generation(prompt.len(), max_tokens)?;
        let job = Job {
            prompt,
            max_tokens: max_tokens as usize,
            eos_tokens,
            semantics,
            config,
        };
        Ok(run_speculative(self, &job, drafter).0)
    }
}

/// One admitted generation request.
struct Job<'a> {
    prompt: &'a [u32],
    max_tokens: usize,
    eos_tokens: &'a [u32],
    semantics: GenerationSemantics,
    config: SpeculativeConfig,
}

fn nanos_since(start: Instant) -> u64 {
    u64::try_from(start.elapsed().as_nanos()).unwrap_or(u64::MAX)
}

/// The speculative loop. Also returns the final cache, which the tests compare
/// with plain decoding's.
fn run_speculative(
    model: &CachedIntegerModel,
    job: &Job<'_>,
    drafter: &mut dyn Drafter,
) -> (SpeculativeOutput, KVCache) {
    let started = Instant::now();
    let mut stats = SpeculativeStats::default();
    let mut cache = KVCache::new(model.config.n_layers);
    let bos = model.config.bos_token;
    let mut generated: Vec<u32> = Vec::with_capacity(job.max_tokens.min(4096));

    // Prefill: the same calls, in the same order, as the plain function.
    let first_pending = match job.semantics {
        GenerationSemantics::LegacyV1 => {
            let _ = model.forward_one_token(bos, &mut cache);
            for &token in job.prompt {
                let _ = model.forward_one_token(token, &mut cache);
            }
            Some(job.prompt.last().copied().unwrap_or(0))
        }
        GenerationSemantics::V2 | GenerationSemantics::V2Greedy => {
            let mut logits = model.forward_one_token(bos, &mut cache);
            if let Some(last) = model.prefill_prompt_into_cache(job.prompt, &mut cache) {
                logits = last;
            }
            if job.max_tokens == 0 {
                None
            } else {
                let first = job.semantics.select(&mut logits, &generated);
                generated.push(first);
                let done = job.eos_tokens.contains(&first) || generated.len() >= job.max_tokens;
                (!done).then_some(first)
            }
        }
    };
    // Everything the target has consumed, mirrored for the drafter.
    let mut consumed: Vec<u32> = Vec::with_capacity(job.prompt.len() + job.max_tokens.min(4096));
    consumed.push(bos);
    consumed.extend_from_slice(job.prompt);
    stats.prefill_nanos = nanos_since(started);

    if let Some(mut pending) = first_pending {
        let vocab = model.config.vocab_size;
        let mut governor = AcceptanceGovernor::new(job.config);
        while generated.len() < job.max_tokens {
            // A pass emits at most `drafts + 1` tokens, so drafting stops one
            // short of the remaining budget. That also keeps every position
            // inside the window the preflight admitted.
            let remaining = job.max_tokens - generated.len();
            let budget = if can_embed(model, pending) {
                governor.budget().min(remaining - 1)
            } else {
                // forward_one_token emits empty logits for such a token and
                // leaves the cache alone; mirror that one row exactly.
                0
            };
            let drafts = if budget > 0 {
                consumed.push(pending);
                let drafting = Instant::now();
                let mut drafts = drafter.propose(&consumed, &generated, budget);
                stats.draft_nanos += nanos_since(drafting);
                consumed.pop();
                drafts.truncate(budget);
                // The target only ever emits embeddable in-vocabulary tokens,
                // so a guess outside that set can never be accepted.
                if let Some(cut) = drafts
                    .iter()
                    .position(|&token| token as usize >= vocab || !can_embed(model, token))
                {
                    drafts.truncate(cut);
                }
                drafts
            } else {
                Vec::new()
            };

            let base = cache.seq_len;
            let rows: Vec<u32> = std::iter::once(pending)
                .chain(drafts.iter().copied())
                .collect();
            let verifying = Instant::now();
            let mut row_logits = model.forward_rows_exact(&rows, &mut cache);
            stats.verify_nanos += nanos_since(verifying);
            stats.passes += 1;
            stats.target_rows += rows.len() as u64;
            if !drafts.is_empty() {
                stats.drafted_passes += 1;
                stats.drafted_tokens += drafts.len() as u64;
            }

            // Row `r` saw rows[..=r]; its logits pick the token after them.
            // Row `r + 1` is the row plain decoding forwards next only if that
            // token equals drafts[r].
            let mut kept = 0usize;
            let mut accepted = 0usize;
            let mut finished = false;
            for (row, logits) in row_logits.iter_mut().enumerate() {
                kept = row + 1;
                let next = job.semantics.select(logits, &generated);
                generated.push(next);
                if job.eos_tokens.contains(&next) || generated.len() >= job.max_tokens {
                    finished = true;
                    break;
                }
                if drafts.get(row) == Some(&next) {
                    accepted += 1;
                } else {
                    pending = next;
                    break;
                }
            }
            // Drop the rows of rejected guesses: the cache now holds exactly
            // what plain decoding holds after emitting the same tokens.
            cache.truncate(base + kept);
            consumed.extend_from_slice(&rows[..kept]);
            stats.emitted_tokens += kept as u64;
            stats.accepted_tokens += accepted as u64;
            governor.record(drafts.len(), accepted);
            if finished {
                break;
            }
        }
    }

    stats.generated_tokens = generated.len() as u64;
    stats.total_nanos = nanos_since(started);
    let output_hash = token_output_hash(&generated);
    (
        SpeculativeOutput {
            tokens: generated,
            output_hash,
            stats,
        },
        cache,
    )
}

/// BLAKE3 of the little-endian token ids, as every generation path hashes it.
fn token_output_hash(tokens: &[u32]) -> Hash256 {
    let bytes: Vec<u8> = tokens
        .iter()
        .flat_map(|token| token.to_le_bytes())
        .collect();
    arc_crypto::hash_bytes(&bytes)
}

// ─── Synthetic models ─────────────────────────────────────────────────────────

/// Geometry and seed of a synthetic canonical-I8 model: random weights, not a
/// language model. Tests and benchmarks use it to exercise exact paths.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SyntheticModelSpec {
    pub vocab_size: usize,
    pub d_model: usize,
    pub n_heads: usize,
    pub n_kv_heads: usize,
    pub d_ff: usize,
    pub n_layers: usize,
    pub max_seq: usize,
    pub seed: u64,
}

impl SyntheticModelSpec {
    /// 64 tokens, width 32, 4 heads, 3 layers, 512 positions.
    pub const fn tiny(seed: u64) -> Self {
        Self {
            vocab_size: 64,
            d_model: 32,
            n_heads: 4,
            n_kv_heads: 4,
            d_ff: 64,
            n_layers: 3,
            max_seq: 512,
            seed,
        }
    }
}

/// SplitMix64: a fixed integer generator, identical on every target.
struct SplitMix64(u64);

impl SplitMix64 {
    fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// Uniform in [-0.1, 0.1); `k / 2^24` is exact in f32.
    fn weight(&mut self) -> f32 {
        let unit = (self.next_u64() >> 40) as f32 / (1u32 << 24) as f32;
        (unit - 0.5) * 0.2
    }

    fn matrix(&mut self, rows: usize, cols: usize) -> I8Weights {
        let values: Vec<f32> = (0..rows * cols).map(|_| self.weight()).collect();
        I8Weights::quantize_f32(&values, rows, cols)
    }
}

/// Build a complete canonical-I8 model with seeded random weights.
pub fn synthetic_canonical_model(spec: SyntheticModelSpec) -> CachedIntegerModel {
    let d = spec.d_model;
    let d_head = d / spec.n_heads;
    let d_kv = d_head * spec.n_kv_heads;
    let mut rng = SplitMix64(spec.seed);
    let embedding_i8 = rng.matrix(spec.vocab_size, d);
    let embedding_q16: Vec<i64> = embedding_i8
        .data
        .chunks_exact(d)
        .zip(&embedding_i8.scales)
        .flat_map(|(row, &scale)| row.iter().map(move |&weight| i64::from(weight) * scale))
        .collect();
    let output_weight = rng.matrix(spec.vocab_size, d);
    let layers: Vec<CachedLayer> = (0..spec.n_layers)
        .map(|_| CachedLayer {
            wq: rng.matrix(d, d),
            wk: rng.matrix(d_kv, d),
            wv: rng.matrix(d_kv, d),
            wo: rng.matrix(d, d),
            w_gate: rng.matrix(spec.d_ff, d),
            w_up: rng.matrix(spec.d_ff, d),
            w_down: rng.matrix(d, spec.d_ff),
            attn_norm: vec![ONE; d],
            ffn_norm: vec![ONE; d],
        })
        .collect();
    let (rope_cos, rope_sin) =
        crate::cached_integer_model::compute_rope_tables(d_head, spec.max_seq, 10_000.0);
    CachedIntegerModel {
        config: ModelConfig {
            n_layers: spec.n_layers,
            d_model: d,
            n_heads: spec.n_heads,
            n_kv_heads: spec.n_kv_heads,
            d_ff: spec.d_ff,
            d_head,
            d_kv,
            vocab_size: spec.vocab_size,
            attn_scale: integer_isqrt((d_head as i64) * ONE),
            rope_cos,
            rope_sin,
            max_seq: spec.max_seq,
            eos_tokens: vec![2],
            bos_token: 1,
            chat_template: String::new(),
            arithmetic_profile: crate::cached_integer_model::ArithmeticProfile::LegacySplitHalfV0,
        },
        embedding_q16,
        embedding_i8,
        layers,
        final_norm: vec![ONE; d],
        output_weight,
        vocab: (0..spec.vocab_size)
            .map(|token| format!("tok_{token}"))
            .collect(),
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

#[cfg(test)]
mod tests {
    use super::*;
    use rayon::ThreadPoolBuilder;

    /// How a scripted drafter guesses, relative to the plain output.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum Script {
        /// Every guess is the token plain decoding emits.
        AllRight,
        /// Every guess differs from it.
        AllWrong,
        /// Right, wrong, right, ... within each proposal.
        AlternatingTokens,
        /// Whole proposals alternate between right and wrong.
        AlternatingPasses,
        /// Seeded random token ids.
        Random(u64),
        /// The second guess is past the vocabulary.
        OutOfVocab,
        /// Right guesses followed by more tokens than were asked for.
        Overlong,
        /// Never proposes anything.
        Silent,
    }

    const SCRIPTS: [Script; 8] = [
        Script::AllRight,
        Script::AllWrong,
        Script::AlternatingTokens,
        Script::AlternatingPasses,
        Script::Random(0x5eed),
        Script::OutOfVocab,
        Script::Overlong,
        Script::Silent,
    ];

    struct ScriptedDrafter {
        script: Script,
        /// The plain output, which the scripts are written against.
        plain: Vec<u32>,
        vocab: u32,
        calls: u64,
        rng: SplitMix64,
    }

    impl ScriptedDrafter {
        fn new(script: Script, plain: &[u32], vocab: usize) -> Self {
            let seed = match script {
                Script::Random(seed) => seed,
                _ => 0,
            };
            Self {
                script,
                plain: plain.to_vec(),
                vocab: vocab as u32,
                calls: 0,
                rng: SplitMix64(seed),
            }
        }
    }

    impl Drafter for ScriptedDrafter {
        fn label(&self) -> &str {
            "scripted"
        }

        fn propose(&mut self, _stream: &[u32], generated: &[u32], max_draft: usize) -> Vec<u32> {
            self.calls += 1;
            let vocab = self.vocab;
            let wrong = |token: u32| (token + 1) % vocab;
            let mut drafts = Vec::new();
            for index in 0..max_draft {
                // Past the plain output the target has stopped; guess anyway.
                let truth = self
                    .plain
                    .get(generated.len() + index)
                    .copied()
                    .unwrap_or(0);
                let token = match self.script {
                    Script::AllRight | Script::Overlong => truth,
                    Script::AllWrong => wrong(truth),
                    Script::AlternatingTokens => {
                        if index.is_multiple_of(2) {
                            truth
                        } else {
                            wrong(truth)
                        }
                    }
                    Script::AlternatingPasses => {
                        if self.calls.is_multiple_of(2) {
                            truth
                        } else {
                            wrong(truth)
                        }
                    }
                    Script::Random(_) => (self.rng.next_u64() % u64::from(vocab)) as u32,
                    Script::OutOfVocab => {
                        if index == 1 {
                            vocab + 7
                        } else {
                            truth
                        }
                    }
                    Script::Silent => break,
                };
                drafts.push(token);
            }
            if self.script == Script::Overlong {
                drafts.extend(std::iter::repeat_n(3, 2 * max_draft + 3));
            }
            drafts
        }
    }

    fn tiny_model(seed: u64) -> CachedIntegerModel {
        synthetic_canonical_model(SyntheticModelSpec::tiny(seed))
    }

    /// Plain v1 written out so its final cache can be compared, and anchored
    /// to `try_generate` by the callers.
    fn plain_v1_with_cache(
        model: &CachedIntegerModel,
        prompt: &[u32],
        max_tokens: u32,
        eos: &[u32],
    ) -> (Vec<u32>, KVCache) {
        let mut cache = KVCache::new(model.config.n_layers);
        let _ = model.forward_one_token(model.config.bos_token, &mut cache);
        for &token in prompt {
            let _ = model.forward_one_token(token, &mut cache);
        }
        let mut generated: Vec<u32> = Vec::new();
        for _ in 0..max_tokens {
            let last = generated
                .last()
                .copied()
                .unwrap_or(*prompt.last().unwrap_or(&0));
            let mut logits = model.forward_one_token(last, &mut cache);
            let next = select_next_token_with_repetition_penalty(&mut logits, &generated);
            generated.push(next);
            if eos.contains(&next) {
                break;
            }
        }
        (generated, cache)
    }

    fn assert_same_cache(a: &KVCache, b: &KVCache, case: &str) {
        assert_eq!(a.seq_len, b.seq_len, "seq_len differs: {case}");
        assert_eq!(a.k_data, b.k_data, "K cache differs: {case}");
        assert_eq!(a.v_data, b.v_data, "V cache differs: {case}");
    }

    fn prompts() -> Vec<Vec<u32>> {
        vec![
            vec![7],
            vec![5, 9, 13, 21, 34, 55, 3],
            // Repetitive, so the n-gram drafter finds matches.
            [10u32, 11, 12, 13].repeat(4),
        ]
    }

    fn run(
        model: &CachedIntegerModel,
        prompt: &[u32],
        max_tokens: u32,
        eos: &[u32],
        semantics: GenerationSemantics,
        drafter: &mut dyn Drafter,
        k: usize,
    ) -> (SpeculativeOutput, KVCache) {
        let _admission = model
            .preflight_generation(prompt.len(), max_tokens)
            .expect("test requests fit the window");
        let job = Job {
            prompt,
            max_tokens: max_tokens as usize,
            eos_tokens: eos,
            semantics,
            config: SpeculativeConfig::with_max_draft(k),
        };
        run_speculative(model, &job, drafter)
    }

    #[test]
    fn kv_cache_truncate_restores_the_earlier_cache() {
        let model = tiny_model(1);
        let tokens = [4u32, 8, 15, 16, 23, 42];
        let mut reference = KVCache::new(model.config.n_layers);
        for &token in &tokens[..3] {
            let _ = model.forward_one_token(token, &mut reference);
        }
        let mut cache = KVCache::new(model.config.n_layers);
        for &token in &tokens {
            let _ = model.forward_one_token(token, &mut cache);
        }
        cache.truncate(3);
        assert_same_cache(&reference, &cache, "truncate to 3");
        cache.truncate(9);
        assert_same_cache(&reference, &cache, "truncate past the end is a no-op");

        // Decoding on from the truncated cache matches decoding on from the
        // reference, so nothing of the removed rows survives.
        let a = model.forward_one_token(9, &mut reference);
        let b = model.forward_one_token(9, &mut cache);
        assert_eq!(a, b);
        assert_same_cache(&reference, &cache, "after one more token");

        cache.truncate(0);
        assert_eq!(cache.seq_len, 0);
        assert!(cache.k_data.iter().chain(&cache.v_data).all(Vec::is_empty));
    }

    #[test]
    fn forward_rows_exact_matches_token_at_a_time() {
        let _switch = crate::canonical_simd::kernel_switch_guard();
        let model = tiny_model(2);
        let head = [3u32, 1, 4, 1, 5];
        for rows in 1..=9usize {
            let tail: Vec<u32> = (0..rows as u32).map(|i| (i * 7 + 2) % 64).collect();
            let mut reference = KVCache::new(model.config.n_layers);
            let mut cache = KVCache::new(model.config.n_layers);
            for &token in &head {
                let _ = model.forward_one_token(token, &mut reference);
                let _ = model.forward_one_token(token, &mut cache);
            }
            let expected: Vec<Vec<i64>> = tail
                .iter()
                .map(|&token| model.forward_one_token(token, &mut reference))
                .collect();
            let got = model.forward_rows_exact(&tail, &mut cache);
            assert_eq!(got, expected, "{rows} rows");
            assert_same_cache(&reference, &cache, &format!("{rows} rows"));
        }
    }

    #[test]
    fn v1_speculation_matches_try_generate_for_every_script_and_k() {
        let _switch = crate::canonical_simd::kernel_switch_guard();
        let model = tiny_model(3);
        let vocab = model.config.vocab_size;
        for prompt in prompts() {
            for max_tokens in [1u32, 2, 17] {
                let (unbounded, _) = GenerationSemantics::LegacyV1
                    .generate_plain(&model, &prompt, max_tokens, &[])
                    .unwrap();
                // No EOS, then an EOS that cuts the answer short mid-way.
                let mut eos_lists = vec![Vec::new()];
                if let Some(&stop) = unbounded.get(unbounded.len() / 2) {
                    eos_lists.push(vec![stop]);
                }
                for eos in &eos_lists {
                    let (plain, plain_hash) = model.try_generate(&prompt, max_tokens, eos).unwrap();
                    let (reference, reference_cache) =
                        plain_v1_with_cache(&model, &prompt, max_tokens, eos);
                    assert_eq!(reference, plain, "the reference loop must be try_generate");
                    for script in SCRIPTS {
                        for k in [0usize, 1, 2, 3, 4, 7, 16] {
                            let case = format!(
                                "prompt={prompt:?} max={max_tokens} eos={eos:?} {script:?} k={k}"
                            );
                            let mut drafter = ScriptedDrafter::new(script, &plain, vocab);
                            let (out, cache) = run(
                                &model,
                                &prompt,
                                max_tokens,
                                eos,
                                GenerationSemantics::LegacyV1,
                                &mut drafter,
                                k,
                            );
                            assert_eq!(out.tokens, plain, "{case}");
                            assert_eq!(out.output_hash, plain_hash, "{case}");
                            assert_same_cache(&reference_cache, &cache, &case);
                            let stats = &out.stats;
                            assert_eq!(stats.generated_tokens, plain.len() as u64, "{case}");
                            assert_eq!(stats.emitted_tokens, plain.len() as u64, "{case}");
                            assert!(stats.accepted_tokens <= stats.drafted_tokens, "{case}");
                            assert!(
                                stats.drafted_tokens <= (k as u64) * stats.passes,
                                "{case}: a pass carried more than k guesses"
                            );
                            assert_eq!(
                                stats.target_rows,
                                stats.passes + stats.drafted_tokens,
                                "{case}"
                            );
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn v2_and_greedy_speculation_match_their_plain_functions() {
        let _switch = crate::canonical_simd::kernel_switch_guard();
        let model = tiny_model(4);
        let vocab = model.config.vocab_size;
        for semantics in [GenerationSemantics::V2, GenerationSemantics::V2Greedy] {
            for prompt in prompts() {
                for max_tokens in [0u32, 1, 3, 17] {
                    let eos_options: [&[u32]; 2] = [&[], &[2, 5]];
                    for eos in eos_options {
                        let (plain, plain_hash) = semantics
                            .generate_plain(&model, &prompt, max_tokens, eos)
                            .unwrap();
                        for script in SCRIPTS {
                            for k in [1usize, 3, 8] {
                                let case = format!(
                                    "{semantics:?} prompt={prompt:?} max={max_tokens} eos={eos:?} {script:?} k={k}"
                                );
                                let mut drafter = ScriptedDrafter::new(script, &plain, vocab);
                                let out = model
                                    .try_generate_speculative(
                                        &prompt,
                                        max_tokens,
                                        eos,
                                        semantics,
                                        &mut drafter,
                                        SpeculativeConfig::with_max_draft(k),
                                    )
                                    .unwrap();
                                assert_eq!(out.tokens, plain, "{case}");
                                assert_eq!(out.output_hash, plain_hash, "{case}");
                            }
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn ngram_and_draft_model_drafters_match_plain_decoding() {
        let _switch = crate::canonical_simd::kernel_switch_guard();
        let target = tiny_model(5);
        let same = Arc::new(tiny_model(5));
        let other = Arc::new(tiny_model(6));
        assert!(check_draft_compatible(&target, &same).is_ok());
        assert!(check_draft_compatible(&target, &other).is_ok());
        for semantics in [
            GenerationSemantics::LegacyV1,
            GenerationSemantics::V2,
            GenerationSemantics::V2Greedy,
        ] {
            for prompt in prompts() {
                let max_tokens = 24u32;
                let (plain, plain_hash) = semantics
                    .generate_plain(&target, &prompt, max_tokens, &[])
                    .unwrap();
                for k in [1usize, 3, 5, 8] {
                    let penalty = semantics != GenerationSemantics::V2Greedy;
                    let mut drafters: [Box<dyn Drafter>; 4] = [
                        Box::new(NgramDrafter::new(DEFAULT_MIN_NGRAM, DEFAULT_MAX_NGRAM)),
                        Box::new(NgramDrafter::new(1, 8)),
                        Box::new(
                            DraftModelDrafter::new(Arc::clone(&same))
                                .with_repetition_penalty(penalty),
                        ),
                        Box::new(
                            DraftModelDrafter::new(Arc::clone(&other))
                                .with_repetition_penalty(penalty),
                        ),
                    ];
                    for (index, drafter) in drafters.iter_mut().enumerate() {
                        let case = format!("{semantics:?} prompt={prompt:?} k={k} drafter#{index}");
                        let out = target
                            .try_generate_speculative(
                                &prompt,
                                max_tokens,
                                &[],
                                semantics,
                                drafter.as_mut(),
                                SpeculativeConfig::with_max_draft(k),
                            )
                            .unwrap();
                        assert_eq!(out.tokens, plain, "{case}");
                        assert_eq!(out.output_hash, plain_hash, "{case}");
                        if index == 2 {
                            // The target drafting for itself, with its own
                            // selection rule, is right every time.
                            assert!(out.stats.drafted_tokens > 0, "{case}");
                            assert_eq!(
                                out.stats.accepted_tokens, out.stats.drafted_tokens,
                                "{case}"
                            );
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn right_guesses_cut_target_passes_and_wrong_ones_back_off() {
        let _switch = crate::canonical_simd::kernel_switch_guard();
        let model = tiny_model(7);
        let vocab = model.config.vocab_size;
        let prompt = [9u32, 8, 7];
        let max_tokens = 24u32;
        let (plain, _) = model.try_generate(&prompt, max_tokens, &[]).unwrap();
        assert_eq!(plain.len(), 24);

        // k = 3 and every guess right: four tokens per pass.
        let mut right = ScriptedDrafter::new(Script::AllRight, &plain, vocab);
        let out = model
            .try_generate_speculative(
                &prompt,
                max_tokens,
                &[],
                GenerationSemantics::LegacyV1,
                &mut right,
                SpeculativeConfig::with_max_draft(3),
            )
            .unwrap();
        assert_eq!(out.tokens, plain);
        assert_eq!(out.stats.passes, 6);
        assert_eq!(out.stats.accepted_tokens, out.stats.drafted_tokens);
        assert!((out.stats.tokens_per_pass() - 4.0).abs() < 1e-9);

        // Every guess wrong: one token per pass, and the governor pauses
        // drafting instead of paying for doomed rows on every pass.
        let mut wrong = ScriptedDrafter::new(Script::AllWrong, &plain, vocab);
        let out = model
            .try_generate_speculative(
                &prompt,
                max_tokens,
                &[],
                GenerationSemantics::LegacyV1,
                &mut wrong,
                SpeculativeConfig::with_max_draft(3),
            )
            .unwrap();
        assert_eq!(out.tokens, plain);
        assert_eq!(out.stats.passes, 24);
        assert_eq!(out.stats.accepted_tokens, 0);
        assert!(out.stats.drafted_passes < 10, "{:?}", out.stats);

        // Without the governor every pass carries the full wrong draft.
        let mut wrong = ScriptedDrafter::new(Script::AllWrong, &plain, vocab);
        let out = model
            .try_generate_speculative(
                &prompt,
                max_tokens,
                &[],
                GenerationSemantics::LegacyV1,
                &mut wrong,
                SpeculativeConfig {
                    max_draft: 3,
                    adaptive: false,
                },
            )
            .unwrap();
        assert_eq!(out.tokens, plain);
        // The last pass has one token left to emit, so it carries no guess.
        assert_eq!(out.stats.drafted_passes, 23);

        // k = 0 is plain decoding: one row per pass, no drafter calls.
        let mut silent = ScriptedDrafter::new(Script::AllRight, &plain, vocab);
        let out = model
            .try_generate_speculative(
                &prompt,
                max_tokens,
                &[],
                GenerationSemantics::LegacyV1,
                &mut silent,
                SpeculativeConfig::with_max_draft(0),
            )
            .unwrap();
        assert_eq!(out.tokens, plain);
        assert_eq!(out.stats.target_rows, 24);
        assert_eq!(silent.calls, 0);
    }

    #[test]
    fn thread_count_and_kernel_do_not_change_speculative_output() {
        let _switch = crate::canonical_simd::kernel_switch_guard();
        let previous = crate::canonical_simd::fast_canonical_kernel_enabled();
        let model = tiny_model(8);
        let draft = Arc::new(tiny_model(9));
        let vocab = model.config.vocab_size;
        let prompt = [10u32, 11, 12, 13, 10, 11, 12];
        let (plain, plain_hash) = model.try_generate(&prompt, 20, &[]).unwrap();
        for simd in [false, true] {
            if simd && !crate::canonical_simd::dotprod_available() {
                continue;
            }
            crate::canonical_simd::set_fast_canonical_kernel(simd);
            for threads in [1usize, 2, 3, 4] {
                let pool = ThreadPoolBuilder::new()
                    .num_threads(threads)
                    .build()
                    .expect("test pool");
                pool.install(|| {
                    let mut drafters: [Box<dyn Drafter>; 4] = [
                        Box::new(NgramDrafter::new(DEFAULT_MIN_NGRAM, DEFAULT_MAX_NGRAM)),
                        Box::new(DraftModelDrafter::new(Arc::clone(&draft))),
                        Box::new(ScriptedDrafter::new(Script::AllRight, &plain, vocab)),
                        Box::new(ScriptedDrafter::new(
                            Script::AlternatingTokens,
                            &plain,
                            vocab,
                        )),
                    ];
                    for drafter in &mut drafters {
                        for k in [2usize, 3, 7] {
                            let out = model
                                .try_generate_speculative(
                                    &prompt,
                                    20,
                                    &[],
                                    GenerationSemantics::LegacyV1,
                                    drafter.as_mut(),
                                    SpeculativeConfig::with_max_draft(k),
                                )
                                .unwrap();
                            let case =
                                format!("simd={simd} threads={threads} {} k={k}", drafter.label());
                            assert_eq!(out.tokens, plain, "{case}");
                            assert_eq!(out.output_hash, plain_hash, "{case}");
                        }
                    }
                });
            }
        }
        crate::canonical_simd::set_fast_canonical_kernel(previous);
    }

    #[test]
    fn context_window_boundary_and_admission_match_plain() {
        let _switch = crate::canonical_simd::kernel_switch_guard();
        let spec = SyntheticModelSpec {
            max_seq: 24,
            ..SyntheticModelSpec::tiny(10)
        };
        let model = synthetic_canonical_model(spec);
        let vocab = model.config.vocab_size;
        let prompt = [4u32, 5, 6, 4, 5];
        // 1 BOS + 5 prompt + 18 generated = 24 positions: exactly the window.
        let (plain, plain_hash) = model.try_generate(&prompt, 18, &[]).unwrap();
        for script in SCRIPTS {
            for k in [1usize, 4, 16, 32, 1000] {
                let mut drafter = ScriptedDrafter::new(script, &plain, vocab);
                let out = model
                    .try_generate_speculative(
                        &prompt,
                        18,
                        &[],
                        GenerationSemantics::LegacyV1,
                        &mut drafter,
                        SpeculativeConfig::with_max_draft(k),
                    )
                    .unwrap();
                assert_eq!(out.tokens, plain, "{script:?} k={k}");
                assert_eq!(out.output_hash, plain_hash, "{script:?} k={k}");
            }
        }
        // One position over: the same typed refusal as the plain function.
        let plain_error = model.try_generate(&prompt, 19, &[]).unwrap_err();
        let error = model
            .try_generate_speculative(
                &prompt,
                19,
                &[],
                GenerationSemantics::LegacyV1,
                &mut NgramDrafter::default(),
                SpeculativeConfig::default(),
            )
            .unwrap_err();
        assert_eq!(error, plain_error);
    }

    #[test]
    fn ngram_drafter_continues_a_period_past_the_end() {
        let mut drafter = NgramDrafter::default();
        let stream = [1u32, 20, 21, 22, 20, 21, 22, 20, 21];
        // The suffix [21, 22, 20, 21] last occurred ending at index 6, and
        // that match extends back one more token. It was followed by
        // 22, 20, 21; the copy then runs on into its own output.
        assert_eq!(
            drafter.propose(&stream, &[], 6),
            vec![22, 20, 21, 22, 20, 21]
        );
        // `max_draft` caps the proposal.
        assert_eq!(drafter.propose(&stream, &[], 2), vec![22, 20]);
        assert!(drafter.propose(&stream, &[], 0).is_empty());
    }

    #[test]
    fn ngram_drafter_prefers_the_longest_most_recent_match() {
        let mut drafter = NgramDrafter::new(1, 4);
        // [5, 6] occurs twice; the longer suffix [9, 5, 6] only once.
        let stream = [9u32, 5, 6, 30, 7, 5, 6, 40, 9, 5, 6];
        assert_eq!(drafter.propose(&stream, &[], 1), vec![30]);
        // Without the 3-gram, the most recent [5, 6] wins.
        let mut short = NgramDrafter::new(1, 2);
        assert_eq!(short.propose(&stream, &[], 1), vec![40]);
    }

    #[test]
    fn ngram_drafter_scales_drafts_with_match_length_and_finds_nothing_new() {
        let mut drafter = NgramDrafter::new(2, 2);
        // A 2-token match allows 2 * 2 = 4 guesses even when 8 are asked for.
        let stream = [50u32, 51, 1, 2, 3, 4, 5, 6, 7, 50, 51];
        assert_eq!(drafter.propose(&stream, &[], 8), vec![1, 2, 3, 4]);
        // No earlier occurrence of the suffix: no guess.
        let mut fresh = NgramDrafter::default();
        assert!(fresh.propose(&[1, 2, 3, 4, 5], &[], 4).is_empty());
        assert!(fresh.propose(&[], &[], 4).is_empty());
    }

    #[test]
    fn ngram_drafter_reindexes_a_stream_that_is_not_an_extension() {
        let mut drafter = NgramDrafter::default();
        let first = [1u32, 2, 3, 9, 1, 2];
        assert_eq!(drafter.propose(&first, &[], 1), vec![3]);
        // A different stream with the same suffix must not see the old index.
        let second = [1u32, 2, 4, 9, 1, 2];
        assert_eq!(drafter.propose(&second, &[], 1), vec![4]);
        // Growing the stream keeps the index and adds the new tokens.
        let grown = [1u32, 2, 4, 9, 1, 2, 7, 1, 2];
        assert_eq!(drafter.propose(&grown, &[], 1), vec![7]);
    }

    #[test]
    fn draft_model_drafter_refuses_streams_it_cannot_run() {
        let _switch = crate::canonical_simd::kernel_switch_guard();
        let draft = Arc::new(synthetic_canonical_model(SyntheticModelSpec {
            max_seq: 8,
            ..SyntheticModelSpec::tiny(11)
        }));
        let mut drafter = DraftModelDrafter::new(Arc::clone(&draft));
        // Out of vocabulary.
        assert!(drafter.propose(&[1, 64], &[], 3).is_empty());
        // Longer than the drafter's window.
        assert!(drafter.propose(&[1; 9], &[], 3).is_empty());
        // At the window edge only the guesses that fit are made.
        assert_eq!(drafter.propose(&[1; 7], &[], 5).len(), 2);
        assert_eq!(drafter.propose(&[1; 8], &[], 5).len(), 1);
    }

    #[test]
    fn incompatible_draft_models_are_refused() {
        let target = tiny_model(12);
        let mut wider = synthetic_canonical_model(SyntheticModelSpec {
            vocab_size: 65,
            ..SyntheticModelSpec::tiny(12)
        });
        assert!(check_draft_compatible(&target, &wider).is_err());
        wider = tiny_model(13);
        wider.vocab[5] = "renamed".into();
        assert!(check_draft_compatible(&target, &wider).is_err());
        let mut other_bos = tiny_model(14);
        other_bos.config.bos_token = 0;
        assert!(check_draft_compatible(&target, &other_bos).is_err());
        let mut partial = tiny_model(15);
        partial.layers[1] = CachedLayer::placeholder();
        assert!(check_draft_compatible(&target, &partial).is_err());
    }

    #[test]
    fn governor_backs_off_pauses_and_recovers() {
        let mut governor = AcceptanceGovernor::new(SpeculativeConfig::with_max_draft(8));
        assert_eq!(governor.budget(), 8);
        governor.record(8, 0);
        assert_eq!(governor.budget(), 4);
        governor.record(4, 0);
        // Second miss in a row: one paused pass, then the halved budget.
        assert_eq!(governor.budget(), 0);
        assert_eq!(governor.budget(), 2);
        governor.record(2, 0);
        assert_eq!((governor.budget(), governor.budget()), (0, 0));
        assert_eq!(governor.budget(), 1);
        // Full acceptance doubles the budget back up to k.
        governor.record(1, 1);
        assert_eq!(governor.budget(), 2);
        governor.record(2, 2);
        governor.record(4, 4);
        governor.record(8, 8);
        assert_eq!(governor.budget(), 8);
        // A partial hit resets the miss streak without growing the budget.
        governor.record(8, 3);
        assert_eq!(governor.budget(), 8);
        // Passes without drafts change nothing.
        governor.record(0, 0);
        assert_eq!(governor.budget(), 8);
    }
}
