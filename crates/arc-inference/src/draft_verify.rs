//! Exact speculative decoding with an external drafter.
//!
//! A drafter, which can be any engine, proposes the next `k` tokens from the
//! exact prefix: llama.cpp running the same GGUF, a smaller model, a lookup
//! table. ARC's exact engine then verifies every proposal in one multi-row
//! call per stage ([`CachedIntegerModel::forward_shard_rows`], on the whole
//! model or on a split into stages) and commits the longest run of drafts that
//! equal the token ARC itself selects at each row, plus ARC's own next token.
//! The rows of rejected drafts are dropped with
//! [`CachedIntegerModel::rollback_rows`].
//!
//! Every committed token is ARC's own selection, made with plain decoding's
//! rule from logits that equal plain decoding's bit for bit, so the output
//! (tokens and output hash) is byte-identical to plain exact decoding whatever
//! a drafter proposes. A drafter changes only how many rows a pass verifies:
//! a wrong, slow or failed drafter costs time, never exactness.
//!
//! Draft lengths follow the batched kernel's cost. A pass verifies `k + 1`
//! rows (the pending token and `k` drafts), and the kernel pays for every
//! started quad of four rows, so the lengths [`DraftPolicy`] moves between are
//! `3, 7, 15, 31`: every row of the quads paid for. One or two rows never pay
//! (see `forward_shard_rows`), so a pass that has nothing to verify is a plain
//! one-row step.

use crate::cached_integer_model::{
    CachedIntegerModel, GenerationError, KVCache, ShardForwardError, ShardInput, ShardOutput,
    ShardRowsError, ShardRowsInput, ShardRowsOutput, select_next_token_with_repetition_penalty,
};
use arc_crypto::Hash256;
use std::collections::BTreeMap;
use std::fmt::{self, Write as _};
use std::io::{BufRead, BufReader, Write};
use std::ops::Range;
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::time::Instant;

/// The plain generation a run reproduces exactly.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExactSemantics {
    /// [`CachedIntegerModel::try_generate`], the community worker's contract:
    /// one BOS forward, the prompt, then the final prompt token forwarded
    /// again as the first decode step, ARC's repetition penalty choosing every
    /// token.
    Worker,
    /// [`CachedIntegerModel::try_generate_v2`]: the final prompt row's logits
    /// choose the first token, with the repetition penalty.
    V2,
}

/// Proposes tokens to follow an exact prefix.
pub trait TokenDrafter {
    /// Short stable name for logs and reports.
    fn label(&self) -> &str;

    /// Propose up to `max_draft` tokens to follow `stream`.
    ///
    /// `stream` is every token the verifier has consumed, in order, followed
    /// by the pending token it consumes next; the proposals are for the tokens
    /// it emits after that. `generated` is the output so far, which is ARC's
    /// repetition-penalty history. Proposals beyond `max_draft`, and any token
    /// outside the vocabulary with everything after it, are ignored.
    fn propose(
        &mut self,
        stream: &[u32],
        generated: &[u32],
        max_draft: usize,
    ) -> Result<Vec<u32>, DraftError>;
}

/// Never proposes anything: every step is plain exact decoding.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct NoDrafter;

impl TokenDrafter for NoDrafter {
    fn label(&self) -> &str {
        "none"
    }

    fn propose(&mut self, _: &[u32], _: &[u32], _: usize) -> Result<Vec<u32>, DraftError> {
        Ok(Vec::new())
    }
}

/// A drafter failed. The run continues with plain steps.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DraftError(pub String);

impl fmt::Display for DraftError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for DraftError {}

/// How the draft length moves between passes.
///
/// After a pass in which every draft matched, `k` grows to `2k + 1` (at most
/// `cap`); after a miss it shrinks to `(k - 1) / 2` (at least `min`). A pass
/// at `min` that matches nothing switches drafting off for `probe_after`
/// plain steps, then the next pass tries `min` again; `probe_after == 0`
/// never switches it off.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DraftPolicy {
    pub min: usize,
    pub start: usize,
    pub cap: usize,
    pub probe_after: usize,
}

