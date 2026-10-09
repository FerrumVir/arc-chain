//! Asynchronous pipelined speculative decoding on the regional ring (ENG-8
//! prototype): many speculative passes of one answer in flight at once.
//!
//! Plain decoding keeps one position of an answer on the ring, so every
//! token pays every hop. Here the coordinator keeps up to `depth` passes of
//! `rows` positions on the ring: pass `i + 1` carries drafted tokens that
//! continue pass `i`'s drafts and leaves before pass `i` returns (PipeInfer's
//! continuous speculation). With `depth` 1 it is synchronous chain
//! speculation; with `depth` and `rows` 1 it is plain decoding.
//!
//! * **One item per position.** Every decode position travels as its own
//!   item, so the last stage commits a selection at every position, exactly
//!   as when it decodes that position alone.
//! * **Shared-prefix KV, no copies.** Every pass extends the sequence's one
//!   live cache on every stage. The speculative branch is the suffix beyond
//!   the verified length, and [`Frame::Rollback`] truncates it in place.
//! * **Cancellation.** When the target's selection at position `p` differs
//!   from the draft at `p + 1`, every position from `p + 1` on is dead. The
//!   coordinator marks every later pass cancelled (their results are
//!   ignored), sends `Rollback { keep: p + 1 }` and continues from the
//!   target's token. Links and stages are first-in first-out, so each stage
//!   applies the rollback after the dead passes and before the replacement.
//!   A stage that finds the rollback already queued skips the dead items
//!   instead of computing them ([`super::worker::skip_superseded`]).
//! * **Exactness.** A result is accepted only at a position whose input
//!   token is verified and whose cache holds only verified positions. The
//!   integer engine then gives that position the activation hashes, logits
//!   hash and selection of plain decoding, for any drafter, acceptance
//!   pattern, depth and stage split, and the ledger records nothing else.
//!   Drafts change speed, never bytes.

use std::collections::{HashMap, HashSet, VecDeque};
use std::time::Instant;

use super::commit::Ledger;
use super::coordinator::{Completion, Coordinator, Request, RunStats};
use super::wire::{Frame, Item};
use crate::modern::ModernError;

/// Proposes draft tokens. A drafter may be cheap, untrusted or wrong: drafts
/// only decide which positions are computed ahead of time, never which bytes
/// are committed.
pub trait Drafter {
    /// A short label for reports.
    fn name(&self) -> String;

    /// Called before each request is generated.
    fn begin(&mut self, _request: &Request) {}

    /// Up to `max` tokens to follow `context`: the prompt (`prompt_len`
    /// tokens), the verified output, then drafts already on the ring. Fewer,
    /// or none, is allowed; a token outside the vocabulary ends the draft.
    fn draft(&mut self, context: &[u32], prompt_len: usize, max: usize) -> Vec<u32>;
}

/// How one answer uses the ring.
#[derive(Debug, Clone)]
pub struct SpecConfig {
    /// Passes of one answer on the ring at once (`D >= 1`). With 1 the next
    /// pass leaves only after the previous one returned (synchronous).
    pub depth: usize,
    /// Positions per decode pass (`R >= 1`). Depth 1 with `R = k + 1`
    /// verifies a chain of `k` drafts per round trip; depth 1 with `R = 1`
    /// is plain decoding.
    pub rows: usize,
    /// Benchmark padding per position (emulates wider activations on a link).
    pub pad_bytes_per_position: u32,
    /// Drop finished sequences' activation logs (benchmarks); keep them for
    /// audits otherwise.
    pub forget_finished: bool,
}

impl Default for SpecConfig {
    fn default() -> Self {
        Self {
            depth: 1,
            rows: 1,
            pad_bytes_per_position: 0,
            forget_finished: false,
        }
    }
}

/// What speculation did during a run, over every answer.
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct SpecStats {
    /// Decode passes sent (the prefill pass is not counted).
    pub passes: u64,
    /// Positions those passes carried.
    pub positions: u64,
    /// Passes cancelled while on the ring: they followed a rejected draft or
    /// the end of the answer.
    pub cancelled_passes: u64,
    /// Positions sent and then discarded.
    pub cancelled_positions: u64,
    /// Rollback frames sent.
    pub rollbacks: u64,
    /// Draft tokens proposed and sent.
    pub drafted: u64,
    /// Drafts the target's selection confirmed.
    pub accepted: u64,
    /// Drafts the target's selection replaced.
    pub rejected: u64,
    /// The most passes of one answer on the ring at once.
    pub max_in_flight: u64,
    /// Time spent in the drafter.
    pub draft_seconds: f64,
}

