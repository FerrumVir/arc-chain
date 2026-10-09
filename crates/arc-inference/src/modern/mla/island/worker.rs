//! A stage worker: one process (or thread) holding layers `[a, b)`.
//!
//! It receives frames from upstream, runs every item through its layers in
//! frame order, appends its commitment, logs what it received, and forwards
//! the frame downstream. The activation log is both the audit log (a stage
//! reveals it when audited) and the recovery log: a restarted worker replays
//! it to rebuild every open sequence's KV cache, and checks that replay
//! reproduces every hash it committed before the restart (research-6 §4.3:
//! "same log, two uses").
//!
//! Speculative decoding extends a sequence's one live cache with draft
//! positions and drops them in place with a rollback (also logged). Draft
//! trees branch off the same cache depth first. Neither copies a cache.

use std::collections::{HashMap, HashSet, VecDeque};
use std::fs::{File, OpenOptions};
use std::io::{BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc::{Receiver, channel};
use std::time::{Duration, Instant};

use super::transport::{Link, Listener, Transport};
use super::wire::{Frame, Item, Reader, Revealed, StageCommit, TreeNode, Writer};
use crate::modern::ModernError;
use crate::modern::arith;
use crate::modern::mla::boundary::activation_hash;
use crate::modern::mla::model::{StageCache, StageInput, StageModel};

/// Deliberate misbehaviour, for tests of the audit path only.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fault {
    /// Add 1 to the first output value at `(seq, position)` and commit the
    /// hash of the altered output: a consistent lie that only re-execution
    /// catches.
    TamperOutput { seq: u64, position: u32 },
    /// Send the honest output at `(seq, position)` but commit a different
    /// output hash: the link check catches it.
    LieInCommit { seq: u64, position: u32 },
    /// The last stage emits (and commits) a token other than its sampler's
    /// at `(seq, position)`; every hash stays honest, so only the audit's
    /// re-selection catches it.
    WrongToken { seq: u64, position: u32 },
}

impl Fault {
    /// `tamper:SEQ:POS`, `lie:SEQ:POS` or `wrong-token:SEQ:POS`.
    pub fn parse(text: &str) -> Result<Self, ModernError> {
        let bad = || {
            ModernError::Invalid(format!(
                "fault {text:?} is not tamper|lie|wrong-token:SEQ:POS"
            ))
        };
        let mut parts = text.split(':');
        let (kind, seq, position) = (parts.next(), parts.next(), parts.next());
        let seq = seq.and_then(|s| s.parse().ok()).ok_or_else(bad)?;
        let position = position.and_then(|s| s.parse().ok()).ok_or_else(bad)?;
        match kind {
            Some("tamper") => Ok(Fault::TamperOutput { seq, position }),
            Some("lie") => Ok(Fault::LieInCommit { seq, position }),
            Some("wrong-token") => Ok(Fault::WrongToken { seq, position }),
            _ => Err(bad()),
        }
    }
}

/// How a worker runs.
#[derive(Debug, Clone, Default)]
pub struct WorkerConfig {
    /// Directory of the persistent activation log; `None` keeps it in memory
    /// only (no recovery after a crash).
    pub log_dir: Option<PathBuf>,
    pub fault: Option<Fault>,
}

/// Counters of one worker.
#[derive(Debug, Clone, Default)]
pub struct WorkerStats {
    pub frames: u64,
    pub items: u64,
    pub positions: u64,
    pub compute_seconds: f64,
    /// Positions replayed from the log at start.
    pub replayed_positions: u64,
    /// Computed positions later dropped by a rollback (speculative work
    /// that turned out to follow a rejected draft).
    pub rolled_back_positions: u64,
    /// Items not computed because a rollback already queued behind them
    /// had discarded them (early cancellation), and their positions.
    pub skipped_items: u64,
    pub skipped_positions: u64,
}

#[derive(Clone)]
struct SeqState {
    /// `None` once the sequence is closed (its log stays for audits).
    cache: Option<StageCache>,
    prompt_len: u32,
    selection: arith::Selection,
    /// Every token forwarded so far (the last stage selects from them).
    tokens: Vec<u32>,
    /// Every boundary row received (empty for the first stage).
    inputs: Vec<i64>,
}

const LOG_ITEM: u8 = 1;
const LOG_CLOSE: u8 = 2;
const LOG_ROLLBACK: u8 = 3;