impl DraftPolicy {
    /// Never drafts: plain exact decoding through the same verifier.
    pub const OFF: Self = Self {
        min: 0,
        start: 0,
        cap: 0,
        probe_after: 0,
    };

    /// Quad-aligned lengths from 3 to 31. The cap comes from the measured
    /// agreement of llama.cpp with ARC's engine on the same Llama-2-7B GGUF
    /// (interleaved profile, agreement experiment CI run 38018929905): the
    /// 90th percentile run of agreeing tokens is 29, and the tokens committed
    /// per pass stop growing between k = 32 and k = 64.
    pub const MEASURED: Self = Self {
        min: 3,
        start: 3,
        cap: 31,
        probe_after: 16,
    };

    /// Always `k` drafts per pass, never switched off.
    pub const fn fixed(k: usize) -> Self {
        Self {
            min: k,
            start: k,
            cap: k,
            probe_after: 0,
        }
    }
}

/// The draft length state of one generation.
#[derive(Clone, Debug)]
struct DraftLength {
    policy: DraftPolicy,
    k: usize,
    off_left: usize,
    disabled: bool,
}

impl DraftLength {
    fn new(policy: DraftPolicy) -> Self {
        let cap = policy.cap.max(policy.min);
        Self {
            policy: DraftPolicy { cap, ..policy },
            k: policy.start.clamp(policy.min, cap),
            off_left: 0,
            disabled: false,
        }
    }

    /// Drafts to ask for in the next pass; 0 means a plain step.
    fn current(&self) -> usize {
        if self.disabled || self.off_left > 0 {
            0
        } else {
            self.k
        }
    }

    fn after_plain_step(&mut self) {
        self.off_left = self.off_left.saturating_sub(1);
    }

    /// `offered` drafts were verified and the first `matched` were ARC's.
    fn after_pass(&mut self, offered: usize, matched: usize) {
        if matched == offered {
            if offered == self.k {
                self.k = (2 * self.k + 1).min(self.policy.cap);
            }
        } else if matched == 0 && self.k == self.policy.min && self.policy.probe_after > 0 {
            self.off_left = self.policy.probe_after;
        } else {
            self.k = (self.k.saturating_sub(1) / 2).max(self.policy.min);
        }
    }
}

/// What one generation did, for reports. Times are wall-clock nanoseconds.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DraftStats {
    /// Passes that verified at least one draft.
    pub passes: usize,
    /// One-row steps (no drafts to verify).
    pub plain_steps: usize,
    /// Drafts verified, and how many of them were ARC's token.
    pub drafted: usize,
    pub accepted: usize,
    /// Rows run in multi-row passes, and rows of rejected drafts dropped.
    pub rows_verified: usize,
    pub rows_rolled_back: usize,
    pub drafter_errors: usize,
    /// Passes per number of drafts verified.
    pub draft_lengths: BTreeMap<usize, usize>,
    pub prefill_nanos: u64,
    /// Time in the drafter, including talking to it.
    pub draft_nanos: u64,
    /// Time in ARC's engine after the prefill: forward calls, token
    /// selection and rollback.
    pub verify_nanos: u64,
}

/// The output of a generation and how it was produced.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DraftOutput {
    pub tokens: Vec<u32>,
    pub output_hash: Hash256,
    pub stats: DraftStats,
}

/// A generation that the verifier refused or could not run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DraftVerifyError {
    /// The request does not fit the model's context window.
    Generation(GenerationError),
    Shard(ShardForwardError),
    Rows(ShardRowsError),
    /// `stage_ends` is not a strictly increasing list ending at `n_layers`.
    BadStages {
        n_layers: usize,
        stage_ends: Vec<usize>,
    },
    /// The last stage returned hidden states instead of a token or logits.
    NoHead,
}

