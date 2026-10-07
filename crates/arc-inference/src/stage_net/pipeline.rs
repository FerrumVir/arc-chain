//! A pipeline-parallel ring over [`StageSink`]/[`StageSource`] links, with
//! micro-batching and compute/transfer overlap, instrumented per hop.
//!
//! ```text
//!  driver ──local──▶ stage 0 ──hop 0──▶ stage 1 ──hop 1──▶ … stage S-1 ──hop S-1──▶ driver
//!   (co-located with stage 0)                                  (sampled tokens back)
//! ```
//!
//! For `S > 1` the ring has `S` network hops, matching research-7's
//! `t_pass = Σ compute + S·(r/2 + o) + serialization`. A single stage has
//! none.
//!
//! Each stage runs three threads:
//! * a reader that pulls whole frames off its source and timestamps arrival;
//! * the compute loop: decode → forward every entry → commit → encode;
//! * with `overlap`, a sender that owns the outgoing sink, so the compute loop
//!   starts the next micro-batch while the previous one is still on the wire.
//!   Without `overlap` the compute loop sends inline and waits for the
//!   uplink.
//!
//! Every hidden state leaving a stage is committed with
//! [`codec::commit_i64`] (and every sampled token with its logits hash) under
//! `(seq, position, stage)`. Sequences never share arithmetic, so the tokens
//! and every commitment are independent of the split, the micro-batch
//! composition, the codec, the transport and the overlap setting. The tests
//! and the benchmark check exactly that against [`reference`].

use super::codec::{self};
use super::shaper::{LinkProfile, ShapedSink};
use super::wire::{
    self, EncodeOptions, EncodeStats, Entry, EntryBody, Message, MessageKind, StageSink,
    StageSource, TcpTuning,
};
use crate::cached_integer_model::{CachedIntegerModel, KVCache, ShardInput, ShardOutput};
use arc_crypto::Hash256;
use std::collections::{BTreeMap, HashMap};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::Instant;

/// `hop` value on messages the driver injects into stage 0 (not a network hop).
pub const DRIVER_HOP: u16 = u16::MAX;

/// Per-stage commitments keyed by `(seq, position, stage)`.
pub type Commitments = BTreeMap<(u64, u32, usize), Hash256>;

/// Both halves of one link.
type Link = (Box<dyn StageSink>, Box<dyn StageSource>);

// ─── Compute ────────────────────────────────────────────────────────────────

pub enum StageInput {
    Token(u32),
    Hidden(Vec<i64>),
}

pub enum StageResult {
    Hidden(Vec<i64>),
    Token { id: u32, logits_hash: Hash256 },
}

/// What a pipeline stage computes. The island runtime (ENG-6) and the
/// MoE/MLA engine (ENG-5) plug in here; [`IntegerSlice`] is the dense
/// integer engine's implementation.
pub trait StageCompute: Send {
    fn forward(
        &mut self,
        seq: u64,
        position: u32,
        input: StageInput,
    ) -> Result<StageResult, String>;
}

/// Layers `[start, end)` of a [`CachedIntegerModel`], with one KV cache per
/// sequence.
pub struct IntegerSlice {
    model: Arc<CachedIntegerModel>,
    start: usize,
    end: usize,
    caches: HashMap<u64, KVCache>,
}

impl IntegerSlice {
    pub fn new(model: Arc<CachedIntegerModel>, start: usize, end: usize) -> Self {
        Self {
            model,
            start,
            end,
            caches: HashMap::new(),
        }
    }
}

impl StageCompute for IntegerSlice {
    fn forward(
        &mut self,
        seq: u64,
        position: u32,
        input: StageInput,
    ) -> Result<StageResult, String> {
        let n_layers = self.model.config.n_layers;
        let cache = self
            .caches
            .entry(seq)
            .or_insert_with(|| KVCache::new(n_layers));
        let input = match input {
            StageInput::Token(t) => ShardInput::Token(t),
            StageInput::Hidden(h) => ShardInput::Hidden(h),
        };
        match self
            .model
            .forward_shard_token(input, cache, self.start, self.end, position as usize)
            .map_err(|e| format!("layers {}..{}: {e}", self.start, self.end))?
        {
            ShardOutput::Hidden(h) => Ok(StageResult::Hidden(h)),
            ShardOutput::Token { id, logits_hash } => Ok(StageResult::Token { id, logits_hash }),
        }
    }
}