/// The error a stage records on an item that a rollback queued behind it has
/// already discarded: the item is passed on without being computed.
pub const SUPERSEDED: &str = "superseded by a queued rollback";

/// One stage of an island.
pub struct StageWorker {
    model: StageModel,
    seqs: HashMap<u64, SeqState>,
    log: Option<BufWriter<File>>,
    fault: Option<Fault>,
    pub stats: WorkerStats,
}

impl StageWorker {
    /// A worker for `model`. With a log directory, the stage's log there is
    /// replayed first (a restart) and then appended to.
    pub fn new(model: StageModel, config: WorkerConfig) -> Result<Self, ModernError> {
        let mut worker = Self {
            model,
            seqs: HashMap::new(),
            log: None,
            fault: config.fault,
            stats: WorkerStats::default(),
        };
        if let Some(dir) = config.log_dir {
            std::fs::create_dir_all(&dir)
                .map_err(|e| ModernError::io(&dir.display().to_string(), e))?;
            let path = worker.log_path(&dir);
            if path.exists() {
                worker.replay(&path)?;
            }
            let file = OpenOptions::new()
                .create(true)
                .append(true)
                .open(&path)
                .map_err(|e| ModernError::io(&path.display().to_string(), e))?;
            worker.log = Some(BufWriter::new(file));
        }
        Ok(worker)
    }

    fn log_path(&self, dir: &Path) -> PathBuf {
        let s = self.model.stage();
        dir.join(format!("stage-{}-{}.arclog", s.first_layer, s.end_layer))
    }

    /// `[a, b)`.
    pub fn name(&self) -> String {
        let s = self.model.stage();
        format!("[{}, {})", s.first_layer, s.end_layer)
    }

    pub fn model(&self) -> &StageModel {
        &self.model
    }

    /// Positions currently cached for `seq` (`None` if unknown or closed).
    pub fn cached_positions(&self, seq: u64) -> Option<usize> {
        self.seqs
            .get(&seq)
            .and_then(|s| s.cache.as_ref())
            .map(StageCache::positions)
    }

    /// Process one frame; the result goes downstream.
    pub fn process(&mut self, frame: Frame) -> Result<Frame, ModernError> {
        self.stats.frames += 1;
        let out = match frame {
            Frame::Step { id, mut items } => {
                for item in &mut items {
                    self.step_item(item)?;
                }
                Frame::Step { id, items }
            }
            Frame::Tree {
                id,
                prefix,
                mut nodes,
            } => {
                self.verify_tree(prefix, &mut nodes)?;
                Frame::Tree { id, prefix, nodes }
            }
            Frame::Rollback { seq, keep } => {
                let applied = self.rollback(seq, keep as usize)?;
                if applied && let Some(log) = &mut self.log {
                    let mut w = Writer::default();
                    w.u8(LOG_ROLLBACK);
                    w.u64(seq);
                    w.u32(keep);
                    write_record(log, &w.bytes)?;
                }
                Frame::Rollback { seq, keep }
            }
            Frame::Close { seqs, forget } => {
                for &seq in &seqs {
                    self.close(seq, forget);
                }
                if let Some(log) = &mut self.log {
                    for &seq in &seqs {
                        let mut w = Writer::default();
                        w.u8(LOG_CLOSE);
                        w.u64(seq);
                        w.u8(u8::from(forget));
                        write_record(log, &w.bytes)?;
                    }
                }
                Frame::Close { seqs, forget }
            }
            Frame::Reveal { seq, mut stages } => {
                let s = self.model.stage();
                if let Some(state) = self.seqs.get(&seq) {
                    stages.push(Revealed {
                        seq,
                        first_layer: s.first_layer as u32,
                        end_layer: s.end_layer as u32,
                        prompt_len: state.prompt_len,
                        selection: state.selection,
                        tokens: state.tokens.clone(),
                        inputs: state.inputs.clone(),
                    });
                }
                Frame::Reveal { seq, stages }
            }
            other => other,
        };
        if let Some(log) = &mut self.log {
            log.flush()
                .map_err(|e| ModernError::io("activation log", e))?;
        }
        Ok(out)
    }