impl fmt::Display for DraftVerifyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Generation(e) => write!(f, "{e}"),
            Self::Shard(e) => write!(f, "{e}"),
            Self::Rows(e) => write!(f, "{e}"),
            Self::BadStages {
                n_layers,
                stage_ends,
            } => write!(
                f,
                "stage ends {stage_ends:?} do not cut {n_layers} layers into contiguous stages"
            ),
            Self::NoHead => f.write_str("the last stage did not return a token"),
        }
    }
}

impl std::error::Error for DraftVerifyError {}

impl From<GenerationError> for DraftVerifyError {
    fn from(e: GenerationError) -> Self {
        Self::Generation(e)
    }
}

impl From<ShardForwardError> for DraftVerifyError {
    fn from(e: ShardForwardError) -> Self {
        Self::Shard(e)
    }
}

impl From<ShardRowsError> for DraftVerifyError {
    fn from(e: ShardRowsError) -> Self {
        Self::Rows(e)
    }
}

/// One generation's settings.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DraftVerifyConfig {
    pub semantics: ExactSemantics,
    /// The exclusive end layer of every verifier stage, in order; the last is
    /// `n_layers`. `[n_layers]` is the whole model in one stage.
    pub stage_ends: Vec<usize>,
    pub policy: DraftPolicy,
}

/// Generates with `drafter` proposing and ARC's exact engine verifying.
///
/// The tokens and output hash equal `try_generate` (for
/// [`ExactSemantics::Worker`]) or `try_generate_v2` (for
/// [`ExactSemantics::V2`]) on the same arguments, whatever the drafter
/// proposes. Canonical per-row I8 models only: other profiles are refused by
/// `forward_shard_rows`.
pub fn generate_with_drafter(
    model: &CachedIntegerModel,
    prompt: &[u32],
    max_tokens: u32,
    eos_tokens: &[u32],
    drafter: &mut dyn TokenDrafter,
    config: &DraftVerifyConfig,
) -> Result<DraftOutput, DraftVerifyError> {
    let _admission = model.preflight_generation(prompt.len(), max_tokens)?;
    let mut run = Verifier::new(model, config)?;
    run.generate(prompt, max_tokens as usize, eos_tokens, drafter)?;
    Ok(run.finish())
}

/// The state of one generation: every stage's cache, the consumed tokens and
/// the output so far.
pub struct Verifier<'m> {
    model: &'m CachedIntegerModel,
    semantics: ExactSemantics,
    stages: Vec<Range<usize>>,
    caches: Vec<KVCache>,
    max_rows: usize,
    length: DraftLength,
    /// Every token forwarded so far, in order.
    stream: Vec<u32>,
    generated: Vec<u32>,
    stats: DraftStats,
}

impl<'m> Verifier<'m> {
    pub fn new(
        model: &'m CachedIntegerModel,
        config: &DraftVerifyConfig,
    ) -> Result<Self, DraftVerifyError> {
        let n_layers = model.config.n_layers;
        let bad = || DraftVerifyError::BadStages {
            n_layers,
            stage_ends: config.stage_ends.clone(),
        };
        if config.stage_ends.last() != Some(&n_layers) {
            return Err(bad());
        }
        let mut stages = Vec::with_capacity(config.stage_ends.len());
        let mut start = 0;
        for &end in &config.stage_ends {
            if end <= start {
                return Err(bad());
            }
            stages.push(start..end);
            start = end;
        }
        let max_rows = stages
            .iter()
            .map(|stage| model.max_shard_rows(stage.end))
            .min()
            .unwrap_or(1);
        Ok(Self {
            model,
            semantics: config.semantics,
            caches: stages.iter().map(|_| KVCache::new(n_layers)).collect(),
            stages,
            max_rows,
            length: DraftLength::new(config.policy),
            stream: Vec::new(),
            generated: Vec::new(),
            stats: DraftStats::default(),
        })
    }

    /// Every stage's cache, in stage order. Each holds only its own layers.
    pub fn caches(&self) -> &[KVCache] {
        &self.caches
    }

    /// Every token forwarded so far, in order.
    pub fn stream(&self) -> &[u32] {
        &self.stream
    }

