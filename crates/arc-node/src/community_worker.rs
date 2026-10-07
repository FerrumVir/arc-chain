//! Local status of this process's community worker, and the optional
//! keep-awake guard held while it computes a job.
//!
//! One arc-node process runs at most one community worker, so the status is
//! a process-wide value installed by the worker at startup and served by the
//! node's own `GET /community/worker/status`. The desktop app reads it from
//! 127.0.0.1 to show whether the worker is polling or computing and how many
//! jobs it completed and had verified. Counters cover this process's
//! lifetime; they are local observations, not chain or reward evidence.

use std::collections::{BTreeSet, HashMap};
use std::sync::atomic::{AtomicU8, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, PoisonError};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// Route on the node's own RPC that serves [`CommunityWorkerStatus::snapshot`].
pub const COMMUNITY_WORKER_STATUS_PATH: &str = "/community/worker/status";

/// Snapshot schema; bump it when a field changes meaning.
pub const COMMUNITY_WORKER_STATUS_SCHEMA: &str = "arc.community.worker-status.v1";

/// Pause between claim rounds while coordinators answer normally.
pub const CLAIM_IDLE_REPOLL: Duration = Duration::from_millis(500);

/// Longest pause between claim rounds while no coordinator answers.
pub const CLAIM_RETRY_CAP: Duration = Duration::from_secs(30);

/// What the worker is doing right now.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkerState {
    /// Long-polling coordinators for an assignment.
    Polling,
    /// Executing an assignment.
    Computing,
    /// No coordinator answered the last claim round; retrying with backoff.
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

/// How one claimed assignment ended, from this worker's side.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum JobOutcome {
    /// The coordinator accepted the result. `verified` is true when its
    /// response reports the output passed the authenticated replica quorum.
    Completed { verified: bool },
    /// Declined before compute: malformed, mismatched, or quiescing.
    Declined,
    /// Compute failed, or the coordinator rejected or never acknowledged
    /// the result.
    Failed,
}

/// Live counters for this process's community worker.
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
    /// Which backend computes jobs, and the dyadic GPU gate's result.
    inference_backend: Mutex<WorkerBackendStatus>,
}

/// What `GET /community/worker/status` returns.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct CommunityWorkerSnapshot {
    pub schema: &'static str,
    pub worker_id: String,
    pub public_name: String,
    pub state: WorkerState,
    pub coordinators_total: u32,
    /// Coordinators that accepted this worker's last registration or
    /// heartbeat.
    pub coordinators_registered: u32,
    /// Assignments received since this process started.
    pub jobs_claimed: u64,
    /// Results the coordinator accepted.
    pub jobs_completed: u64,
    /// Accepted results the coordinator reported as quorum-verified.
    pub jobs_verified: u64,
    pub jobs_failed: u64,
    pub jobs_declined: u64,
    pub last_job_completed_unix_ms: Option<u64>,
    pub last_registration_unix_ms: Option<u64>,
    pub started_unix_ms: u64,
    pub prevent_sleep_during_jobs: bool,
    /// The backend that computes this worker's jobs (always the CPU in this
    /// version) and, separately, the `--gpu-inference` self-test result.
    pub inference_backend: WorkerBackendStatus,
}

/// Schema of [`WorkerBackendStatus`].
pub const WORKER_BACKEND_SCHEMA: &str = "arc.community.worker-backend.v1";
/// The backend that computes community jobs.
pub const SERVING_BACKEND_CPU: &str = "cpu";
/// Why jobs run on the CPU.
pub const SERVING_REASON: &str = "community jobs use the canonical INT8 reward profile, which has no \
     GPU kernels; the GPU self-test covers the dyadic profile only";