// ─── Instrumentation ────────────────────────────────────────────────────────

/// A clock shared by every thread of one process.
#[derive(Clone, Copy, Debug)]
pub struct Clock(Instant);

impl Clock {
    pub fn start() -> Self {
        Self(Instant::now())
    }
    pub fn now_ns(&self) -> u64 {
        self.0.elapsed().as_nanos() as u64
    }
}

/// One message crossing one hop, measured at the receiver.
#[derive(Clone, Copy, Debug, Default)]
pub struct HopSample {
    /// Index of the sending stage (hop `i` goes from stage `i` to `i+1`, the
    /// last hop back to the driver).
    pub hop: usize,
    pub entries: usize,
    pub wire_bytes: usize,
    /// Commit + encode at the sender.
    pub serialize_ns: u64,
    /// Handover by the sender's compute loop → frame fully received:
    /// sender-queue wait, uplink, propagation and kernel time.
    pub transfer_ns: u64,
    /// Waiting in the receiver's inbox before decoding started.
    pub queue_ns: u64,
    /// Decode + commitment check at the receiver.
    pub deserialize_ns: u64,
    /// The receiving stage's compute for the whole micro-batch (0 at the
    /// driver).
    pub compute_ns: u64,
}

/// One micro-batch's trip around the ring, measured at the driver.
#[derive(Clone, Copy, Debug)]
pub struct PassSample {
    pub group: usize,
    pub position: u32,
    pub sent_ns: u64,
    pub recv_ns: u64,
    /// The pass produced a generated (not prompt) token.
    pub decode: bool,
}

/// One micro-batch's compute on one stage.
#[derive(Clone, Copy, Debug)]
pub struct StageComputeSample {
    pub stage: usize,
    pub entries: usize,
    pub compute_ns: u64,
}

#[derive(Default)]
pub struct Recorder {
    hops: Mutex<Vec<HopSample>>,
    sends: Mutex<Vec<(usize, EncodeStats)>>,
    computes: Mutex<Vec<StageComputeSample>>,
    commitments: Mutex<Commitments>,
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

impl Recorder {
    fn hop(&self, s: HopSample) {
        lock(&self.hops).push(s);
    }
    fn send(&self, hop: usize, s: EncodeStats) {
        lock(&self.sends).push((hop, s));
    }
    fn compute(&self, s: StageComputeSample) {
        lock(&self.computes).push(s);
    }
    fn commit(&self, seq: u64, position: u32, stage: usize, c: Hash256) {
        lock(&self.commitments).insert((seq, position, stage), c);
    }
}

// ─── Stage loop ─────────────────────────────────────────────────────────────

#[derive(Clone, Copy, Debug)]
pub struct StageConfig {
    pub index: usize,
    pub overlap: bool,
    /// Recompute and check every incoming hidden-state commitment.
    pub verify: bool,
    pub encode: EncodeOptions,
}

enum Outbox {
    Inline(Box<dyn StageSink>),
    Thread {
        tx: Option<mpsc::Sender<Vec<u8>>>,
        worker: Option<std::thread::JoinHandle<Result<(), String>>>,
    },
}

impl Outbox {
    fn new(mut sink: Box<dyn StageSink>, overlap: bool) -> Self {
        if !overlap {
            return Outbox::Inline(sink);
        }
        let (tx, rx) = mpsc::channel::<Vec<u8>>();
        let worker = std::thread::Builder::new()
            .name("stage-net-send".into())
            .spawn(move || {
                for frame in rx {
                    sink.send_frame(&frame).map_err(|e| format!("send: {e}"))?;
                }
                Ok(())
            })
            .expect("spawn sender thread");
        Outbox::Thread {
            tx: Some(tx),
            worker: Some(worker),
        }
    }