    pub fn finish(self) -> DraftOutput {
        let bytes: Vec<u8> = self
            .generated
            .iter()
            .flat_map(|t| t.to_le_bytes())
            .collect();
        DraftOutput {
            output_hash: arc_crypto::hash_bytes(&bytes),
            tokens: self.generated,
            stats: self.stats,
        }
    }

    /// Runs the whole generation. `max_tokens` must have passed
    /// `preflight_generation` for this prompt.
    pub fn generate(
        &mut self,
        prompt: &[u32],
        max_tokens: usize,
        eos_tokens: &[u32],
        drafter: &mut dyn TokenDrafter,
    ) -> Result<(), DraftVerifyError> {
        if max_tokens == 0 {
            return Ok(());
        }
        let started = Instant::now();
        let mut fed = Vec::with_capacity(prompt.len() + 1);
        fed.push(self.model.config.bos_token);
        fed.extend_from_slice(prompt);
        let mut last_logits = Vec::new();
        for chunk in fed.chunks(self.max_rows) {
            let position = self.stream.len();
            let mut rows = self.forward_rows(chunk.to_vec(), position)?;
            self.stream.extend_from_slice(chunk);
            last_logits = rows.pop().ok_or(DraftVerifyError::NoHead)?;
        }
        let mut pending = match self.semantics {
            // The worker forwards the final prompt token again.
            ExactSemantics::Worker => prompt.last().copied().unwrap_or(0),
            // V2 chooses the first token from the final prompt row.
            ExactSemantics::V2 => {
                let first =
                    select_next_token_with_repetition_penalty(&mut last_logits, &self.generated);
                self.generated.push(first);
                if eos_tokens.contains(&first) {
                    self.stats.prefill_nanos = elapsed_nanos(started);
                    return Ok(());
                }
                first
            }
        };
        self.stats.prefill_nanos = elapsed_nanos(started);

        while self.generated.len() < max_tokens {
            let remaining = max_tokens - self.generated.len();
            let want = self
                .length
                .current()
                .min(remaining - 1)
                .min(self.max_rows - 1);
            let drafts = if want == 0 {
                Vec::new()
            } else {
                self.ask(drafter, pending, want)
            };
            let verify_started = Instant::now();
            let done = if drafts.is_empty() {
                self.length.after_plain_step();
                self.plain_step(pending, eos_tokens)?
            } else {
                self.verify(pending, &drafts, max_tokens, eos_tokens)?
            };
            self.stats.verify_nanos += elapsed_nanos(verify_started);
            if done {
                break;
            }
            pending = *self.generated.last().ok_or(DraftVerifyError::NoHead)?;
        }
        Ok(())
    }

    /// Asks the drafter for up to `want` tokens to follow `pending`. A failed
    /// drafter is switched off for the rest of the generation.
    fn ask(&mut self, drafter: &mut dyn TokenDrafter, pending: u32, want: usize) -> Vec<u32> {
        let started = Instant::now();
        self.stream.push(pending);
        let proposed = drafter.propose(&self.stream, &self.generated, want);
        self.stream.pop();
        self.stats.draft_nanos += elapsed_nanos(started);
        match proposed {
            Ok(mut drafts) => {
                drafts.truncate(want);
                let vocab = self.model.config.vocab_size;
                if let Some(bad) = drafts.iter().position(|&t| t as usize >= vocab) {
                    drafts.truncate(bad);
                }
                drafts
            }
            Err(_) => {
                self.stats.drafter_errors += 1;
                self.length.disabled = true;
                Vec::new()
            }
        }
    }