impl SpecStats {
    /// Accepted drafts over verified drafts (`None` before any).
    pub fn acceptance(&self) -> Option<f64> {
        let verified = self.accepted + self.rejected;
        if verified == 0 {
            None
        } else {
            Some(self.accepted as f64 / verified as f64)
        }
    }
}

/// A frame of the answer on the ring, in send order: every link and stage is
/// first-in first-out, so frames come back in this order.
enum Sent {
    /// Positions `start .. start + count`, as one item (the prompt) or one
    /// item per position. `live` is false once an earlier result cancelled
    /// it.
    Pass {
        id: u64,
        start: usize,
        count: usize,
        prefill: bool,
        live: bool,
    },
    Rollback,
    Close,
}

/// Mark every live pass on the ring cancelled; returns how many there were.
fn cancel_all(ring: &mut VecDeque<Sent>) -> u64 {
    let mut cancelled = 0;
    for sent in ring.iter_mut() {
        if let Sent::Pass { live, .. } = sent
            && *live
        {
            *live = false;
            cancelled += 1;
        }
    }
    cancelled
}

fn kind(frame: &Frame) -> &'static str {
    match frame {
        Frame::Step { .. } => "step",
        Frame::Tree { .. } => "tree",
        Frame::Rollback { .. } => "rollback",
        Frame::Close { .. } => "close",
        Frame::Reveal { .. } => "reveal",
        Frame::Ping { .. } => "ping",
        Frame::Shutdown => "shutdown",
        Frame::Error { .. } => "error",
    }
}

/// The clock and counters of one run.
struct Tally {
    clock: Instant,
    stats: RunStats,
    spec: SpecStats,
}

impl Coordinator {
    /// Generate every request with up to `config.depth` speculative passes of
    /// `config.rows` positions on the ring, one answer at a time; completions
    /// are in request order. Tokens, logits hashes and every stage's
    /// commitments at every position equal plain decoding's
    /// ([`Coordinator::run`]) whatever `drafter` proposes. The ring must
    /// carry no other work meanwhile.
    pub fn run_speculative(
        &mut self,
        requests: &[Request],
        config: &SpecConfig,
        drafter: &mut dyn Drafter,
    ) -> Result<(Vec<Completion>, RunStats, SpecStats), ModernError> {
        if config.depth == 0 || config.rows == 0 {
            return Err(ModernError::Invalid(
                "speculation needs at least one pass in flight and one position per pass".into(),
            ));
        }
        let mut ids = HashSet::new();
        if let Some(repeated) = requests.iter().find(|r| !ids.insert(r.id)) {
            return Err(ModernError::Invalid(format!(
                "request id {} is repeated",
                repeated.id
            )));
        }
        let mut tally = Tally {
            clock: Instant::now(),
            stats: RunStats::default(),
            spec: SpecStats::default(),
        };
        let mut done = Vec::with_capacity(requests.len());
        for request in requests {
            done.push(self.speculate(request, config, drafter, &mut tally)?);
        }
        tally.stats.seconds = tally.clock.elapsed().as_secs_f64();
        Ok((done, tally.stats, tally.spec))
    }

