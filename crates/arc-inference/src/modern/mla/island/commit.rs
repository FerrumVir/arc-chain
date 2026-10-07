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

use super::wire::{Revealed, StageCommit};
use crate::modern::ModernError;
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

/// Everything committed for one sequence, by every stage.
#[derive(Debug, Clone)]
pub struct Ledger {
    n_layers: usize,
    /// Per position: the hash at every boundary `0 ..= L` once known.
    traces: Vec<Vec<Option<[u8; 32]>>>,
    /// Per stage range: per position, the hashes that stage committed.
    stages: BTreeMap<(u32, u32), Vec<Vec<[u8; 32]>>>,
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
            if commit.end_layer as usize > self.n_layers || commit.positions() != positions {
                self.malformed.push(format!(
                    "stage [{}, {}) committed {} hashes for {positions} positions",
                    range.0,
                    range.1,
                    commit.hashes.len()
                ));
                continue;
            }
            let per_stage = self.stages.entry(range).or_default();
            if per_stage.len() < start + positions {
                per_stage.resize(start + positions, Vec::new());
            }
            for i in 0..positions {
                let p = start + i;
                let hashes = commit.position(i);
                per_stage[p] = hashes.to_vec();
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

    /// The hashes stage `[a, b)` committed, per position.
    pub fn stage_commits(&self, first_layer: u32, end_layer: u32) -> Option<&[Vec<[u8; 32]>]> {
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
    /// position's committed hashes in order.
    pub fn stage_root(&self, seq: u64, first_layer: u32, end_layer: u32) -> Option<[u8; 32]> {
        let commits = self.stage_commits(first_layer, end_layer)?;
        let mut h = blake3::Hasher::new();
        h.update(STAGE_RECORD_DOMAIN);
        h.update(&seq.to_le_bytes());
        h.update(&first_layer.to_le_bytes());
        h.update(&end_layer.to_le_bytes());
        for position in commits {
            for hash in position {
                h.update(hash);
            }
        }
        Some(*h.finalize().as_bytes())
    }
}

/// The outcome of re-executing one stage.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// Every committed hash was reproduced.
    Valid { positions: usize },
    /// The revealed input does not hash to the committed input (the stage
    /// withheld or altered its activation log).
    InputMismatch { position: usize },
    /// The stage's committed hash at `boundary` differs from re-execution:
    /// the first faulty boundary of the first faulty position.
    Fault { position: usize, boundary: usize },
    /// The reveal does not match the commitments' shape, or re-execution
    /// failed.
    Refused(String),
}

/// Re-execute stage `model` (layers `[a, b)`, any package or sub-range that
/// holds them) on the inputs it revealed and compare every committed hash.
/// `committed` is what the ledger holds for that stage, per position.
pub fn audit_stage(
    model: &StageModel,
    revealed: &Revealed,
    committed: &[Vec<[u8; 32]>],
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
    let first = stage.has_embed();
    if (first && !revealed.inputs.is_empty())
        || (!first && revealed.inputs.len() != positions * c.d_model)
    {
        return Verdict::Refused("revealed inputs do not match the positions".into());
    }
    let mut cache = model.new_cache();
    let mut trace = Vec::new();
    for (p, &token) in revealed.tokens.iter().enumerate() {
        let input = if first {
            StageInput::Token(token)
        } else {
            StageInput::Hidden(&revealed.inputs[p * c.d_model..(p + 1) * c.d_model])
        };
        trace.clear();
        if let Err(e) = model.forward(input, &mut cache, Some(&mut trace)) {
            return Verdict::Refused(format!("re-execution failed at position {p}: {e}"));
        }
        let expected = &committed[p];
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
) -> Result<Vec<StageVerdict>, ModernError> {
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
            Some(committed) => audit_stage(model, r, committed),
            None => Verdict::Refused("the stage committed nothing".into()),
        };
        verdicts.push((range, verdict));
    }
    Ok(verdicts)
}