    /// Plain decoding's step: forwards `pending` and selects the next token.
    /// Returns whether the generation is finished.
    fn plain_step(&mut self, pending: u32, eos_tokens: &[u32]) -> Result<bool, DraftVerifyError> {
        let position = self.stream.len();
        let n_layers = self.model.config.n_layers;
        let mut input = ShardInput::Token(pending);
        let mut chosen = None;
        for (stage, cache) in self.stages.iter().zip(self.caches.iter_mut()) {
            let history: &[u32] = if stage.end == n_layers {
                &self.generated
            } else {
                &[]
            };
            match self.model.forward_shard_token_with_history(
                input,
                cache,
                stage.start,
                stage.end,
                position,
                history,
            )? {
                ShardOutput::Hidden(hidden) => input = ShardInput::Hidden(hidden),
                ShardOutput::Token { id, .. } => {
                    chosen = Some(id);
                    break;
                }
            }
        }
        let next = chosen.ok_or(DraftVerifyError::NoHead)?;
        self.stream.push(pending);
        self.generated.push(next);
        self.stats.plain_steps += 1;
        Ok(eos_tokens.contains(&next))
    }

    /// Verifies `drafts` after `pending` in one multi-row pass, commits ARC's
    /// tokens and drops the rows of rejected drafts. Returns whether the
    /// generation is finished.
    fn verify(
        &mut self,
        pending: u32,
        drafts: &[u32],
        max_tokens: usize,
        eos_tokens: &[u32],
    ) -> Result<bool, DraftVerifyError> {
        let position = self.stream.len();
        let mut rows = Vec::with_capacity(drafts.len() + 1);
        rows.push(pending);
        rows.extend_from_slice(drafts);
        let logits = self.forward_rows(rows, position)?;

        // Row j's logits choose the token after row j, exactly as plain
        // decoding would after forwarding the same tokens.
        let mut matched = 0;
        let mut last = 0;
        let mut done = false;
        for (j, mut row) in logits.into_iter().enumerate() {
            let next = select_next_token_with_repetition_penalty(&mut row, &self.generated);
            self.generated.push(next);
            last = j;
            let is_match = drafts.get(j) == Some(&next);
            if is_match {
                matched += 1;
            }
            if eos_tokens.contains(&next) || self.generated.len() >= max_tokens {
                done = true;
                break;
            }
            if !is_match {
                break;
            }
        }

        // Rows 0..=last were forwarded on the committed path: `pending` and
        // the drafts before the last committed token. The rest go.
        let keep = position + 1 + last;
        for cache in &mut self.caches {
            self.model.rollback_rows(cache, keep)?;
        }
        self.stream.push(pending);
        self.stream.extend_from_slice(&drafts[..last]);

        self.stats.passes += 1;
        self.stats.drafted += drafts.len();
        self.stats.accepted += matched;
        self.stats.rows_verified += drafts.len() + 1;
        self.stats.rows_rolled_back += drafts.len() - last;
        *self.stats.draft_lengths.entry(drafts.len()).or_default() += 1;
        self.length.after_pass(drafts.len(), matched);
        Ok(done)
    }

    /// Runs `rows` at `position..` through every stage and returns the last
    /// stage's logits, one row per input row.
    fn forward_rows(
        &mut self,
        rows: Vec<u32>,
        position: usize,
    ) -> Result<Vec<Vec<i64>>, DraftVerifyError> {
        let mut input = ShardRowsInput::Tokens(rows);
        for (stage, cache) in self.stages.iter().zip(self.caches.iter_mut()) {
            match self
                .model
                .forward_shard_rows(input, cache, stage.start, stage.end, position)?
            {
                ShardRowsOutput::Hidden(hidden) => input = ShardRowsInput::Hidden(hidden),
                ShardRowsOutput::Logits(logits) => return Ok(logits),
            }
        }
        Err(DraftVerifyError::NoHead)
    }
}

fn elapsed_nanos(started: Instant) -> u64 {
    u64::try_from(started.elapsed().as_nanos()).unwrap_or(u64::MAX)
}

// ── A drafter in another process ────────────────────────────────────────────