/// What the worker computes jobs on, kept apart from what the GPU gate found.
/// Local only (`GET /community/worker/status`): registration never carries
/// GPU facts ([`registration_request`]).
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct WorkerBackendStatus {
    pub schema: &'static str,
    /// The backend that computes community jobs: `cpu`.
    pub serving_backend: &'static str,
    /// The execution profile of those jobs.
    pub serving_profile: &'static str,
    pub serving_reason: &'static str,
    /// `--gpu-inference` was given.
    pub gpu_inference_requested: bool,
    /// The dyadic-profile GPU gate (`arc.inference-backend.v1`: whether a GPU
    /// passed, the reason, the self-test digests and the adapter's vendor,
    /// device, API and driver); `null` until it ran, and when the switch is
    /// off.
    pub dyadic_gpu_self_test: Option<serde_json::Value>,
}

impl WorkerBackendStatus {
    fn new() -> Self {
        Self {
            schema: WORKER_BACKEND_SCHEMA,
            serving_backend: SERVING_BACKEND_CPU,
            serving_profile:
                arc_inference::cached_integer_model::CANONICAL_REWARD_INFERENCE_PROFILE,
            serving_reason: SERVING_REASON,
            gpu_inference_requested: false,
            dyadic_gpu_self_test: None,
        }
    }
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
            inference_backend: Mutex::new(WorkerBackendStatus::new()),
        }
    }

    /// Record that `--gpu-inference` was given.
    pub fn set_gpu_inference_requested(&self, requested: bool) {
        self.inference_backend
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .gpu_inference_requested = requested;
    }

    /// Record the dyadic GPU gate's decision. It changes nothing about how
    /// jobs are computed or what registration carries.
    pub fn record_dyadic_gpu_self_test(
        &self,
        decision: &arc_inference::modern::gpu::backend::BackendDecision,
    ) {
        self.inference_backend
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .dyadic_gpu_self_test = Some(decision.to_json());
    }

    /// The backend status as the snapshot reports it.
    pub fn inference_backend(&self) -> WorkerBackendStatus {
        self.inference_backend
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    pub fn set_state(&self, state: WorkerState) {
        self.state.store(state.encode(), Ordering::Relaxed);
    }

    /// Record how many coordinators accepted one registration/heartbeat round.
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

    pub fn snapshot(&self) -> CommunityWorkerSnapshot {
        CommunityWorkerSnapshot {
            schema: COMMUNITY_WORKER_STATUS_SCHEMA,
            worker_id: self.worker_id.clone(),
            public_name: self.public_name.clone(),
            state: WorkerState::decode(self.state.load(Ordering::Relaxed)),
            coordinators_total: self.coordinators_total,
            coordinators_registered: self.coordinators_registered.load(Ordering::Relaxed),
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
            inference_backend: self.inference_backend(),
        }
    }
}

/// The registration request: `inference` for a worker with a complete
/// canonical model, otherwise `relay`. It never carries GPU facts: jobs run
/// on the CPU, so a GPU that passed the dyadic self-test is reported only in
/// the local status ([`WorkerBackendStatus`]).
pub fn registration_request(
    worker_id: String,
    name: String,
    model: Option<(String, String)>,
    execution_profile: Option<String>,
    platform: String,
) -> crate::rpc::CommunityRegisterRequest {
    let capabilities = if model.is_some() {
        vec!["inference".to_string()]
    } else {
        vec!["relay".to_string()]
    };
    let (model, model_id) = model.unzip();
    crate::rpc::CommunityRegisterRequest {
        worker_id,
        name,
        capabilities,
        model,
        model_id,
        execution_profile,
        platform,
    }
}

/// One computed community job.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CanonicalJobOutput {
    pub tokens: Vec<u32>,
    pub output_hash: arc_crypto::Hash256,
    pub text: String,
    /// The backend that computed it ([`SERVING_BACKEND_CPU`]).
    pub backend: &'static str,
}

