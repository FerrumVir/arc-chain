//! Continuous batching: every iteration runs one batched step over all
//! running requests, so they share each weight read. Requests join and leave
//! between steps.
//!
//! A step holds, in this order, one decode row per decoding request (plus its
//! drafted tokens when speculation is on) and then prefill chunks of the
//! requests still reading their prompts, up to a row budget. Putting decode
//! first protects inter-token latency, and chunking prefill bounds how long
//! a long prompt can stall the streams already running. None of these choices
//! can change a token: every row is computed as if alone (the
//! [`BatchModel`] contract). Scheduling decides only when it is computed.
//!
//! Each request follows `ModernModel::generate` exactly: the same refusals at
//! submission, the same selection rule and history, and the same stop
//! conditions (EOS or `max_tokens`, the last token never forwarded). A request
//! that fails mid-flight is retired with its error; its cache is rolled back
//! and nothing else in the batch changes.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use super::prefix::{PrefixCache, PrefixConfig, PrefixStats};
use super::spec::{Drafter, PromptLookup, accept};
use super::{BatchModel, Row, SeqKv, StepOutput};
use crate::modern::ModernError;
use crate::modern::arith::{self, Selection};

/// A generation request in token ids, with the semantics of
/// `ModernModel::generate`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Request {
    /// Caller's identifier, echoed in the completion.
    pub id: u64,
    /// Prompt token ids.
    pub prompt: Vec<u32>,
    /// Generated tokens at most.
    pub max_tokens: usize,
    /// Stop tokens.
    pub eos: Vec<u32>,
    /// Selection rule.
    pub selection: Selection,
}

/// Scheduler settings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SchedulerConfig {
    /// Requests running together (the batch cap a latency target sets).
    pub max_running: usize,
    /// Rows per step: decode tokens, drafted tokens and prefill tokens.
    pub step_tokens: usize,
    /// Prompt tokens one request may prefill per step.
    pub prefill_chunk: usize,
    /// Compute and record the logits of every prompt position, as
    /// `ModernModel::generate` does (golden mode). This bypasses the prefix
    /// cache, whose hits skip those positions. Off, only the last prompt
    /// position runs the LM head.
    pub all_logits: bool,
    /// Most drafted tokens verified per decode step; 0 turns speculation off.
    /// Each request adapts its own draft length below this bound: it doubles
    /// after a step whose drafts were all accepted and drops to the accepted
    /// length plus one after a rejection. Outputs cannot change; only the
    /// number of rows verified does.
    pub draft_tokens: usize,
    /// The prefix cache, if any.
    pub prefix: Option<PrefixConfig>,
    /// Record each finished request's KV digest.
    pub kv_digests: bool,
}

impl Default for SchedulerConfig {
    fn default() -> Self {
        Self {
            max_running: 8,
            step_tokens: 256,
            prefill_chunk: 64,
            all_logits: false,
            draft_tokens: 0,
            prefix: None,
            kv_digests: false,
        }
    }
}

/// A finished request's output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Generated {
    /// Generated token ids.
    pub tokens: Vec<u32>,
    /// BLAKE3 of the tokens as little-endian `u32` (spec §6.2).
    pub output_hash: [u8; 32],
    /// Hashes of the logits this request computed, in order: every prompt
    /// position with `all_logits` (otherwise the last one), then one per
    /// decode pass, exactly as `ModernModel::generate` records them.
    pub logits_hashes: Vec<[u8; 32]>,
    /// Prompt tokens served from the prefix cache.
    pub cached_tokens: usize,
    /// Drafted tokens verified.
    pub drafted: usize,
    /// Drafted tokens accepted.
    pub accepted: usize,
    /// Decode steps the request took part in.
    pub decode_steps: usize,
    /// KV digest at the end, when `kv_digests` is on.
    pub kv_digest: Option<[u8; 32]>,
}

/// Wall-clock milestones of one request, measured from its submission.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Timing {
    /// Until admission.
    pub queued: Duration,
    /// Until the first generated token (time to first token).
    pub first_token: Duration,
    /// Until completion.
    pub total: Duration,
}

/// A retired request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Completion {
    /// The request's identifier.
    pub id: u64,
    /// The output, or the error that retired the request.
    pub result: Result<Generated, String>,
    /// Milestones.
    pub timing: Timing,
}

/// Counters of one step.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct StepStats {
    /// Rows in the step.
    pub rows: usize,
    /// Prompt rows.
    pub prefill_rows: usize,
    /// Decode rows (one per decoding request).
    pub decode_rows: usize,
    /// Drafted rows verified.
    pub draft_rows: usize,
    /// Tokens emitted.
    pub emitted: usize,
    /// Requests running.
    pub running: usize,
    /// Wall time of the batched forward pass.
    pub seconds: f64,
}

