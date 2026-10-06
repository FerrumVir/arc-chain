//! Local status of this process's community worker, and the optional
//! keep-awake guard held while it computes a job.
//!
//! One arc-node process runs at most one community worker, so the status is
//! a process-wide value installed by the worker at startup and served by the
//! node's own `GET /community/worker/status`. The desktop app reads it from
//! 127.0.0.1 to show whether the worker is polling or computing and how many
//! jobs it completed and had verified. Counters cover this process's
//! lifetime; they are local observations, not chain or reward evidence.

use std::sync::atomic::{AtomicU8, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

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
        }
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
        }
    }
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