    fn speculate(
        &mut self,
        request: &Request,
        config: &SpecConfig,
        drafter: &mut dyn Drafter,
        tally: &mut Tally,
    ) -> Result<Completion, ModernError> {
        let n_layers = self.config().n_layers;
        let vocab = self.config().vocab_size;
        let mut completion = Completion {
            id: request.id,
            tokens: Vec::new(),
            logits_hashes: Vec::new(),
            ledger: Ledger::new(n_layers),
            error: self.refusal(request),
            admitted_at: tally.clock.elapsed().as_secs_f64(),
            first_token_at: 0.0,
            token_times: Vec::new(),
            finished_at: 0.0,
        };
        if completion.error.is_some() {
            return Ok(completion);
        }
        drafter.begin(request);
        let prompt_len = request.prompt.len();
        // Plain decoding forwards the prompt and every output but the last.
        let horizon = prompt_len + request.max_tokens - 1;
        let make_item = |position: usize, tokens: Vec<u32>| {
            let mut item = Item::new(
                request.id,
                position as u32,
                prompt_len as u32,
                request.selection,
                tokens,
            );
            item.pad = config.pad_bytes_per_position * item.tokens.len() as u32;
            item
        };
        // The prompt, the verified output, then drafts on the ring.
        let mut tokens = request.prompt.clone();
        // Positions `[0, sent)` were sent and not rolled back.
        let mut sent = 0usize;
        let mut ring: VecDeque<Sent> = VecDeque::new();
        let mut live = 0usize;
        let mut finished = false;
        let mut next_id = 0u64;
        loop {
            // Keep `depth` passes on the ring while there is anything to send.
            while !finished && live < config.depth && sent < horizon {
                let prefill = sent == 0;
                let count = if prefill {
                    prompt_len
                } else {
                    let want = config.rows.min(horizon - sent);
                    if tokens.len() < sent + want {
                        let need = sent + want - tokens.len();
                        let timer = Instant::now();
                        let mut drafted = drafter.draft(&tokens, prompt_len, need);
                        tally.spec.draft_seconds += timer.elapsed().as_secs_f64();
                        drafted.truncate(need);
                        if let Some(bad) = drafted.iter().position(|&t| t as usize >= vocab) {
                            drafted.truncate(bad);
                        }
                        tally.spec.drafted += drafted.len() as u64;
                        tokens.extend_from_slice(&drafted);
                    }
                    want.min(tokens.len() - sent)
                };
                if count == 0 {
                    break;
                }
                let items = if prefill {
                    vec![make_item(0, tokens[..prompt_len].to_vec())]
                } else {
                    (sent..sent + count)
                        .map(|p| make_item(p, vec![tokens[p]]))
                        .collect()
                };
                let id = next_id;
                next_id += 1;
                self.send(&Frame::Step { id, items }, &mut tally.stats)?;
                tally.stats.forwarded_positions += count as u64;
                if !prefill {
                    tally.spec.passes += 1;
                    tally.spec.positions += count as u64;
                }
                ring.push_back(Sent::Pass {
                    id,
                    start: sent,
                    count,
                    prefill,
                    live: true,
                });
                live += 1;
                tally.spec.max_in_flight = tally.spec.max_in_flight.max(live as u64);
                sent += count;
            }
            let Some(expected) = ring.pop_front() else {
                break;
            };
            let frame = self.recv(&mut tally.stats)?;
            let now = tally.clock.elapsed().as_secs_f64();
            let (start, count, prefill, items) = match (expected, frame) {
                (Sent::Rollback, Frame::Rollback { .. }) | (Sent::Close, Frame::Close { .. }) => {
                    continue;
                }
                (
                    Sent::Pass {
                        id, live: false, ..
                    },
                    Frame::Step { id: got, .. },
                ) if got == id => {
                    continue;
                }
                (
                    Sent::Pass {
                        id,
                        start,
                        count,
                        prefill,
                        ..
                    },
                    Frame::Step { id: got, items },
                ) if got == id => (start, count, prefill, items),
                (_, frame) => {
                    return Err(ModernError::Invalid(format!(
                        "the ring returned an unexpected {} frame",
                        kind(&frame)
                    )));
                }
            };
            live -= 1;
            let mut failure: Option<String> = None;
            let mut cancel_at = None;
            let expected_items = if prefill { 1 } else { count };
            if items.len() != expected_items {
                failure = Some("a pass came back with missing or extra items".into());
            }
            // A pass whose shape is wrong is not read at all.
            let readable = if failure.is_none() { items } else { Vec::new() };
            for (j, item) in readable.into_iter().enumerate() {
                let (position, n) = if prefill { (0, count) } else { (start + j, 1) };
                if item.seq != request.id
                    || item.start as usize != position
                    || item.tokens[..] != tokens[position..position + n]
                    || item.prompt_len as usize != prompt_len
                    || item.selection != request.selection
                {
                    failure = Some("returned item changed trusted request/input metadata".into());
                    break;
                }
                if let Some(e) = item.error {
                    failure = Some(e);
                    break;
                }
                completion.ledger.record(position, n, &item.commits);
                // The token and logits come from the last stage's commitment,
                // the record its audit checks.
                let head = item
                    .commits
                    .last()
                    .filter(|h| h.end_layer as usize == n_layers && h.logits.len() == n);
                let Some(head) = head else {
                    failure = Some("the last stage committed no logits".into());
                    break;
                };
                completion.logits_hashes.extend_from_slice(&head.logits);
                let Some(token) = head.selected else {
                    failure = Some("the last stage returned no token".into());
                    break;
                };
                // `token` is the target's token at position `at`: it confirms
                // or replaces the draft sent there.
                let at = position + n;
                let rejected = match tokens.get(at) {
                    Some(&draft) if draft == token => {
                        tally.spec.accepted += 1;
                        false
                    }
                    Some(_) => {
                        tally.spec.rejected += 1;
                        tokens.truncate(at);
                        tokens.push(token);
                        true
                    }
                    None => {
                        tokens.push(token);
                        false
                    }
                };
                if completion.tokens.is_empty() {
                    completion.first_token_at = now;
                }
                completion.tokens.push(token);
                completion.token_times.push(now);
                tally.stats.generated_tokens += 1;
                finished =
                    request.eos.contains(&token) || completion.tokens.len() == request.max_tokens;
                if rejected || finished {
                    cancel_at = Some(at);
                    break;
                }
            }
            if let Some(reason) = failure {
                completion.error = Some(reason);
                finished = true;
                tally.spec.cancelled_passes += cancel_all(&mut ring);
                live = 0;
            } else if let Some(at) = cancel_at {
                // Everything sent from `at` on ran on a rejected draft or lies
                // past the end of the answer.
                tally.spec.cancelled_passes += cancel_all(&mut ring);
                live = 0;
                if sent > at {
                    tally.spec.cancelled_positions += (sent - at) as u64;
                    tally.spec.rollbacks += 1;
                    let rollback = Frame::Rollback {
                        seq: request.id,
                        keep: at as u32,
                    };
                    self.send(&rollback, &mut tally.stats)?;
                    ring.push_back(Sent::Rollback);
                    sent = at;
                }
            }
            if finished {
                completion.finished_at = now;
                let close = Frame::Close {
                    seqs: vec![request.id],
                    forget: config.forget_finished,
                };
                self.send(&close, &mut tally.stats)?;
                ring.push_back(Sent::Close);
            }
        }
        if !finished {
            // Unreachable: with nothing on the ring an unfinished answer
            // always holds a verified token to send.
            return Err(ModernError::Invalid("speculative decoding stalled".into()));
        }
        Ok(completion)
    }
}

