use std::collections::{BTreeSet, HashMap};
use std::sync::atomic::{AtomicU8, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, PoisonError};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

pub const COMMUNITY_WORKER_STATUS_PATH: &str = "/community/worker/status";

pub const COMMUNITY_WORKER_STATUS_SCHEMA: &str = "arc.community.worker-status.v1";

pub const CLAIM_IDLE_REPOLL: Duration = Duration::from_millis(500);

pub const CLAIM_RETRY_CAP: Duration = Duration::from_secs(30);

const MAX_STATUS_ERROR_BYTES: usize = 240;

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkerState {
    Polling,
    Computing,
    Reconnecting,
}

impl WorkerState {
    fn encode(self) -> u8 {
        match self {
            Self::Polling => 0,
            Self::Computing => 1,
            Self::Reconnecting => 2,
        }
    }

    fn decode(value: u8) -> Self {
        match value {
            1 => Self::Computing,
            2 => Self::Reconnecting,
            _ => Self::Polling,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkerMode {
    PartialShard,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PartialShardPhase {
    Starting,
    Loading,
    Ready,
    Computing,
    Reconnecting,
    Failed,
    Stopped,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct PartialShardLayerRangeSnapshot {
    pub start_layer: usize,
    pub end_layer: usize,
    pub total_layers: usize,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct PartialShardWorkerSnapshot {
    pub phase: PartialShardPhase,
    pub model_name: String,
    pub model_id: String,
    pub lease_seq: Option<u64>,
    pub lease_epoch: Option<u64>,
    pub assigned_range: Option<PartialShardLayerRangeSnapshot>,
    pub ready_expires_at_unix_ms: Option<u64>,
    pub lease_live: bool,
    pub steps_executed: u64,
    pub steps_submitted: u64,
    pub submit_retries: u64,
    pub submit_stale_conflicts: u64,
    pub cancellations_applied: u64,
    pub rebalances_completed: u64,
    pub reconnect_attempts: u64,
    pub last_heartbeat_unix_ms: Option<u64>,
    pub last_step_submitted_unix_ms: Option<u64>,
    pub last_error: Option<String>,
    pub rewards_enabled: bool,
    pub reward_eligible: bool,
}

#[derive(Debug)]
struct PartialShardTelemetry {
    model_name: String,
    model_id: String,
    total_layers: usize,
    phase: PartialShardPhase,
    lease_seq: Option<u64>,
    lease_epoch: Option<u64>,
    assigned_range: Option<PartialShardLayerRangeSnapshot>,
    ready_expires_at_unix_ms: Option<u64>,
    steps_executed: u64,
    steps_submitted: u64,
    submit_retries: u64,
    submit_stale_conflicts: u64,
    cancellations_applied: u64,
    rebalances_completed: u64,
    reconnect_attempts: u64,
    last_heartbeat_unix_ms: Option<u64>,
    last_step_submitted_unix_ms: Option<u64>,
    last_error: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum JobOutcome {
    Completed { verified: bool },
    Declined,
    Failed,
}

#[derive(Debug)]
pub struct CommunityWorkerStatus {
    worker_id: String,
    public_name: String,
    coordinators_total: u32,
    prevent_sleep_during_jobs: bool,
    started_unix_ms: u64,
    state: AtomicU8,
    coordinators_registered: AtomicU32,
    jobs_claimed: AtomicU64,
    jobs_completed: AtomicU64,
    jobs_verified: AtomicU64,
    jobs_failed: AtomicU64,
    jobs_declined: AtomicU64,
    last_job_completed_unix_ms: AtomicU64,
    last_registration_unix_ms: AtomicU64,
    shard: Option<Mutex<PartialShardTelemetry>>,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct CommunityWorkerSnapshot {
    pub schema: &'static str,
    pub worker_id: String,
    pub public_name: String,
    pub state: WorkerState,
    pub coordinators_total: u32,
    pub coordinators_registered: u32,
    pub jobs_claimed: u64,
    pub jobs_completed: u64,
    pub jobs_verified: u64,
    pub jobs_failed: u64,
    pub jobs_declined: u64,
    pub last_job_completed_unix_ms: Option<u64>,
    pub last_registration_unix_ms: Option<u64>,
    pub started_unix_ms: u64,
    pub prevent_sleep_during_jobs: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mode: Option<WorkerMode>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub shard: Option<PartialShardWorkerSnapshot>,
}

fn unix_ms_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

fn nonzero(value: u64) -> Option<u64> {
    (value != 0).then_some(value)
}

pub fn sanitize_status_error(raw: &str) -> String {
    let mut cleaned = String::new();
    let mut last_space = true;
    for ch in raw.chars() {
        if ch.is_control() || ch.is_whitespace() {
            if !last_space {
                cleaned.push(' ');
                last_space = true;
            }
        } else {
            cleaned.push(ch);
            last_space = false;
        }
        if cleaned.len() >= MAX_STATUS_ERROR_BYTES {
            break;
        }
    }
    let trimmed = cleaned.trim();
    if trimmed.is_empty() {
        return "worker error".to_string();
    }
    let mut end = trimmed.len().min(MAX_STATUS_ERROR_BYTES);
    while end > 0 && !trimmed.is_char_boundary(end) {
        end -= 1;
    }
    trimmed[..end].to_string()
}

impl CommunityWorkerStatus {
    pub fn new(
        worker_id: impl Into<String>,
        public_name: impl Into<String>,
        coordinators_total: usize,
        prevent_sleep_during_jobs: bool,
    ) -> Self {
        Self {
            worker_id: worker_id.into(),
            public_name: public_name.into(),
            coordinators_total: u32::try_from(coordinators_total).unwrap_or(u32::MAX),
            prevent_sleep_during_jobs,
            started_unix_ms: unix_ms_now(),
            state: AtomicU8::new(WorkerState::Polling.encode()),
            coordinators_registered: AtomicU32::new(0),
            jobs_claimed: AtomicU64::new(0),
            jobs_completed: AtomicU64::new(0),
            jobs_verified: AtomicU64::new(0),
            jobs_failed: AtomicU64::new(0),
            jobs_declined: AtomicU64::new(0),
            last_job_completed_unix_ms: AtomicU64::new(0),
            last_registration_unix_ms: AtomicU64::new(0),
            shard: None,
        }
    }

    pub fn new_sharded(
        worker_id: impl Into<String>,
        public_name: impl Into<String>,
        model_name: impl Into<String>,
        model_id: impl Into<String>,
        total_layers: usize,
        prevent_sleep_during_jobs: bool,
    ) -> Self {
        Self {
            worker_id: worker_id.into(),
            public_name: public_name.into(),
            coordinators_total: 1,
            prevent_sleep_during_jobs,
            started_unix_ms: unix_ms_now(),
            state: AtomicU8::new(WorkerState::Reconnecting.encode()),
            coordinators_registered: AtomicU32::new(0),
            jobs_claimed: AtomicU64::new(0),
            jobs_completed: AtomicU64::new(0),
            jobs_verified: AtomicU64::new(0),
            jobs_failed: AtomicU64::new(0),
            jobs_declined: AtomicU64::new(0),
            last_job_completed_unix_ms: AtomicU64::new(0),
            last_registration_unix_ms: AtomicU64::new(0),
            shard: Some(Mutex::new(PartialShardTelemetry {
                model_name: model_name.into(),
                model_id: model_id.into(),
                total_layers,
                phase: PartialShardPhase::Starting,
                lease_seq: None,
                lease_epoch: None,
                assigned_range: None,
                ready_expires_at_unix_ms: None,
                steps_executed: 0,
                steps_submitted: 0,
                submit_retries: 0,
                submit_stale_conflicts: 0,
                cancellations_applied: 0,
                rebalances_completed: 0,
                reconnect_attempts: 0,
                last_heartbeat_unix_ms: None,
                last_step_submitted_unix_ms: None,
                last_error: None,
            })),
        }
    }

    pub fn set_state(&self, state: WorkerState) {
        self.state.store(state.encode(), Ordering::Relaxed);
    }

    pub fn record_registration_round(&self, accepted: usize) {
        let accepted = u32::try_from(accepted).unwrap_or(u32::MAX);
        self.coordinators_registered
            .store(accepted, Ordering::Relaxed);
        if accepted > 0 {
            self.last_registration_unix_ms
                .store(unix_ms_now(), Ordering::Relaxed);
        }
    }

    pub fn record_claim(&self) {
        self.jobs_claimed.fetch_add(1, Ordering::Relaxed);
    }

    pub fn record_outcome(&self, outcome: JobOutcome) {
        match outcome {
            JobOutcome::Completed { verified } => {
                self.jobs_completed.fetch_add(1, Ordering::Relaxed);
                if verified {
                    self.jobs_verified.fetch_add(1, Ordering::Relaxed);
                }
                self.last_job_completed_unix_ms
                    .store(unix_ms_now(), Ordering::Relaxed);
            }
            JobOutcome::Declined => {
                self.jobs_declined.fetch_add(1, Ordering::Relaxed);
            }
            JobOutcome::Failed => {
                self.jobs_failed.fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    fn with_shard<F>(&self, f: F)
    where
        F: FnOnce(&mut PartialShardTelemetry),
    {
        if let Some(lock) = self.shard.as_ref() {
            let mut guard = lock.lock().unwrap_or_else(PoisonError::into_inner);
            f(&mut guard);
        }
    }

    pub fn record_shard_registered(&self) {
        self.record_registration_round(1);
    }

    pub fn record_shard_loading(
        &self,
        lease_seq: u64,
        lease_epoch: u64,
        start_layer: usize,
        end_layer: usize,
        total_layers: usize,
    ) {
        self.set_state(WorkerState::Reconnecting);
        self.with_shard(|shard| {
            shard.total_layers = total_layers;
            shard.phase = PartialShardPhase::Loading;
            shard.lease_seq = Some(lease_seq);
            shard.lease_epoch = Some(lease_epoch);
            shard.assigned_range = Some(PartialShardLayerRangeSnapshot {
                start_layer,
                end_layer,
                total_layers,
            });
            shard.ready_expires_at_unix_ms = None;
        });
    }

    pub fn record_shard_heartbeat_provisioning(&self) {
        self.with_shard(|shard| {
            shard.last_heartbeat_unix_ms = Some(unix_ms_now());
        });
    }

    pub fn record_shard_ready(
        &self,
        lease_seq: u64,
        lease_epoch: u64,
        range: PartialShardLayerRangeSnapshot,
        expires_at_unix_ms: u64,
    ) {
        self.record_registration_round(1);
        self.set_state(WorkerState::Polling);
        self.with_shard(|shard| {
            shard.total_layers = range.total_layers;
            shard.phase = PartialShardPhase::Ready;
            shard.lease_seq = Some(lease_seq);
            shard.lease_epoch = Some(lease_epoch);
            shard.assigned_range = Some(range);
            shard.ready_expires_at_unix_ms = Some(expires_at_unix_ms);
            shard.last_heartbeat_unix_ms = Some(unix_ms_now());
            shard.last_error = None;
        });
    }

    pub fn record_shard_heartbeat_active(&self, expires_at_unix_ms: u64) {
        self.with_shard(|shard| {
            shard.ready_expires_at_unix_ms = Some(expires_at_unix_ms);
            shard.last_heartbeat_unix_ms = Some(unix_ms_now());
            if shard.phase == PartialShardPhase::Ready
                || shard.phase == PartialShardPhase::Computing
            {
                shard.last_error = None;
            }
        });
    }

    pub fn record_shard_computing(&self) {
        self.set_state(WorkerState::Computing);
        self.with_shard(|shard| {
            shard.phase = PartialShardPhase::Computing;
        });
    }

    pub fn record_shard_idle_ready(&self) {
        self.with_shard(|shard| {
            if shard.phase == PartialShardPhase::Computing {
                shard.phase = PartialShardPhase::Ready;
                self.set_state(WorkerState::Polling);
            }
        });
    }

    pub fn record_shard_step_executed(&self) {
        self.with_shard(|shard| {
            shard.steps_executed = shard.steps_executed.saturating_add(1);
        });
    }

    pub fn record_shard_step_submitted(&self) {
        self.with_shard(|shard| {
            shard.steps_submitted = shard.steps_submitted.saturating_add(1);
            shard.last_step_submitted_unix_ms = Some(unix_ms_now());
        });
    }

    pub fn record_shard_submit_retry(&self) {
        self.with_shard(|shard| {
            shard.submit_retries = shard.submit_retries.saturating_add(1);
        });
    }

    pub fn record_shard_submit_stale_conflict(&self) {
        self.with_shard(|shard| {
            shard.submit_stale_conflicts = shard.submit_stale_conflicts.saturating_add(1);
        });
    }

    pub fn record_shard_cancellation_applied(&self) {
        self.with_shard(|shard| {
            shard.cancellations_applied = shard.cancellations_applied.saturating_add(1);
        });
    }

    pub fn record_shard_rebalance_completed(&self) {
        self.with_shard(|shard| {
            shard.rebalances_completed = shard.rebalances_completed.saturating_add(1);
        });
    }

    pub fn record_shard_reconnecting(&self, reason: &str) {
        self.record_registration_round(0);
        self.set_state(WorkerState::Reconnecting);
        self.with_shard(|shard| {
            shard.phase = PartialShardPhase::Reconnecting;
            shard.lease_seq = None;
            shard.lease_epoch = None;
            shard.assigned_range = None;
            shard.ready_expires_at_unix_ms = None;
            shard.last_error = Some(sanitize_status_error(reason));
        });
    }

    pub fn record_shard_reconnect_attempt(&self, reason: &str) {
        self.record_registration_round(0);
        self.set_state(WorkerState::Reconnecting);
        self.with_shard(|shard| {
            shard.phase = PartialShardPhase::Reconnecting;
            shard.lease_seq = None;
            shard.lease_epoch = None;
            shard.assigned_range = None;
            shard.ready_expires_at_unix_ms = None;
            shard.reconnect_attempts = shard.reconnect_attempts.saturating_add(1);
            shard.last_error = Some(sanitize_status_error(reason));
        });
    }

    pub fn record_shard_failed(&self, reason: &str) {
        self.record_registration_round(0);
        self.set_state(WorkerState::Reconnecting);
        self.with_shard(|shard| {
            shard.phase = PartialShardPhase::Failed;
            shard.lease_seq = None;
            shard.lease_epoch = None;
            shard.assigned_range = None;
            shard.ready_expires_at_unix_ms = None;
            shard.last_error = Some(sanitize_status_error(reason));
        });
    }

    pub fn record_shard_stopped(&self) {
        self.record_registration_round(0);
        self.set_state(WorkerState::Reconnecting);
        self.with_shard(|shard| {
            shard.phase = PartialShardPhase::Stopped;
            shard.lease_seq = None;
            shard.lease_epoch = None;
            shard.assigned_range = None;
            shard.ready_expires_at_unix_ms = None;
        });
    }

    pub fn snapshot(&self) -> CommunityWorkerSnapshot {
        self.snapshot_at(unix_ms_now())
    }

    pub fn snapshot_at(&self, now_unix_ms: u64) -> CommunityWorkerSnapshot {
        let mut state = WorkerState::decode(self.state.load(Ordering::Relaxed));
        let mut coordinators_registered = self.coordinators_registered.load(Ordering::Relaxed);
        let (mode, shard) = match self.shard.as_ref() {
            Some(lock) => {
                let guard = lock.lock().unwrap_or_else(PoisonError::into_inner);
                let mut phase = guard.phase;
                let mut lease_seq = guard.lease_seq;
                let mut lease_epoch = guard.lease_epoch;
                let mut assigned_range = guard
                    .assigned_range
                    .as_ref()
                    .filter(|range| {
                        range.total_layers == guard.total_layers
                            && range.total_layers >= 2
                            && range.start_layer < range.end_layer
                            && range.end_layer <= range.total_layers
                            && range.end_layer - range.start_layer < range.total_layers
                    })
                    .cloned();
                let mut ready_expires_at_unix_ms = guard.ready_expires_at_unix_ms;
                let lease_live = matches!(
                    phase,
                    PartialShardPhase::Ready | PartialShardPhase::Computing
                ) && assigned_range.is_some()
                    && lease_seq.is_some_and(|seq| seq > 0)
                    && lease_epoch.is_some_and(|epoch| epoch > 0)
                    && ready_expires_at_unix_ms.is_some_and(|exp| now_unix_ms < exp);
                if matches!(
                    phase,
                    PartialShardPhase::Ready | PartialShardPhase::Computing
                ) && !lease_live
                {
                    phase = PartialShardPhase::Reconnecting;
                    state = WorkerState::Reconnecting;
                    coordinators_registered = 0;
                    lease_seq = None;
                    lease_epoch = None;
                    assigned_range = None;
                    ready_expires_at_unix_ms = None;
                }
                (
                    Some(WorkerMode::PartialShard),
                    Some(PartialShardWorkerSnapshot {
                        phase,
                        model_name: guard.model_name.clone(),
                        model_id: guard.model_id.clone(),
                        lease_seq,
                        lease_epoch,
                        assigned_range,
                        ready_expires_at_unix_ms,
                        lease_live,
                        steps_executed: guard.steps_executed,
                        steps_submitted: guard.steps_submitted,
                        submit_retries: guard.submit_retries,
                        submit_stale_conflicts: guard.submit_stale_conflicts,
                        cancellations_applied: guard.cancellations_applied,
                        rebalances_completed: guard.rebalances_completed,
                        reconnect_attempts: guard.reconnect_attempts,
                        last_heartbeat_unix_ms: guard.last_heartbeat_unix_ms,
                        last_step_submitted_unix_ms: guard.last_step_submitted_unix_ms,
                        last_error: guard.last_error.clone(),
                        rewards_enabled: false,
                        reward_eligible: false,
                    }),
                )
            }
            None => (None, None),
        };
        CommunityWorkerSnapshot {
            schema: COMMUNITY_WORKER_STATUS_SCHEMA,
            worker_id: self.worker_id.clone(),
            public_name: self.public_name.clone(),
            state,
            coordinators_total: self.coordinators_total,
            coordinators_registered,
            jobs_claimed: self.jobs_claimed.load(Ordering::Relaxed),
            jobs_completed: self.jobs_completed.load(Ordering::Relaxed),
            jobs_verified: self.jobs_verified.load(Ordering::Relaxed),
            jobs_failed: self.jobs_failed.load(Ordering::Relaxed),
            jobs_declined: self.jobs_declined.load(Ordering::Relaxed),
            last_job_completed_unix_ms: nonzero(
                self.last_job_completed_unix_ms.load(Ordering::Relaxed),
            ),
            last_registration_unix_ms: nonzero(
                self.last_registration_unix_ms.load(Ordering::Relaxed),
            ),
            started_unix_ms: self.started_unix_ms,
            prevent_sleep_during_jobs: self.prevent_sleep_during_jobs,
            mode,
            shard,
        }
    }
}

static INSTALLED: OnceLock<Arc<CommunityWorkerStatus>> = OnceLock::new();

pub fn install(status: Arc<CommunityWorkerStatus>) -> Arc<CommunityWorkerStatus> {
    INSTALLED.get_or_init(|| status).clone()
}

pub fn installed() -> Option<Arc<CommunityWorkerStatus>> {
    INSTALLED.get().cloned()
}

pub fn submit_response_is_quorum_verified(body: &str) -> bool {
    serde_json::from_str::<serde_json::Value>(body)
        .ok()
        .and_then(|value| value.get("verification")?.get("quorum_verified")?.as_bool())
        .unwrap_or(false)
}

pub fn claim_retry_delay(failed_rounds: u32) -> Duration {
    if failed_rounds == 0 {
        return CLAIM_IDLE_REPOLL;
    }
    let exponent = (failed_rounds - 1).min(16);
    Duration::from_secs(1)
        .saturating_mul(1u32 << exponent)
        .min(CLAIM_RETRY_CAP)
}

pub fn wall_clock_gap_suggests_sleep(
    previous_round: SystemTime,
    now: SystemTime,
    interval: Duration,
) -> bool {
    now.duration_since(previous_round)
        .is_ok_and(|gap| gap > interval.saturating_mul(3))
}

pub const REREGISTER_COOLDOWN: Duration = Duration::from_secs(15);

pub const REREGISTER_JITTER_MAX: Duration = Duration::from_secs(5);

pub struct ReregistrationQueue {
    state: Mutex<ReregistrationState>,
    wakeup: tokio::sync::Notify,
    cooldown: Duration,
    jitter_max: Duration,
}

#[derive(Default)]
struct ReregistrationState {
    pending: BTreeSet<String>,
    next_allowed: HashMap<String, Instant>,
}

impl Default for ReregistrationQueue {
    fn default() -> Self {
        Self::new()
    }
}

impl ReregistrationQueue {
    pub fn new() -> Self {
        Self::with_cooldown(REREGISTER_COOLDOWN, REREGISTER_JITTER_MAX)
    }

    pub fn with_cooldown(cooldown: Duration, jitter_max: Duration) -> Self {
        Self {
            state: Mutex::new(ReregistrationState::default()),
            wakeup: tokio::sync::Notify::new(),
            cooldown,
            jitter_max,
        }
    }

    pub fn request(&self, coordinator: &str) -> bool {
        self.request_at(coordinator, Instant::now())
    }

    pub fn request_at(&self, coordinator: &str, now: Instant) -> bool {
        let jitter = self.jitter();
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        if state
            .next_allowed
            .get(coordinator)
            .is_some_and(|allowed| *allowed > now)
        {
            return false;
        }
        state
            .next_allowed
            .insert(coordinator.to_string(), now + self.cooldown + jitter);
        state.pending.insert(coordinator.to_string());
        drop(state);
        self.wakeup.notify_one();
        true
    }

    pub fn take_pending(&self) -> Vec<String> {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        std::mem::take(&mut state.pending).into_iter().collect()
    }

    pub async fn notified(&self) {
        self.wakeup.notified().await;
    }

    fn jitter(&self) -> Duration {
        use std::hash::{BuildHasher as _, Hasher as _};
        let max_ms = u64::try_from(self.jitter_max.as_millis()).unwrap_or(u64::MAX);
        if max_ms == 0 {
            return Duration::ZERO;
        }
        let roll = std::collections::hash_map::RandomState::new()
            .build_hasher()
            .finish();
        Duration::from_millis(roll % max_ms.saturating_add(1))
    }
}

#[path = "../../../shared/keep_awake.rs"]
mod keep_awake;
pub use keep_awake::{KeepAwake, keep_awake_helper_command};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn partial_snapshot_rejects_expired_or_malformed_leases_without_counting_whole_jobs() {
        for computing in [false, true] {
            let status =
                CommunityWorkerStatus::new_sharded("worker", "node", "model", "id", 4, false);
            status.record_shard_ready(
                1,
                1,
                PartialShardLayerRangeSnapshot {
                    start_layer: 0,
                    end_layer: 2,
                    total_layers: 4,
                },
                200,
            );
            status.record_shard_step_executed();
            status.record_shard_step_submitted();
            if computing {
                status.record_shard_computing();
            }
            let live = status.snapshot_at(199);
            let shard = live.shard.unwrap();
            assert!(shard.lease_live);
            assert_eq!(shard.steps_submitted, 1);
            assert_eq!((live.jobs_completed, live.jobs_verified), (0, 0));
            assert!(!shard.reward_eligible && !shard.rewards_enabled);
            let expired = status.snapshot_at(200);
            let shard = expired.shard.unwrap();
            assert_eq!(shard.phase, PartialShardPhase::Reconnecting);
            assert!(!shard.lease_live);
            assert!(
                shard.assigned_range.is_none()
                    && shard.lease_seq.is_none()
                    && shard.lease_epoch.is_none()
            );
            assert_eq!(expired.coordinators_registered, 0);
            status.record_shard_ready(
                1,
                1,
                PartialShardLayerRangeSnapshot {
                    start_layer: 0,
                    end_layer: 4,
                    total_layers: 4,
                },
                300,
            );
            let malformed = status.snapshot_at(201).shard.unwrap();
            assert_eq!(malformed.phase, PartialShardPhase::Reconnecting);
            assert!(!malformed.lease_live);
        }
    }

    #[test]
    fn snapshot_counts_claims_completions_verifications_and_failures() {
        let status = CommunityWorkerStatus::new("0xabc", "node-abc", 6, true);
        status.record_registration_round(4);
        status.record_claim();
        status.record_claim();
        status.record_claim();
        status.record_claim();
        status.record_outcome(JobOutcome::Completed { verified: true });
        status.record_outcome(JobOutcome::Completed { verified: false });
        status.record_outcome(JobOutcome::Declined);
        status.record_outcome(JobOutcome::Failed);
        status.set_state(WorkerState::Computing);

        let snapshot = status.snapshot();
        assert_eq!(snapshot.schema, COMMUNITY_WORKER_STATUS_SCHEMA);
        assert_eq!(snapshot.worker_id, "0xabc");
        assert_eq!(snapshot.public_name, "node-abc");
        assert_eq!(snapshot.state, WorkerState::Computing);
        assert_eq!(
            (
                snapshot.coordinators_registered,
                snapshot.coordinators_total
            ),
            (4, 6)
        );
        assert_eq!(snapshot.jobs_claimed, 4);
        assert_eq!(snapshot.jobs_completed, 2);
        assert_eq!(snapshot.jobs_verified, 1);
        assert_eq!(snapshot.jobs_declined, 1);
        assert_eq!(snapshot.jobs_failed, 1);
        assert!(snapshot.last_job_completed_unix_ms.is_some());
        assert!(snapshot.last_registration_unix_ms.is_some());
        assert!(snapshot.prevent_sleep_during_jobs);

        let json = serde_json::to_value(&snapshot).unwrap();
        assert_eq!(json["state"], "computing");
        assert_eq!(json["jobs_verified"], 1);
    }

    #[test]
    fn a_fresh_worker_reports_no_jobs_and_no_timestamps() {
        let snapshot = CommunityWorkerStatus::new("0xabc", "node-abc", 6, false).snapshot();
        assert_eq!(snapshot.state, WorkerState::Polling);
        assert_eq!(snapshot.jobs_completed, 0);
        assert_eq!(snapshot.last_job_completed_unix_ms, None);
        assert_eq!(snapshot.last_registration_unix_ms, None);
        assert_eq!(snapshot.coordinators_registered, 0);
        assert!(!snapshot.prevent_sleep_during_jobs);
    }

    #[test]
    fn only_a_quorum_verified_submit_response_counts_as_verified() {
        assert!(submit_response_is_quorum_verified(
            r#"{"ok":true,"verification":{"quorum_verified":true}}"#
        ));
        assert!(!submit_response_is_quorum_verified(
            r#"{"ok":true,"verification":{"quorum_verified":false}}"#
        ));
        assert!(!submit_response_is_quorum_verified(
            r#"{"ok":true,"verification":null}"#
        ));
        assert!(!submit_response_is_quorum_verified("not json"));
    }

    #[test]
    fn failed_claim_rounds_back_off_exponentially_to_a_cap() {
        let delays: Vec<u64> = [0, 1, 2, 3, 5, 6, 40]
            .into_iter()
            .map(|rounds| claim_retry_delay(rounds).as_millis() as u64)
            .collect();
        assert_eq!(delays, [500, 1_000, 2_000, 4_000, 16_000, 30_000, 30_000]);
    }

    #[test]
    fn a_wall_clock_jump_marks_a_sleep_but_normal_ticks_do_not() {
        let interval = Duration::from_secs(15);
        let before = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000);
        assert!(!wall_clock_gap_suggests_sleep(
            before,
            before + Duration::from_secs(16),
            interval
        ));
        assert!(!wall_clock_gap_suggests_sleep(
            before,
            before + Duration::from_secs(45),
            interval
        ));
        assert!(wall_clock_gap_suggests_sleep(
            before,
            before + Duration::from_secs(46),
            interval
        ));
        assert!(!wall_clock_gap_suggests_sleep(
            before,
            before - Duration::from_secs(600),
            interval
        ));
    }

    #[test]
    fn a_coordinator_that_keeps_answering_404_is_re_registered_alone_once_per_cooldown() {
        let queue = ReregistrationQueue::with_cooldown(Duration::from_secs(15), Duration::ZERO);
        let start = Instant::now();
        let failing = "https://a.example";
        assert!(queue.request_at(failing, start));
        assert_eq!(queue.take_pending(), vec![failing.to_string()]);
        for round in 1..30u64 {
            assert!(!queue.request_at(failing, start + Duration::from_millis(500 * round)));
        }
        assert!(queue.take_pending().is_empty());
        assert!(queue.request_at(failing, start + Duration::from_secs(15)));
        assert_eq!(queue.take_pending(), vec![failing.to_string()]);
        assert!(!queue.request_at(failing, start + Duration::from_secs(29)));
        assert!(queue.request_at(failing, start + Duration::from_secs(30)));
    }

    #[test]
    fn coordinators_cool_down_independently_and_the_jitter_is_bounded() {
        let queue =
            ReregistrationQueue::with_cooldown(Duration::from_secs(15), Duration::from_secs(5));
        let start = Instant::now();
        assert!(queue.request_at("https://a.example", start));
        assert!(queue.request_at("https://b.example", start + Duration::from_secs(7)));
        assert_eq!(
            queue.take_pending(),
            vec![
                "https://a.example".to_string(),
                "https://b.example".to_string()
            ]
        );
        assert!(!queue.request_at("https://a.example", start + Duration::from_millis(14_999)));
        assert!(queue.request_at("https://a.example", start + Duration::from_secs(20)));
        assert!(!queue.request_at("https://b.example", start + Duration::from_millis(21_999)));
        assert!(queue.request_at("https://b.example", start + Duration::from_secs(27)));
    }

    #[tokio::test]
    async fn a_queued_coordinator_wakes_the_registration_task_once() {
        let queue = ReregistrationQueue::with_cooldown(Duration::from_secs(15), Duration::ZERO);
        assert!(queue.request("https://a.example"));
        tokio::time::timeout(Duration::from_millis(200), queue.notified())
            .await
            .expect("the first report wakes the registration task");
        assert_eq!(queue.take_pending(), vec!["https://a.example".to_string()]);
        assert!(!queue.request("https://a.example"));
        assert!(
            tokio::time::timeout(Duration::from_millis(50), queue.notified())
                .await
                .is_err(),
            "a coalesced report does not wake it again"
        );
    }

    #[test]
    fn keep_awake_helpers_exit_with_the_node_and_are_inert_when_disabled() {
        let command = keep_awake_helper_command(4242);
        #[cfg(target_os = "macos")]
        {
            let (program, args) = command.expect("macOS keeps awake with caffeinate");
            assert_eq!(program, "/usr/bin/caffeinate");
            assert_eq!(args, ["-i", "-w", "4242"]);
        }
        #[cfg(target_os = "linux")]
        {
            let (program, args) = command.expect("Linux keeps awake with systemd-inhibit");
            assert_eq!(program, "systemd-inhibit");
            assert!(args.iter().any(|arg| arg == "--what=idle:sleep"));
            assert!(args.iter().any(|arg| arg == "--pid=4242"));
        }
        #[cfg(not(any(target_os = "macos", target_os = "linux")))]
        assert!(command.is_none());

        let guard = KeepAwake::begin(false);
        assert!(!guard.is_active());
    }
}

pub fn successful_work_result(
    claim: &serde_json::Value,
    job_id: String,
    worker: arc_crypto::Hash256,
    generated: Vec<u32>,
    output: String,
    total_ms: u64,
    signed_attestation_hex: Option<String>,
) -> crate::rpc::WorkResult {
    let bytes: Vec<u8> = generated
        .iter()
        .flat_map(|token| token.to_le_bytes())
        .collect();
    let tokens_generated = generated.len() as u64;
    crate::rpc::WorkResult {
        job_id,
        worker_id: format!("0x{}", worker.to_hex()),
        success: true,
        declined: false,
        output,
        output_hash: format!("0x{}", arc_crypto::hash_bytes(&bytes).to_hex()),
        tokens_generated,
        total_ms,
        ms_per_token: total_ms.checked_div(tokens_generated).unwrap_or(0),
        engine: arc_inference::cached_integer_model::CANONICAL_REWARD_INFERENCE_PROFILE.to_string(),
        error: None,
        signed_attestation_hex,
        output_tokens: (claim
            .get("accepts_output_tokens")
            .and_then(serde_json::Value::as_bool)
            == Some(true))
        .then_some(generated),
    }
}

#[cfg(test)]
mod token_claim_tests {
    use super::*;
    #[test]
    fn success_tokens_are_negotiated_and_hash_the_exact_list() {
        for capable in [false, true] {
            let claim = if capable {
                serde_json::json!({"accepts_output_tokens": true})
            } else {
                serde_json::json!({"job_id": "old"})
            };
            let result = successful_work_result(
                &claim,
                "job".into(),
                arc_crypto::Hash256::ZERO,
                vec![1, 257, 2],
                "text".into(),
                12,
                None,
            );
            assert_eq!(result.output_tokens, capable.then_some(vec![1, 257, 2]));
            assert_eq!(
                result.output_hash,
                format!(
                    "0x{}",
                    arc_crypto::hash_bytes(&[1, 0, 0, 0, 1, 1, 0, 0, 2, 0, 0, 0]).to_hex()
                )
            );
            assert_eq!(
                serde_json::to_value(&result)
                    .unwrap()
                    .get("output_tokens")
                    .is_some(),
                capable
            );
        }
    }
}
