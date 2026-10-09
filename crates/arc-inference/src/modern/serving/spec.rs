//! Speculative decoding that provably emits the plain greedy tokens.
//!
//! A drafter proposes `d_1..d_k`. The target runs `last, d_1, .., d_k` as one
//! batched step at positions `p..p+k` and returns logits `L_0..L_k`. Row `j`
//! is the forward pass of `d_j` on the prefix `.., last, d_1, .., d_{j-1}`.
//! By batch and phase invariance it equals plain decoding's forward pass for
//! the same prefix, bit for bit. The accept rule ([`accept`]) walks the rows
//! in order. At row `j` it selects the next token from `L_j` with the request's
//! own rule and history, exactly as plain decoding would, and emits it. It
//! continues to row `j + 1` only if that token equals `d_{j+1}`: only then is
//! row `j + 1` the pass plain decoding would run next. It stops at the first
//! mismatch, at EOS or at `max_tokens`. Every emitted token is therefore the
//! token plain decoding emits at that step, and the rows used are the passes
//! plain decoding runs, so even the logits hashes agree. The scheduler drops
//! the KV of the rows after the last one used.
//!
//! What the drafter proposes changes only the speed. The default drafter is
//! prompt lookup ([`PromptLookup`]): it continues the most recent earlier
//! occurrence of the context's last n-gram. It needs no second model, which
//! suits agent traffic that quotes its prompt (code edits, tool output, RAG).

use crate::modern::ModernError;
use crate::modern::arith::{self, Selection};

/// Proposes draft tokens. Correctness never depends on what it proposes.
pub trait Drafter {
    /// Up to `max` tokens predicted to follow `context` (prompt plus every
    /// token generated so far).
    fn propose(&self, context: &[u32], max: usize) -> Vec<u32>;
}

/// Prompt-lookup drafting: find the most recent earlier occurrence of the
/// context's last `n` tokens, longest `n` first, and propose what followed it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PromptLookup {
    /// Shortest suffix to match.
    pub min_ngram: usize,
    /// Longest suffix to match.
    pub max_ngram: usize,
}

impl Default for PromptLookup {
    fn default() -> Self {
        Self {
            min_ngram: 1,
            max_ngram: 4,
        }
    }
}

impl Drafter for PromptLookup {
    fn propose(&self, context: &[u32], max: usize) -> Vec<u32> {
        let len = context.len();
        if max == 0 {
            return Vec::new();
        }
        for n in (self.min_ngram.max(1)..=self.max_ngram).rev() {
            if len <= n {
                continue;
            }
            let pattern = &context[len - n..];
            for start in (0..len - n).rev() {
                if context[start..].starts_with(pattern) {
                    let from = start + n;
                    let to = (from + max).min(len);
                    return context[from..to].to_vec();
                }
            }
        }
        Vec::new()
    }
}

/// The outcome of verifying one speculative step.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Verified {
    /// Tokens plain greedy decoding emits next, in order (at least one).
    pub emitted: Vec<u32>,
    /// Rows whose logits were used (`emitted.len()`); their KV is kept.
    pub rows_used: usize,
    /// Generation ended (EOS or `max_tokens`).
    pub finished: bool,
}

/// The accept rule (see the module docs).
///
/// `logits[j]` belongs to the row whose input is the last emitted token for
/// `j = 0`, and `drafts[j - 1]` otherwise. `generated` holds the tokens emitted
/// before this step, which the repetition penalty reads.
pub fn accept(
    logits: &[Vec<i64>],
    drafts: &[u32],
    generated: &[u32],
    selection: Selection,
    eos: &[u32],
    max_tokens: usize,
) -> Result<Verified, ModernError> {
    if logits.is_empty() || logits.len() > drafts.len() + 1 {
        return Err(ModernError::Invalid(format!(
            "{} rows of logits for {} drafted tokens",
            logits.len(),
            drafts.len()
        )));
    }
    let mut history = generated.to_vec();
    let mut emitted = Vec::new();
    for (j, row) in logits.iter().enumerate() {
        let next = arith::select(row, &history, selection)?;
        history.push(next);
        emitted.push(next);
        let finished = eos.contains(&next) || history.len() >= max_tokens;
        if finished || drafts.get(j) != Some(&next) || j + 1 == logits.len() {
            return Ok(Verified {
                emitted,
                rows_used: j + 1,
                finished,
            });
        }
    }
    unreachable!("the loop returns at its last row")
}