/// Prompt-lookup (suffix) drafting without weights: find the most recent
/// earlier occurrence of the context's last `n` tokens, longest `n` first,
/// and propose what followed it. A match that runs into the end of the
/// context continues through its own proposals, extending a repeating cycle.
#[derive(Debug, Clone)]
pub struct NgramDrafter {
    pub min_ngram: usize,
    pub max_ngram: usize,
}

impl Default for NgramDrafter {
    fn default() -> Self {
        Self {
            min_ngram: 1,
            max_ngram: 4,
        }
    }
}

impl Drafter for NgramDrafter {
    fn name(&self) -> String {
        format!("ngram {}-{}", self.min_ngram, self.max_ngram)
    }

    fn draft(&mut self, context: &[u32], _prompt_len: usize, max: usize) -> Vec<u32> {
        let len = context.len();
        let longest = self.max_ngram.min(len.saturating_sub(1));
        for n in (self.min_ngram.max(1)..=longest).rev() {
            let key = &context[len - n..];
            let Some(at) = context[..len - 1].windows(n).rposition(|w| w == key) else {
                continue;
            };
            let mut drafts = Vec::with_capacity(max);
            for k in 0..max {
                let source = at + n + k;
                let token = if source < len {
                    context[source]
                } else {
                    drafts[source - len]
                };
                drafts.push(token);
            }
            return drafts;
        }
        Vec::new()
    }
}

/// Whether a scripted draft equals the target's token.
#[derive(Debug, Clone)]
pub enum Script {
    /// Each draft is right with probability `rate`, from a fixed hash of
    /// (seed, request, output index): independent, reproducible acceptance.
    Rate { rate: f64, seed: u64 },
    /// The draft for output `i` is right when `pattern[i % pattern.len()]`
    /// (an empty pattern is never right).
    Pattern(Vec<bool>),
}

impl Script {
    /// Whether the draft for output `index` of `request` is the target's
    /// token.
    pub fn right(&self, request: u64, index: usize) -> bool {
        match self {
            Script::Rate { rate, seed } => unit(mix(*seed, request, index as u64)) < *rate,
            Script::Pattern(pattern) => !pattern.is_empty() && pattern[index % pattern.len()],
        }
    }
}

/// SplitMix64 of (seed, request, index): a fixed, well-spread hash.
fn mix(seed: u64, request: u64, index: u64) -> u64 {
    let mut z = seed
        ^ request.wrapping_mul(0x9e37_79b9_7f4a_7c15)
        ^ index.wrapping_mul(0xc2b2_ae3d_27d4_eb4f);
    z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    z ^ (z >> 31)
}

/// The top 53 bits of `bits` as a value in `[0, 1)`.
fn unit(bits: u64) -> f64 {
    (bits >> 11) as f64 / (1u64 << 53) as f64
}