    /// Evaluate a draft tree on the live prefix's own state, depth first.
    /// Before each node the prefix's cache, forwarded tokens and inputs are
    /// truncated back to the node's parent, so siblings never see each
    /// other's positions and no cache is copied. Afterwards, even on failure,
    /// the prefix is truncated back to exactly its own positions, and no draft
    /// reaches the persistent log. Returned items carry the same per-layer
    /// commitments as ordinary sequential decoding of their individual paths.
    fn verify_tree(&mut self, prefix: u64, nodes: &mut [TreeNode]) -> Result<(), ModernError> {
        let invalid = || ModernError::Invalid("invalid tree topology or prefix".into());
        let base = self.seqs.get(&prefix).ok_or_else(invalid)?;
        let start = base.cache.as_ref().ok_or_else(invalid)?.positions();
        if nodes.is_empty() || nodes.len() > 4096 {
            return Err(invalid());
        }
        let mut ids = HashSet::new();
        let mut children = vec![Vec::new(); nodes.len()];
        let mut roots = Vec::new();
        for (i, node) in nodes.iter().enumerate() {
            let at = match node.parent {
                Some(p) if (p as usize) < i => {
                    children[p as usize].push(i);
                    nodes[p as usize].item.start as usize + 1
                }
                Some(_) => return Err(invalid()),
                None => {
                    roots.push(i);
                    start
                }
            };
            if node.item.tokens.len() != 1
                || node.item.start as usize != at
                || node.item.prompt_len != base.prompt_len
                || node.item.selection != base.selection
                || node.item.error.is_some()
                || self.seqs.contains_key(&node.item.seq)
                || !ids.insert(node.item.seq)
            {
                return Err(invalid());
            }
        }
        // Depth-first preorder, children in frame order: every node follows
        // its parent or a finished subtree of an earlier sibling.
        let mut order = Vec::with_capacity(nodes.len());
        let mut stack: Vec<usize> = roots.into_iter().rev().collect();
        while let Some(i) = stack.pop() {
            order.push(i);
            stack.extend(children[i].iter().rev());
        }
        let result = self.run_tree(prefix, nodes, &order);
        self.truncate_state(prefix, start)?;
        result
    }

    /// The nodes of a validated tree, in `order`, on `prefix`'s state.
    fn run_tree(
        &mut self,
        prefix: u64,
        nodes: &mut [TreeNode],
        order: &[usize],
    ) -> Result<(), ModernError> {
        for &i in order {
            // The positions before a node hold the prefix and its ancestors.
            self.truncate_state(prefix, nodes[i].item.start as usize)?;
            let commit = self.run_on(prefix, &mut nodes[i].item)?;
            nodes[i].item.commits.push(commit);
        }
        Ok(())
    }

    /// Speculative rollback: keep `seq`'s first `keep` positions (cache,
    /// forwarded tokens, received inputs) and drop the rest in place. An
    /// unknown sequence is ignored (a failed item already dropped it here);
    /// a closed one, or `keep` beyond what this stage holds, is refused.
    /// Returns whether the rollback applied.
    fn rollback(&mut self, seq: u64, keep: usize) -> Result<bool, ModernError> {
        if !self.seqs.contains_key(&seq) {
            return Ok(false);
        }
        let dropped = self.truncate_state(seq, keep)?;
        self.stats.rolled_back_positions += dropped as u64;
        Ok(true)
    }

    /// Keep the first `keep` positions of `seq`'s cache, forwarded tokens and
    /// received inputs; returns how many positions were dropped.
    fn truncate_state(&mut self, seq: u64, keep: usize) -> Result<usize, ModernError> {
        let d = self.model.config().d_model;
        let state = self
            .seqs
            .get_mut(&seq)
            .ok_or_else(|| ModernError::Invalid(format!("sequence {seq} is unknown here")))?;
        let cache = state
            .cache
            .as_mut()
            .ok_or_else(|| ModernError::Invalid(format!("sequence {seq} is closed")))?;
        let held = cache.positions();
        self.model.truncate_cache(cache, keep)?;
        state.tokens.truncate(keep);
        // The first stage receives token ids only; its inputs stay empty.
        state.inputs.truncate(keep * d);
        Ok(held - keep)
    }

    fn close(&mut self, seq: u64, forget: bool) {
        if forget {
            self.seqs.remove(&seq);
        } else if let Some(state) = self.seqs.get_mut(&seq) {
            state.cache = None;
        }
    }

