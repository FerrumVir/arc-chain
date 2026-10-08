//! The island coordinator (the ingress, on the first stage's machine): it
//! admits requests, schedules them through the ring in micro-batches, turns
//! the last stage's selected tokens into the next step, and keeps every
//! stage's commitments in a per-sequence [`Ledger`].
//!
//! Scheduling never changes a byte. Each sequence's positions run through
//! each stage in order, on that sequence's own cache, and every stage
//! processes the items of a frame one after another, so a sequence's tokens,
//! logits hashes and commitments are the same alone, in any micro-batch, at
//! any concurrency, and with any number of micro-batches in flight.

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::sync::mpsc::Receiver;
use std::time::{Duration, Instant};

use super::commit::Ledger;
use super::transport::{Listener, Transport};
use super::wire::{Frame, Item, Revealed, TreeNode};
use super::worker::{Downstream, incoming};
use crate::modern::ModernError;
use crate::modern::arith::{self, Selection};
use crate::modern::mla::config::MlaConfig;

/// One generation request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Request {
    /// Unique within a run; it is the sequence id on the ring.
    pub id: u64,
    pub prompt: Vec<u32>,
    pub max_tokens: usize,
    pub eos: Vec<u32>,
    pub selection: Selection,
}

/// What a request produced.
#[derive(Debug, Clone)]
pub struct Completion {
    pub id: u64,
    pub tokens: Vec<u32>,
    /// One logits hash per forwarded position (prompt and generated).
    pub logits_hashes: Vec<[u8; 32]>,
    pub ledger: Ledger,
    pub error: Option<String>,
    /// Seconds from the start of the run.
    pub admitted_at: f64,
    pub first_token_at: f64,
    /// When each generated token came back (seconds from the run's start).
    pub token_times: Vec<f64>,
    pub finished_at: f64,
}

impl Completion {
    pub fn output_hash(&self) -> [u8; 32] {
        arith::tokens_hash(&self.tokens)
    }

    pub fn logits_digest(&self) -> [u8; 32] {
        arith::logits_digest(&self.logits_hashes)
    }

    /// Decode speed of this answer: generated tokens after the first, over
    /// the time after the first.
    pub fn decode_tokens_per_second(&self) -> Option<f64> {
        let span = self.finished_at - self.first_token_at;
        (self.tokens.len() > 1 && span > 0.0).then(|| (self.tokens.len() - 1) as f64 / span)
    }
}

/// How requests share the ring.
#[derive(Debug, Clone)]
pub struct Schedule {
    /// Micro-batches in flight at once (G). With G >= stages every stage can
    /// be busy at the same time.
    pub micro_batches: usize,
    /// Sequences in flight at once (B), spread over the micro-batches.
    pub concurrency: usize,
    /// Prompt positions per prefill item; 0 sends the whole prompt at once.
    pub prefill_chunk: usize,
    /// Benchmark padding per position (emulates wider activations on a link).
    pub pad_bytes_per_position: u32,
    /// Drop finished sequences' activation logs (benchmarks); keep them for
    /// audits otherwise.
    pub forget_finished: bool,
}

impl Default for Schedule {
    fn default() -> Self {
        Self {
            micro_batches: 1,
            concurrency: 1,
            prefill_chunk: 0,
            pad_bytes_per_position: 0,
            forget_finished: false,
        }
    }
}

/// Counters of one run.
#[derive(Debug, Clone, Default)]
pub struct RunStats {
    pub seconds: f64,
    pub frames: u64,
    /// Bytes of the frames sent to the first stage.
    pub bytes_sent: u64,
    /// Bytes of the frames returned by the last stage.
    pub bytes_received: u64,
    pub generated_tokens: u64,
    pub forwarded_positions: u64,
}

struct Live {
    index: usize,
    next_start: usize,
    next_tokens: Vec<u32>,
}

/// The coordinator of one island.
pub struct Coordinator {
    downstream: Downstream,
    returns: Receiver<Vec<u8>>,
    config: MlaConfig,
    /// How long to wait for any frame to come back.
    pub timeout: Duration,
    next_ping: u64,
}