struct Active {
    request: Request,
    submitted: Instant,
    admitted: Instant,
    first_token: Option<Instant>,
    kv: SeqKv,
    cached: usize,
    generated: Vec<u32>,
    hashes: Vec<[u8; 32]>,
    drafted: usize,
    accepted: usize,
    decode_steps: usize,
    draft_window: usize,
    finished: bool,
    error: Option<String>,
}

/// Draft length a request starts with.
const FIRST_DRAFT_WINDOW: usize = 2;

impl Active {
    fn prefilling(&self) -> bool {
        self.kv.len() < self.request.prompt.len()
    }

    fn fail(&mut self, error: &ModernError) {
        self.error = Some(error.to_string());
        self.finished = true;
    }
}

enum Kind {
    Prefill { count: usize },
    Decode { drafts: Vec<u32> },
}

struct Plan {
    seq: usize,
    first_row: usize,
    kind: Kind,
}

/// The continuous-batching scheduler over one model.
pub struct Scheduler<'m, M: BatchModel> {
    model: &'m M,
    config: SchedulerConfig,
    drafter: Box<dyn Drafter>,
    prefix: Option<PrefixCache>,
    waiting: VecDeque<(Request, Instant)>,
    running: Vec<Active>,
    finished: Vec<Completion>,
    steps: Vec<StepStats>,
}

impl<'m, M: BatchModel> Scheduler<'m, M> {
    /// A scheduler with prompt-lookup drafting.
    pub fn new(model: &'m M, config: SchedulerConfig) -> Self {
        let prefix = config
            .prefix
            .map(|settings| PrefixCache::new(model.identity(), settings));
        Self {
            model,
            config,
            drafter: Box::new(PromptLookup::default()),
            prefix,
            waiting: VecDeque::new(),
            running: Vec::new(),
            finished: Vec::new(),
            steps: Vec::new(),
        }
    }

    /// Use another drafter. Outputs cannot change; only speed can.
    pub fn with_drafter(mut self, drafter: Box<dyn Drafter>) -> Self {
        self.drafter = drafter;
        self
    }

    /// Settings.
    pub fn config(&self) -> SchedulerConfig {
        self.config
    }

    /// Change the concurrency cap (a latency target's batch limit) between
    /// steps; requests already running finish normally.
    pub fn set_max_running(&mut self, max_running: usize) {
        self.config.max_running = max_running;
    }

    /// Change how many drafted tokens each decode step verifies (0 turns
    /// speculation off). Outputs cannot change; only speed can.
    pub fn set_draft_tokens(&mut self, draft_tokens: usize) {
        self.config.draft_tokens = draft_tokens;
    }

    /// Queue a request, or refuse it as `ModernModel::generate` would. A
    /// refusal never affects other requests.
    pub fn submit(&mut self, request: Request) -> Result<(), ModernError> {
        let max_positions = self.model.max_positions();
        let vocab = self.model.vocab_size();
        if request.prompt.is_empty() || request.max_tokens == 0 {
            return Err(ModernError::Invalid(
                "generation needs a non-empty prompt and max_tokens >= 1".into(),
            ));
        }
        if request.prompt.len() + request.max_tokens > max_positions {
            return Err(ModernError::Domain(format!(
                "{} prompt tokens + {} generated tokens exceed the {max_positions}-position context",
                request.prompt.len(),
                request.max_tokens
            )));
        }
        if let Some(&bad) = request.prompt.iter().find(|&&t| t as usize >= vocab) {
            return Err(ModernError::Domain(format!(
                "prompt token {bad} is outside the vocabulary"
            )));
        }
        self.waiting.push_back((request, Instant::now()));
        Ok(())
    }

    /// Whether nothing is queued or running.
    pub fn is_idle(&self) -> bool {
        self.waiting.is_empty() && self.running.is_empty()
    }

    /// Requests running.
    pub fn running(&self) -> usize {
        self.running.len()
    }

    /// Requests queued.
    pub fn waiting(&self) -> usize {
        self.waiting.len()
    }

    /// Prefix-cache counters, if the cache is on.
    pub fn prefix_stats(&self) -> Option<PrefixStats> {
        self.prefix.as_ref().map(PrefixCache::stats)
    }

    /// The prefix cache itself, if it is on (inspection and audits).
    pub fn prefix_cache_mut(&mut self) -> Option<&mut PrefixCache> {
        self.prefix.as_mut()
    }

    /// Every step so far.
    pub fn steps(&self) -> &[StepStats] {
        &self.steps
    }

    /// Take the requests retired so far.
    pub fn take_finished(&mut self) -> Vec<Completion> {
        std::mem::take(&mut self.finished)
    }

    /// Step until idle and return every completion.
    pub fn run(&mut self) -> Vec<Completion> {
        while self.step().is_some() {}
        self.take_finished()
    }