/// A drafter that runs as a separate process and speaks a line protocol on
/// its standard input and output, so any engine can draft without becoming a
/// dependency of ARC. `tools/llama-drafter` is the llama.cpp implementation.
///
/// The drafter first prints `READY <vocab_size>`. Each request is one line,
/// `PROPOSE <max_draft> <n> <stream...> <m> <generated...>` (see
/// [`TokenDrafter::propose`]), and each reply is one line,
/// `OK <micros> <count> <tokens...>` or `ERR <reason>`, where `micros` is the
/// drafter's own compute time. `QUIT` ends it.
pub struct ProcessDrafter {
    label: String,
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    reported_micros: u64,
}

impl ProcessDrafter {
    /// Starts `command` with piped standard input and output and waits for its
    /// `READY` line, which must name `vocab_size`.
    pub fn spawn(
        command: &mut Command,
        label: &str,
        vocab_size: usize,
    ) -> Result<Self, DraftError> {
        let mut child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .map_err(|e| DraftError(format!("cannot start the drafter: {e}")))?;
        let (Some(stdin), Some(stdout)) = (child.stdin.take(), child.stdout.take()) else {
            let _ = child.kill();
            let _ = child.wait();
            return Err(DraftError(
                "the drafter has no piped stdin or stdout".into(),
            ));
        };
        let mut drafter = Self {
            label: label.to_string(),
            child,
            stdin,
            stdout: BufReader::new(stdout),
            reported_micros: 0,
        };
        let line = drafter.read_line()?;
        let vocab = parse_ready(&line)?;
        if vocab != vocab_size {
            return Err(DraftError(format!(
                "the drafter's vocabulary has {vocab} tokens, the model's {vocab_size}"
            )));
        }
        Ok(drafter)
    }

    /// Compute time the drafter reported, summed over every proposal.
    pub fn reported_micros(&self) -> u64 {
        self.reported_micros
    }

    fn read_line(&mut self) -> Result<String, DraftError> {
        let mut line = String::new();
        let read = self
            .stdout
            .read_line(&mut line)
            .map_err(|e| DraftError(format!("reading the drafter: {e}")))?;
        if read == 0 {
            return Err(DraftError("the drafter closed its output".into()));
        }
        Ok(line)
    }
}

impl TokenDrafter for ProcessDrafter {
    fn label(&self) -> &str {
        &self.label
    }

    fn propose(
        &mut self,
        stream: &[u32],
        generated: &[u32],
        max_draft: usize,
    ) -> Result<Vec<u32>, DraftError> {
        let request = draft_request(stream, generated, max_draft);
        self.stdin
            .write_all(request.as_bytes())
            .and_then(|()| self.stdin.flush())
            .map_err(|e| DraftError(format!("writing to the drafter: {e}")))?;
        let line = self.read_line()?;
        let (micros, tokens) = parse_draft_reply(&line, max_draft)?;
        self.reported_micros += micros;
        Ok(tokens)
    }
}

