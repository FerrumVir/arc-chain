//! Per-stage hash commitments and their verification (research-6 §4.2).
//!
//! Every stage commits, for every position it runs, the activation hash at
//! each layer boundary it covers, its input boundary first. The coordinator
//! keeps them in a [`Ledger`] per sequence and checks the links: stage
//! `s + 1`'s input hash, computed from the bytes it received, must equal
//! stage `s`'s committed output hash. Because the engine is exact, the
//! per-boundary hashes do not depend on how the model is split, so a
//! sequence's [`Ledger::boundary_digests`] equal the single-process
//! generation's `boundary_digests` for every split.
//!
//! An audit ([`audit_stage`]) re-executes one stage from the inputs it
//! revealed, holding only that stage's weights, and compares every committed
//! hash. With exact arithmetic any mismatch is decisive: no thresholds.

use std::collections::BTreeMap;

use super::coordinator::Request;
use super::wire::{Revealed, StageCommit};
use crate::modern::ModernError;
use crate::modern::arith;
use crate::modern::mla::model::{StageInput, StageModel};

/// Domain of a stage record root.
pub const STAGE_RECORD_DOMAIN: &[u8] = b"arc.island.stage-record.v1";

/// A link between two stages whose hashes disagree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinkFault {
    pub position: usize,
    /// The boundary both stages committed.
    pub boundary: usize,
    /// The stage range that committed the output.
    pub upstream: (u32, u32),
    pub downstream: (u32, u32),
}

/// What one stage committed for one position.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PositionCommit {
    /// Activation hashes at the stage's boundaries, input first.
    pub hashes: Vec<[u8; 32]>,
    /// The logits hash (the last stage only).
    pub logits: Option<[u8; 32]>,
    /// The token selected from these logits (the last stage, at positions
    /// where a token was emitted).
    pub selected: Option<u32>,
}

/// Everything committed for one sequence, by every stage.
#[derive(Debug, Clone)]
pub struct Ledger {
    n_layers: usize,
    /// Per position: the hash at every boundary `0 ..= L` once known.
    traces: Vec<Vec<Option<[u8; 32]>>>,
    /// Per stage range: per position, what that stage committed.
    stages: BTreeMap<(u32, u32), Vec<PositionCommit>>,
    /// Link checks that failed.
    pub link_faults: Vec<LinkFault>,
    /// Shape violations (a stage committed the wrong number of hashes).
    pub malformed: Vec<String>,
}

impl Ledger {
    pub fn new(n_layers: usize) -> Self {
        Self {
            n_layers,
            traces: Vec::new(),
            stages: BTreeMap::new(),
            link_faults: Vec::new(),
            malformed: Vec::new(),
        }
    }

    /// Record the commits of one item covering positions `start ..`. Commits
    /// must arrive in stage order (as they do along the ring).
    pub fn record(&mut self, start: usize, positions: usize, commits: &[StageCommit]) {
        if self.traces.len() < start + positions {
            self.traces
                .resize_with(start + positions, || vec![None; self.n_layers + 1]);
        }
        let mut upstream: Option<(u32, u32)> = None;
        for commit in commits {
            let range = (commit.first_layer, commit.end_layer);
            let head = commit.end_layer as usize == self.n_layers;
            let head_shape = if head {
                commit.logits.len() == positions
            } else {
                commit.logits.is_empty() && commit.selected.is_none()
            };
            if commit.end_layer as usize > self.n_layers
                || commit.positions() != positions
                || !head_shape
            {
                self.malformed.push(format!(
                    "stage [{}, {}) committed {} hashes and {} logits for {positions} positions",
                    range.0,
                    range.1,
                    commit.hashes.len(),
                    commit.logits.len()
                ));
                continue;
            }
            let per_stage = self.stages.entry(range).or_default();
            if per_stage.len() < start + positions {
                per_stage.resize(start + positions, PositionCommit::default());
            }
            for i in 0..positions {
                let p = start + i;
                let hashes = commit.position(i);
                per_stage[p] = PositionCommit {
                    hashes: hashes.to_vec(),
                    logits: commit.logits.get(i).copied(),
                    selected: if i + 1 == positions {
                        commit.selected
                    } else {
                        None
                    },
                };
                for (k, hash) in hashes.iter().enumerate() {
                    let boundary = commit.first_layer as usize + k;
                    let slot = &mut self.traces[p][boundary];
                    match slot {
                        Some(existing) if existing != hash => {
                            self.link_faults.push(LinkFault {
                                position: p,
                                boundary,
                                upstream: upstream.unwrap_or(range),
                                downstream: range,
                            });
                        }
                        _ => *slot = Some(*hash),
                    }
                }
            }
            upstream = Some(range);
        }
    }