impl Coordinator {
    /// Frames go to `first_stage`; the last stage sends results to
    /// `listener`.
    pub fn new(
        transport: Arc<dyn Transport>,
        first_stage: String,
        listener: Box<dyn Listener>,
        config: MlaConfig,
    ) -> Self {
        Self {
            downstream: Downstream::new(transport, first_stage),
            returns: incoming(listener),
            config,
            timeout: Duration::from_secs(300),
            next_ping: 0,
        }
    }

    fn send(&mut self, frame: &Frame, stats: &mut RunStats) -> Result<(), ModernError> {
        let bytes = frame.encode();
        stats.frames += 1;
        stats.bytes_sent += bytes.len() as u64;
        self.downstream.send(&bytes)
    }

    fn recv(&mut self, stats: &mut RunStats) -> Result<Frame, ModernError> {
        let bytes = self
            .returns
            .recv_timeout(self.timeout)
            .map_err(|_| ModernError::Io("no frame came back from the island in time".into()))?;
        stats.bytes_received += bytes.len() as u64;
        match Frame::decode(&bytes)? {
            Frame::Error { stage, message } => Err(ModernError::Invalid(format!(
                "stage {stage} refused a frame: {message}"
            ))),
            frame => Ok(frame),
        }
    }

    /// The refusals `StageModel::generate` makes before any forward call.
    fn refusal(&self, request: &Request) -> Option<String> {
        let c = &self.config;
        if request.prompt.is_empty() || request.max_tokens == 0 {
            return Some("generation needs a non-empty prompt and max_tokens >= 1".into());
        }
        if request.prompt.len() + request.max_tokens > c.max_seq {
            return Some(format!(
                "{} prompt tokens + {} generated tokens exceed the {}-position context",
                request.prompt.len(),
                request.max_tokens,
                c.max_seq
            ));
        }
        request
            .prompt
            .iter()
            .find(|&&t| t as usize >= c.vocab_size)
            .map(|bad| format!("prompt token {bad} is outside the vocabulary"))
    }

    /// Generate every request; completions are in request order.
    pub fn run(
        &mut self,
        requests: &[Request],
        schedule: &Schedule,
    ) -> Result<(Vec<Completion>, RunStats), ModernError> {
        self.run_with(requests, schedule, &mut |_| Ok(()))
    }