    fn push(&mut self, frame: Vec<u8>) -> Result<(), String> {
        match self {
            Outbox::Inline(sink) => sink.send_frame(&frame).map_err(|e| format!("send: {e}")),
            Outbox::Thread { tx, .. } => tx
                .as_ref()
                .expect("open until finish")
                .send(frame)
                .map_err(|_| "sender thread stopped".to_string()),
        }
    }

    fn finish(mut self) -> Result<(), String> {
        if let Outbox::Thread { tx, worker } = &mut self {
            tx.take();
            if let Some(w) = worker.take() {
                return w.join().map_err(|_| "sender thread panicked".to_string())?;
            }
        }
        Ok(())
    }
}

/// Encode, stamp and hand a message to the outbox; returns serialize time.
fn ship(
    outbox: &mut Outbox,
    msg: &Message,
    opts: &EncodeOptions,
    clock: Clock,
    rec: &Recorder,
    serialize_started_ns: u64,
) -> Result<(), String> {
    let mut frame = Vec::new();
    let stats = wire::encode(msg, opts, &mut frame);
    let t_send = clock.now_ns();
    let serialize_ns = t_send.saturating_sub(serialize_started_ns);
    wire::stamp(&mut frame, t_send, serialize_ns.min(u32::MAX as u64) as u32);
    if msg.kind == MessageKind::Data && msg.hop != DRIVER_HOP {
        rec.send(msg.hop as usize, stats);
    }
    outbox.push(frame)
}

type Inbox = mpsc::Receiver<(Vec<u8>, u64)>;

fn spawn_reader(
    mut source: Box<dyn StageSource>,
    clock: Clock,
) -> (Inbox, std::thread::JoinHandle<Result<(), String>>) {
    let (tx, rx) = mpsc::channel();
    let handle = std::thread::Builder::new()
        .name("stage-net-recv".into())
        .spawn(move || {
            loop {
                let mut buf = Vec::new();
                match source.recv_frame(&mut buf) {
                    Ok(true) => {
                        if tx.send((buf, clock.now_ns())).is_err() {
                            return Ok(());
                        }
                    }
                    Ok(false) => return Ok(()),
                    Err(e) => return Err(format!("recv: {e}")),
                }
            }
        })
        .expect("spawn reader thread");
    (rx, handle)
}

/// Run one stage until a shutdown message passes through it.
pub fn run_stage(
    mut compute: Box<dyn StageCompute>,
    source: Box<dyn StageSource>,
    sink: Box<dyn StageSink>,
    cfg: StageConfig,
    clock: Clock,
    rec: Arc<Recorder>,
) -> Result<(), String> {
    let (inbox, reader) = spawn_reader(source, clock);
    let mut outbox = Outbox::new(sink, cfg.overlap);
    let mut result = Ok(());
    for (frame, recv_ns) in inbox.iter() {
        let t_dec = clock.now_ns();
        let (msg, trace) = match wire::decode(&frame, cfg.verify) {
            Ok(m) => m,
            Err(e) => {
                result = Err(format!("stage {}: {e}", cfg.index));
                break;
            }
        };
        let t_compute = clock.now_ns();
        if msg.kind == MessageKind::Shutdown {
            let fwd = Message {
                hop: cfg.index as u16,
                ..msg
            };
            result = ship(&mut outbox, &fwd, &cfg.encode, clock, &rec, t_compute);
            break;
        }
        let mut out = Vec::with_capacity(msg.entries.len());
        let mut compute_ns = 0u64;
        let mut commit_ns = 0u64;
        for e in msg.entries {
            let input = match e.body {
                EntryBody::Token(t) => StageInput::Token(t),
                EntryBody::Hidden(h) => StageInput::Hidden(h),
            };
            let c0 = clock.now_ns();
            let r = match compute.forward(e.seq, e.position, input) {
                Ok(r) => r,
                Err(err) => {
                    result = Err(format!("stage {}: {err}", cfg.index));
                    break;
                }
            };
            let c1 = clock.now_ns();
            let (body, commitment) = match r {
                StageResult::Hidden(h) => {
                    let c = codec::commit_i64(&h);
                    (EntryBody::Hidden(h), c)
                }
                StageResult::Token { id, logits_hash } => (EntryBody::Token(id), logits_hash),
            };
            let c2 = clock.now_ns();
            compute_ns += c1 - c0;
            commit_ns += c2 - c1;
            rec.commit(e.seq, e.position, cfg.index, commitment);
            out.push(Entry {
                seq: e.seq,
                position: e.position,
                body,
                commitment,
            });
        }
        if result.is_err() {
            break;
        }
        rec.compute(StageComputeSample {
            stage: cfg.index,
            entries: out.len(),
            compute_ns,
        });
        if msg.hop != DRIVER_HOP {
            rec.hop(HopSample {
                hop: msg.hop as usize,
                entries: out.len(),
                wire_bytes: frame.len(),
                serialize_ns: trace.encode_ns as u64,
                transfer_ns: recv_ns.saturating_sub(trace.t_send_ns),
                queue_ns: t_dec.saturating_sub(recv_ns),
                deserialize_ns: t_compute - t_dec,
                compute_ns,
            });
        }
        let reply = Message {
            kind: MessageKind::Data,
            hop: cfg.index as u16,
            msg_id: msg.msg_id,
            entries: out,
        };
        // Serialization time includes the output commitments.
        let ser_start = clock.now_ns() - commit_ns;
        if let Err(e) = ship(&mut outbox, &reply, &cfg.encode, clock, &rec, ser_start) {
            result = Err(e);
            break;
        }
    }
    let sent = outbox.finish();
    drop(inbox);
    // The reader ends when upstream closes its sink. A read error after the
    // shutdown passed (e.g. a reset socket) does not affect the results.
    let _ = reader.join();
    result.and(sent)
}

// ─── Driver ─────────────────────────────────────────────────────────────────

/// Sequences to run: equal-length prompts, `gen_tokens` greedy tokens each,
/// moving around the ring in micro-batches of `microbatch` sequences.
#[derive(Clone, Debug)]
pub struct Workload {
    pub prompts: Vec<Vec<u32>>,
    pub gen_tokens: usize,
    pub microbatch: usize,
}

impl Workload {
    /// `n` distinct deterministic prompts of `len` tokens below `vocab`.
    pub fn synthetic(
        n: usize,
        len: usize,
        gen_tokens: usize,
        microbatch: usize,
        vocab: u32,
    ) -> Self {
        let prompts = (0..n)
            .map(|s| {
                (0..len)
                    .map(|i| 1 + ((s as u32 * 7919 + i as u32 * 104_729) % (vocab - 1)))
                    .collect()
            })
            .collect();
        Self {
            prompts,
            gen_tokens,
            microbatch: microbatch.max(1),
        }
    }