    /// Committed positions.
    pub fn positions(&self) -> usize {
        self.traces.len()
    }

    /// Whether every boundary of every position was committed and every
    /// link agreed.
    pub fn complete(&self) -> bool {
        self.link_faults.is_empty()
            && self.malformed.is_empty()
            && self.traces.iter().all(|t| t.iter().all(Option::is_some))
    }

    /// `boundary_digest` at boundaries `0 ..= L`: per boundary, BLAKE3 over
    /// the activation hash of every position in order (the same digest
    /// `StageModel::generate` reports). `None` until complete.
    pub fn boundary_digests(&self) -> Option<Vec<[u8; 32]>> {
        if !self.complete() {
            return None;
        }
        Some(
            (0..=self.n_layers)
                .map(|l| {
                    let mut h = blake3::Hasher::new();
                    for trace in &self.traces {
                        h.update(&trace[l].expect("complete"));
                    }
                    *h.finalize().as_bytes()
                })
                .collect(),
        )
    }

    /// What stage `[a, b)` committed, per position.
    pub fn stage_commits(&self, first_layer: u32, end_layer: u32) -> Option<&[PositionCommit]> {
        self.stages
            .get(&(first_layer, end_layer))
            .map(Vec::as_slice)
    }

    /// The stage ranges that committed.
    pub fn stage_ranges(&self) -> Vec<(u32, u32)> {
        self.stages.keys().copied().collect()
    }

    /// The root a stage would sign for this sequence (research-6 §4.2):
    /// BLAKE3 over the domain, the sequence id, the range, then every
    /// position's committed hashes in order (with, for the last stage, the
    /// logits hash and the selected token, if any).
    pub fn stage_root(&self, seq: u64, first_layer: u32, end_layer: u32) -> Option<[u8; 32]> {
        let commits = self.stage_commits(first_layer, end_layer)?;
        let mut h = blake3::Hasher::new();
        h.update(STAGE_RECORD_DOMAIN);
        h.update(&seq.to_le_bytes());
        h.update(&first_layer.to_le_bytes());
        h.update(&end_layer.to_le_bytes());
        for position in commits {
            for hash in &position.hashes {
                h.update(hash);
            }
            if let Some(logits) = &position.logits {
                h.update(logits);
            }
            match position.selected {
                Some(t) => {
                    h.update(&[1]);
                    h.update(&t.to_le_bytes());
                }
                None => {
                    h.update(&[0]);
                }
            }
        }
        Some(*h.finalize().as_bytes())
    }
}

/// Verifier-owned generation evidence, captured from the original request and
/// the outputs accepted by the coordinator. Never construct this from a reveal
/// or worker-supplied replacement request. Accepted tokens are the observed
/// output transcript, not a claim they have already passed arithmetic audits.
/// Every head selection is still independently re-executed below.
#[derive(Debug, Clone)]
pub struct AuditContext {
    request: Request,
    accepted_tokens: Vec<u32>,
}

impl AuditContext {
    pub fn new(request: &Request, accepted_tokens: &[u32]) -> Self {
        Self {
            request: request.clone(),
            accepted_tokens: accepted_tokens.to_vec(),
        }
    }