    /// One iteration: admit, run one batched step, emit tokens and retire
    /// finished requests. Returns `None` when there is nothing to run.
    pub fn step(&mut self) -> Option<StepStats> {
        self.admit();
        if self.running.is_empty() {
            return None;
        }
        let (rows, plans, mut stats) = self.plan();
        let started = Instant::now();
        let output = {
            let mut kvs: Vec<&mut SeqKv> = self.running.iter_mut().map(|a| &mut a.kv).collect();
            self.model.forward_rows(&rows, &mut kvs)
        };
        stats.seconds = started.elapsed().as_secs_f64();
        stats.emitted = self.apply(plans, output);
        self.retire_finished();
        self.steps.push(stats);
        Some(stats)
    }

    fn admit(&mut self) {
        while self.running.len() < self.config.max_running.max(1) {
            let Some((request, submitted)) = self.waiting.pop_front() else {
                break;
            };
            let mut kv = self.model.new_kv();
            let cached = match self.prefix.as_mut() {
                Some(cache) if !self.config.all_logits => {
                    cache.lookup_into(&request.prompt, request.prompt.len() - 1, &mut kv)
                }
                _ => 0,
            };
            self.running.push(Active {
                request,
                submitted,
                admitted: Instant::now(),
                first_token: None,
                kv,
                cached,
                generated: Vec::new(),
                hashes: Vec::new(),
                drafted: 0,
                accepted: 0,
                decode_steps: 0,
                draft_window: FIRST_DRAFT_WINDOW,
                finished: false,
                error: None,
            });
        }
    }

    /// Rows of the next step: decode rows (with drafts) first, then prefill.
    fn plan(&self) -> (Vec<Row>, Vec<Plan>, StepStats) {
        let max_positions = self.model.max_positions();
        let vocab = self.model.vocab_size();
        let mut rows: Vec<Row> = Vec::new();
        let mut plans: Vec<Plan> = Vec::new();
        let mut budget = self.config.step_tokens.max(1);
        let mut stats = StepStats {
            running: self.running.len(),
            ..StepStats::default()
        };
        for (seq, active) in self.running.iter().enumerate() {
            if active.prefilling() || budget == 0 {
                continue;
            }
            let Some(&last) = active.generated.last() else {
                continue;
            };
            let position = active.kv.len();
            let remaining = active
                .request
                .max_tokens
                .saturating_sub(active.generated.len());
            let room = max_positions.saturating_sub(position + 1);
            let k = self
                .config
                .draft_tokens
                .min(active.draft_window)
                .min(remaining.saturating_sub(1))
                .min(room)
                .min(budget - 1);
            let mut drafts = Vec::new();
            if k > 0 {
                let mut context = active.request.prompt.clone();
                context.extend_from_slice(&active.generated);
                drafts = self.drafter.propose(&context, k);
                drafts.truncate(k);
                if let Some(bad) = drafts.iter().position(|&t| t as usize >= vocab) {
                    drafts.truncate(bad);
                }
            }
            let first_row = rows.len();
            rows.push(Row {
                seq,
                token: last,
                position,
                logits: true,
            });
            for (j, &token) in drafts.iter().enumerate() {
                rows.push(Row {
                    seq,
                    token,
                    position: position + 1 + j,
                    logits: true,
                });
            }
            budget -= 1 + drafts.len();
            stats.decode_rows += 1;
            stats.draft_rows += drafts.len();
            plans.push(Plan {
                seq,
                first_row,
                kind: Kind::Decode { drafts },
            });
        }
        for (seq, active) in self.running.iter().enumerate() {
            if !active.prefilling() || budget == 0 {
                continue;
            }
            let prompt = &active.request.prompt;
            let start = active.kv.len();
            let count = (prompt.len() - start)
                .min(self.config.prefill_chunk.max(1))
                .min(budget);
            let first_row = rows.len();
            for (offset, &token) in prompt[start..start + count].iter().enumerate() {
                let position = start + offset;
                rows.push(Row {
                    seq,
                    token,
                    position,
                    logits: self.config.all_logits || position + 1 == prompt.len(),
                });
            }
            budget -= count;
            stats.prefill_rows += count;
            plans.push(Plan {
                seq,
                first_row,
                kind: Kind::Prefill { count },
            });
        }
        stats.rows = rows.len();
        (rows, plans, stats)
    }