/// Compute one community job: the worker's whole compute step, on the
/// backend the status names for the canonical reward profile. That is the
/// CPU whatever the dyadic GPU gate found, because no GPU kernels exist for
/// this profile; a GPU path for it must keep this function's output
/// byte-identical (the offline tests compare both switch settings).
pub fn compute_canonical_job(
    model: &arc_inference::cached_integer_model::CachedIntegerModel,
    prompt: &[u32],
    max_tokens: u32,
    status: &CommunityWorkerStatus,
) -> Result<CanonicalJobOutput, arc_inference::cached_integer_model::GenerationError> {
    let backend = status.inference_backend().serving_backend;
    debug_assert_eq!(backend, SERVING_BACKEND_CPU);
    let (tokens, output_hash) = model.try_generate(prompt, max_tokens, &model.config.eos_tokens)?;
    let text = model.decode(&tokens);
    Ok(CanonicalJobOutput {
        tokens,
        output_hash,
        text,
        backend,
    })
}

static INSTALLED: OnceLock<Arc<CommunityWorkerStatus>> = OnceLock::new();

/// Publish this process's worker status. A process has one worker, so the
/// first installation wins and is returned.
pub fn install(status: Arc<CommunityWorkerStatus>) -> Arc<CommunityWorkerStatus> {
    INSTALLED.get_or_init(|| status).clone()
}

/// The installed worker status, if this process runs a community worker.
pub fn installed() -> Option<Arc<CommunityWorkerStatus>> {
    INSTALLED.get().cloned()
}

/// Whether a coordinator's `/community/submit_work` response reports that
/// the output passed its authenticated replica quorum.
pub fn submit_response_is_quorum_verified(body: &str) -> bool {
    serde_json::from_str::<serde_json::Value>(body)
        .ok()
        .and_then(|value| value.get("verification")?.get("quorum_verified")?.as_bool())
        .unwrap_or(false)
}

/// Pause before the next claim round after `failed_rounds` consecutive
/// rounds in which no coordinator answered at all.
///
/// The worker used to re-poll every 500 ms whatever happened, so a worker
/// that was offline, unregistered, or waking from sleep sent two signed
/// requests per second to every coordinator. Normal long-poll completions
/// (`no_work`) still re-poll after 500 ms; failures back off 1 s, 2 s, 4 s ...
/// up to [`CLAIM_RETRY_CAP`].
pub fn claim_retry_delay(failed_rounds: u32) -> Duration {
    if failed_rounds == 0 {
        return CLAIM_IDLE_REPOLL;
    }
    let exponent = (failed_rounds - 1).min(16);
    Duration::from_secs(1)
        .saturating_mul(1u32 << exponent)
        .min(CLAIM_RETRY_CAP)
}

/// Whether the wall clock advanced by more than three registration
/// intervals between two rounds: the machine slept or the process was
/// suspended. Coordinators prune a worker after 90 s without a heartbeat,
/// so the next round must register again instead of heartbeating.
///
/// Monotonic clocks stop during sleep on macOS and Linux, which is why the
/// wall clock is compared.
pub fn wall_clock_gap_suggests_sleep(
    previous_round: SystemTime,
    now: SystemTime,
    interval: Duration,
) -> bool {
    now.duration_since(previous_round)
        .is_ok_and(|gap| gap > interval.saturating_mul(3))
}

/// Pause before a coordinator that answered a claim with 404 is registered
/// with again at the claim loop's request, after the immediate first try.
pub const REREGISTER_COOLDOWN: Duration = Duration::from_secs(15);

/// Spread repeated re-registrations across workers by up to this much, so a
/// coordinator that restarted is not hit by every worker on the same tick.
pub const REREGISTER_JITTER_MAX: Duration = Duration::from_secs(5);

/// Coordinators whose claim long-poll answered 404 ("unknown worker"),
/// waiting for the registration task to register with them again.
///
/// The first report for a coordinator wakes the registration task at once:
/// a coordinator that restarted knows no workers, and waiting for the next
/// one-minute registration tick would leave this worker idle there. Later
/// reports for the same coordinator within [`REREGISTER_COOLDOWN`] plus a
/// jitter are coalesced, so a coordinator that keeps answering 404 costs one
/// signed registration per cooldown instead of one per 500 ms claim round,
/// and only the reported coordinators are registered with again; the others
/// keep their 15 s heartbeat cadence.
pub struct ReregistrationQueue {
    state: Mutex<ReregistrationState>,
    wakeup: tokio::sync::Notify,
    cooldown: Duration,
    jitter_max: Duration,
}