    fn groups(&self) -> Vec<Vec<usize>> {
        (0..self.prompts.len())
            .collect::<Vec<_>>()
            .chunks(self.microbatch)
            .map(|c| c.to_vec())
            .collect()
    }

    fn prompt_len(&self) -> Result<usize, String> {
        let len = self.prompts.first().map(|p| p.len()).unwrap_or(0);
        if len == 0 || self.prompts.iter().any(|p| p.len() != len) {
            return Err("prompts must be non-empty and of equal length".into());
        }
        Ok(len)
    }
}

struct DriverOut {
    tokens: Vec<Vec<u32>>,
    passes: Vec<PassSample>,
}

fn run_driver(
    to_first: Box<dyn StageSink>,
    from_last: Box<dyn StageSource>,
    workload: &Workload,
    encode: EncodeOptions,
    verify: bool,
    clock: Clock,
    rec: &Recorder,
) -> Result<DriverOut, String> {
    let p_len = workload.prompt_len()?;
    let groups = workload.groups();
    let mut tokens = vec![Vec::new(); workload.prompts.len()];
    let mut passes = Vec::new();
    let mut sent_ns = vec![0u64; groups.len()];
    let mut outbox = Outbox::new(to_first, false);
    let (inbox, reader) = spawn_reader(from_last, clock);

    let send = |outbox: &mut Outbox,
                sent_ns: &mut [u64],
                g: usize,
                position: u32,
                inputs: Vec<(u64, u32)>|
     -> Result<(), String> {
        let msg = Message {
            kind: MessageKind::Data,
            hop: DRIVER_HOP,
            msg_id: g as u64,
            entries: inputs
                .into_iter()
                .map(|(seq, t)| Entry {
                    seq,
                    position,
                    body: EntryBody::Token(t),
                    commitment: Hash256([0; 32]),
                })
                .collect(),
        };
        let t = clock.now_ns();
        sent_ns[g] = t;
        ship(outbox, &msg, &encode, clock, rec, t)
    };

    for (g, members) in groups.iter().enumerate() {
        let inputs = members
            .iter()
            .map(|&s| (s as u64, workload.prompts[s][0]))
            .collect();
        send(&mut outbox, &mut sent_ns, g, 0, inputs)?;
    }
    let mut live = groups.len();
    let mut result = Ok(());
    while live > 0 {
        let Ok((frame, recv_ns)) = inbox.recv() else {
            result = Err("ring closed before every sequence finished".to_string());
            break;
        };
        let t_dec = clock.now_ns();
        let (msg, trace) = match wire::decode(&frame, verify) {
            Ok(m) => m,
            Err(e) => {
                result = Err(format!("driver: {e}"));
                break;
            }
        };
        rec.hop(HopSample {
            hop: msg.hop as usize,
            entries: msg.entries.len(),
            wire_bytes: frame.len(),
            serialize_ns: trace.encode_ns as u64,
            transfer_ns: recv_ns.saturating_sub(trace.t_send_ns),
            queue_ns: t_dec.saturating_sub(recv_ns),
            deserialize_ns: clock.now_ns() - t_dec,
            compute_ns: 0,
        });
        let g = msg.msg_id as usize;
        let Some(position) = msg.entries.first().map(|e| e.position) else {
            result = Err("empty micro-batch".into());
            break;
        };
        let decode = position as usize + 1 >= p_len;
        passes.push(PassSample {
            group: g,
            position,
            sent_ns: sent_ns[g],
            recv_ns,
            decode,
        });
        let mut next = Vec::with_capacity(msg.entries.len());
        let mut done = false;
        for e in &msg.entries {
            let EntryBody::Token(t) = e.body else {
                result = Err("driver received a hidden state".into());
                break;
            };
            let s = e.seq as usize;
            if decode {
                tokens[s].push(t);
                done = tokens[s].len() >= workload.gen_tokens;
                next.push((e.seq, t));
            } else {
                next.push((e.seq, workload.prompts[s][position as usize + 1]));
            }
        }
        if result.is_err() {
            break;
        }
        if done {
            live -= 1;
        } else if let Err(e) = send(&mut outbox, &mut sent_ns, g, position + 1, next) {
            result = Err(e);
            break;
        }
    }
    // Drain the ring: the shutdown message visits every stage and comes back.
    let shutdown = Message {
        kind: MessageKind::Shutdown,
        hop: DRIVER_HOP,
        msg_id: u64::MAX,
        entries: Vec::new(),
    };
    let t = clock.now_ns();
    let shut = ship(&mut outbox, &shutdown, &encode, clock, rec, t);
    if result.is_ok() && shut.is_ok() {
        while let Ok((frame, _)) = inbox.recv() {
            if let Ok((m, _)) = wire::decode(&frame, false)
                && m.kind == MessageKind::Shutdown
            {
                break;
            }
        }
    }
    let finished = outbox.finish();
    drop(inbox);
    let _ = reader.join();
    result?;
    shut?;
    finished?;
    Ok(DriverOut { tokens, passes })
}

// ─── Ring assembly ──────────────────────────────────────────────────────────

#[derive(Clone, Copy, Debug)]
pub enum LinkKind {
    /// In-process channels.
    Mem,
    /// Persistent loopback TCP.
    Tcp(TcpTuning),
}

#[derive(Clone, Debug)]
pub struct RingConfig {
    /// Contiguous layer ranges, one per stage, covering `0..n_layers`.
    pub splits: Vec<(usize, usize)>,
    pub link: LinkKind,
    /// One profile per network hop (`splits.len()` of them when there are two
    /// or more stages; ignored for one stage).
    pub profiles: Vec<LinkProfile>,
    pub overlap: bool,
    pub verify: bool,
    pub encode: EncodeOptions,
}

pub struct RingReport {
    pub tokens: Vec<Vec<u32>>,
    pub commitments: Commitments,
    pub hops: Vec<HopSample>,
    pub sends: Vec<(usize, EncodeStats)>,
    pub computes: Vec<StageComputeSample>,
    pub passes: Vec<PassSample>,
    pub wall_ns: u64,
}

fn make_link(kind: LinkKind, profile: LinkProfile) -> Result<Link, String> {
    let (sink, source): (Box<dyn StageSink>, Box<dyn StageSource>) = match kind {
        LinkKind::Mem => {
            let (a, b) = wire::mem_link();
            (Box::new(a), Box::new(b))
        }
        LinkKind::Tcp(tuning) => {
            let (a, b) = wire::tcp_loopback(tuning).map_err(|e| format!("tcp: {e}"))?;
            (Box::new(a), Box::new(b))
        }
    };
    if profile.is_ideal() {
        Ok((sink, source))
    } else {
        Ok((Box::new(ShapedSink::new(sink, profile)), source))
    }
}

/// Validate that `splits` tile `0..n_layers` contiguously.
pub fn check_splits(splits: &[(usize, usize)], n_layers: usize) -> Result<(), String> {
    let mut at = 0;
    for &(s, e) in splits {
        if s != at || e <= s {
            return Err(format!("splits {splits:?} do not tile 0..{n_layers}"));
        }
        at = e;
    }
    if at != n_layers {
        return Err(format!("splits {splits:?} do not tile 0..{n_layers}"));
    }
    Ok(())
}

/// Run `workload` around a ring of stages built from `model` and `cfg`.
pub fn run_ring(
    model: Arc<CachedIntegerModel>,
    cfg: &RingConfig,
    workload: &Workload,
) -> Result<RingReport, String> {
    let s = cfg.splits.len();
    check_splits(&cfg.splits, model.config.n_layers)?;
    if s > 1 && cfg.profiles.len() != s {
        return Err(format!("{s} stages need {s} hop profiles"));
    }
    let profile = |i: usize| {
        if s > 1 {
            cfg.profiles[i]
        } else {
            LinkProfile::ideal()
        }
    };
    let clock = Clock::start();
    let rec = Arc::new(Recorder::default());
    // Driver → stage 0 is local; hop i: stage i → stage i+1; hop S-1 → driver.
    let (to_first, mut upstream) = make_link(LinkKind::Mem, LinkProfile::ideal())?;
    let mut handles = Vec::with_capacity(s);
    for (i, &(start, end)) in cfg.splits.iter().enumerate() {
        let kind = if s > 1 { cfg.link } else { LinkKind::Mem };
        let (sink, next_source) = make_link(kind, profile(i))?;
        let source = std::mem::replace(&mut upstream, next_source);
        let compute: Box<dyn StageCompute> = Box::new(IntegerSlice::new(model.clone(), start, end));
        let stage_cfg = StageConfig {
            index: i,
            overlap: cfg.overlap,
            verify: cfg.verify,
            encode: cfg.encode,
        };
        let rec = rec.clone();
        handles.push(
            std::thread::Builder::new()
                .name(format!("stage-{i}"))
                .spawn(move || run_stage(compute, source, sink, stage_cfg, clock, rec))
                .map_err(|e| format!("spawn stage {i}: {e}"))?,
        );
    }
    let t0 = clock.now_ns();
    let driven = run_driver(
        to_first, upstream, workload, cfg.encode, cfg.verify, clock, &rec,
    );
    let wall_ns = clock.now_ns() - t0;
    let mut stage_err = None;
    for (i, h) in handles.into_iter().enumerate() {
        match h.join() {
            Ok(Ok(())) => {}
            Ok(Err(e)) => stage_err = stage_err.or(Some(e)),
            Err(_) => stage_err = stage_err.or(Some(format!("stage {i} panicked"))),
        }
    }
    let out = match (driven, stage_err) {
        (Ok(out), None) => out,
        (Err(e), Some(se)) => return Err(format!("{e}; {se}")),
        (Err(e), None) | (Ok(_), Some(e)) => return Err(e),
    };
    let rec = Arc::try_unwrap(rec).map_err(|_| "recorder still shared".to_string())?;
    Ok(RingReport {
        tokens: out.tokens,
        commitments: rec
            .commitments
            .into_inner()
            .unwrap_or_else(|p| p.into_inner()),
        hops: rec.hops.into_inner().unwrap_or_else(|p| p.into_inner()),
        sends: rec.sends.into_inner().unwrap_or_else(|p| p.into_inner()),
        computes: rec.computes.into_inner().unwrap_or_else(|p| p.into_inner()),
        passes: out.passes,
        wall_ns,
    })
}

/// The same workload computed locally, one sequence at a time, with no
/// transport: the tokens and per-stage commitments every ring run must match.
pub fn reference(
    model: &Arc<CachedIntegerModel>,
    splits: &[(usize, usize)],
    workload: &Workload,
) -> Result<(Vec<Vec<u32>>, Commitments), String> {
    check_splits(splits, model.config.n_layers)?;
    let p_len = workload.prompt_len()?;
    let mut tokens = vec![Vec::new(); workload.prompts.len()];
    let mut commits = BTreeMap::new();
    for (s, prompt) in workload.prompts.iter().enumerate() {
        let mut stages: Vec<IntegerSlice> = splits
            .iter()
            .map(|&(a, b)| IntegerSlice::new(model.clone(), a, b))
            .collect();
        let mut input = prompt[0];
        let mut position = 0u32;
        loop {
            let mut x = StageInput::Token(input);
            let mut sampled = None;
            for (i, st) in stages.iter_mut().enumerate() {
                match st.forward(s as u64, position, x)? {
                    StageResult::Hidden(h) => {
                        commits.insert((s as u64, position, i), codec::commit_i64(&h));
                        x = StageInput::Hidden(h);
                    }
                    StageResult::Token { id, logits_hash } => {
                        commits.insert((s as u64, position, i), logits_hash);
                        sampled = Some(id);
                        x = StageInput::Token(id);
                    }
                }
            }
            let t = sampled.ok_or("last stage did not sample")?;
            if position as usize + 1 >= p_len {
                tokens[s].push(t);
                if tokens[s].len() >= workload.gen_tokens {
                    break;
                }
                input = t;
            } else {
                input = prompt[position as usize + 1];
            }
            position += 1;
        }
    }
    Ok((tokens, commits))
}

/// Split `n_layers` into `stages` near-equal contiguous ranges.
pub fn even_splits(n_layers: usize, stages: usize) -> Vec<(usize, usize)> {
    let stages = stages.clamp(1, n_layers.max(1));
    (0..stages)
        .map(|i| (i * n_layers / stages, (i + 1) * n_layers / stages))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stage_net::codec::CodecChoice;

    fn model() -> Arc<CachedIntegerModel> {
        Arc::new(CachedIntegerModel::synthetic(42, 64, 32, 2, 64, 4))
    }

    fn cfg(splits: Vec<(usize, usize)>, link: LinkKind, overlap: bool) -> RingConfig {
        let n = splits.len();
        RingConfig {
            splits,
            link,
            profiles: vec![LinkProfile::ideal(); n],
            overlap,
            verify: true,
            encode: EncodeOptions::default(),
        }
    }

    #[test]
    fn ring_matches_reference_for_every_split_batch_and_overlap() {
        let m = model();
        let wl1 = Workload::synthetic(5, 3, 4, 1, 64);
        let (want_tokens, _) = reference(&m, &[(0, 4)], &wl1).expect("reference");
        for splits in [
            vec![(0, 4)],
            vec![(0, 2), (2, 4)],
            vec![(0, 1), (1, 3), (3, 4)],
            vec![(0, 1), (1, 2), (2, 3), (3, 4)],
        ] {
            let (ref_tokens, ref_commits) = reference(&m, &splits, &wl1).expect("ref");
            assert_eq!(ref_tokens, want_tokens, "split {splits:?} changed tokens");
            for microbatch in [1, 2, 5] {
                for overlap in [false, true] {
                    let wl = Workload {
                        microbatch,
                        ..wl1.clone()
                    };
                    let r = run_ring(m.clone(), &cfg(splits.clone(), LinkKind::Mem, overlap), &wl)
                        .expect("ring");
                    assert_eq!(r.tokens, want_tokens, "{splits:?} mb={microbatch}");
                    assert_eq!(r.commitments, ref_commits, "{splits:?} mb={microbatch}");
                }
            }
        }
    }

    #[test]
    fn tcp_shaped_ring_is_bit_exact_and_measures_hops() {
        let m = model();
        let wl = Workload::synthetic(3, 2, 3, 1, 64);
        let splits = vec![(0, 2), (2, 4)];
        let (ref_tokens, ref_commits) = reference(&m, &splits, &wl).expect("ref");
        let mut c = cfg(splits, LinkKind::Tcp(TcpTuning::default()), true);
        c.profiles = vec![
            LinkProfile {
                rtt_ms: 4.0,
                jitter_ms: 0.5,
                uplink_mbps: Some(100.0),
                seed: 1,
            };
            2
        ];
        for codec in [CodecChoice::Raw, CodecChoice::Auto] {
            c.encode = EncodeOptions {
                codec,
                ballast_dim: Some(256),
            };
            let r = run_ring(m.clone(), &c, &wl).expect("ring");
            assert_eq!(r.tokens, ref_tokens);
            assert_eq!(r.commitments, ref_commits);
            // Every pass crosses two hops of ≥ ~1.5 ms one-way each.
            let decode: Vec<_> = r.passes.iter().filter(|p| p.decode).collect();
            assert_eq!(decode.len(), 3 * 3);
            assert!(
                decode.iter().all(|p| p.recv_ns - p.sent_ns >= 3_000_000),
                "WAN delay not applied"
            );
            assert!(r.hops.iter().any(|h| h.hop == 0));
            assert!(r.hops.iter().any(|h| h.hop == 1));
        }
    }

    #[test]
    fn tampered_link_is_rejected() {
        // A sink that flips a bit in the first hidden value of every frame.
        struct Corrupt(Box<dyn StageSink>);
        impl StageSink for Corrupt {
            fn send_frame(&mut self, frame: &[u8]) -> std::io::Result<()> {
                let mut f = frame.to_vec();
                let at = wire::HEADER_LEN + wire::ENTRY_HEADER_LEN;
                if f.len() > at + 8 && f[wire::HEADER_LEN + 12] == 1 {
                    f[at + 7] ^= 0x10;
                }
                self.0.send_frame(&f)
            }
        }
        let m = model();
        let (a, b) = wire::mem_link();
        let (c, d) = wire::mem_link();
        let clock = Clock::start();
        let rec = Arc::new(Recorder::default());
        let stage = std::thread::spawn({
            let rec = rec.clone();
            move || {
                run_stage(
                    Box::new(IntegerSlice::new(m, 2, 4)),
                    Box::new(b),
                    Box::new(c),
                    StageConfig {
                        index: 1,
                        overlap: false,
                        verify: true,
                        encode: EncodeOptions::default(),
                    },
                    clock,
                    rec,
                )
            }
        });
        let hidden = vec![65_536i64; 32];
        let msg = Message {
            kind: MessageKind::Data,
            hop: 0,
            msg_id: 0,
            entries: vec![Entry {
                seq: 0,
                position: 0,
                commitment: codec::commit_i64(&hidden),
                body: EntryBody::Hidden(hidden),
            }],
        };
        let mut frame = Vec::new();
        wire::encode(
            &msg,
            &EncodeOptions {
                codec: CodecChoice::Raw,
                ballast_dim: None,
            },
            &mut frame,
        );
        let mut corrupt = Corrupt(Box::new(a));
        corrupt.send_frame(&frame).expect("send");
        drop(corrupt);
        let err = stage.join().expect("join").expect_err("must reject");
        assert!(err.contains("commitment"), "{err}");
        drop(d);
    }

    #[test]
    fn even_splits_tile() {
        for n in 1..20 {
            for s in 1..6 {
                check_splits(&even_splits(n, s), n).expect("tiles");
            }
        }
    }
}