    fn validate(&self, model: &StageModel, positions: usize) -> Result<(), String> {
        let r = &self.request;
        let c = model.config();
        if r.prompt.is_empty()
            || r.max_tokens == 0
            || positions == 0
            || r.prompt
                .len()
                .checked_add(r.max_tokens)
                .is_none_or(|n| n > c.max_seq)
            || r.prompt
                .iter()
                .chain(&self.accepted_tokens)
                .any(|&t| t as usize >= c.vocab_size)
            || self.accepted_tokens.len() > r.max_tokens
        {
            return Err("missing or invalid trusted request/history".into());
        }
        // A committed generation contains the whole prompt and one forwarded
        // position for each accepted output except the final, unforwarded one.
        // Incomplete prefill is also auditable, but cannot have emitted tokens.
        let outputs = if positions < r.prompt.len() {
            0
        } else {
            positions - r.prompt.len() + 1
        };
        if self.accepted_tokens.len() != outputs {
            return Err("trusted output transcript does not cover committed positions".into());
        }
        if self
            .accepted_tokens
            .iter()
            .take(outputs.saturating_sub(1))
            .any(|t| r.eos.contains(t))
        {
            return Err("trusted history continues after EOS".into());
        }
        Ok(())
    }

    fn input_token(&self, position: usize) -> u32 {
        if position < self.request.prompt.len() {
            self.request.prompt[position]
        } else {
            self.accepted_tokens[position - self.request.prompt.len()]
        }
    }
}

/// The outcome of re-executing one stage.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// Every committed hash was reproduced.
    Valid { positions: usize },
    /// Reveal metadata attempts to redefine the verifier's request.
    RequestMismatch { field: &'static str },
    /// The revealed input does not hash to the committed input (the stage
    /// withheld or altered its activation log).
    InputMismatch { position: usize },
    /// The stage's committed hash at `boundary` differs from re-execution:
    /// the first faulty boundary of the first faulty position.
    Fault { position: usize, boundary: usize },
    /// The last stage's committed logits hash differs from re-execution.
    LogitsMismatch { position: usize },
    /// The last stage committed (and emitted) a token its selection rule
    /// does not give for these logits.
    WrongToken {
        position: usize,
        committed: u32,
        expected: u32,
    },
    /// The token committed at `position` is not the token the stage was
    /// fed at `position + 1`: the emitted token was altered on its way back.
    ForwardMismatch { position: usize },
    /// The reveal does not match the commitments' shape, or re-execution
    /// failed.
    Refused(String),
}