#[derive(Default)]
struct ReregistrationState {
    /// Coordinators to register with at the next wakeup, in a stable order.
    pending: BTreeSet<String>,
    /// The earliest instant each coordinator may be queued again.
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

    /// Queue `coordinator` for registration and wake the registration task.
    /// Returns `false`, waking nothing, when the coordinator was queued less
    /// than a cooldown ago: that request stands, or has just been served.
    pub fn request(&self, coordinator: &str) -> bool {
        self.request_at(coordinator, Instant::now())
    }

    /// [`Self::request`] at a given instant, for tests.
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

    /// The coordinators queued since the last call.
    pub fn take_pending(&self) -> Vec<String> {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        std::mem::take(&mut state.pending).into_iter().collect()
    }

    /// Resolves once a coordinator has been queued. A request made while
    /// nobody waits is remembered until the next call, like
    /// [`tokio::sync::Notify::notify_one`].
    pub async fn notified(&self) {
        self.wakeup.notified().await;
    }

    fn jitter(&self) -> Duration {
        use std::hash::{BuildHasher as _, Hasher as _};
        let max_ms = u64::try_from(self.jitter_max.as_millis()).unwrap_or(u64::MAX);
        if max_ms == 0 {
            return Duration::ZERO;
        }
        // Fresh random keys on every call; the node crate has no rand.
        let roll = std::collections::hash_map::RandomState::new()
            .build_hasher()
            .finish();
        Duration::from_millis(roll % max_ms.saturating_add(1))
    }
}

/// Keeps the computer from idle-sleeping while a community job computes,
/// when the operator enabled `--prevent-sleep-during-jobs`. Dropping the
/// guard releases it. The machine may still sleep when the lid closes or
/// when it is idle between jobs.
///
/// macOS holds `caffeinate -i -w <arc-node pid>` and Linux holds
/// `systemd-inhibit ... tail --pid=<arc-node pid>`, so a crashed node can
/// never leave the machine awake. Windows sets `ES_SYSTEM_REQUIRED` on the
/// computing thread and clears it on drop; create and drop the guard on the
/// same thread.
#[must_use = "the computer may sleep again as soon as the guard is dropped"]
pub struct KeepAwake {
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    helper: Option<std::process::Child>,
    #[cfg(windows)]
    thread_state_set: bool,
}

/// The helper command that keeps the machine awake while `pid` lives, on
/// platforms that use one.
pub fn keep_awake_helper_command(pid: u32) -> Option<(&'static str, Vec<String>)> {
    if cfg!(target_os = "macos") {
        Some((
            "/usr/bin/caffeinate",
            vec!["-i".to_string(), "-w".to_string(), pid.to_string()],
        ))
    } else if cfg!(target_os = "linux") {
        Some((
            "systemd-inhibit",
            vec![
                "--what=idle:sleep".to_string(),
                "--who=ARC Node".to_string(),
                "--why=Running an ARC network job".to_string(),
                "--mode=block".to_string(),
                "tail".to_string(),
                format!("--pid={pid}"),
                "-f".to_string(),
                "/dev/null".to_string(),
            ],
        ))
    } else {
        None
    }
}

#[cfg(windows)]
mod windows_power {
    pub const ES_CONTINUOUS: u32 = 0x8000_0000;
    pub const ES_SYSTEM_REQUIRED: u32 = 0x0000_0001;

    #[link(name = "kernel32")]
    unsafe extern "system" {
        pub fn SetThreadExecutionState(es_flags: u32) -> u32;
    }
}