impl Drop for ProcessDrafter {
    fn drop(&mut self) {
        let _ = self.stdin.write_all(b"QUIT\n");
        let _ = self.stdin.flush();
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// The request line for [`ProcessDrafter`].
pub fn draft_request(stream: &[u32], generated: &[u32], max_draft: usize) -> String {
    let mut line = String::with_capacity(24 + 7 * (stream.len() + generated.len()));
    let _ = write!(line, "PROPOSE {max_draft} {}", stream.len());
    for token in stream {
        let _ = write!(line, " {token}");
    }
    let _ = write!(line, " {}", generated.len());
    for token in generated {
        let _ = write!(line, " {token}");
    }
    line.push('\n');
    line
}

/// Parses a reply line from a [`ProcessDrafter`]: the reported compute
/// microseconds and the proposed tokens.
pub fn parse_draft_reply(line: &str, max_draft: usize) -> Result<(u64, Vec<u32>), DraftError> {
    let bad = || DraftError(format!("bad drafter reply: {:.80}", line.trim_end()));
    let mut words = line.split_ascii_whitespace();
    match words.next() {
        Some("OK") => {
            let micros: u64 = words.next().and_then(|w| w.parse().ok()).ok_or_else(bad)?;
            let count: usize = words.next().and_then(|w| w.parse().ok()).ok_or_else(bad)?;
            let tokens: Vec<u32> = words
                .map(str::parse)
                .collect::<Result<_, _>>()
                .map_err(|_| bad())?;
            if tokens.len() != count || count > max_draft {
                return Err(bad());
            }
            Ok((micros, tokens))
        }
        Some("ERR") => Err(DraftError(format!(
            "the drafter refused: {:.200}",
            line.trim_end()
        ))),
        _ => Err(bad()),
    }
}

fn parse_ready(line: &str) -> Result<usize, DraftError> {
    let mut words = line.split_ascii_whitespace();
    match (words.next(), words.next().and_then(|w| w.parse().ok())) {
        (Some("READY"), Some(vocab)) => Ok(vocab),
        _ => Err(DraftError(format!(
            "the drafter did not start: {:.200}",
            line.trim_end()
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn walk(policy: DraftPolicy, passes: &[(usize, usize)]) -> Vec<usize> {
        let mut length = DraftLength::new(policy);
        let mut seen = vec![length.current()];
        for &(offered, matched) in passes {
            length.after_pass(offered, matched);
            seen.push(length.current());
        }
        seen
    }

    #[test]
    fn draft_policy_grows_on_full_acceptance_and_shrinks_on_a_miss() {
        let measured = DraftPolicy::MEASURED;
        assert_eq!(
            walk(measured, &[(3, 3), (7, 7), (15, 15), (31, 31)]),
            vec![3, 7, 15, 31, 31]
        );
        assert_eq!(
            walk(
                measured,
                &[(3, 3), (7, 7), (15, 15), (31, 4), (15, 0), (7, 2), (3, 1)]
            ),
            vec![3, 7, 15, 31, 15, 7, 3, 3]
        );
        // Fewer drafts than asked for (the end of the output) neither grows
        // nor shrinks the length.
        assert_eq!(walk(measured, &[(3, 3), (5, 5)]), vec![3, 7, 7]);
    }

    #[test]
    fn draft_policy_switches_off_and_probes() {
        let mut length = DraftLength::new(DraftPolicy::MEASURED);
        length.after_pass(3, 0);
        for _ in 0..DraftPolicy::MEASURED.probe_after {
            assert_eq!(length.current(), 0);
            length.after_plain_step();
        }
        assert_eq!(length.current(), 3);
    }

    #[test]
    fn draft_policy_fixed_and_off_never_move() {
        assert_eq!(
            walk(DraftPolicy::fixed(5), &[(5, 5), (5, 0), (5, 2)]),
            vec![5, 5, 5, 5]
        );
        assert_eq!(walk(DraftPolicy::OFF, &[(0, 0)]), vec![0, 0]);
    }

    #[test]
    fn draft_protocol_round_trips() {
        assert_eq!(
            draft_request(&[1, 2, 3], &[9], 4),
            "PROPOSE 4 3 1 2 3 1 9\n"
        );
        assert_eq!(draft_request(&[1], &[], 2), "PROPOSE 2 1 1 0\n");
        assert_eq!(
            parse_draft_reply("OK 120 2 5 6\n", 4),
            Ok((120, vec![5, 6]))
        );
        assert_eq!(parse_draft_reply("OK 0 0\n", 4), Ok((0, Vec::new())));
        for bad in [
            "",
            "OK",
            "OK 1",
            "OK 1 2 5",
            "OK 1 1 5 6",
            "OK x 1 5",
            "OK 1 5 1 2 3 4 5",
            "NO 1 1 1",
            "OK 1 1 -3",
        ] {
            assert!(parse_draft_reply(bad, 4).is_err(), "{bad:?}");
        }
        let refused = parse_draft_reply("ERR out of memory\n", 4).expect_err("ERR is an error");
        assert!(refused.0.contains("out of memory"), "{refused}");
        assert_eq!(parse_ready("READY 32000\n"), Ok(32000));
        assert!(parse_ready("ERR no model\n").is_err());
    }
}