/// Re-execute stage `model` (layers `[a, b)`, any package or sub-range that
/// holds them) on the inputs it revealed and compare every committed hash.
/// For the last stage it also recomputes every logits hash and re-applies
/// the sequence's selection rule to every committed token (research-6
/// §4.2.2: the output tokens must equal the selection rule applied to the
/// logits). `committed` is what the verifier's ledger holds for that stage.
/// `trusted` is mandatory; no self-consistency fallback accepts reveal-owned
/// metadata or history. An absent/inconsistent transcript cannot return Valid.
pub fn audit_stage(
    model: &StageModel,
    revealed: &Revealed,
    committed: &[PositionCommit],
    trusted: &AuditContext,
) -> Verdict {
    let stage = model.stage();
    let c = model.config();
    if (stage.first_layer as u32, stage.end_layer as u32)
        != (revealed.first_layer, revealed.end_layer)
    {
        return Verdict::Refused("the model does not hold the revealed stage".into());
    }
    let positions = revealed.tokens.len();
    if positions != committed.len() {
        return Verdict::Refused(format!(
            "{positions} revealed positions, {} committed",
            committed.len()
        ));
    }
    if let Err(reason) = trusted.validate(model, positions) {
        return Verdict::Refused(reason);
    }
    let request = &trusted.request;
    for (differs, field) in [
        (revealed.seq != request.id, "sequence"),
        (
            revealed.prompt_len as usize != request.prompt.len(),
            "prompt boundary",
        ),
        (revealed.selection != request.selection, "selection"),
    ] {
        if differs {
            return Verdict::RequestMismatch { field };
        }
    }
    for (p, &token) in revealed.tokens.iter().enumerate() {
        if token != trusted.input_token(p) {
            return if p < request.prompt.len() {
                Verdict::RequestMismatch {
                    field: "prompt tokens",
                }
            } else {
                Verdict::ForwardMismatch { position: p - 1 }
            };
        }
    }
    let first = stage.has_embed();
    if (first && !revealed.inputs.is_empty())
        || (!first && revealed.inputs.len() != positions * c.d_model)
    {
        return Verdict::Refused("revealed inputs do not match the positions".into());
    }
    let head = stage.has_head(c);
    let prompt_len = request.prompt.len();
    let mut cache = model.new_cache();
    let mut trace = Vec::new();
    for (p, position_commit) in committed.iter().enumerate() {
        let token = trusted.input_token(p);
        let input = if first {
            StageInput::Token(token)
        } else {
            StageInput::Hidden(&revealed.inputs[p * c.d_model..(p + 1) * c.d_model])
        };
        trace.clear();
        let logits = match model.forward(input, &mut cache, Some(&mut trace)) {
            Ok((_, logits)) => logits,
            Err(e) => {
                return Verdict::Refused(format!("re-execution failed at position {p}: {e}"));
            }
        };
        let expected = &position_commit.hashes;
        if expected.len() != trace.len() {
            return Verdict::Refused(format!("position {p}: commitment shape"));
        }
        if expected[0] != trace[0] {
            return Verdict::InputMismatch { position: p };
        }
        if let Some(k) = (1..trace.len()).find(|&k| expected[k] != trace[k]) {
            return Verdict::Fault {
                position: p,
                boundary: stage.first_layer + k,
            };
        }
        if !head {
            if position_commit.logits.is_some() || position_commit.selected.is_some() {
                return Verdict::Refused(format!("position {p}: logits from a middle stage"));
            }
            continue;
        }
        let Some(logits) = logits else {
            return Verdict::Refused("the last stage produced no logits".into());
        };
        if position_commit.logits != Some(arith::logits_hash(&logits)) {
            return Verdict::LogitsMismatch { position: p };
        }
        let expects_output = p + 1 >= prompt_len;
        if position_commit.selected.is_some() != expects_output {
            return Verdict::Refused(format!(
                "position {p}: missing or unexpected selected token"
            ));
        }
        if let Some(emitted) = position_commit.selected {
            let output_index = p + 1 - prompt_len;
            let history = &trusted.accepted_tokens[..output_index];
            let reselected = match arith::select(&logits, history, request.selection) {
                Ok(t) => t,
                Err(e) => return Verdict::Refused(format!("re-selection failed at {p}: {e}")),
            };
            if reselected != emitted {
                return Verdict::WrongToken {
                    position: p,
                    committed: emitted,
                    expected: reselected,
                };
            }
            // Includes the final emitted token, which has no next input row.
            if trusted.accepted_tokens[output_index] != emitted {
                return Verdict::ForwardMismatch { position: p };
            }
        }
    }
    Verdict::Valid { positions }
}

/// A stage range and what its audit found.
pub type StageVerdict = ((u32, u32), Verdict);

/// Re-execute every stage `models` cover and return the first faulty one:
/// stages are independent, so each check needs only that stage's weights.
pub fn audit_all(
    ledger: &Ledger,
    revealed: &[Revealed],
    models: &[&StageModel],
    trusted: &AuditContext,
) -> Result<Vec<StageVerdict>, ModernError> {
    if !ledger.complete() || ledger.positions() == 0 {
        return Err(ModernError::Invalid(
            "audit requires a complete, nonempty ledger".into(),
        ));
    }
    let ranges = ledger.stage_ranges();
    let supplied: std::collections::BTreeSet<_> = revealed
        .iter()
        .map(|r| (r.first_layer, r.end_layer))
        .collect();
    if supplied.len() != revealed.len() || supplied.into_iter().collect::<Vec<_>>() != ranges {
        return Err(ModernError::Invalid(
            "audit requires exactly one reveal for every committed stage".into(),
        ));
    }
    let mut verdicts = Vec::new();
    for r in revealed {
        let range = (r.first_layer, r.end_layer);
        let model = models
            .iter()
            .find(|m| {
                let s = m.stage();
                (s.first_layer as u32, s.end_layer as u32) == range
            })
            .ok_or_else(|| {
                ModernError::Invalid(format!("no verifier holds [{}, {})", range.0, range.1))
            })?;
        let verdict = match ledger.stage_commits(range.0, range.1) {
            Some(committed) => audit_stage(model, r, committed, trusted),
            None => Verdict::Refused("the stage committed nothing".into()),
        };
        verdicts.push((range, verdict));
    }
    Ok(verdicts)
}