    /// [`Coordinator::run`], calling `quiescent(steps)` whenever a step has
    /// come back and nothing is in flight on the ring (`steps` counts the
    /// steps returned so far). A stage may be stopped and restarted there:
    /// that is how the forced-restart tests crash a stage mid-generation.
    pub fn run_with(
        &mut self,
        requests: &[Request],
        schedule: &Schedule,
        quiescent: &mut dyn FnMut(usize) -> Result<(), ModernError>,
    ) -> Result<(Vec<Completion>, RunStats), ModernError> {
        let groups = schedule.micro_batches.max(1);
        let concurrency = schedule.concurrency.max(1);
        let mut by_seq = HashMap::new();
        for (i, r) in requests.iter().enumerate() {
            if by_seq.insert(r.id, i).is_some() {
                return Err(ModernError::Invalid(format!(
                    "request id {} is repeated",
                    r.id
                )));
            }
        }
        let start = Instant::now();
        let mut stats = RunStats::default();
        let mut completions: Vec<Completion> = requests
            .iter()
            .map(|r| Completion {
                id: r.id,
                tokens: Vec::new(),
                logits_hashes: Vec::new(),
                ledger: Ledger::new(self.config.n_layers),
                error: self.refusal(r),
                admitted_at: 0.0,
                first_token_at: 0.0,
                token_times: Vec::new(),
                finished_at: 0.0,
            })
            .collect();
        let mut queue: VecDeque<usize> = (0..requests.len())
            .filter(|&i| completions[i].error.is_none())
            .collect();
        let mut groups_live: Vec<Vec<Live>> = (0..groups).map(|_| Vec::new()).collect();
        let mut in_flight = vec![false; groups];
        let mut live = 0usize;
        let mut outstanding = 0usize;
        let mut steps = 0usize;
        let mut pause = false;
        let chunk = |r: &Request, from: usize| -> Vec<u32> {
            let n = if schedule.prefill_chunk == 0 {
                r.prompt.len()
            } else {
                schedule.prefill_chunk
            };
            r.prompt[from..(from + n).min(r.prompt.len())].to_vec()
        };
        loop {
            if pause && outstanding == 0 {
                quiescent(steps)?;
            }
            pause = false;
            // Fill and launch every idle micro-batch, round-robin.
            let mut admitted = true;
            while admitted {
                admitted = false;
                for g in 0..groups {
                    if in_flight[g] || live >= concurrency {
                        continue;
                    }
                    if let Some(i) = queue.pop_front() {
                        completions[i].admitted_at = start.elapsed().as_secs_f64();
                        groups_live[g].push(Live {
                            index: i,
                            next_start: 0,
                            next_tokens: chunk(&requests[i], 0),
                        });
                        live += 1;
                        admitted = true;
                    }
                }
            }
            for g in 0..groups {
                if in_flight[g] || groups_live[g].is_empty() {
                    continue;
                }
                let items = groups_live[g]
                    .iter()
                    .map(|l| {
                        let r = &requests[l.index];
                        let mut item = Item::new(
                            r.id,
                            l.next_start as u32,
                            r.prompt.len() as u32,
                            r.selection,
                            l.next_tokens.clone(),
                        );
                        item.pad = schedule.pad_bytes_per_position * l.next_tokens.len() as u32;
                        stats.forwarded_positions += l.next_tokens.len() as u64;
                        item
                    })
                    .collect();
                self.send(
                    &Frame::Step {
                        id: g as u64,
                        items,
                    },
                    &mut stats,
                )?;
                in_flight[g] = true;
                outstanding += 1;
            }
            if outstanding == 0 {
                break;
            }
            match self.recv(&mut stats)? {
                Frame::Step { id, items } => {
                    let g = id as usize;
                    if g >= groups || !in_flight[g] {
                        return Err(ModernError::Invalid(format!("unexpected micro-batch {id}")));
                    }
                    in_flight[g] = false;
                    outstanding -= 1;
                    steps += 1;
                    pause = true;
                    let now = start.elapsed().as_secs_f64();
                    let mut finished = Vec::new();
                    for item in items {
                        let i = *by_seq.get(&item.seq).ok_or_else(|| {
                            ModernError::Invalid(format!("unknown sequence {}", item.seq))
                        })?;
                        let r = &requests[i];
                        let l = groups_live[g]
                            .iter_mut()
                            .find(|l| l.index == i)
                            .ok_or_else(|| {
                                ModernError::Invalid(format!(
                                    "sequence {} is not in micro-batch {g}",
                                    item.seq
                                ))
                            })?;
                        let comp = &mut completions[i];
                        if item.start as usize != l.next_start
                            || item.tokens != l.next_tokens
                            || item.prompt_len as usize != r.prompt.len()
                            || item.selection != r.selection
                        {
                            comp.error =
                                Some("returned item changed trusted request/input metadata".into());
                            finished.push(i);
                            continue;
                        }
                        let n = item.tokens.len();
                        if let Some(e) = item.error {
                            comp.error = Some(e);
                            finished.push(i);
                            continue;
                        }
                        comp.ledger.record(item.start as usize, n, &item.commits);
                        // The token and logits come from the last stage's
                        // commitment, the record its audit checks.
                        let head = item.commits.last().filter(|h| {
                            h.end_layer as usize == self.config.n_layers && h.logits.len() == n
                        });
                        let Some(head) = head else {
                            comp.error = Some("the last stage committed no logits".into());
                            finished.push(i);
                            continue;
                        };
                        comp.logits_hashes.extend_from_slice(&head.logits);
                        let selected = head.selected;
                        l.next_start += n;
                        if l.next_start < r.prompt.len() {
                            l.next_tokens = chunk(r, l.next_start);
                            continue;
                        }
                        let Some(token) = selected else {
                            comp.error = Some("the last stage returned no token".into());
                            finished.push(i);
                            continue;
                        };
                        if comp.tokens.is_empty() {
                            comp.first_token_at = now;
                        }
                        comp.tokens.push(token);
                        comp.token_times.push(now);
                        stats.generated_tokens += 1;
                        if r.eos.contains(&token) || comp.tokens.len() == r.max_tokens {
                            finished.push(i);
                        } else {
                            l.next_tokens = vec![token];
                        }
                    }
                    if !finished.is_empty() {
                        for &i in &finished {
                            completions[i].finished_at = now;
                        }
                        groups_live[g].retain(|l| !finished.contains(&l.index));
                        live -= finished.len();
                        let seqs = finished.iter().map(|&i| requests[i].id).collect();
                        self.send(
                            &Frame::Close {
                                seqs,
                                forget: schedule.forget_finished,
                            },
                            &mut stats,
                        )?;
                        outstanding += 1;
                    }
                }
                Frame::Close { .. } => outstanding -= 1,
                other => {
                    return Err(ModernError::Invalid(format!(
                        "unexpected frame during a run: {other:?}"
                    )));
                }
            }
        }
        stats.seconds = start.elapsed().as_secs_f64();
        Ok((completions, stats))
    }