/// A test drafter with controllable acceptance. It holds every request's
/// plain-decoding output and proposes the target's next token where its
/// script says so and a different token elsewhere, so the acceptance
/// pattern along the verified path is exactly the script's.
#[derive(Debug, Clone)]
pub struct ScriptedDrafter {
    truth: HashMap<u64, Vec<u32>>,
    vocab: u32,
    script: Script,
    current: Option<u64>,
}

impl ScriptedDrafter {
    /// `truth` maps request ids to their outputs under plain decoding.
    pub fn new(truth: HashMap<u64, Vec<u32>>, vocab_size: usize, script: Script) -> Self {
        Self {
            truth,
            vocab: vocab_size.max(2) as u32,
            script,
            current: None,
        }
    }
}

impl Drafter for ScriptedDrafter {
    fn name(&self) -> String {
        match &self.script {
            Script::Rate { rate, .. } => format!("scripted acceptance {rate}"),
            Script::Pattern(pattern) => {
                let bits: String = pattern.iter().map(|&b| if b { '1' } else { '0' }).collect();
                format!("scripted pattern {bits}")
            }
        }
    }

    fn begin(&mut self, request: &Request) {
        self.current = Some(request.id);
    }

    fn draft(&mut self, context: &[u32], prompt_len: usize, max: usize) -> Vec<u32> {
        let Some(request) = self.current else {
            return Vec::new();
        };
        let Some(truth) = self.truth.get(&request) else {
            return Vec::new();
        };
        let output = context.get(prompt_len..).unwrap_or_default();
        // Drafts follow the target only while the context does.
        let mut on_path = truth.starts_with(output);
        let mut drafts = Vec::with_capacity(max);
        for (index, &target) in truth.iter().enumerate().skip(output.len()).take(max) {
            on_path = on_path && self.script.right(request, index);
            drafts.push(if on_path {
                target
            } else {
                (target + 1) % self.vocab
            });
        }
        drafts
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::modern::arith::Selection;

    fn request(id: u64) -> Request {
        Request {
            id,
            prompt: vec![1, 2],
            max_tokens: 8,
            eos: Vec::new(),
            selection: Selection::Argmax,
        }
    }

    #[test]
    fn ngram_drafts_continue_the_latest_match_and_extend_cycles() {
        let mut d = NgramDrafter::default();
        // "1 2" last occurred at the start, followed by 3, 1, 2, then the
        // cycle continues through the proposals themselves.
        assert_eq!(d.draft(&[1, 2, 3, 1, 2], 2, 5), vec![3, 1, 2, 3, 1]);
        // Only the one-token suffix "2" recurs: what followed it is proposed.
        assert_eq!(d.draft(&[7, 2, 5, 9, 2], 1, 2), vec![5, 9]);
        // No earlier occurrence, too little context, or nothing asked.
        assert!(d.draft(&[1, 2, 3], 1, 4).is_empty());
        assert!(d.draft(&[4], 1, 4).is_empty());
        assert!(d.draft(&[1, 2, 1, 2], 2, 0).is_empty());
    }

    #[test]
    fn scripted_drafts_follow_the_script_on_the_true_path_only() {
        let truth = HashMap::from([(7, vec![10, 11, 12, 13, 14, 15])]);
        let script = Script::Pattern(vec![true, true, false]);
        let mut d = ScriptedDrafter::new(truth, 50, script);
        d.begin(&request(7));
        // Outputs 0 and 1 are right, 2 is wrong, and so is everything after
        // it (the context is then off the target's path).
        assert_eq!(d.draft(&[1, 2], 2, 4), vec![10, 11, 13, 14]);
        // Once the target corrected output 2, drafting follows it again.
        assert_eq!(d.draft(&[1, 2, 10, 11, 12], 2, 2), vec![13, 14]);
        // A context off the path only gets wrong drafts; past the end, none.
        assert_eq!(d.draft(&[1, 2, 10, 99], 2, 2), vec![13, 14]);
        assert!(d.draft(&[1, 2, 10, 11, 12, 13, 14, 15], 2, 3).is_empty());
        d.begin(&request(8));
        assert!(d.draft(&[1, 2], 2, 3).is_empty());
        // Rates are independent per output and close to nominal.
        let rate = Script::Rate { rate: 0.7, seed: 4 };
        let right = (0..20_000).filter(|&i| rate.right(3, i)).count();
        assert!((13_400..=14_600).contains(&right), "{right}");
        assert!(!Script::Pattern(Vec::new()).right(1, 0));
    }
}