    /// Run one item through the stage. A failure fails the item (and drops
    /// the sequence's cache, which may hold a partial position), never the
    /// frame: other sequences are untouched.
    fn step_item(&mut self, item: &mut Item) -> Result<(), ModernError> {
        if item.error.is_some() {
            return Ok(());
        }
        let input = item.hidden.clone();
        match self.run_item(item) {
            Ok(commit) => {
                if let Some(log) = &mut self.log {
                    let mut w = Writer::default();
                    w.u8(LOG_ITEM);
                    w.u64(item.seq);
                    w.u32(item.start);
                    w.u32(item.prompt_len);
                    w.selection(item.selection);
                    w.u32s(&item.tokens);
                    w.acts(&input);
                    commit.write(&mut w);
                    write_record(log, &w.bytes)?;
                }
                item.commits.push(commit);
            }
            Err(e) => {
                self.seqs.remove(&item.seq);
                item.error = Some(format!("stage {}: {e}", self.name()));
                item.hidden.clear();
            }
        }
        Ok(())
    }

    fn run_item(&mut self, item: &mut Item) -> Result<StageCommit, ModernError> {
        if !self.seqs.contains_key(&item.seq) {
            if item.start != 0 {
                return Err(ModernError::Invalid(format!(
                    "sequence {} is unknown here (position {})",
                    item.seq, item.start
                )));
            }
            let cache = self.model.new_cache();
            self.seqs.insert(
                item.seq,
                SeqState {
                    cache: Some(cache),
                    prompt_len: item.prompt_len,
                    selection: item.selection,
                    tokens: Vec::new(),
                    inputs: Vec::new(),
                },
            );
        }
        self.run_on(item.seq, item)
    }

    /// Run `item`'s positions on `key`'s state: the item's own sequence, or
    /// the live prefix that a draft-tree node temporarily extends.
    fn run_on(&mut self, key: u64, item: &mut Item) -> Result<StageCommit, ModernError> {
        let stage = self.model.stage();
        let c = self.model.config();
        let d = c.d_model;
        let n = item.tokens.len();
        if n == 0 {
            return Err(ModernError::Invalid(
                "an item needs at least one position".into(),
            ));
        }
        let first = stage.has_embed();
        if first != item.hidden.is_empty() || (!first && item.hidden.len() != n * d) {
            return Err(ModernError::Invalid(
                "item activations do not match the positions".into(),
            ));
        }
        let start = item.start as usize;
        let state = self
            .seqs
            .get_mut(&key)
            .ok_or_else(|| ModernError::Invalid(format!("sequence {key} is unknown here")))?;
        let cache = state
            .cache
            .as_mut()
            .ok_or_else(|| ModernError::Invalid(format!("sequence {} is closed", item.seq)))?;
        if cache.positions() != start
            || state.prompt_len != item.prompt_len
            || state.selection != item.selection
        {
            return Err(ModernError::Invalid(format!(
                "sequence {}: item at position {start}, stage at {}",
                item.seq,
                cache.positions()
            )));
        }
        let timer = Instant::now();
        let per = stage.end_layer - stage.first_layer + 1;
        let mut hashes = Vec::with_capacity(n * per);
        let mut outputs = Vec::with_capacity(n * d);
        let mut trace = Vec::with_capacity(per);
        let mut last_logits = None;
        let mut logits_hashes = Vec::new();
        for i in 0..n {
            let input = if first {
                StageInput::Token(item.tokens[i])
            } else {
                StageInput::Hidden(&item.hidden[i * d..(i + 1) * d])
            };
            trace.clear();
            let (mut out, logits) = self.model.forward(input, cache, Some(&mut trace))?;
            let position = (start + i) as u32;
            match self.fault {
                Some(Fault::TamperOutput { seq, position: p })
                    if seq == item.seq && p == position =>
                {
                    out[0] += 1;
                    *trace.last_mut().expect("trace") = activation_hash(&out);
                }
                Some(Fault::LieInCommit { seq, position: p })
                    if seq == item.seq && p == position =>
                {
                    trace.last_mut().expect("trace")[0] ^= 1;
                }
                _ => {}
            }
            hashes.extend_from_slice(&trace);
            if let Some(logits) = logits {
                logits_hashes.push(arith::logits_hash(&logits));
                last_logits = Some(logits);
            } else {
                outputs.extend_from_slice(&out);
            }
        }
        state.tokens.extend_from_slice(&item.tokens);
        if !first {
            state.inputs.extend_from_slice(&item.hidden);
        }
        let mut selected = None;
        if let Some(logits) = last_logits {
            let position = start + n - 1;
            let prompt_len = item.prompt_len as usize;
            if position + 1 >= prompt_len {
                let history = &state.tokens[prompt_len..=position];
                let mut token = arith::select(&logits, history, item.selection)?;
                if let Some(Fault::WrongToken { seq, position: p }) = self.fault
                    && seq == item.seq
                    && p as usize == position
                {
                    token = (token + 1) % c.vocab_size as u32;
                }
                selected = Some(token);
            }
        }
        item.hidden = outputs;
        if stage.has_head(c) {
            // The last stage returns commitments only: the benchmark padding
            // that stands for a wide activation stops here too.
            item.pad = 0;
        }
        self.stats.items += 1;
        self.stats.positions += n as u64;
        self.stats.compute_seconds += timer.elapsed().as_secs_f64();
        Ok(StageCommit {
            first_layer: stage.first_layer as u32,
            end_layer: stage.end_layer as u32,
            hashes,
            logits: logits_hashes,
            selected,
        })
    }