impl KeepAwake {
    /// Start keeping the machine awake if `enabled`; otherwise inert.
    pub fn begin(enabled: bool) -> Self {
        #[cfg(any(target_os = "macos", target_os = "linux"))]
        {
            let command = if enabled {
                keep_awake_helper_command(std::process::id())
            } else {
                None
            };
            let helper = command.and_then(|(program, args)| {
                let spawned = std::process::Command::new(program)
                    .args(args)
                    .stdin(std::process::Stdio::null())
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .spawn();
                match spawned {
                    Ok(child) => Some(child),
                    Err(error) => {
                        tracing::debug!(
                            %error,
                            program,
                            "could not keep the computer awake for a community job"
                        );
                        None
                    }
                }
            });
            Self { helper }
        }
        #[cfg(windows)]
        {
            let thread_state_set = enabled && {
                // SAFETY: SetThreadExecutionState only updates the calling
                // thread's power request flags; it takes no pointers.
                let previous = unsafe {
                    windows_power::SetThreadExecutionState(
                        windows_power::ES_CONTINUOUS | windows_power::ES_SYSTEM_REQUIRED,
                    )
                };
                previous != 0
            };
            Self { thread_state_set }
        }
        #[cfg(not(any(target_os = "macos", target_os = "linux", windows)))]
        {
            let _ = enabled;
            Self {}
        }
    }

    /// Whether the guard is currently holding the machine awake.
    pub fn is_active(&self) -> bool {
        #[cfg(any(target_os = "macos", target_os = "linux"))]
        {
            self.helper.is_some()
        }
        #[cfg(windows)]
        {
            self.thread_state_set
        }
        #[cfg(not(any(target_os = "macos", target_os = "linux", windows)))]
        {
            false
        }
    }
}

