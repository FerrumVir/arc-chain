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
    /// The inference backend decision (`arc.inference-backend.v1`) and the
    /// capabilities it adds to registration; `None` until a GPU gate ran.
    inference_backend: Mutex<Option<(serde_json::Value, Vec<String>)>>,
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
    /// `--gpu-inference`: which backend serves the dyadic profile and why
    /// (`arc.inference-backend.v1`: backend, reason, self-test digests, the
    /// adapter's vendor, device, backend and driver). `null` when the switch
    /// is off.
    pub inference_backend: Option<serde_json::Value>,
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
            inference_backend: Mutex::new(None),
        }
    }

    /// Record the GPU gate's decision and the capabilities it adds.
    pub fn set_inference_backend(&self, decision: serde_json::Value, capabilities: Vec<String>) {
        *self
            .inference_backend
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some((decision, capabilities));
    }

    /// Capabilities the inference backend adds to registration (empty unless
    /// a GPU passed the gate).
    pub fn inference_backend_capabilities(&self) -> Vec<String> {
        self.inference_backend
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .as_ref()
            .map(|(_, capabilities)| capabilities.clone())
            .unwrap_or_default()
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
            inference_backend: self
                .inference_backend
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .as_ref()
                .map(|(decision, _)| decision.clone()),
        }
    }
}

/// Longest capability a coordinator accepts, and how many it accepts
/// (`validate_community_registration_shape` in `rpc.rs`).
const CAPABILITY_MAX_BYTES: usize = 32;
const CAPABILITIES_MAX: usize = 16;

/// `prefix` and `raw` as one capability: lower-case ASCII letters, digits
/// and single hyphens, at most 32 bytes; `None` when `raw` has no letter or
/// digit.
fn capability(prefix: &str, raw: &str) -> Option<String> {
    let mut token = String::new();
    for c in raw.chars() {
        if c.is_ascii_alphanumeric() {
            token.push(c.to_ascii_lowercase());
        } else if !token.is_empty() && !token.ends_with('-') {
            token.push('-');
        }
    }
    let token = token.trim_end_matches('-');
    if token.is_empty() {
        return None;
    }
    let mut out = format!("{prefix}{token}");
    out.truncate(CAPABILITY_MAX_BYTES);
    Some(out.trim_end_matches('-').to_string())
}

/// What a worker whose GPU passed the gate adds to its registered
/// capabilities: `gpu-wgpu` (the Proof Kit's backend name) and the adapter's
/// API, vendor, device and driver as capability tokens. Registration has no
/// other field for them, and a coordinator rejects unknown fields, so the
/// exact strings stay in the local status
/// ([`CommunityWorkerSnapshot::inference_backend`]).
pub fn gpu_capabilities(adapter: &arc_gpu::modern::AdapterReport) -> Vec<String> {
    let driver = format!("{} {}", adapter.driver, adapter.driver_info);
    let mut out = vec![arc_inference::modern::gpu::backend::PROOF_BACKEND.to_string()];
    out.extend(
        [
            capability("gpu-api-", &adapter.backend),
            capability("gpu-vendor-", &adapter.vendor),
            capability("gpu-device-", &adapter.name),
            capability("gpu-driver-", &driver),
        ]
        .into_iter()
        .flatten(),
    );
    out.dedup();
    out
}

/// `base` capabilities plus the backend's, within the coordinator's limits
/// (unique entries, at most 16). The backend's are added only to a worker
/// that advertises `inference`.
pub fn registration_capabilities(base: &[String], backend: Vec<String>) -> Vec<String> {
    let mut out = base.to_vec();
    if !base.iter().any(|c| c == "inference") {
        return out;
    }
    for capability in backend {
        if out.len() >= CAPABILITIES_MAX {
            break;
        }
        if !out.contains(&capability) {
            out.push(capability);
        }
    }
    out
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

    fn adapter(
        name: &str,
        vendor: &str,
        backend: &str,
        driver: &str,
    ) -> arc_gpu::modern::AdapterReport {
        arc_gpu::modern::AdapterReport {
            index: 0,
            name: name.into(),
            vendor_id: 0,
            vendor: vendor.into(),
            device_id: 0,
            device_type: "DiscreteGpu".into(),
            backend: backend.into(),
            driver: driver.into(),
            driver_info: String::new(),
            software: false,
        }
    }

    /// The coordinator's rule for one capability (`rpc.rs`).
    fn coordinator_accepts(capability: &str) -> bool {
        !capability.is_empty()
            && capability.len() <= CAPABILITY_MAX_BYTES
            && capability
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
    }

    #[test]
    fn gpu_capabilities_name_the_adapter_within_the_coordinator_rules() {
        let apple = gpu_capabilities(&adapter("Apple M2 Ultra", "Apple", "Metal", ""));
        assert_eq!(
            apple,
            [
                "gpu-wgpu",
                "gpu-api-metal",
                "gpu-vendor-apple",
                "gpu-device-apple-m2-ultra"
            ]
        );
        let long = gpu_capabilities(&adapter(
            "NVIDIA GeForce RTX 4090 Laptop GPU (Engineering Sample)",
            "NVIDIA",
            "Vulkan",
            "NVIDIA 560.94",
        ));
        assert_eq!(long[4], "gpu-driver-nvidia-560-94");
        assert_eq!(long[3], "gpu-device-nvidia-geforce-rtx-40");
        for capability in apple.iter().chain(&long) {
            assert!(coordinator_accepts(capability), "{capability}");
        }
        assert!(capability("gpu-x-", "--").is_none());
        assert_eq!(capability("gpu-x-", " -A__b- ").unwrap(), "gpu-x-a-b");
    }

    #[test]
    fn registration_adds_backend_capabilities_only_to_inference_workers() {
        let gpu = vec!["gpu-wgpu".to_string(), "inference".to_string()];
        let inference = vec!["inference".to_string()];
        assert_eq!(
            registration_capabilities(&inference, gpu.clone()),
            ["inference", "gpu-wgpu"]
        );
        let relay = vec!["relay".to_string()];
        assert_eq!(registration_capabilities(&relay, gpu), ["relay"]);
        let many: Vec<String> = (0..20).map(|i| format!("gpu-x-{i}")).collect();
        assert_eq!(
            registration_capabilities(&inference, many).len(),
            CAPABILITIES_MAX
        );
    }

    #[test]
    fn the_status_reports_the_backend_decision() {
        let status = CommunityWorkerStatus::new("0xabc", "node-abc", 6, false);
        assert!(status.inference_backend_capabilities().is_empty());
        assert_eq!(
            serde_json::to_value(status.snapshot()).unwrap()["inference_backend"],
            serde_json::Value::Null
        );
        status.set_inference_backend(
            serde_json::json!({"backend": "gpu-wgpu"}),
            vec!["gpu-wgpu".into()],
        );
        assert_eq!(status.inference_backend_capabilities(), ["gpu-wgpu"]);
        assert_eq!(
            serde_json::to_value(status.snapshot()).unwrap()["inference_backend"]["backend"],
            "gpu-wgpu"
        );
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