    /// Wait for a specific frame, skipping late acknowledgements.
    fn await_frame(&mut self, matches: impl Fn(&Frame) -> bool) -> Result<Frame, ModernError> {
        let mut stats = RunStats::default();
        loop {
            let frame = self.recv(&mut stats)?;
            if matches(&frame) {
                return Ok(frame);
            }
        }
    }

    /// One ring round trip of a frame carrying `payload` bytes, in seconds.
    pub fn ping(&mut self, payload: usize) -> Result<f64, ModernError> {
        let id = self.next_ping;
        self.next_ping += 1;
        let frame = Frame::Ping {
            id,
            payload: vec![0x5a; payload],
        };
        let start = Instant::now();
        self.send(&frame, &mut RunStats::default())?;
        self.await_frame(|f| matches!(f, Frame::Ping { id: got, .. } if *got == id))?;
        Ok(start.elapsed().as_secs_f64())
    }

    /// Advance explicit batched streams without closing their caches. ENG-8
    /// can prefill a prefix, verify trees, then commit an accepted path here.
    /// The caller owns IDs/positions and must keep all other work quiescent.
    pub fn forward_batch(&mut self, id: u64, items: Vec<Item>) -> Result<Vec<Item>, ModernError> {
        self.send(&Frame::Step { id, items }, &mut RunStats::default())?;
        match self.await_frame(|f| matches!(f, Frame::Step { id: got, .. } if *got == id))? {
            Frame::Step { items, .. } => Ok(items),
            _ => unreachable!("matched step"),
        }
    }

    /// Release explicit streams admitted through `forward_batch`.
    pub fn close_sequences(&mut self, seqs: Vec<u64>, forget: bool) -> Result<(), ModernError> {
        self.send(
            &Frame::Close {
                seqs: seqs.clone(),
                forget,
            },
            &mut RunStats::default(),
        )?;
        self.await_frame(|f| matches!(f, Frame::Close { seqs: got, .. } if *got == seqs))?;
        Ok(())
    }

    /// Verify drafted branches against a still-live prefix in one ring pass.
    /// Requires an idle ring: `await_frame` discards nonmatching frames, so do
    /// not interleave this with batched streams. Accepted paths are re-executed
    /// through ordinary Step frames; this call does not advance the live KV.
    /// ENG-8 owns draft proposal and path acceptance; this API returns exact
    /// per-node logits/commitments only.
    pub fn verify_tree(
        &mut self,
        id: u64,
        prefix: u64,
        nodes: Vec<TreeNode>,
    ) -> Result<Vec<TreeNode>, ModernError> {
        self.send(&Frame::Tree { id, prefix, nodes }, &mut RunStats::default())?;
        match self.await_frame(|f| matches!(f, Frame::Tree { id: got, .. } if *got == id))? {
            Frame::Tree { nodes, .. } => Ok(nodes),
            _ => unreachable!("matched tree"),
        }
    }

    /// Every stage's activation log of `seq`, in stage order.
    pub fn reveal(&mut self, seq: u64) -> Result<Vec<Revealed>, ModernError> {
        self.send(
            &Frame::Reveal {
                seq,
                stages: Vec::new(),
            },
            &mut RunStats::default(),
        )?;
        match self.await_frame(|f| matches!(f, Frame::Reveal { seq: got, .. } if *got == seq))? {
            Frame::Reveal { stages, .. } => Ok(stages),
            _ => unreachable!("matched a reveal"),
        }
    }

    /// Stop every stage (each forwards the shutdown, then exits).
    pub fn shutdown(&mut self) -> Result<(), ModernError> {
        self.send(&Frame::Shutdown, &mut RunStats::default())?;
        self.await_frame(|f| matches!(f, Frame::Shutdown))?;
        Ok(())
    }
}