impl Drop for KeepAwake {
    fn drop(&mut self) {
        #[cfg(any(target_os = "macos", target_os = "linux"))]
        if let Some(mut helper) = self.helper.take() {
            let _ = helper.kill();
            let _ = helper.wait();
        }
        #[cfg(windows)]
        if self.thread_state_set {
            // SAFETY: as in `begin`; ES_CONTINUOUS alone clears this
            // thread's system-required request.
            unsafe {
                windows_power::SetThreadExecutionState(windows_power::ES_CONTINUOUS);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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

    /// A complete canonical INT8 model (one layer), built in process: the
    /// worker's job path with no file, network or registration.
    fn canonical_model() -> arc_inference::cached_integer_model::CachedIntegerModel {
        use arc_inference::cached_integer_model::{
            ArithmeticProfile, CachedIntegerModel, CachedLayer, I8Weights, ModelConfig,
        };
        const ONE: i64 = 1 << 16;
        let (d, d_ff, vocab_size) = (8usize, 16usize, 12usize);
        let weights = |rows: usize, cols: usize, salt: usize| {
            let values: Vec<f32> = (0..rows * cols)
                .map(|i| (((i * 7 + salt) % 13) as f32 - 6.0) / 40.0)
                .collect();
            I8Weights::quantize_f32(&values, rows, cols)
        };
        let embedding: Vec<f32> = (0..vocab_size * d)
            .map(|i| (((i + 5) % 17) as f32 - 8.0) / 30.0)
            .collect();
        let model = CachedIntegerModel {
            config: ModelConfig {
                n_layers: 1,
                d_model: d,
                n_heads: 2,
                n_kv_heads: 2,
                d_ff,
                d_head: d / 2,
                d_kv: d,
                vocab_size,
                attn_scale: ONE / 2,
                rope_cos: vec![ONE; 64],
                rope_sin: vec![0; 64],
                max_seq: 48,
                eos_tokens: Vec::new(),
                bos_token: 1,
                chat_template: String::new(),
                arithmetic_profile: ArithmeticProfile::LegacySplitHalfV0,
            },
            embedding_q16: embedding
                .iter()
                .map(|v| (*v * ONE as f32).round() as i64)
                .collect(),
            embedding_i8: I8Weights::quantize_f32(&embedding, vocab_size, d),
            layers: vec![CachedLayer {
                wq: weights(d, d, 1),
                wk: weights(d, d, 2),
                wv: weights(d, d, 3),
                wo: weights(d, d, 4),
                w_gate: weights(d_ff, d, 5),
                w_up: weights(d_ff, d, 6),
                w_down: weights(d, d_ff, 7),
                attn_norm: vec![ONE; d],
                ffn_norm: vec![ONE; d],
            }],
            final_norm: vec![ONE; d],
            output_weight: weights(vocab_size, d, 8),
            vocab: [
                "<unk>",
                "<s>",
                "</s>",
                "▁ARC",
                "▁proof",
                "▁GPU",
                "▁exact",
                "▁bit",
                "▁worker",
                "▁job",
                "▁yes",
                "▁no",
            ]
            .iter()
            .map(|t| t.to_string())
            .collect(),
            q4_layers: None,
            q4_output: None,
            i16_layers: None,
            i16_output: None,
            block_i8_layers: None,
            block_i8_output: None,
            ternary_layers: None,
            ternary_output: None,
            ternary_hybrid_layers: None,
            ternary_hybrid_output: None,
        };
        assert!(model.has_canonical_i8_profile());
        model
    }

    const JOB_PROMPT: [u32; 4] = [3, 4, 5, 6];

    /// The registration request this worker sends, as `main` builds it.
    fn registration(status: &CommunityWorkerStatus) -> crate::rpc::CommunityRegisterRequest {
        // `status` is deliberately available: nothing in it may reach
        // registration.
        let _ = status.snapshot();
        registration_request(
            "0xabc".into(),
            "node-abc".into(),
            Some(("arc-1L-8d-2h-12v".into(), "0x01".into())),
            Some(arc_inference::cached_integer_model::CANONICAL_REWARD_INFERENCE_PROFILE.into()),
            "macos-aarch64".into(),
        )
    }

    /// The worker path offline, with the switch off and then on (the gate on
    /// this machine's adapter; required on the CI GPU jobs): the same job
    /// output, computed on the CPU, and the same registration request without
    /// any GPU fact.
    #[test]
    fn gpu_inference_changes_neither_job_output_nor_registration() {
        use arc_inference::modern::gpu::backend::{GpuBackendConfig, select};

        let model = canonical_model();
        let off = CommunityWorkerStatus::new("0xabc", "node-abc", 6, false);
        off.record_dyadic_gpu_self_test(&select(&GpuBackendConfig::default()));
        let disabled = compute_canonical_job(&model, &JOB_PROMPT, 6, &off).unwrap();
        assert_eq!(disabled.backend, "cpu");
        assert_eq!(disabled.tokens.len(), 6);
        // The worker computes exactly what the model's own generation does.
        let (tokens, hash) = model.generate(&JOB_PROMPT, 6, &[]);
        assert_eq!(
            (disabled.tokens.clone(), disabled.output_hash),
            (tokens, hash)
        );

        let on = CommunityWorkerStatus::new("0xabc", "node-abc", 6, false);
        on.set_gpu_inference_requested(true);
        let config = GpuBackendConfig {
            enabled: true,
            adapter: std::env::var("ARC_GPU_ADAPTER").ok(),
            self_test_rounds: 2,
            ..GpuBackendConfig::default()
        };
        let decision = select(&config);
        eprintln!("dyadic GPU gate: {}", decision.reason);
        if std::env::var("ARC_GPU_REQUIRE").as_deref() == Ok("1") {
            assert!(decision.gpu_eligible(), "{}", decision.reason);
        }
        on.record_dyadic_gpu_self_test(&decision);
        let enabled = compute_canonical_job(&model, &JOB_PROMPT, 6, &on).unwrap();
        assert_eq!(enabled, disabled, "the switch changed a job's output");
        assert_eq!(enabled.backend, SERVING_BACKEND_CPU);

        // Registration: identical with the switch off and on, `inference`
        // only, and no GPU fact anywhere in the signed payload.
        let (reg_off, reg_on) = (registration(&off), registration(&on));
        assert_eq!(reg_on.capabilities, ["inference"]);
        let (json_off, json_on) = (
            serde_json::to_string(&reg_off).unwrap(),
            serde_json::to_string(&reg_on).unwrap(),
        );
        assert_eq!(json_on, json_off);
        assert!(!json_on.to_lowercase().contains("gpu"), "{json_on}");
        if let Some(adapter) = decision.adapter() {
            assert!(!json_on.contains(&adapter.name), "{json_on}");
        }

        // The status keeps the two apart: jobs on the CPU, the gate's result
        // (and adapter) beside it.
        let status = serde_json::to_value(on.snapshot()).unwrap()["inference_backend"].clone();
        assert_eq!(status["schema"], WORKER_BACKEND_SCHEMA);
        assert_eq!(status["serving_backend"], "cpu");
        assert_eq!(
            status["serving_profile"],
            arc_inference::cached_integer_model::CANONICAL_REWARD_INFERENCE_PROFILE
        );
        assert_eq!(status["gpu_inference_requested"], true);
        let gate = &status["dyadic_gpu_self_test"];
        assert_eq!(gate["profile"], arc_inference::modern::PROFILE);
        assert_eq!(gate["gpu_self_test_passed"], decision.gpu_eligible());
        assert!(
            !gate["reason"]
                .as_str()
                .unwrap()
                .contains("serving on the GPU")
        );
        if decision.gpu_eligible() {
            assert_eq!(gate["dyadic_backend"], "gpu-wgpu");
            assert!(gate["adapter"]["name"].is_string());
        }
        let off_status = serde_json::to_value(off.snapshot()).unwrap()["inference_backend"].clone();
        assert_eq!(off_status["serving_backend"], "cpu");
        assert_eq!(off_status["gpu_inference_requested"], false);
        assert_eq!(
            off_status["dyadic_gpu_self_test"]["gpu_self_test_passed"],
            false
        );
    }

    #[test]
    fn a_relay_registers_as_relay_and_a_fresh_status_has_no_gate_result() {
        let relay = registration_request(
            "0xabc".into(),
            "node-abc".into(),
            None,
            None,
            "linux-x86_64".into(),
        );
        assert_eq!(relay.capabilities, ["relay"]);
        assert_eq!((relay.model, relay.model_id), (None, None));
        let status = CommunityWorkerStatus::new("0xabc", "node-abc", 6, false).inference_backend();
        assert_eq!(status.serving_backend, "cpu");
        assert!(!status.gpu_inference_requested);
        assert_eq!(status.dyadic_gpu_self_test, None);
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
        // A clock set backwards is not a sleep.
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
        // The first 404 is recovered at once, with that coordinator alone.
        assert!(queue.request_at(failing, start));
        assert_eq!(queue.take_pending(), vec![failing.to_string()]);
        // Another coordinator answers idle, so the claim loop re-polls every
        // 500 ms and the failing one keeps answering 404: every report
        // within the cooldown is coalesced, and the idle coordinator is
        // never queued because it never asked.
        for round in 1..30u64 {
            assert!(!queue.request_at(failing, start + Duration::from_millis(500 * round)));
        }
        assert!(queue.take_pending().is_empty());
        // After the cooldown, one more registration, and the cycle repeats.
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
        // A second coordinator's first 404 is not held back by the first's
        // cooldown.
        assert!(queue.request_at("https://b.example", start + Duration::from_secs(7)));
        assert_eq!(
            queue.take_pending(),
            vec![
                "https://a.example".to_string(),
                "https://b.example".to_string()
            ]
        );
        // Within the cooldown a report is coalesced whatever the jitter; a
        // cooldown plus the whole jitter later it is accepted.
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
