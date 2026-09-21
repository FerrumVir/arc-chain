//! Bounded, process-wide consensus counters, exposed at
//! `GET /consensus/diagnostics`.
//!
//! These exist so a throughput question is answered by measurement rather than
//! by counting log lines. A rejection count alone never says where wall time
//! went: these record, for each path, how often it ran, how it ended, and how
//! long it took - in fixed-size atomics that cost the same after a day as after
//! a minute.
//!
//! One consensus loop runs per process, so a single static is the whole state.
//! Unit tests that build several managers in one process share it; that only
//! affects these diagnostics, never consensus.

use std::sync::atomic::{AtomicU64, Ordering::Relaxed};

macro_rules! counters {
    ($($name:ident),* $(,)?) => {
        pub struct ConsensusDiagnostics { $(pub $name: AtomicU64,)* }
        impl ConsensusDiagnostics {
            pub const fn new() -> Self { Self { $($name: AtomicU64::new(0),)* } }
            pub fn snapshot(&self) -> serde_json::Map<String, serde_json::Value> {
                let mut map = serde_json::Map::new();
                $(map.insert(stringify!($name).to_string(),
                             serde_json::Value::from(self.$name.load(Relaxed)));)*
                map
            }
        }
    };
}

counters!(
    // live gossip
    live_blocks_received,
    live_blocks_accepted,
    live_blocks_rejected_missing_parents,
    live_blocks_rejected_future_round,
    live_blocks_rejected_duplicate,
    live_blocks_rejected_other,
    live_block_validate_us,
    // history, requesting side
    history_requests_sent,
    history_requests_throttled,
    history_responses_received,
    history_imports_ok,
    history_import_rounds_advanced,
    history_import_blocks_inserted,
    history_import_rejected_nothing_new,
    history_import_rejected_thin_first_round,
    history_import_rejected_gap,
    history_import_rejected_above_waiting_round,
    history_import_rejected_invalid_block,
    history_import_rejected_other,
    history_import_us,
    // history, serving side
    history_requests_served,
    history_blocks_served,
    history_requests_empty_pruned,
    history_requests_empty_not_held,
    // progress
    rounds_advanced,
    canonical_blocks_produced,
    // durability costs
    signing_record_persists,
    signing_record_persist_us,
    dag_block_persist_us,
    // outbound messages the transport refused (channel full or closed)
    outbound_dropped,
    // early blocks: held instead of dropped (arc_consensus::pending)
    pending_blocks_held,
    pending_blocks_released,
    pending_blocks_expired,
    pending_blocks_now,
    targeted_history_requests,
    // where the consensus loop's time goes
    loop_busy_us,
    loop_max_iteration_us,
    loop_slow_iterations,
    phase_inbound_us,
    phase_certificates_us,
    phase_history_us,
    phase_checkpoint_us,
    phase_absence_us,
    phase_propose_us,
    phase_commit_us,
    commit_execute_us,
    state_snapshot_publish_us,
    // already-receipted transactions refused re-proposal / re-admission
    stale_transactions_dropped,
);

pub static DIAG: ConsensusDiagnostics = ConsensusDiagnostics::new();

/// Add elapsed microseconds since `start` to a counter.
pub fn add_elapsed(counter: &AtomicU64, start: std::time::Instant) {
    counter.fetch_add(start.elapsed().as_micros() as u64, Relaxed);
}

pub fn bump(counter: &AtomicU64) {
    counter.fetch_add(1, Relaxed);
}

/// Classify a live-block insertion error by its cause, so "missing parents"
/// (a node that is behind) is never confused with a malformed block.
pub fn classify_live_rejection(message: &str) {
    let counter = if message.contains("missing or wrong-round parents") {
        &DIAG.live_blocks_rejected_missing_parents
    } else if message.contains("ahead") || message.contains("future") {
        &DIAG.live_blocks_rejected_future_round
    } else if message.contains("already") || message.contains("duplicate") {
        &DIAG.live_blocks_rejected_duplicate
    } else {
        &DIAG.live_blocks_rejected_other
    };
    bump(counter);
}

/// Classify a history import refusal by its cause.
pub fn classify_import_rejection(message: &str) {
    let counter = if message.contains("added nothing") {
        &DIAG.history_import_rejected_nothing_new
    } else if message.contains("below quorum") {
        &DIAG.history_import_rejected_thin_first_round
    } else if message.contains("gap") {
        &DIAG.history_import_rejected_gap
    } else if message.contains("waiting") || message.contains("above") {
        &DIAG.history_import_rejected_above_waiting_round
    } else if message.contains("invalid") {
        &DIAG.history_import_rejected_invalid_block
    } else {
        &DIAG.history_import_rejected_other
    };
    bump(counter);
}

/// Record a `try_send` outcome; a refusal is a message that never left.
pub fn note_send<T, E>(result: &Result<T, E>) {
    if result.is_err() {
        bump(&DIAG.outbound_dropped);
    }
}

/// Add the time since `mark` to `counter` and return a fresh mark.
pub fn phase(counter: &AtomicU64, mark: std::time::Instant) -> std::time::Instant {
    add_elapsed(counter, mark);
    std::time::Instant::now()
}

/// Inbound message kinds timed individually, so "the inbound phase took most
/// of the loop" can say WHICH messages.
pub const INBOUND_KINDS: [&str; 12] = [
    "dag_block",
    "transactions",
    "finality_vote",
    "finality_certificate",
    "absence_vote",
    "absence_certificate",
    "history_request",
    "history_response",
    "heartbeat",
    "peer_connected",
    "peer_disconnected",
    "other",
];

pub struct KindCounters {
    pub count: [AtomicU64; 12],
    pub us: [AtomicU64; 12],
}

#[allow(clippy::declare_interior_mutable_const)]
const ZERO: AtomicU64 = AtomicU64::new(0);

pub static INBOUND: KindCounters = KindCounters {
    count: [ZERO; 12],
    us: [ZERO; 12],
};

/// Records one inbound message's handling time when dropped - including when
/// the handler leaves early with `continue`, which is most of them.
pub struct InboundTimer {
    kind: usize,
    started: std::time::Instant,
}

impl InboundTimer {
    pub fn start(kind: usize) -> Self {
        Self {
            kind: kind.min(INBOUND_KINDS.len() - 1),
            started: std::time::Instant::now(),
        }
    }
}

impl Drop for InboundTimer {
    fn drop(&mut self) {
        INBOUND.count[self.kind].fetch_add(1, Relaxed);
        INBOUND.us[self.kind].fetch_add(self.started.elapsed().as_micros() as u64, Relaxed);
    }
}

/// The inbound counters, by name.
pub fn inbound_snapshot() -> serde_json::Map<String, serde_json::Value> {
    let mut map = serde_json::Map::new();
    for (i, name) in INBOUND_KINDS.iter().enumerate() {
        map.insert(format!("inbound_{name}_count"), INBOUND.count[i].load(Relaxed).into());
        map.insert(format!("inbound_{name}_us"), INBOUND.us[i].load(Relaxed).into());
    }
    map
}