    /// Rebuild state from the activation log, checking every replayed hash
    /// against the hash committed before the restart. A truncated last
    /// record (a crash mid-write) is ignored: its frame was never forwarded.
    fn replay(&mut self, path: &Path) -> Result<(), ModernError> {
        let context = path.display().to_string();
        let mut bytes = Vec::new();
        File::open(path)
            .and_then(|mut f| f.read_to_end(&mut bytes))
            .map_err(|e| ModernError::io(&context, e))?;
        let mut at = 0usize;
        let mut valid = 0usize;
        while at + 4 <= bytes.len() {
            let len = u32::from_le_bytes(bytes[at..at + 4].try_into().expect("4")) as usize;
            if at + 4 + len > bytes.len() {
                break;
            }
            let record = &bytes[at + 4..at + 4 + len];
            at += 4 + len;
            let mut r = Reader::new(record);
            match r.u8()? {
                LOG_ITEM => {
                    let mut item =
                        Item::new(r.u64()?, r.u32()?, r.u32()?, r.selection()?, Vec::new());
                    item.tokens = r.u32s()?;
                    item.hidden = r.acts()?;
                    let committed = StageCommit::read(&mut r)?;
                    r.done()?;
                    let fault = self.fault.take();
                    let replayed = self.run_item(&mut item);
                    self.fault = fault;
                    let replayed = replayed?;
                    if replayed != committed {
                        return Err(ModernError::Invalid(format!(
                            "{context}: replay of sequence {} at {} does not reproduce its commitment",
                            item.seq, item.start
                        )));
                    }
                    self.stats.replayed_positions += item.tokens.len() as u64;
                }
                LOG_CLOSE => {
                    let seq = r.u64()?;
                    let forget = r.u8()? != 0;
                    r.done()?;
                    self.close(seq, forget);
                }
                LOG_ROLLBACK => {
                    let seq = r.u64()?;
                    let keep = r.u32()? as usize;
                    r.done()?;
                    self.rollback(seq, keep)?;
                }
                other => {
                    return Err(ModernError::Invalid(format!(
                        "{context}: record kind {other}"
                    )));
                }
            }
            valid = at;
        }
        if valid < bytes.len() {
            // Drop the torn tail so appends start at a record boundary.
            let file = OpenOptions::new()
                .write(true)
                .open(path)
                .map_err(|e| ModernError::io(&context, e))?;
            file.set_len(valid as u64)
                .map_err(|e| ModernError::io(&context, e))?;
        }
        Ok(())
    }
}

fn write_record(log: &mut BufWriter<File>, record: &[u8]) -> Result<(), ModernError> {
    log.write_all(&(record.len() as u32).to_le_bytes())
        .and_then(|()| log.write_all(record))
        .map_err(|e| ModernError::io("activation log", e))
}

/// A send side that reconnects: the next stage may restart and listen again.
pub struct Downstream {
    transport: Arc<dyn Transport>,
    address: String,
    link: Option<Box<dyn Link>>,
    /// How long a send keeps trying to (re)connect.
    pub patience: Duration,
}