    /// Fold a step's output into the requests; returns the tokens emitted.
    fn apply(&mut self, plans: Vec<Plan>, output: StepOutput) -> usize {
        let StepOutput {
            mut logits,
            mut errors,
            ..
        } = output;
        let all_logits = self.config.all_logits;
        let max_draft = self.config.draft_tokens.max(1);
        let mut emitted = 0usize;
        for plan in plans {
            let active = &mut self.running[plan.seq];
            let failure = errors.get_mut(plan.seq).and_then(Option::take);
            match plan.kind {
                Kind::Prefill { count } => {
                    // A prompt token the model refuses is refused by
                    // `generate` too, with the same error.
                    if let Some(failure) = failure {
                        active.fail(&failure.error);
                        continue;
                    }
                    let rows = &mut logits[plan.first_row..plan.first_row + count];
                    if all_logits {
                        for values in rows.iter().flatten() {
                            active.hashes.push(arith::logits_hash(values));
                        }
                    }
                    if active.prefilling() {
                        continue;
                    }
                    let Some(values) = rows[count - 1].take() else {
                        active.fail(&ModernError::Invalid(
                            "the last prompt row returned no logits".into(),
                        ));
                        continue;
                    };
                    if !all_logits {
                        active.hashes.push(arith::logits_hash(&values));
                    }
                    if let Some(cache) = self.prefix.as_mut() {
                        cache.insert(&active.request.prompt, &active.kv);
                    }
                    let request = &active.request;
                    match accept(
                        &[values],
                        &[],
                        &[],
                        request.selection,
                        &request.eos,
                        request.max_tokens,
                    ) {
                        Ok(verified) => {
                            active.first_token = Some(Instant::now());
                            emitted += verified.emitted.len();
                            active.generated.extend(verified.emitted);
                            active.finished = verified.finished;
                        }
                        Err(e) => active.fail(&e),
                    }
                }
                Kind::Decode { drafts } => {
                    let width = 1 + drafts.len();
                    // The rows before the first failing one, if any, are the
                    // passes plain decoding may run. The failing row matters
                    // only if plain decoding would run it: when it is the
                    // decode row itself, or when the accept rule reaches it.
                    let valid = failure.as_ref().map_or(width, |f| f.kept);
                    if valid == 0 {
                        if let Some(failure) = failure {
                            active.fail(&failure.error);
                        }
                        continue;
                    }
                    let used: Vec<Vec<i64>> = logits[plan.first_row..plan.first_row + valid]
                        .iter_mut()
                        .map(|slot| slot.take().unwrap_or_default())
                        .collect();
                    let before = active.kv.len().saturating_sub(valid);
                    let request = &active.request;
                    match accept(
                        &used,
                        &drafts[..valid - 1],
                        &active.generated,
                        request.selection,
                        &request.eos,
                        request.max_tokens,
                    ) {
                        Ok(verified) => {
                            // Plain decoding would feed the failing row's token
                            // next, and fail as that row did.
                            if let Some(failure) = failure
                                && verified.rows_used == valid
                                && !verified.finished
                                && verified.emitted.last() == drafts.get(valid - 1)
                            {
                                active.fail(&failure.error);
                                continue;
                            }
                            for values in &used[..verified.rows_used] {
                                active.hashes.push(arith::logits_hash(values));
                            }
                            active.kv.rollback(before + verified.rows_used);
                            let accepted = verified.rows_used - 1;
                            if !drafts.is_empty() {
                                active.draft_window = if accepted == drafts.len() {
                                    (active.draft_window * 2).min(max_draft)
                                } else {
                                    accepted + 1
                                };
                            }
                            active.drafted += drafts.len();
                            active.accepted += accepted;
                            active.decode_steps += 1;
                            emitted += verified.emitted.len();
                            active.generated.extend(verified.emitted);
                            active.finished = verified.finished;
                        }
                        Err(e) => active.fail(&e),
                    }
                }
            }
        }
        emitted
    }

    fn retire_finished(&mut self) {
        let mut index = 0;
        while index < self.running.len() {
            if self.running[index].finished {
                let active = self.running.remove(index);
                self.retire(active);
            } else {
                index += 1;
            }
        }
    }

    fn retire(&mut self, active: Active) {
        let now = Instant::now();
        let since = |t: Instant| t.saturating_duration_since(active.submitted);
        let timing = Timing {
            queued: since(active.admitted),
            first_token: since(active.first_token.unwrap_or(now)),
            total: since(now),
        };
        let result = match active.error {
            Some(error) => Err(error),
            None => {
                if let Some(cache) = self.prefix.as_mut() {
                    let mut sequence = active.request.prompt.clone();
                    sequence.extend_from_slice(&active.generated);
                    cache.insert(&sequence, &active.kv);
                }
                Ok(Generated {
                    output_hash: arith::tokens_hash(&active.generated),
                    kv_digest: self.config.kv_digests.then(|| active.kv.digest()),
                    tokens: active.generated,
                    logits_hashes: active.hashes,
                    cached_tokens: active.cached,
                    drafted: active.drafted,
                    accepted: active.accepted,
                    decode_steps: active.decode_steps,
                })
            }
        };
        self.finished.push(Completion {
            id: active.request.id,
            result,
            timing,
        });
    }
}