impl Downstream {
    pub fn new(transport: Arc<dyn Transport>, address: String) -> Self {
        Self {
            transport,
            address,
            link: None,
            patience: Duration::from_secs(120),
        }
    }

    pub fn send(&mut self, frame: &[u8]) -> Result<(), ModernError> {
        let deadline = Instant::now() + self.patience;
        loop {
            if let Some(link) = &mut self.link {
                if link.alive() && link.send(frame).is_ok() {
                    return Ok(());
                }
                self.link = None;
            }
            match self.transport.connect(&self.address) {
                Ok(link) => self.link = Some(link),
                Err(e) if Instant::now() >= deadline => {
                    return Err(ModernError::Io(format!("{}: {e}", self.address)));
                }
                Err(_) => std::thread::sleep(Duration::from_millis(20)),
            }
        }
    }
}

/// Accept connections on `listener` forever, feeding every frame any of them
/// carries into one channel (a restarted upstream simply connects again).
pub fn incoming(mut listener: Box<dyn Listener>) -> Receiver<Vec<u8>> {
    let (tx, rx) = channel();
    std::thread::spawn(move || {
        while let Ok(mut link) = listener.accept() {
            let tx = tx.clone();
            std::thread::spawn(move || {
                while let Ok(frame) = link.recv() {
                    if tx.send(frame).is_err() {
                        return;
                    }
                }
            });
        }
    });
    rx
}

/// Run a worker: frames from `listener`, results to `next`, until a
/// shutdown frame has been forwarded. Frames that have already arrived wait
/// in a local queue, so a stage that falls behind sees a rollback queued
/// behind speculative work and skips the items it discards (early
/// cancellation) instead of computing them.
pub fn serve(
    worker: StageWorker,
    listener: Box<dyn Listener>,
    transport: Arc<dyn Transport>,
    next: String,
) -> Result<StageWorker, ModernError> {
    serve_from(worker, incoming(listener), transport, next)
}

/// [`serve`] on frames from a channel, as [`incoming`] delivers them.
pub fn serve_from(
    mut worker: StageWorker,
    frames: Receiver<Vec<u8>>,
    transport: Arc<dyn Transport>,
    next: String,
) -> Result<StageWorker, ModernError> {
    let mut downstream = Downstream::new(transport, next);
    let mut queue = VecDeque::new();
    loop {
        if queue.is_empty() {
            let Ok(bytes) = frames.recv() else {
                break;
            };
            queue.push_back(Frame::decode(&bytes));
        }
        queue.extend(frames.try_iter().map(|bytes| Frame::decode(&bytes)));
        let Some(decoded) = queue.pop_front() else {
            break;
        };
        let out = match decoded {
            Ok(Frame::Shutdown) => {
                downstream.send(&Frame::Shutdown.encode())?;
                return Ok(worker);
            }
            Ok(mut frame) => {
                skip_superseded(&mut frame, &queue, &mut worker.stats);
                worker.process(frame).unwrap_or_else(|e| Frame::Error {
                    stage: worker.name(),
                    message: e.to_string(),
                })
            }
            Err(e) => Frame::Error {
                stage: worker.name(),
                message: e.to_string(),
            },
        };
        downstream.send(&out.encode())?;
    }
    Ok(worker)
}

/// Mark every item of a `Step` that a rollback already queued behind it
/// discards (same sequence, at or beyond the kept length) as superseded, so
/// this stage and every later one pass it on without computing it. The
/// rollback's sender had cancelled those items before sending it: links are
/// first-in first-out, so they were on the ring ahead of it.
pub fn skip_superseded(
    frame: &mut Frame,
    queued: &VecDeque<Result<Frame, ModernError>>,
    stats: &mut WorkerStats,
) {
    let Frame::Step { items, .. } = frame else {
        return;
    };
    for item in items.iter_mut() {
        let superseded = item.error.is_none()
            && queued.iter().any(|ahead| {
                matches!(ahead, Ok(Frame::Rollback { seq, keep })
                    if *seq == item.seq && *keep <= item.start)
            });
        if superseded {
            item.error = Some(SUPERSEDED.into());
            item.hidden.clear();
            item.pad = 0;
            stats.skipped_items += 1;
            stats.skipped_positions += item.tokens.len() as u64;
        }
    }
}
