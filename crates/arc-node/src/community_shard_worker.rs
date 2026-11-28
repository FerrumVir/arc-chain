use crate::community_shard_executor::{
    CommunityShardExecutor, MAX_FENCED_BOOT_NONCES_HARD_CAP, MAX_KV_CACHE_BYTES_HARD_CAP,
    MAX_SESSION_TTL_MS, MAX_STEP_DEADLINE_WINDOW_MS, MAX_TOMBSTONES_HARD_CAP, ShardExecutorConfig,
    ShardExecutorError,
};
use crate::community_sharding::{
    COMMUNITY_SHARD_PROTOCOL_VERSION, HeartbeatDirective, MAX_ACTIVE_SESSIONS_HARD_CAP,
    MAX_CANCELLATIONS_PER_WORKER_HARD_CAP, MAX_EOS_TOKENS, MAX_HIDDEN_LEN_HARD_CAP,
    MAX_MODEL_LAYERS_HARD_CAP, MAX_REASON_BYTES, MAX_SEQUENCE_POSITIONS_HARD_CAP,
    SessionCancellationNotice, SessionId, ShardClaimStepRequest, ShardClaimStepResponse,
    ShardHeartbeatRequest, ShardHeartbeatResponse, ShardLeaseAssignment, ShardLeaseRequest,
    ShardLeaseResponse, ShardReleaseRequest, ShardReleaseResponse, ShardStepOutcome,
    ShardStepSubmitRequest, ShardStepSubmitResponse, ShardStepWork, TrustedModelSpec,
};
use crate::community_worker::CommunityWorkerStatus;
use crate::native_inference::{WorkerExecutionGate, WorkerExecutionPermit};
use crate::rpc::community_shard_http::{
    COMMUNITY_SHARD_CAPABILITY, COMMUNITY_SHARD_CLAIM_STEP_PATH, COMMUNITY_SHARD_HEARTBEAT_PATH,
    COMMUNITY_SHARD_LEASE_PATH, COMMUNITY_SHARD_RELEASE_PATH,
    COMMUNITY_SHARD_SUBMIT_BODY_LIMIT_BYTES, COMMUNITY_SHARD_SUBMIT_STEP_PATH,
    ShardWorkerRequestIdentity, sign_shard_worker_request,
};
use crate::rpc::{
    COMMUNITY_REGISTER_PATH, CommunityRegisterRequest, sign_community_request,
    validate_community_rpc_bases,
};
use arc_crypto::{Hash256, KeyPair};
use arc_inference::cached_integer_model::{CANONICAL_REWARD_INFERENCE_PROFILE, CachedIntegerModel};
use reqwest::{Client, StatusCode, Url, header, redirect};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeSet, VecDeque};
use std::net::IpAddr;
use std::sync::Arc;
use std::time::Duration;
use thiserror::Error;
use tokio::sync::watch;
use tokio::time::Instant;

pub const DEFAULT_HEARTBEAT_INTERVAL_MS: u64 = 10_000;
pub const MIN_HEARTBEAT_INTERVAL_MS: u64 = 20;
pub const MAX_IDLE_POLL_INTERVAL_MS: u64 = 10_000;
pub const DEFAULT_IDLE_POLL_INTERVAL_MS: u64 = 250;
pub const MAX_CONNECT_TIMEOUT_MS: u64 = 10_000;
pub const DEFAULT_CONNECT_TIMEOUT_MS: u64 = 5_000;
pub const MAX_REQUEST_TIMEOUT_MS: u64 = 30_000;
pub const DEFAULT_REQUEST_TIMEOUT_MS: u64 = 15_000;
pub const MAX_RELEASE_TIMEOUT_MS: u64 = 10_000;
pub const DEFAULT_RELEASE_TIMEOUT_MS: u64 = 5_000;
pub const MAX_RETRY_BACKOFF_MS: u64 = 10_000;
pub const DEFAULT_RETRY_BASE_BACKOFF_MS: u64 = 100;
pub const DEFAULT_RETRY_MAX_BACKOFF_MS: u64 = 2_000;
pub const MAX_REACQUIRE_BACKOFF_MS: u64 = 30_000;
pub const DEFAULT_REACQUIRE_BASE_BACKOFF_MS: u64 = 250;
pub const DEFAULT_REACQUIRE_MAX_BACKOFF_MS: u64 = 5_000;
pub const DEFAULT_MAX_CONSECUTIVE_LEASE_REACQUIRES: usize = 8;
pub const MAX_CONSECUTIVE_LEASE_REACQUIRES_HARD_CAP: usize = 64;
pub const DEFAULT_RECONNECT_BASE_BACKOFF_MS: u64 = 5_000;
pub const DEFAULT_RECONNECT_MAX_BACKOFF_MS: u64 = 60_000;
pub const MAX_RECONNECT_BACKOFF_MS: u64 = 60_000;

const MAX_RESPONSE_BODY_BYTES: usize = COMMUNITY_SHARD_SUBMIT_BODY_LIMIT_BYTES;
const MAX_ERROR_BODY_BYTES: usize = 4 * 1024;
const WORKER_NAME_MAX_BYTES: usize = 96;
const WORKER_PLATFORM_MAX_BYTES: usize = 64;
const WORKER_MODEL_MAX_BYTES: usize = 128;
const MAX_CONSECUTIVE_REREGISTERS: usize = 4;
const MAX_REGISTER_TRANSIENT_ATTEMPTS: usize = 6;
const MAX_LEASE_TRANSIENT_ATTEMPTS: usize = 6;
const MAX_RELEASE_TRANSIENT_ATTEMPTS: usize = 2;

pub type PartialModelLoader =
    Arc<dyn Fn(&ShardLeaseAssignment) -> anyhow::Result<Arc<CachedIntegerModel>> + Send + Sync>;

#[derive(Clone, Debug)]
pub struct ShardWorkerTelemetrySink(Arc<CommunityWorkerStatus>);

impl ShardWorkerTelemetrySink {
    pub fn new(status: Arc<CommunityWorkerStatus>) -> Self {
        Self(status)
    }

    pub fn status(&self) -> &Arc<CommunityWorkerStatus> {
        &self.0
    }
}

impl From<Arc<CommunityWorkerStatus>> for ShardWorkerTelemetrySink {
    fn from(status: Arc<CommunityWorkerStatus>) -> Self {
        Self(status)
    }
}

impl PartialEq for ShardWorkerTelemetrySink {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

impl Eq for ShardWorkerTelemetrySink {}

#[derive(Clone)]
pub struct ShardWorkerExecutionGate(Arc<WorkerExecutionGate>);

impl ShardWorkerExecutionGate {
    pub fn new(gate: Arc<WorkerExecutionGate>) -> Self {
        Self(gate)
    }

    pub fn gate(&self) -> &Arc<WorkerExecutionGate> {
        &self.0
    }
}

impl From<Arc<WorkerExecutionGate>> for ShardWorkerExecutionGate {
    fn from(gate: Arc<WorkerExecutionGate>) -> Self {
        Self(gate)
    }
}

impl std::fmt::Debug for ShardWorkerExecutionGate {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ShardWorkerExecutionGate")
            .field("closed", &self.0.is_closed())
            .field("active_jobs", &self.0.active_jobs())
            .finish()
    }
}

impl PartialEq for ShardWorkerExecutionGate {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

impl Eq for ShardWorkerExecutionGate {}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommunityShardWorkerConfig {
    pub coordinator_base_url: String,
    pub coordinator_id: Hash256,
    pub transaction_domain: Option<Hash256>,
    pub max_resident_layers: usize,
    pub executor_config: ShardExecutorConfig,
    pub worker_name: String,
    pub model_name: String,
    pub platform: String,
    pub allow_insecure_for_loopback: bool,
    pub prevent_sleep_during_jobs: bool,
    pub auto_reconnect: bool,
    pub heartbeat_interval_ms: u64,
    pub idle_poll_interval_ms: u64,
    pub connect_timeout_ms: u64,
    pub request_timeout_ms: u64,
    pub release_timeout_ms: u64,
    pub retry_base_backoff_ms: u64,
    pub retry_max_backoff_ms: u64,
    pub reacquire_base_backoff_ms: u64,
    pub reacquire_max_backoff_ms: u64,
    pub max_consecutive_lease_reacquires: usize,
    pub reconnect_base_backoff_ms: u64,
    pub reconnect_max_backoff_ms: u64,
    pub telemetry: Option<ShardWorkerTelemetrySink>,
    pub worker_execution_gate: Option<ShardWorkerExecutionGate>,
}

impl CommunityShardWorkerConfig {
    pub fn new(
        coordinator_base_url: impl Into<String>,
        coordinator_id: Hash256,
        transaction_domain: Option<Hash256>,
        max_resident_layers: usize,
        worker_name: impl Into<String>,
        model_name: impl Into<String>,
        platform: impl Into<String>,
    ) -> Self {
        Self {
            coordinator_base_url: coordinator_base_url.into(),
            coordinator_id,
            transaction_domain,
            max_resident_layers,
            executor_config: ShardExecutorConfig::default(),
            worker_name: worker_name.into(),
            model_name: model_name.into(),
            platform: platform.into(),
            allow_insecure_for_loopback: false,
            prevent_sleep_during_jobs: false,
            auto_reconnect: false,
            heartbeat_interval_ms: DEFAULT_HEARTBEAT_INTERVAL_MS,
            idle_poll_interval_ms: DEFAULT_IDLE_POLL_INTERVAL_MS,
            connect_timeout_ms: DEFAULT_CONNECT_TIMEOUT_MS,
            request_timeout_ms: DEFAULT_REQUEST_TIMEOUT_MS,
            release_timeout_ms: DEFAULT_RELEASE_TIMEOUT_MS,
            retry_base_backoff_ms: DEFAULT_RETRY_BASE_BACKOFF_MS,
            retry_max_backoff_ms: DEFAULT_RETRY_MAX_BACKOFF_MS,
            reacquire_base_backoff_ms: DEFAULT_REACQUIRE_BASE_BACKOFF_MS,
            reacquire_max_backoff_ms: DEFAULT_REACQUIRE_MAX_BACKOFF_MS,
            max_consecutive_lease_reacquires: DEFAULT_MAX_CONSECUTIVE_LEASE_REACQUIRES,
            reconnect_base_backoff_ms: DEFAULT_RECONNECT_BASE_BACKOFF_MS,
            reconnect_max_backoff_ms: DEFAULT_RECONNECT_MAX_BACKOFF_MS,
            telemetry: None,
            worker_execution_gate: None,
        }
    }

    pub fn validate(
        &self,
        model_spec: &TrustedModelSpec,
    ) -> Result<String, CommunityShardWorkerError> {
        validate_model_spec(model_spec)?;
        validate_executor_config(&self.executor_config)?;
        if self.coordinator_id == Hash256::ZERO {
            return Err(CommunityShardWorkerError::InvalidConfig(
                "coordinator_id must be nonzero".to_string(),
            ));
        }
        let max_allowed_layers = model_spec.n_layers.div_ceil(2);
        if self.max_resident_layers == 0
            || self.max_resident_layers >= model_spec.n_layers
            || self.max_resident_layers > max_allowed_layers
        {
            return Err(CommunityShardWorkerError::InvalidConfig(format!(
                "max_resident_layers {} must be in 1..={} and strictly less than n_layers {}",
                self.max_resident_layers, max_allowed_layers, model_spec.n_layers
            )));
        }
        let worker_name = self.worker_name.trim();
        if worker_name.is_empty() || worker_name.len() > WORKER_NAME_MAX_BYTES {
            return Err(CommunityShardWorkerError::InvalidConfig(format!(
                "worker_name must be 1..={WORKER_NAME_MAX_BYTES} bytes"
            )));
        }
        let model_name = self.model_name.trim();
        if model_name.is_empty() || model_name.len() > WORKER_MODEL_MAX_BYTES {
            return Err(CommunityShardWorkerError::InvalidConfig(format!(
                "model_name must be 1..={WORKER_MODEL_MAX_BYTES} bytes"
            )));
        }
        let platform = self.platform.trim();
        if platform.is_empty() || platform.len() > WORKER_PLATFORM_MAX_BYTES {
            return Err(CommunityShardWorkerError::InvalidConfig(format!(
                "platform must be 1..={WORKER_PLATFORM_MAX_BYTES} bytes"
            )));
        }
        if self.heartbeat_interval_ms < MIN_HEARTBEAT_INTERVAL_MS
            || self.heartbeat_interval_ms > DEFAULT_HEARTBEAT_INTERVAL_MS
        {
            return Err(CommunityShardWorkerError::InvalidConfig(format!(
                "heartbeat_interval_ms must be in {MIN_HEARTBEAT_INTERVAL_MS}..={DEFAULT_HEARTBEAT_INTERVAL_MS}"
            )));
        }
        if self.idle_poll_interval_ms == 0 || self.idle_poll_interval_ms > MAX_IDLE_POLL_INTERVAL_MS
        {
            return Err(CommunityShardWorkerError::InvalidConfig(format!(
                "idle_poll_interval_ms must be in 1..={MAX_IDLE_POLL_INTERVAL_MS}"
            )));
        }
        if self.connect_timeout_ms == 0 || self.connect_timeout_ms > MAX_CONNECT_TIMEOUT_MS {
            return Err(CommunityShardWorkerError::InvalidConfig(format!(
                "connect_timeout_ms must be in 1..={MAX_CONNECT_TIMEOUT_MS}"
            )));
        }
        if self.request_timeout_ms == 0 || self.request_timeout_ms > MAX_REQUEST_TIMEOUT_MS {
            return Err(CommunityShardWorkerError::InvalidConfig(format!(
                "request_timeout_ms must be in 1..={MAX_REQUEST_TIMEOUT_MS}"
            )));
        }
        if self.connect_timeout_ms > self.request_timeout_ms {
            return Err(CommunityShardWorkerError::InvalidConfig(
                "connect_timeout_ms cannot exceed request_timeout_ms".to_string(),
            ));
        }
        if self.release_timeout_ms == 0 || self.release_timeout_ms > MAX_RELEASE_TIMEOUT_MS {
            return Err(CommunityShardWorkerError::InvalidConfig(format!(
                "release_timeout_ms must be in 1..={MAX_RELEASE_TIMEOUT_MS}"
            )));
        }
        if self.retry_base_backoff_ms == 0
            || self.retry_base_backoff_ms > self.retry_max_backoff_ms
            || self.retry_max_backoff_ms > MAX_RETRY_BACKOFF_MS
        {
            return Err(CommunityShardWorkerError::InvalidConfig(format!(
                "retry backoff bounds must be in 1..={MAX_RETRY_BACKOFF_MS} with base <= max"
            )));
        }
        if self.reacquire_base_backoff_ms == 0
            || self.reacquire_base_backoff_ms > self.reacquire_max_backoff_ms
            || self.reacquire_max_backoff_ms > MAX_REACQUIRE_BACKOFF_MS
        {
            return Err(CommunityShardWorkerError::InvalidConfig(format!(
                "reacquire backoff bounds must be in 1..={MAX_REACQUIRE_BACKOFF_MS} with base <= max"
            )));
        }
        if self.max_consecutive_lease_reacquires == 0
            || self.max_consecutive_lease_reacquires > MAX_CONSECUTIVE_LEASE_REACQUIRES_HARD_CAP
        {
            return Err(CommunityShardWorkerError::InvalidConfig(format!(
                "max_consecutive_lease_reacquires must be in 1..={MAX_CONSECUTIVE_LEASE_REACQUIRES_HARD_CAP}"
            )));
        }
        if self.reconnect_base_backoff_ms == 0
            || self.reconnect_base_backoff_ms > self.reconnect_max_backoff_ms
            || self.reconnect_max_backoff_ms > MAX_RECONNECT_BACKOFF_MS
        {
            return Err(CommunityShardWorkerError::InvalidConfig(format!(
                "reconnect backoff bounds must be in 1..={MAX_RECONNECT_BACKOFF_MS} with base <= max"
            )));
        }
        normalize_and_validate_coordinator_url(
            &self.coordinator_base_url,
            self.allow_insecure_for_loopback,
        )
    }
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct WorkerRunSummary {
    pub registrations: u64,
    pub leases_acquired: u64,
    pub models_loaded: u64,
    pub heartbeats_sent: u64,
    pub steps_executed: u64,
    pub steps_submitted: u64,
    pub submit_retries: u64,
    pub submit_stale_conflicts: u64,
    pub cancellations_applied: u64,
    pub rebalances_completed: u64,
    pub clean_release_sent: bool,
    pub reconnect_attempts: u64,
}

#[derive(Debug, Error)]
pub enum CommunityShardWorkerError {
    #[error("invalid worker configuration: {0}")]
    InvalidConfig(String),
    #[error("invalid worker keypair: {0}")]
    InvalidKeyPair(String),
    #[error("invalid trusted model spec: {0}")]
    InvalidModelSpec(String),
    #[error("system clock cannot be represented as Unix milliseconds")]
    InvalidSystemClock,
    #[error("request signing failed: {0}")]
    Signing(String),
    #[error("request serialization failed: {0}")]
    Serialization(String),
    #[error("HTTP client construction failed: {0}")]
    HttpClientBuild(String),
    #[error("HTTP redirect rejected on {path} with status {status}")]
    RedirectRejected { path: &'static str, status: u16 },
    #[error(
        "coordinator authentication or authorization rejected {path} with HTTP {status}: {detail}"
    )]
    AuthenticationOrAuthorization {
        path: &'static str,
        status: u16,
        detail: String,
    },
    #[error("response body on {path} exceeded {max_bytes} byte limit")]
    ResponseTooLarge {
        path: &'static str,
        max_bytes: usize,
    },
    #[error("malformed response body on {path}: {detail}")]
    MalformedResponse { path: &'static str, detail: String },
    #[error("coordinator rejected {path} with HTTP {status}: {detail}")]
    ProtocolRejected {
        path: &'static str,
        status: u16,
        detail: String,
    },
    #[error("invalid lease assignment from coordinator: {0}")]
    InvalidLease(String),
    #[error("partial model loader failed: {0}")]
    ModelLoadFailed(String),
    #[error("blocking worker task failed: {0}")]
    BlockingTaskFailed(String),
    #[error("shard executor error: {0}")]
    Executor(#[from] ShardExecutorError),
    #[error("transient coordinator failure exhausted retry budget on {path}: {detail}")]
    TransientBudgetExhausted { path: &'static str, detail: String },
    #[error("exceeded maximum consecutive lease reacquire attempts ({limit})")]
    LeaseReacquireLimitExceeded { limit: usize },
    #[error("exceeded maximum consecutive worker re-registration attempts ({limit})")]
    ReregisterLimitExceeded { limit: usize },
}

#[derive(Debug, Deserialize)]
struct CommunityRegisterResponse {
    ok: bool,
    worker_id: String,
}

#[derive(Debug, Default)]
struct LeaseFenceTracker {
    high_water: Option<(Hash256, u64, u64)>,
    fenced_boot_nonces: BTreeSet<[u8; 32]>,
}

impl LeaseFenceTracker {
    fn is_current_high_water(&self, assignment: &ShardLeaseAssignment) -> bool {
        let boot = assignment.lease_id.boot_nonce;
        !self.fenced_boot_nonces.contains(&boot.0)
            && self.high_water == Some((boot, assignment.lease_id.seq, assignment.lease_epoch))
    }

    fn validate_next(
        &self,
        assignment: &ShardLeaseAssignment,
    ) -> Result<(), CommunityShardWorkerError> {
        let boot = assignment.lease_id.boot_nonce;
        if self.fenced_boot_nonces.contains(&boot.0) {
            return Err(CommunityShardWorkerError::InvalidLease(
                "coordinator boot_nonce is permanently fenced".to_string(),
            ));
        }
        if let Some((hw_boot, hw_seq, hw_epoch)) = self.high_water {
            if boot == hw_boot {
                let advances = assignment.lease_id.seq > hw_seq
                    || (assignment.lease_id.seq == hw_seq && assignment.lease_epoch > hw_epoch);
                if !advances {
                    return Err(CommunityShardWorkerError::InvalidLease(
                        "lease progression must strictly advance sequence or epoch".to_string(),
                    ));
                }
            } else if self.fenced_boot_nonces.len() >= MAX_FENCED_BOOT_NONCES_HARD_CAP {
                return Err(CommunityShardWorkerError::InvalidLease(
                    "fenced coordinator boot_nonce capacity reached".to_string(),
                ));
            }
        }
        Ok(())
    }

    fn record(&mut self, assignment: &ShardLeaseAssignment) {
        let boot = assignment.lease_id.boot_nonce;
        if let Some((hw_boot, _, _)) = self.high_water
            && boot != hw_boot
        {
            self.fenced_boot_nonces.insert(hw_boot.0);
        }
        self.high_water = Some((boot, assignment.lease_id.seq, assignment.lease_epoch));
    }
}

enum HttpCallResult<T> {
    Ok(T),
    HttpError { status: StatusCode, body: String },
    Transient(String),
}

enum CancellableCall<T> {
    Completed(HttpCallResult<T>),
    Shutdown,
}

enum HeartbeatOutcome {
    Continue {
        cancellations: Vec<SessionCancellationNotice>,
        expires_at_unix_ms: Option<u64>,
    },
    Transient,
    Rebalance(ShardLeaseAssignment),
    Reacquire {
        needs_reregister: bool,
    },
    Shutdown,
    Fatal(CommunityShardWorkerError),
}

enum LoadAbandonment {
    Rebalance(ShardLeaseAssignment),
    Reacquire { needs_reregister: bool },
    Shutdown,
    Fatal(CommunityShardWorkerError),
}

enum LoadOutcome {
    Bound,
    Rebalance(ShardLeaseAssignment),
    Reacquire { needs_reregister: bool },
    Shutdown,
}

enum ComputeStop {
    Rebalance(ShardLeaseAssignment),
    Reacquire { needs_reregister: bool },
    Shutdown,
    Fatal(CommunityShardWorkerError),
}

enum StepCycleOutcome {
    Continue,
    Rebalance(ShardLeaseAssignment),
    Reacquire { needs_reregister: bool },
    Shutdown,
}

enum WaitOutcome {
    Elapsed,
    WatchedSessionCancelled,
    Rebalance(ShardLeaseAssignment),
    Reacquire { needs_reregister: bool },
    Shutdown,
}

enum LeaseAcquireOutcome {
    Acquired(ShardLeaseResponse),
    PriorLeaseReleaseRetry { needs_reregister: bool },
    ReregisterRequired,
    Shutdown,
}

enum GateAdmission {
    Admitted(Option<WorkerExecutionPermit>),
    Paused,
}

enum RegisterOutcome {
    Registered,
    Shutdown,
}

enum SubmitOutcome {
    Accepted,
    StaleOrExpired,
    Rebalance(ShardLeaseAssignment),
    Reacquire { needs_reregister: bool },
    Shutdown,
}

struct WorkerRuntime {
    config: CommunityShardWorkerConfig,
    base_url: String,
    http: Client,
    keypair: Arc<KeyPair>,
    worker_id: Hash256,
    worker_id_hex: String,
    model_spec: TrustedModelSpec,
    loader: PartialModelLoader,
    shutdown: watch::Receiver<bool>,
    executor: Option<CommunityShardExecutor>,
    active_lease: Option<ShardLeaseAssignment>,
    unreleased_prior_lease: Option<ShardLeaseAssignment>,
    ready_ttl_ms: Option<u64>,
    fence: LeaseFenceTracker,
    consecutive_reacquires: usize,
    consecutive_reregisters: usize,
    consecutive_heartbeat_transients: usize,
    consecutive_reconnects: usize,
    summary: WorkerRunSummary,
}

pub async fn run_community_shard_worker(
    config: CommunityShardWorkerConfig,
    keypair: Arc<KeyPair>,
    model_spec: TrustedModelSpec,
    loader: PartialModelLoader,
    shutdown: watch::Receiver<bool>,
) -> Result<WorkerRunSummary, CommunityShardWorkerError> {
    let telemetry = config.telemetry.clone();
    let mut runtime = match WorkerRuntime::new(config, keypair, model_spec, loader, shutdown) {
        Ok(runtime) => runtime,
        Err(err) => {
            if let Some(sink) = telemetry.as_ref() {
                sink.status().record_shard_failed(&err.to_string());
            }
            return Err(err);
        }
    };
    let final_result = loop {
        match runtime.run().await {
            Ok(()) => break Ok(()),
            Err(err) if runtime.config.auto_reconnect && is_auto_reconnectable(&err) => {
                if !runtime.reconnect_after_transient(&err).await {
                    break Ok(());
                }
            }
            Err(err) => break Err(err),
        }
    };
    runtime.cleanup_on_exit().await;
    match &final_result {
        Ok(()) => runtime.telemetry(|status| status.record_shard_stopped()),
        Err(err) => runtime.telemetry(|status| status.record_shard_failed(&err.to_string())),
    }
    final_result?;
    Ok(runtime.summary)
}

impl WorkerRuntime {
    fn new(
        config: CommunityShardWorkerConfig,
        keypair: Arc<KeyPair>,
        model_spec: TrustedModelSpec,
        loader: PartialModelLoader,
        shutdown: watch::Receiver<bool>,
    ) -> Result<Self, CommunityShardWorkerError> {
        if !matches!(keypair.as_ref(), KeyPair::Ed25519(_)) {
            return Err(CommunityShardWorkerError::InvalidKeyPair(
                "community shard worker requires an Ed25519 keypair".to_string(),
            ));
        }
        let worker_id = keypair.address();
        if worker_id == Hash256::ZERO {
            return Err(CommunityShardWorkerError::InvalidKeyPair(
                "worker identity must be nonzero".to_string(),
            ));
        }
        let base_url = config.validate(&model_spec)?;
        let parsed = Url::parse(&base_url).map_err(|err| {
            CommunityShardWorkerError::InvalidConfig(format!("invalid coordinator URL: {err}"))
        })?;
        let mut builder = Client::builder()
            .redirect(redirect::Policy::none())
            .connect_timeout(Duration::from_millis(config.connect_timeout_ms));
        if parsed.scheme() == "http" {
            builder = builder.no_proxy();
        }
        let http = builder
            .build()
            .map_err(|err| CommunityShardWorkerError::HttpClientBuild(err.to_string()))?;
        let worker_id_hex = format!("0x{}", worker_id.to_hex());
        Ok(Self {
            config,
            base_url,
            http,
            keypair,
            worker_id,
            worker_id_hex,
            model_spec,
            loader,
            shutdown,
            executor: None,
            active_lease: None,
            unreleased_prior_lease: None,
            ready_ttl_ms: None,
            fence: LeaseFenceTracker::default(),
            consecutive_reacquires: 0,
            consecutive_reregisters: 0,
            consecutive_heartbeat_transients: 0,
            consecutive_reconnects: 0,
            summary: WorkerRunSummary::default(),
        })
    }

    async fn run(&mut self) -> Result<(), CommunityShardWorkerError> {
        if *self.shutdown.borrow() {
            return Ok(());
        }
        if matches!(self.register_worker().await?, RegisterOutcome::Shutdown) {
            return Ok(());
        }
        let mut pending_rebalance: Option<ShardLeaseAssignment> = None;
        let mut needs_reregister = false;
        loop {
            if *self.shutdown.borrow() {
                return Ok(());
            }
            if needs_reregister {
                needs_reregister = false;
                self.consecutive_reregisters += 1;
                if self.consecutive_reregisters > MAX_CONSECUTIVE_REREGISTERS {
                    return Err(CommunityShardWorkerError::ReregisterLimitExceeded {
                        limit: MAX_CONSECUTIVE_REREGISTERS,
                    });
                }
                let wait_ms = compute_backoff_ms(
                    self.config.reacquire_base_backoff_ms,
                    self.config.reacquire_max_backoff_ms,
                    self.consecutive_reregisters,
                );
                if self
                    .sleep_until_shutdown(Duration::from_millis(wait_ms))
                    .await
                {
                    return Ok(());
                }
                if matches!(self.register_worker().await?, RegisterOutcome::Shutdown) {
                    return Ok(());
                }
            }
            let (assignment, is_rebalance) = if let Some(rebalanced) = pending_rebalance.take() {
                let now_ms = checked_now_unix_ms()?;
                validate_lease_assignment(
                    &self.config,
                    &self.model_spec,
                    &rebalanced,
                    now_ms,
                    &self.fence,
                )?;
                self.drop_loaded_model_only()?;
                (rebalanced, true)
            } else {
                match self.acquire_lease().await? {
                    LeaseAcquireOutcome::Acquired(resp) => {
                        self.ready_ttl_ms = Some(resp.ready_ttl_ms);
                        self.summary.leases_acquired += 1;
                        (resp.assignment, false)
                    }
                    LeaseAcquireOutcome::PriorLeaseReleaseRetry {
                        needs_reregister: rereg,
                    } => {
                        self.telemetry(|status| {
                            status.record_shard_reconnecting("prior lease release pending")
                        });
                        needs_reregister = rereg;
                        if !self.wait_reacquire_backoff().await? {
                            return Ok(());
                        }
                        continue;
                    }
                    LeaseAcquireOutcome::ReregisterRequired => {
                        needs_reregister = true;
                        continue;
                    }
                    LeaseAcquireOutcome::Shutdown => return Ok(()),
                }
            };
            self.fence.record(&assignment);
            self.unreleased_prior_lease = None;
            self.active_lease = Some(assignment.clone());
            self.summary.clean_release_sent = false;
            self.consecutive_heartbeat_transients = 0;
            self.telemetry(|status| {
                status.record_shard_loading(
                    assignment.lease_id.seq,
                    assignment.lease_epoch,
                    assignment.range.start_layer,
                    assignment.range.end_layer,
                    self.model_spec.n_layers,
                )
            });
            match self.load_and_bind_lease(assignment).await? {
                LoadOutcome::Bound => {
                    if is_rebalance {
                        self.summary.rebalances_completed += 1;
                        self.telemetry(|status| status.record_shard_rebalance_completed());
                    }
                }
                LoadOutcome::Rebalance(next) => {
                    pending_rebalance = Some(next);
                    continue;
                }
                LoadOutcome::Reacquire {
                    needs_reregister: rereg,
                } => {
                    self.telemetry(|status| {
                        status.record_shard_reconnecting("lease reacquire required")
                    });
                    self.release_local_and_remote().await;
                    needs_reregister = rereg;
                    if !self.wait_reacquire_backoff().await? {
                        return Ok(());
                    }
                    continue;
                }
                LoadOutcome::Shutdown => return Ok(()),
            }
            match self.transition_to_ready().await? {
                StepCycleOutcome::Continue => {
                    self.consecutive_reacquires = 0;
                    self.consecutive_reregisters = 0;
                    self.consecutive_reconnects = 0;
                }
                StepCycleOutcome::Rebalance(next) => {
                    pending_rebalance = Some(next);
                    continue;
                }
                StepCycleOutcome::Reacquire {
                    needs_reregister: rereg,
                } => {
                    self.telemetry(|status| {
                        status.record_shard_reconnecting("lease reacquire required")
                    });
                    self.release_local_and_remote().await;
                    needs_reregister = rereg;
                    if !self.wait_reacquire_backoff().await? {
                        return Ok(());
                    }
                    continue;
                }
                StepCycleOutcome::Shutdown => return Ok(()),
            }
            match self.run_active_loop().await? {
                StepCycleOutcome::Continue => {}
                StepCycleOutcome::Rebalance(next) => {
                    pending_rebalance = Some(next);
                }
                StepCycleOutcome::Reacquire {
                    needs_reregister: rereg,
                } => {
                    self.telemetry(|status| {
                        status.record_shard_reconnecting("lease reacquire required")
                    });
                    self.release_local_and_remote().await;
                    needs_reregister = rereg;
                    if !self.wait_reacquire_backoff().await? {
                        return Ok(());
                    }
                }
                StepCycleOutcome::Shutdown => return Ok(()),
            }
        }
    }

    fn telemetry<F>(&self, f: F)
    where
        F: FnOnce(&CommunityWorkerStatus),
    {
        if let Some(sink) = self.config.telemetry.as_ref() {
            f(sink.status());
        }
    }

    async fn cleanup_on_exit(&mut self) {
        self.release_local_and_remote().await;
        self.executor = None;
        self.active_lease = None;
        self.unreleased_prior_lease = None;
        self.ready_ttl_ms = None;
    }

    async fn reconnect_after_transient(&mut self, error: &CommunityShardWorkerError) -> bool {
        self.cleanup_for_reconnect().await;
        if self.is_shutdown_requested() {
            return false;
        }
        self.consecutive_reconnects = self.consecutive_reconnects.saturating_add(1);
        self.summary.reconnect_attempts = self.summary.reconnect_attempts.saturating_add(1);
        self.telemetry(|status| status.record_shard_reconnect_attempt(&error.to_string()));
        let backoff_ms = compute_backoff_ms(
            self.config.reconnect_base_backoff_ms,
            self.config.reconnect_max_backoff_ms,
            self.consecutive_reconnects,
        );
        tracing::warn!(
            %error,
            attempt = self.consecutive_reconnects,
            backoff_ms,
            "community shard worker transient outage; backing off before reconnect"
        );
        !self
            .sleep_until_shutdown(Duration::from_millis(backoff_ms))
            .await
    }

    async fn cleanup_for_reconnect(&mut self) {
        self.release_local_and_remote().await;
        if let Some(exec) = self.executor.as_mut()
            && let Ok(now_ms) = checked_now_unix_ms()
        {
            exec.sweep_expired(now_ms);
        }
        self.ready_ttl_ms = None;
        self.consecutive_reacquires = 0;
        self.consecutive_reregisters = 0;
        self.consecutive_heartbeat_transients = 0;
    }

    fn is_shutdown_requested(&self) -> bool {
        *self.shutdown.borrow() || self.shutdown.has_changed().is_err()
    }

    fn drop_loaded_model_only(&mut self) -> Result<(), CommunityShardWorkerError> {
        if let Some(exec) = self.executor.as_mut()
            && exec.lease().is_some()
        {
            let now_ms = checked_now_unix_ms()?;
            let _ = exec.release(now_ms)?;
        }
        Ok(())
    }

    async fn release_local_and_remote(&mut self) {
        let mut release_req = None;
        if let Some(exec) = self.executor.as_mut()
            && exec.lease().is_some()
            && let Ok(now_ms) = checked_now_unix_ms()
            && let Ok(req) = exec.release(now_ms)
        {
            release_req = Some(req);
        }
        let prior_assignment = self
            .active_lease
            .take()
            .or_else(|| self.unreleased_prior_lease.clone());
        if release_req.is_none()
            && let Some(lease) = prior_assignment.as_ref()
        {
            release_req = Some(ShardReleaseRequest {
                worker_id: self.worker_id,
                lease_id: lease.lease_id,
                lease_epoch: lease.lease_epoch,
            });
        }
        if let Some(req) = release_req {
            let mut released = false;
            let release_deadline =
                Instant::now() + Duration::from_millis(self.config.release_timeout_ms);
            for attempt in 0..MAX_RELEASE_TRANSIENT_ATTEMPTS {
                let Some(remaining) = release_deadline.checked_duration_since(Instant::now())
                else {
                    break;
                };
                if remaining.is_zero() {
                    break;
                }
                match self
                    .post_signed_shard_once::<ShardReleaseRequest, ShardReleaseResponse>(
                        COMMUNITY_SHARD_RELEASE_PATH,
                        &req,
                        remaining,
                    )
                    .await
                {
                    Ok(HttpCallResult::Ok(resp)) => {
                        if resp.released {
                            self.summary.clean_release_sent = true;
                            released = true;
                        }
                        break;
                    }
                    Ok(HttpCallResult::Transient(_))
                        if attempt + 1 < MAX_RELEASE_TRANSIENT_ATTEMPTS =>
                    {
                        let Some(left) = release_deadline.checked_duration_since(Instant::now())
                        else {
                            break;
                        };
                        let sleep_dur =
                            Duration::from_millis(self.config.retry_base_backoff_ms).min(left);
                        if sleep_dur.is_zero() {
                            break;
                        }
                        tokio::time::sleep(sleep_dur).await;
                    }
                    _ => break,
                }
            }
            if released {
                self.unreleased_prior_lease = None;
            } else if let Some(lease) = prior_assignment {
                self.unreleased_prior_lease = Some(lease);
            }
        }
    }

    fn is_unreleased_prior_assignment(&self, assignment: &ShardLeaseAssignment) -> bool {
        self.unreleased_prior_lease.as_ref() == Some(assignment)
            && self.fence.is_current_high_water(assignment)
    }

    async fn release_duplicate_prior_lease(
        &mut self,
        assignment: &ShardLeaseAssignment,
    ) -> Result<LeaseAcquireOutcome, CommunityShardWorkerError> {
        let req = ShardReleaseRequest {
            worker_id: self.worker_id,
            lease_id: assignment.lease_id,
            lease_epoch: assignment.lease_epoch,
        };
        let timeout = Duration::from_millis(self.config.release_timeout_ms);
        let call = match self
            .post_signed_shard_cancellable::<ShardReleaseRequest, ShardReleaseResponse>(
                COMMUNITY_SHARD_RELEASE_PATH,
                &req,
                timeout,
            )
            .await
        {
            Ok(call) => call,
            Err(err) => {
                self.unreleased_prior_lease = None;
                return Err(err);
            }
        };
        match call {
            CancellableCall::Shutdown => Ok(LeaseAcquireOutcome::Shutdown),
            CancellableCall::Completed(HttpCallResult::Ok(resp)) => {
                if resp.released {
                    self.summary.clean_release_sent = true;
                    self.unreleased_prior_lease = None;
                }
                Ok(LeaseAcquireOutcome::PriorLeaseReleaseRetry {
                    needs_reregister: false,
                })
            }
            CancellableCall::Completed(HttpCallResult::HttpError { status, body }) => {
                if is_registration_missing_error(status, &body) {
                    return Ok(LeaseAcquireOutcome::PriorLeaseReleaseRetry {
                        needs_reregister: true,
                    });
                }
                if is_lease_not_found_error(status, &body) {
                    self.unreleased_prior_lease = None;
                    return Ok(LeaseAcquireOutcome::PriorLeaseReleaseRetry {
                        needs_reregister: false,
                    });
                }
                if status == StatusCode::CONFLICT && !is_fatal_model_or_profile_conflict(&body) {
                    return Ok(LeaseAcquireOutcome::PriorLeaseReleaseRetry {
                        needs_reregister: false,
                    });
                }
                self.unreleased_prior_lease = None;
                Err(CommunityShardWorkerError::ProtocolRejected {
                    path: COMMUNITY_SHARD_RELEASE_PATH,
                    status: status.as_u16(),
                    detail: body,
                })
            }
            CancellableCall::Completed(HttpCallResult::Transient(_)) => {
                Ok(LeaseAcquireOutcome::PriorLeaseReleaseRetry {
                    needs_reregister: false,
                })
            }
        }
    }

    async fn wait_reacquire_backoff(&mut self) -> Result<bool, CommunityShardWorkerError> {
        self.consecutive_reacquires += 1;
        if self.consecutive_reacquires > self.config.max_consecutive_lease_reacquires {
            return Err(CommunityShardWorkerError::LeaseReacquireLimitExceeded {
                limit: self.config.max_consecutive_lease_reacquires,
            });
        }
        let backoff_ms = compute_backoff_ms(
            self.config.reacquire_base_backoff_ms,
            self.config.reacquire_max_backoff_ms,
            self.consecutive_reacquires,
        );
        Ok(!self
            .sleep_until_shutdown(Duration::from_millis(backoff_ms))
            .await)
    }

    async fn register_worker(&mut self) -> Result<RegisterOutcome, CommunityShardWorkerError> {
        let req = CommunityRegisterRequest {
            worker_id: self.worker_id_hex.clone(),
            name: self.config.worker_name.trim().to_string(),
            capabilities: vec![COMMUNITY_SHARD_CAPABILITY.to_string()],
            model: Some(self.config.model_name.trim().to_string()),
            model_id: Some(format!("0x{}", self.model_spec.model_id.to_hex())),
            execution_profile: Some(self.model_spec.execution_profile.clone()),
            platform: self.config.platform.trim().to_string(),
        };
        let timeout = Duration::from_millis(self.config.request_timeout_ms);
        let mut last_transient = String::new();
        for attempt in 0..MAX_REGISTER_TRANSIENT_ATTEMPTS {
            match self.post_register_cancellable(&req, timeout).await? {
                CancellableCall::Shutdown => return Ok(RegisterOutcome::Shutdown),
                CancellableCall::Completed(HttpCallResult::Ok(resp)) => {
                    if !resp.ok || resp.worker_id != self.worker_id_hex {
                        return Err(CommunityShardWorkerError::MalformedResponse {
                            path: COMMUNITY_REGISTER_PATH,
                            detail: "registration acknowledgement rejected or mismatched worker_id"
                                .to_string(),
                        });
                    }
                    self.summary.registrations += 1;
                    self.telemetry(|status| status.record_shard_registered());
                    return Ok(RegisterOutcome::Registered);
                }
                CancellableCall::Completed(HttpCallResult::HttpError { status, body }) => {
                    return Err(CommunityShardWorkerError::ProtocolRejected {
                        path: COMMUNITY_REGISTER_PATH,
                        status: status.as_u16(),
                        detail: body,
                    });
                }
                CancellableCall::Completed(HttpCallResult::Transient(detail)) => {
                    last_transient = detail;
                    if attempt + 1 < MAX_REGISTER_TRANSIENT_ATTEMPTS {
                        let backoff_ms = compute_backoff_ms(
                            self.config.retry_base_backoff_ms,
                            self.config.retry_max_backoff_ms,
                            attempt + 1,
                        );
                        if self
                            .sleep_until_shutdown(Duration::from_millis(backoff_ms))
                            .await
                        {
                            return Ok(RegisterOutcome::Shutdown);
                        }
                    }
                }
            }
        }
        Err(CommunityShardWorkerError::TransientBudgetExhausted {
            path: COMMUNITY_REGISTER_PATH,
            detail: last_transient,
        })
    }

    async fn acquire_lease(&mut self) -> Result<LeaseAcquireOutcome, CommunityShardWorkerError> {
        let req = ShardLeaseRequest {
            worker_id: self.worker_id,
            protocol_version: COMMUNITY_SHARD_PROTOCOL_VERSION,
            model_id: self.model_spec.model_id,
            execution_profile: self.model_spec.execution_profile.clone(),
            n_layers: self.model_spec.n_layers,
            d_model: self.model_spec.d_model,
            vocab_size: self.model_spec.vocab_size,
            max_seq: self.model_spec.max_seq,
            max_resident_layers: self.config.max_resident_layers,
        };
        let timeout = Duration::from_millis(self.config.request_timeout_ms);
        let mut last_transient = String::new();
        for attempt in 0..MAX_LEASE_TRANSIENT_ATTEMPTS {
            match self
                .post_signed_shard_cancellable::<ShardLeaseRequest, ShardLeaseResponse>(
                    COMMUNITY_SHARD_LEASE_PATH,
                    &req,
                    timeout,
                )
                .await?
            {
                CancellableCall::Shutdown => return Ok(LeaseAcquireOutcome::Shutdown),
                CancellableCall::Completed(HttpCallResult::Ok(resp)) => {
                    let now_ms = checked_now_unix_ms()?;
                    validate_lease_response_envelope(&self.config, &resp)?;
                    validate_lease_assignment_shape(
                        &self.config,
                        &self.model_spec,
                        &resp.assignment,
                    )?;
                    if self.is_unreleased_prior_assignment(&resp.assignment) {
                        return self.release_duplicate_prior_lease(&resp.assignment).await;
                    }
                    validate_lease_assignment_progression(&resp.assignment, now_ms, &self.fence)?;
                    self.unreleased_prior_lease = None;
                    return Ok(LeaseAcquireOutcome::Acquired(resp));
                }
                CancellableCall::Completed(HttpCallResult::HttpError { status, body }) => {
                    if is_registration_missing_error(status, &body) {
                        return Ok(LeaseAcquireOutcome::ReregisterRequired);
                    }
                    return Err(CommunityShardWorkerError::ProtocolRejected {
                        path: COMMUNITY_SHARD_LEASE_PATH,
                        status: status.as_u16(),
                        detail: body,
                    });
                }
                CancellableCall::Completed(HttpCallResult::Transient(detail)) => {
                    last_transient = detail;
                    if attempt + 1 < MAX_LEASE_TRANSIENT_ATTEMPTS {
                        let backoff_ms = compute_backoff_ms(
                            self.config.retry_base_backoff_ms,
                            self.config.retry_max_backoff_ms,
                            attempt + 1,
                        );
                        if self
                            .sleep_until_shutdown(Duration::from_millis(backoff_ms))
                            .await
                        {
                            return Ok(LeaseAcquireOutcome::Shutdown);
                        }
                    }
                }
            }
        }
        Err(CommunityShardWorkerError::TransientBudgetExhausted {
            path: COMMUNITY_SHARD_LEASE_PATH,
            detail: last_transient,
        })
    }

    async fn load_and_bind_lease(
        &mut self,
        assignment: ShardLeaseAssignment,
    ) -> Result<LoadOutcome, CommunityShardWorkerError> {
        let loader = Arc::clone(&self.loader);
        let assignment_for_loader = assignment.clone();
        let mut load_handle = tokio::task::spawn_blocking(move || loader(&assignment_for_loader));
        let mut next_hb = self.next_heartbeat_deadline();
        let mut shutdown = self.shutdown.clone();
        let mut abandoned: Option<LoadAbandonment> = None;

        if *shutdown.borrow() {
            abandoned = Some(LoadAbandonment::Shutdown);
        }

        let join_res = loop {
            if abandoned.is_some() {
                break load_handle.await;
            }
            tokio::select! {
                biased;
                changed = shutdown.changed() => {
                    if changed.is_err() || *shutdown.borrow() {
                        abandoned = Some(LoadAbandonment::Shutdown);
                    }
                }
                res = &mut load_handle => break res,
                _ = tokio::time::sleep_until(next_hb) => {
                    match self.send_heartbeat_once(&assignment, false).await {
                        HeartbeatOutcome::Continue { .. } => {
                            next_hb = self.next_heartbeat_deadline();
                            self.telemetry(|status| status.record_shard_heartbeat_provisioning());
                        }
                        HeartbeatOutcome::Transient => {
                            next_hb = self.next_transient_heartbeat_deadline();
                        }
                        HeartbeatOutcome::Rebalance(next) => {
                            abandoned = Some(LoadAbandonment::Rebalance(next));
                        }
                        HeartbeatOutcome::Reacquire { needs_reregister } => {
                            abandoned = Some(LoadAbandonment::Reacquire { needs_reregister });
                        }
                        HeartbeatOutcome::Shutdown => {
                            abandoned = Some(LoadAbandonment::Shutdown);
                        }
                        HeartbeatOutcome::Fatal(err) => {
                            abandoned = Some(LoadAbandonment::Fatal(err));
                        }
                    }
                }
            }
        };

        if let Some(reason) = abandoned {
            drop(join_res);
            return match reason {
                LoadAbandonment::Rebalance(next) => Ok(LoadOutcome::Rebalance(next)),
                LoadAbandonment::Reacquire { needs_reregister } => {
                    Ok(LoadOutcome::Reacquire { needs_reregister })
                }
                LoadAbandonment::Shutdown => Ok(LoadOutcome::Shutdown),
                LoadAbandonment::Fatal(err) => Err(err),
            };
        }

        let loaded_model = join_res
            .map_err(|err| CommunityShardWorkerError::BlockingTaskFailed(err.to_string()))?
            .map_err(|err| CommunityShardWorkerError::ModelLoadFailed(err.to_string()))?;

        if *self.shutdown.borrow() {
            drop(loaded_model);
            return Ok(LoadOutcome::Shutdown);
        }

        let now_ms = checked_now_unix_ms()?;
        if let Some(exec) = self.executor.as_mut() {
            exec.rebind(
                self.config.coordinator_id,
                now_ms,
                self.model_spec.clone(),
                assignment,
                loaded_model,
            )?;
        } else {
            let exec = CommunityShardExecutor::new(
                self.worker_id,
                self.config.coordinator_id,
                self.model_spec.clone(),
                assignment,
                loaded_model,
                self.config.executor_config.clone(),
            )?;
            self.executor = Some(exec);
        }
        self.summary.models_loaded += 1;
        Ok(LoadOutcome::Bound)
    }

    async fn transition_to_ready(&mut self) -> Result<StepCycleOutcome, CommunityShardWorkerError> {
        let assignment = self.active_lease.clone().ok_or_else(|| {
            CommunityShardWorkerError::InvalidLease("missing active lease".to_string())
        })?;
        let mut attempt = 0usize;
        loop {
            if *self.shutdown.borrow() {
                return Ok(StepCycleOutcome::Shutdown);
            }
            let now_ms = checked_now_unix_ms()?;
            if now_ms >= assignment.provisioning_deadline_unix_ms {
                return Ok(StepCycleOutcome::Reacquire {
                    needs_reregister: false,
                });
            }
            match self.send_heartbeat_once(&assignment, true).await {
                HeartbeatOutcome::Continue {
                    cancellations,
                    expires_at_unix_ms,
                } => {
                    if let Some(expires) = expires_at_unix_ms {
                        self.telemetry(|status| {
                            status.record_shard_ready(
                                assignment.lease_id.seq,
                                assignment.lease_epoch,
                                crate::community_worker::PartialShardLayerRangeSnapshot {
                                    start_layer: assignment.range.start_layer,
                                    end_layer: assignment.range.end_layer,
                                    total_layers: self.model_spec.n_layers,
                                },
                                expires,
                            )
                        });
                    }
                    self.apply_cancellations(&cancellations)?;
                    return Ok(StepCycleOutcome::Continue);
                }
                HeartbeatOutcome::Transient => {
                    attempt = attempt.saturating_add(1);
                    let backoff_ms = compute_backoff_ms(
                        self.config.retry_base_backoff_ms,
                        self.config.retry_max_backoff_ms,
                        attempt,
                    );
                    let remaining_ms = assignment
                        .provisioning_deadline_unix_ms
                        .saturating_sub(checked_now_unix_ms()?);
                    if remaining_ms == 0 || backoff_ms >= remaining_ms {
                        return Ok(StepCycleOutcome::Reacquire {
                            needs_reregister: false,
                        });
                    }
                    if self
                        .sleep_until_shutdown(Duration::from_millis(backoff_ms))
                        .await
                    {
                        return Ok(StepCycleOutcome::Shutdown);
                    }
                }
                HeartbeatOutcome::Rebalance(next) => return Ok(StepCycleOutcome::Rebalance(next)),
                HeartbeatOutcome::Reacquire { needs_reregister } => {
                    return Ok(StepCycleOutcome::Reacquire { needs_reregister });
                }
                HeartbeatOutcome::Shutdown => return Ok(StepCycleOutcome::Shutdown),
                HeartbeatOutcome::Fatal(err) => return Err(err),
            }
        }
    }

    fn try_enter_worker_execution_gate(&self) -> GateAdmission {
        match self.config.worker_execution_gate.as_ref() {
            Some(gate) => match gate.gate().try_enter() {
                Some(permit) => GateAdmission::Admitted(Some(permit)),
                None => GateAdmission::Paused,
            },
            None => GateAdmission::Admitted(None),
        }
    }

    async fn run_active_loop(&mut self) -> Result<StepCycleOutcome, CommunityShardWorkerError> {
        let mut next_heartbeat_at = self.next_heartbeat_deadline();
        let mut transient_claim_errors = 0usize;
        loop {
            if *self.shutdown.borrow() {
                return Ok(StepCycleOutcome::Shutdown);
            }
            let now_ms = checked_now_unix_ms()?;
            if let Some(exec) = self.executor.as_mut() {
                exec.sweep_expired(now_ms);
            }
            let assignment = self.active_lease.clone().ok_or_else(|| {
                CommunityShardWorkerError::InvalidLease("missing active lease".to_string())
            })?;
            if Instant::now() >= next_heartbeat_at {
                match self
                    .send_due_heartbeat(&assignment, &mut next_heartbeat_at)
                    .await?
                {
                    StepCycleOutcome::Continue => {}
                    other => return Ok(other),
                }
            }
            let execution_permit = match self.try_enter_worker_execution_gate() {
                GateAdmission::Admitted(permit) => permit,
                GateAdmission::Paused => {
                    let wait = Duration::from_millis(self.config.idle_poll_interval_ms);
                    match self
                        .wait_with_heartbeats(wait, &mut next_heartbeat_at, None)
                        .await?
                    {
                        WaitOutcome::Elapsed | WaitOutcome::WatchedSessionCancelled => continue,
                        WaitOutcome::Rebalance(next) => {
                            return Ok(StepCycleOutcome::Rebalance(next));
                        }
                        WaitOutcome::Reacquire { needs_reregister } => {
                            return Ok(StepCycleOutcome::Reacquire { needs_reregister });
                        }
                        WaitOutcome::Shutdown => return Ok(StepCycleOutcome::Shutdown),
                    }
                }
            };
            let claim_req = ShardClaimStepRequest {
                worker_id: self.worker_id,
                lease_id: assignment.lease_id,
                lease_epoch: assignment.lease_epoch,
                model_id: assignment.model_id,
                execution_profile: assignment.execution_profile.clone(),
            };
            let timeout = Duration::from_millis(self.active_request_timeout_ms());
            match self
                .post_signed_shard_cancellable::<ShardClaimStepRequest, ShardClaimStepResponse>(
                    COMMUNITY_SHARD_CLAIM_STEP_PATH,
                    &claim_req,
                    timeout,
                )
                .await?
            {
                CancellableCall::Shutdown => {
                    drop(execution_permit);
                    return Ok(StepCycleOutcome::Shutdown);
                }
                CancellableCall::Completed(HttpCallResult::Ok(resp)) => {
                    transient_claim_errors = 0;
                    match resp {
                        ShardClaimStepResponse::Idle => {
                            drop(execution_permit);
                            let wait = Duration::from_millis(self.config.idle_poll_interval_ms);
                            match self
                                .wait_with_heartbeats(wait, &mut next_heartbeat_at, None)
                                .await?
                            {
                                WaitOutcome::Elapsed | WaitOutcome::WatchedSessionCancelled => {}
                                WaitOutcome::Rebalance(next) => {
                                    return Ok(StepCycleOutcome::Rebalance(next));
                                }
                                WaitOutcome::Reacquire { needs_reregister } => {
                                    return Ok(StepCycleOutcome::Reacquire { needs_reregister });
                                }
                                WaitOutcome::Shutdown => return Ok(StepCycleOutcome::Shutdown),
                            }
                        }
                        ShardClaimStepResponse::CancelSessions { cancellations } => {
                            drop(execution_permit);
                            if cancellations.len() > MAX_CANCELLATIONS_PER_WORKER_HARD_CAP {
                                return Err(CommunityShardWorkerError::MalformedResponse {
                                    path: COMMUNITY_SHARD_CLAIM_STEP_PATH,
                                    detail: "cancellations batch exceeds hard cap".to_string(),
                                });
                            }
                            self.apply_cancellations(&cancellations)?;
                        }
                        ShardClaimStepResponse::RebalanceRequired { assignment: next } => {
                            drop(execution_permit);
                            let now_ms = checked_now_unix_ms()?;
                            validate_lease_assignment(
                                &self.config,
                                &self.model_spec,
                                &next,
                                now_ms,
                                &self.fence,
                            )?;
                            return Ok(StepCycleOutcome::Rebalance(next));
                        }
                        ShardClaimStepResponse::Step { work } => {
                            match self
                                .execute_and_submit_step(
                                    *work,
                                    &mut next_heartbeat_at,
                                    execution_permit,
                                )
                                .await?
                            {
                                StepCycleOutcome::Continue => {
                                    self.telemetry(|status| status.record_shard_idle_ready());
                                }
                                other => return Ok(other),
                            }
                        }
                    }
                }
                CancellableCall::Completed(HttpCallResult::HttpError { status, body }) => {
                    drop(execution_permit);
                    if is_registration_missing_error(status, &body) {
                        return Ok(StepCycleOutcome::Reacquire {
                            needs_reregister: true,
                        });
                    }
                    if status == StatusCode::CONFLICT && is_fatal_model_or_profile_conflict(&body) {
                        return Err(CommunityShardWorkerError::ProtocolRejected {
                            path: COMMUNITY_SHARD_CLAIM_STEP_PATH,
                            status: status.as_u16(),
                            detail: body,
                        });
                    }
                    if matches!(
                        status,
                        StatusCode::BAD_REQUEST | StatusCode::NOT_FOUND | StatusCode::CONFLICT
                    ) {
                        return Ok(StepCycleOutcome::Reacquire {
                            needs_reregister: false,
                        });
                    }
                    return Err(CommunityShardWorkerError::ProtocolRejected {
                        path: COMMUNITY_SHARD_CLAIM_STEP_PATH,
                        status: status.as_u16(),
                        detail: body,
                    });
                }
                CancellableCall::Completed(HttpCallResult::Transient(_)) => {
                    drop(execution_permit);
                    transient_claim_errors = transient_claim_errors.saturating_add(1);
                    let wait_ms = compute_backoff_ms(
                        self.config.retry_base_backoff_ms,
                        self.config.retry_max_backoff_ms,
                        transient_claim_errors,
                    );
                    match self
                        .wait_with_heartbeats(
                            Duration::from_millis(wait_ms),
                            &mut next_heartbeat_at,
                            None,
                        )
                        .await?
                    {
                        WaitOutcome::Elapsed | WaitOutcome::WatchedSessionCancelled => {}
                        WaitOutcome::Rebalance(next) => {
                            return Ok(StepCycleOutcome::Rebalance(next));
                        }
                        WaitOutcome::Reacquire { needs_reregister } => {
                            return Ok(StepCycleOutcome::Reacquire { needs_reregister });
                        }
                        WaitOutcome::Shutdown => return Ok(StepCycleOutcome::Shutdown),
                    }
                }
            }
        }
    }

    async fn execute_and_submit_step(
        &mut self,
        work: ShardStepWork,
        next_heartbeat_at: &mut Instant,
        execution_permit: Option<WorkerExecutionPermit>,
    ) -> Result<StepCycleOutcome, CommunityShardWorkerError> {
        let assignment = self.active_lease.clone().ok_or_else(|| {
            CommunityShardWorkerError::InvalidLease("missing active lease".to_string())
        })?;
        if work.lease_id != assignment.lease_id
            || work.lease_epoch != assignment.lease_epoch
            || work.session_id.boot_nonce != assignment.lease_id.boot_nonce
            || work.step_id.boot_nonce != assignment.lease_id.boot_nonce
        {
            return Ok(StepCycleOutcome::Reacquire {
                needs_reregister: false,
            });
        }
        if work.model_id != self.model_spec.model_id
            || work.execution_profile != self.model_spec.execution_profile
            || work.range != assignment.range
        {
            return Err(CommunityShardWorkerError::MalformedResponse {
                path: COMMUNITY_SHARD_CLAIM_STEP_PATH,
                detail: "step work model, profile, or range mismatches active lease".to_string(),
            });
        }
        let start_now_ms = checked_now_unix_ms()?;
        if start_now_ms >= work.deadline_unix_ms {
            return Ok(StepCycleOutcome::Continue);
        }
        let mut exec = self.executor.take().ok_or_else(|| {
            CommunityShardWorkerError::InvalidLease("executor unbound during step".to_string())
        })?;
        self.telemetry(|status| status.record_shard_computing());
        let coord_id = self.config.coordinator_id;
        let prevent_sleep_during_jobs = self.config.prevent_sleep_during_jobs;
        let work_for_exec = work.clone();
        let compute_permit = execution_permit.clone();
        let mut exec_handle = spawn_blocking_with_execution_permit(compute_permit, move || {
            let _keep_awake = crate::community_worker::KeepAwake::begin(prevent_sleep_during_jobs);
            let result = exec.execute_step(coord_id, start_now_ms, &work_for_exec);
            (exec, result)
        });

        let mut queued_cancellations: VecDeque<SessionCancellationNotice> = VecDeque::new();
        let mut current_session_cancelled = false;
        let mut stop_reason: Option<ComputeStop> = None;
        let mut shutdown = self.shutdown.clone();
        if *shutdown.borrow() {
            stop_reason = Some(ComputeStop::Shutdown);
        }

        let join_res = loop {
            if stop_reason.is_some() {
                break exec_handle.await;
            }
            let next_hb = *next_heartbeat_at;
            tokio::select! {
                biased;
                changed = shutdown.changed() => {
                    if changed.is_err() || *shutdown.borrow() {
                        stop_reason = Some(ComputeStop::Shutdown);
                    }
                }
                res = &mut exec_handle => break res,
                _ = tokio::time::sleep_until(next_hb) => {
                    match self.send_heartbeat_once(&assignment, true).await {
                        HeartbeatOutcome::Continue {
                            cancellations,
                            expires_at_unix_ms,
                        } => {
                            *next_heartbeat_at = self.next_heartbeat_deadline();
                            if let Some(expires) = expires_at_unix_ms {
                                self.telemetry(|status| status.record_shard_heartbeat_active(expires));
                            }
                            for notice in cancellations {
                                if notice.session_id == work.session_id
                                    && notice.lease_id == assignment.lease_id
                                    && notice.lease_epoch == assignment.lease_epoch
                                {
                                    current_session_cancelled = true;
                                }
                                push_bounded_cancellation(
                                    &mut queued_cancellations,
                                    notice,
                                    work.session_id,
                                );
                            }
                        }
                        HeartbeatOutcome::Transient => {
                            *next_heartbeat_at = self.next_transient_heartbeat_deadline();
                        }
                        HeartbeatOutcome::Rebalance(next) => {
                            stop_reason = Some(ComputeStop::Rebalance(next));
                        }
                        HeartbeatOutcome::Reacquire { needs_reregister } => {
                            stop_reason = Some(ComputeStop::Reacquire { needs_reregister });
                        }
                        HeartbeatOutcome::Shutdown => {
                            stop_reason = Some(ComputeStop::Shutdown);
                        }
                        HeartbeatOutcome::Fatal(err) => {
                            stop_reason = Some(ComputeStop::Fatal(err));
                        }
                    }
                }
            }
        };

        let (exec, exec_outcome) = join_res
            .map_err(|err| CommunityShardWorkerError::BlockingTaskFailed(err.to_string()))?;
        self.executor = Some(exec);

        let drained: Vec<SessionCancellationNotice> = queued_cancellations.into_iter().collect();
        self.apply_cancellations(&drained)?;

        if let Some(stop) = stop_reason {
            return match stop {
                ComputeStop::Rebalance(next) => Ok(StepCycleOutcome::Rebalance(next)),
                ComputeStop::Reacquire { needs_reregister } => {
                    Ok(StepCycleOutcome::Reacquire { needs_reregister })
                }
                ComputeStop::Shutdown => Ok(StepCycleOutcome::Shutdown),
                ComputeStop::Fatal(err) => Err(err),
            };
        }
        if *self.shutdown.borrow() {
            return Ok(StepCycleOutcome::Shutdown);
        }
        if current_session_cancelled {
            return Ok(StepCycleOutcome::Continue);
        }
        let post_now_ms = checked_now_unix_ms()?;
        if post_now_ms >= work.deadline_unix_ms {
            return Ok(StepCycleOutcome::Continue);
        }

        let (submit_req, is_completed) = match exec_outcome {
            Ok(req) => {
                self.summary.steps_executed += 1;
                self.telemetry(|status| status.record_shard_step_executed());
                (req, true)
            }
            Err(err) => match classify_executor_error(err)? {
                ExecutorFaultAction::IgnoreStale => return Ok(StepCycleOutcome::Continue),
                ExecutorFaultAction::Reacquire => {
                    return Ok(StepCycleOutcome::Reacquire {
                        needs_reregister: false,
                    });
                }
                ExecutorFaultAction::Report(outcome) => (
                    ShardStepSubmitRequest {
                        worker_id: self.worker_id,
                        lease_id: work.lease_id,
                        lease_epoch: work.lease_epoch,
                        session_id: work.session_id,
                        step_id: work.step_id,
                        model_id: work.model_id,
                        execution_profile: work.execution_profile,
                        range: work.range,
                        position: work.position,
                        layers_processed: 0,
                        outcome,
                    },
                    false,
                ),
            },
        };

        let submit_outcome = self
            .submit_step_with_retry(
                submit_req,
                work.deadline_unix_ms,
                is_completed,
                next_heartbeat_at,
            )
            .await?;
        drop(execution_permit);
        match submit_outcome {
            SubmitOutcome::Accepted | SubmitOutcome::StaleOrExpired => {
                Ok(StepCycleOutcome::Continue)
            }
            SubmitOutcome::Rebalance(next) => Ok(StepCycleOutcome::Rebalance(next)),
            SubmitOutcome::Reacquire { needs_reregister } => {
                Ok(StepCycleOutcome::Reacquire { needs_reregister })
            }
            SubmitOutcome::Shutdown => Ok(StepCycleOutcome::Shutdown),
        }
    }

    async fn submit_step_with_retry(
        &mut self,
        submit_req: ShardStepSubmitRequest,
        deadline_unix_ms: u64,
        is_completed: bool,
        next_heartbeat_at: &mut Instant,
    ) -> Result<SubmitOutcome, CommunityShardWorkerError> {
        let mut attempt = 0usize;
        loop {
            if *self.shutdown.borrow() {
                return Ok(SubmitOutcome::Shutdown);
            }
            let now_ms = checked_now_unix_ms()?;
            if now_ms >= deadline_unix_ms {
                return Ok(SubmitOutcome::StaleOrExpired);
            }
            if Instant::now() >= *next_heartbeat_at {
                let Some(assignment) = self.active_lease.clone() else {
                    return Ok(SubmitOutcome::Reacquire {
                        needs_reregister: false,
                    });
                };
                match self.send_heartbeat_once(&assignment, true).await {
                    HeartbeatOutcome::Continue {
                        cancellations,
                        expires_at_unix_ms,
                    } => {
                        *next_heartbeat_at = self.next_heartbeat_deadline();
                        if let Some(expires) = expires_at_unix_ms {
                            self.telemetry(|status| status.record_shard_heartbeat_active(expires));
                        }
                        let cancelled = cancellations.iter().any(|notice| {
                            notice.session_id == submit_req.session_id
                                && notice.lease_id == assignment.lease_id
                                && notice.lease_epoch == assignment.lease_epoch
                        });
                        self.apply_cancellations(&cancellations)?;
                        if cancelled {
                            return Ok(SubmitOutcome::StaleOrExpired);
                        }
                    }
                    HeartbeatOutcome::Transient => {
                        *next_heartbeat_at = self.next_transient_heartbeat_deadline();
                    }
                    HeartbeatOutcome::Rebalance(next) => return Ok(SubmitOutcome::Rebalance(next)),
                    HeartbeatOutcome::Reacquire { needs_reregister } => {
                        return Ok(SubmitOutcome::Reacquire { needs_reregister });
                    }
                    HeartbeatOutcome::Shutdown => return Ok(SubmitOutcome::Shutdown),
                    HeartbeatOutcome::Fatal(err) => return Err(err),
                }
            }
            let now_ms = checked_now_unix_ms()?;
            if now_ms >= deadline_unix_ms {
                return Ok(SubmitOutcome::StaleOrExpired);
            }
            let remaining_ms = deadline_unix_ms.saturating_sub(now_ms);
            let timeout_ms = self.active_request_timeout_ms().min(remaining_ms).max(1);
            match self
                .post_signed_shard_cancellable::<ShardStepSubmitRequest, ShardStepSubmitResponse>(
                    COMMUNITY_SHARD_SUBMIT_STEP_PATH,
                    &submit_req,
                    Duration::from_millis(timeout_ms),
                )
                .await?
            {
                CancellableCall::Shutdown => return Ok(SubmitOutcome::Shutdown),
                CancellableCall::Completed(HttpCallResult::Ok(resp)) => {
                    if resp.accepted {
                        if is_completed {
                            self.summary.steps_submitted += 1;
                            self.telemetry(|status| status.record_shard_step_submitted());
                        }
                        return Ok(SubmitOutcome::Accepted);
                    }
                    self.summary.submit_stale_conflicts += 1;
                    self.telemetry(|status| status.record_shard_submit_stale_conflict());
                    return Ok(SubmitOutcome::StaleOrExpired);
                }
                CancellableCall::Completed(HttpCallResult::HttpError { status, body }) => {
                    if is_registration_missing_error(status, &body) {
                        return Ok(SubmitOutcome::Reacquire {
                            needs_reregister: true,
                        });
                    }
                    if status == StatusCode::CONFLICT && is_fatal_model_or_profile_conflict(&body) {
                        return Err(CommunityShardWorkerError::ProtocolRejected {
                            path: COMMUNITY_SHARD_SUBMIT_STEP_PATH,
                            status: status.as_u16(),
                            detail: body,
                        });
                    }
                    if is_submit_terminal_step_stale(status, &body) {
                        self.summary.submit_stale_conflicts += 1;
                        self.telemetry(|status| status.record_shard_submit_stale_conflict());
                        return Ok(SubmitOutcome::StaleOrExpired);
                    }
                    if matches!(
                        status,
                        StatusCode::BAD_REQUEST | StatusCode::NOT_FOUND | StatusCode::CONFLICT
                    ) {
                        if !is_completed && status == StatusCode::BAD_REQUEST {
                            return Ok(SubmitOutcome::StaleOrExpired);
                        }
                        return Ok(SubmitOutcome::Reacquire {
                            needs_reregister: false,
                        });
                    }
                    return Err(CommunityShardWorkerError::ProtocolRejected {
                        path: COMMUNITY_SHARD_SUBMIT_STEP_PATH,
                        status: status.as_u16(),
                        detail: body,
                    });
                }
                CancellableCall::Completed(HttpCallResult::Transient(_)) => {
                    attempt = attempt.saturating_add(1);
                    self.summary.submit_retries += 1;
                    self.telemetry(|status| status.record_shard_submit_retry());
                    let now_ms = checked_now_unix_ms()?;
                    if now_ms >= deadline_unix_ms {
                        return Ok(SubmitOutcome::StaleOrExpired);
                    }
                    let remaining_ms = deadline_unix_ms.saturating_sub(now_ms);
                    let backoff_ms = compute_backoff_ms(
                        self.config.retry_base_backoff_ms,
                        self.config.retry_max_backoff_ms,
                        attempt,
                    );
                    if backoff_ms >= remaining_ms {
                        return Ok(SubmitOutcome::StaleOrExpired);
                    }
                    match self
                        .wait_with_heartbeats(
                            Duration::from_millis(backoff_ms),
                            next_heartbeat_at,
                            Some(submit_req.session_id),
                        )
                        .await?
                    {
                        WaitOutcome::Elapsed => {}
                        WaitOutcome::WatchedSessionCancelled => {
                            return Ok(SubmitOutcome::StaleOrExpired);
                        }
                        WaitOutcome::Rebalance(next) => {
                            return Ok(SubmitOutcome::Rebalance(next));
                        }
                        WaitOutcome::Reacquire { needs_reregister } => {
                            return Ok(SubmitOutcome::Reacquire { needs_reregister });
                        }
                        WaitOutcome::Shutdown => return Ok(SubmitOutcome::Shutdown),
                    }
                }
            }
        }
    }

    async fn wait_with_heartbeats(
        &mut self,
        duration: Duration,
        next_heartbeat_at: &mut Instant,
        watched_session: Option<SessionId>,
    ) -> Result<WaitOutcome, CommunityShardWorkerError> {
        let mut shutdown = self.shutdown.clone();
        if *shutdown.borrow() {
            return Ok(WaitOutcome::Shutdown);
        }
        let wait_until = Instant::now() + duration;
        loop {
            if *shutdown.borrow() {
                return Ok(WaitOutcome::Shutdown);
            }
            let now = Instant::now();
            if now >= wait_until {
                return Ok(WaitOutcome::Elapsed);
            }
            if now >= *next_heartbeat_at {
                let Some(assignment) = self.active_lease.clone() else {
                    return Ok(WaitOutcome::Reacquire {
                        needs_reregister: false,
                    });
                };
                match self.send_heartbeat_once(&assignment, true).await {
                    HeartbeatOutcome::Continue {
                        cancellations,
                        expires_at_unix_ms,
                    } => {
                        *next_heartbeat_at = self.next_heartbeat_deadline();
                        if let Some(expires) = expires_at_unix_ms {
                            self.telemetry(|status| status.record_shard_heartbeat_active(expires));
                        }
                        let cancelled_watched = watched_session.is_some_and(|sid| {
                            cancellations.iter().any(|notice| {
                                notice.session_id == sid
                                    && notice.lease_id == assignment.lease_id
                                    && notice.lease_epoch == assignment.lease_epoch
                            })
                        });
                        self.apply_cancellations(&cancellations)?;
                        if cancelled_watched {
                            return Ok(WaitOutcome::WatchedSessionCancelled);
                        }
                    }
                    HeartbeatOutcome::Transient => {
                        *next_heartbeat_at = self.next_transient_heartbeat_deadline();
                    }
                    HeartbeatOutcome::Rebalance(next) => {
                        return Ok(WaitOutcome::Rebalance(next));
                    }
                    HeartbeatOutcome::Reacquire { needs_reregister } => {
                        return Ok(WaitOutcome::Reacquire { needs_reregister });
                    }
                    HeartbeatOutcome::Shutdown => return Ok(WaitOutcome::Shutdown),
                    HeartbeatOutcome::Fatal(err) => return Err(err),
                }
                continue;
            }
            let next_hb = *next_heartbeat_at;
            tokio::select! {
                biased;
                changed = shutdown.changed() => {
                    if changed.is_err() || *shutdown.borrow() {
                        return Ok(WaitOutcome::Shutdown);
                    }
                }
                _ = tokio::time::sleep_until(wait_until) => return Ok(WaitOutcome::Elapsed),
                _ = tokio::time::sleep_until(next_hb) => {}
            }
        }
    }

    async fn send_due_heartbeat(
        &mut self,
        assignment: &ShardLeaseAssignment,
        next_heartbeat_at: &mut Instant,
    ) -> Result<StepCycleOutcome, CommunityShardWorkerError> {
        match self.send_heartbeat_once(assignment, true).await {
            HeartbeatOutcome::Continue {
                cancellations,
                expires_at_unix_ms,
            } => {
                *next_heartbeat_at = self.next_heartbeat_deadline();
                if let Some(expires) = expires_at_unix_ms {
                    self.telemetry(|status| status.record_shard_heartbeat_active(expires));
                }
                self.apply_cancellations(&cancellations)?;
                Ok(StepCycleOutcome::Continue)
            }
            HeartbeatOutcome::Transient => {
                *next_heartbeat_at = self.next_transient_heartbeat_deadline();
                Ok(StepCycleOutcome::Continue)
            }
            HeartbeatOutcome::Rebalance(next) => Ok(StepCycleOutcome::Rebalance(next)),
            HeartbeatOutcome::Reacquire { needs_reregister } => {
                Ok(StepCycleOutcome::Reacquire { needs_reregister })
            }
            HeartbeatOutcome::Shutdown => Ok(StepCycleOutcome::Shutdown),
            HeartbeatOutcome::Fatal(err) => Err(err),
        }
    }

    async fn send_heartbeat_once(
        &mut self,
        assignment: &ShardLeaseAssignment,
        ready: bool,
    ) -> HeartbeatOutcome {
        let req = ShardHeartbeatRequest {
            worker_id: self.worker_id,
            lease_id: assignment.lease_id,
            lease_epoch: assignment.lease_epoch,
            model_id: assignment.model_id,
            execution_profile: assignment.execution_profile.clone(),
            loaded_range: assignment.range,
            ready,
        };
        let half_ttl_ms = self
            .ready_ttl_ms
            .map(|ttl| (ttl / 2).max(1))
            .unwrap_or(self.config.request_timeout_ms);
        let timeout_ms = self.active_request_timeout_ms().min(half_ttl_ms).max(1);
        let call = match self
            .post_signed_shard_cancellable::<ShardHeartbeatRequest, ShardHeartbeatResponse>(
                COMMUNITY_SHARD_HEARTBEAT_PATH,
                &req,
                Duration::from_millis(timeout_ms),
            )
            .await
        {
            Ok(call) => call,
            Err(err) => return HeartbeatOutcome::Fatal(err),
        };
        match call {
            CancellableCall::Shutdown => HeartbeatOutcome::Shutdown,
            CancellableCall::Completed(HttpCallResult::Ok(resp)) => {
                if resp.cancellations.len() > MAX_CANCELLATIONS_PER_WORKER_HARD_CAP {
                    return HeartbeatOutcome::Fatal(CommunityShardWorkerError::MalformedResponse {
                        path: COMMUNITY_SHARD_HEARTBEAT_PATH,
                        detail: "heartbeat cancellations exceed hard cap".to_string(),
                    });
                }
                self.summary.heartbeats_sent += 1;
                self.consecutive_heartbeat_transients = 0;
                match resp.directive {
                    HeartbeatDirective::Provisioning { deadline_unix_ms } if !ready => {
                        if deadline_unix_ms == 0 {
                            return HeartbeatOutcome::Fatal(
                                CommunityShardWorkerError::MalformedResponse {
                                    path: COMMUNITY_SHARD_HEARTBEAT_PATH,
                                    detail: "provisioning deadline must be nonzero".to_string(),
                                },
                            );
                        }
                        HeartbeatOutcome::Continue {
                            cancellations: resp.cancellations,
                            expires_at_unix_ms: None,
                        }
                    }
                    HeartbeatDirective::Active { expires_at_unix_ms } if ready => {
                        if expires_at_unix_ms == 0 {
                            return HeartbeatOutcome::Fatal(
                                CommunityShardWorkerError::MalformedResponse {
                                    path: COMMUNITY_SHARD_HEARTBEAT_PATH,
                                    detail: "active expiration timestamp must be nonzero"
                                        .to_string(),
                                },
                            );
                        }
                        HeartbeatOutcome::Continue {
                            cancellations: resp.cancellations,
                            expires_at_unix_ms: Some(expires_at_unix_ms),
                        }
                    }
                    HeartbeatDirective::Rebalance { assignment: next } => {
                        let now_ms = match checked_now_unix_ms() {
                            Ok(now) => now,
                            Err(err) => return HeartbeatOutcome::Fatal(err),
                        };
                        if let Err(err) = validate_lease_assignment(
                            &self.config,
                            &self.model_spec,
                            &next,
                            now_ms,
                            &self.fence,
                        ) {
                            return HeartbeatOutcome::Fatal(err);
                        }
                        HeartbeatOutcome::Rebalance(next)
                    }
                    _ => HeartbeatOutcome::Fatal(CommunityShardWorkerError::MalformedResponse {
                        path: COMMUNITY_SHARD_HEARTBEAT_PATH,
                        detail: "heartbeat directive does not match worker readiness phase"
                            .to_string(),
                    }),
                }
            }
            CancellableCall::Completed(HttpCallResult::HttpError { status, body }) => {
                if is_registration_missing_error(status, &body) {
                    return HeartbeatOutcome::Reacquire {
                        needs_reregister: true,
                    };
                }
                if status == StatusCode::CONFLICT && is_fatal_model_or_profile_conflict(&body) {
                    return HeartbeatOutcome::Fatal(CommunityShardWorkerError::ProtocolRejected {
                        path: COMMUNITY_SHARD_HEARTBEAT_PATH,
                        status: status.as_u16(),
                        detail: body,
                    });
                }
                if matches!(
                    status,
                    StatusCode::BAD_REQUEST | StatusCode::NOT_FOUND | StatusCode::CONFLICT
                ) {
                    return HeartbeatOutcome::Reacquire {
                        needs_reregister: false,
                    };
                }
                HeartbeatOutcome::Fatal(CommunityShardWorkerError::ProtocolRejected {
                    path: COMMUNITY_SHARD_HEARTBEAT_PATH,
                    status: status.as_u16(),
                    detail: body,
                })
            }
            CancellableCall::Completed(HttpCallResult::Transient(_)) => HeartbeatOutcome::Transient,
        }
    }

    fn apply_cancellations(
        &mut self,
        notices: &[SessionCancellationNotice],
    ) -> Result<(), CommunityShardWorkerError> {
        let Some(assignment) = self.active_lease.as_ref() else {
            return Ok(());
        };
        let Some(exec) = self.executor.as_mut() else {
            return Ok(());
        };
        let now_ms = checked_now_unix_ms()?;
        for notice in notices {
            if notice.lease_id != assignment.lease_id
                || notice.lease_epoch != assignment.lease_epoch
                || notice.session_id.boot_nonce != assignment.lease_id.boot_nonce
            {
                continue;
            }
            match exec.apply_cancellation(self.config.coordinator_id, now_ms, notice) {
                Ok(_) => {
                    self.summary.cancellations_applied += 1;
                    if let Some(sink) = self.config.telemetry.as_ref() {
                        sink.status().record_shard_cancellation_applied();
                    }
                }
                Err(
                    ShardExecutorError::ExecutorReleased
                    | ShardExecutorError::BootNonceMismatch
                    | ShardExecutorError::LeaseIdMismatch
                    | ShardExecutorError::LeaseEpochMismatch { .. }
                    | ShardExecutorError::TombstoneCapacityReached,
                ) => {}
                Err(err) => return Err(CommunityShardWorkerError::Executor(err)),
            }
        }
        Ok(())
    }

    fn active_request_timeout_ms(&self) -> u64 {
        self.ready_ttl_ms
            .map(|ttl| {
                let budget = ttl.saturating_sub(self.config.heartbeat_interval_ms).max(1);
                self.config.request_timeout_ms.min(budget)
            })
            .unwrap_or(self.config.request_timeout_ms)
    }

    fn next_heartbeat_deadline(&self) -> Instant {
        Instant::now() + Duration::from_millis(self.config.heartbeat_interval_ms)
    }

    fn next_transient_heartbeat_deadline(&mut self) -> Instant {
        self.consecutive_heartbeat_transients =
            self.consecutive_heartbeat_transients.saturating_add(1);
        let ms = compute_transient_heartbeat_backoff_ms(
            &self.config,
            self.consecutive_heartbeat_transients,
        );
        Instant::now() + Duration::from_millis(ms)
    }

    async fn sleep_until_shutdown(&self, duration: Duration) -> bool {
        let mut shutdown = self.shutdown.clone();
        if *shutdown.borrow() {
            return true;
        }
        let sleep = tokio::time::sleep(duration);
        tokio::pin!(sleep);
        loop {
            tokio::select! {
                _ = &mut sleep => return false,
                changed = shutdown.changed() => {
                    if changed.is_err() || *shutdown.borrow() {
                        return true;
                    }
                }
            }
        }
    }

    async fn post_register_cancellable(
        &self,
        req: &CommunityRegisterRequest,
        timeout: Duration,
    ) -> Result<CancellableCall<CommunityRegisterResponse>, CommunityShardWorkerError> {
        let mut shutdown = self.shutdown.clone();
        if *shutdown.borrow() {
            return Ok(CancellableCall::Shutdown);
        }
        let fut = self.post_register_once(req, timeout);
        tokio::pin!(fut);
        loop {
            tokio::select! {
                res = &mut fut => return Ok(CancellableCall::Completed(res?)),
                changed = shutdown.changed() => {
                    if changed.is_err() || *shutdown.borrow() {
                        return Ok(CancellableCall::Shutdown);
                    }
                }
            }
        }
    }

    async fn post_register_once(
        &self,
        req: &CommunityRegisterRequest,
        timeout: Duration,
    ) -> Result<HttpCallResult<CommunityRegisterResponse>, CommunityShardWorkerError> {
        let signed = sign_community_request(
            COMMUNITY_REGISTER_PATH,
            req.clone(),
            &self.keypair,
            self.config.coordinator_id,
            self.config.transaction_domain,
        )
        .map_err(CommunityShardWorkerError::Signing)?;
        let encoded = serde_json::to_vec(&signed)
            .map_err(|err| CommunityShardWorkerError::Serialization(err.to_string()))?;
        self.post_encoded_json(COMMUNITY_REGISTER_PATH, encoded, timeout)
            .await
    }

    async fn post_signed_shard_cancellable<Req, Resp>(
        &self,
        path: &'static str,
        req: &Req,
        timeout: Duration,
    ) -> Result<CancellableCall<Resp>, CommunityShardWorkerError>
    where
        Req: Clone + Serialize + ShardWorkerRequestIdentity,
        Resp: DeserializeOwned,
    {
        let mut shutdown = self.shutdown.clone();
        if *shutdown.borrow() {
            return Ok(CancellableCall::Shutdown);
        }
        let fut = self.post_signed_shard_once::<Req, Resp>(path, req, timeout);
        tokio::pin!(fut);
        loop {
            tokio::select! {
                res = &mut fut => return Ok(CancellableCall::Completed(res?)),
                changed = shutdown.changed() => {
                    if changed.is_err() || *shutdown.borrow() {
                        return Ok(CancellableCall::Shutdown);
                    }
                }
            }
        }
    }

    async fn post_signed_shard_once<Req, Resp>(
        &self,
        path: &'static str,
        req: &Req,
        timeout: Duration,
    ) -> Result<HttpCallResult<Resp>, CommunityShardWorkerError>
    where
        Req: Clone + Serialize + ShardWorkerRequestIdentity,
        Resp: DeserializeOwned,
    {
        let signed = sign_shard_worker_request(
            path,
            req.clone(),
            &self.keypair,
            self.config.coordinator_id,
            self.config.transaction_domain,
        )
        .map_err(CommunityShardWorkerError::Signing)?;
        let encoded = serde_json::to_vec(&signed)
            .map_err(|err| CommunityShardWorkerError::Serialization(err.to_string()))?;
        self.post_encoded_json(path, encoded, timeout).await
    }

    async fn post_encoded_json<Resp>(
        &self,
        path: &'static str,
        encoded: Vec<u8>,
        timeout: Duration,
    ) -> Result<HttpCallResult<Resp>, CommunityShardWorkerError>
    where
        Resp: DeserializeOwned,
    {
        let deadline = Instant::now() + timeout;
        let url = format!("{}{}", self.base_url, path);
        let send_fut = self
            .http
            .post(url)
            .header(header::CONTENT_TYPE, "application/json")
            .timeout(timeout)
            .body(encoded)
            .send();
        let response = match tokio::time::timeout_at(deadline, send_fut).await {
            Ok(Ok(resp)) => resp,
            Ok(Err(err)) => return Ok(HttpCallResult::Transient(err.to_string())),
            Err(_) => {
                return Ok(HttpCallResult::Transient(
                    "HTTP request timed out waiting for response headers".to_string(),
                ));
            }
        };
        let status = response.status();
        if status.is_redirection() {
            return Err(CommunityShardWorkerError::RedirectRejected {
                path,
                status: status.as_u16(),
            });
        }
        if status == StatusCode::UNAUTHORIZED || status == StatusCode::FORBIDDEN {
            let detail = read_bounded_error_snippet_until(response, deadline).await;
            return Err(CommunityShardWorkerError::AuthenticationOrAuthorization {
                path,
                status: status.as_u16(),
                detail,
            });
        }
        if status == StatusCode::TOO_MANY_REQUESTS || status.is_server_error() {
            let detail = read_bounded_error_snippet_until(response, deadline).await;
            return Ok(HttpCallResult::Transient(format!(
                "HTTP {}: {}",
                status.as_u16(),
                detail
            )));
        }
        if !status.is_success() {
            let body = read_bounded_error_snippet_until(response, deadline).await;
            return Ok(HttpCallResult::HttpError { status, body });
        }
        let body_bytes = match tokio::time::timeout_at(
            deadline,
            read_bounded_success_body(response, path, MAX_RESPONSE_BODY_BYTES),
        )
        .await
        {
            Ok(Ok(BodyReadOutcome::Bytes(bytes))) => bytes,
            Ok(Ok(BodyReadOutcome::TransportError(detail))) => {
                return Ok(HttpCallResult::Transient(detail));
            }
            Ok(Err(err)) => return Err(err),
            Err(_) => {
                return Ok(HttpCallResult::Transient(
                    "HTTP response body timed out".to_string(),
                ));
            }
        };
        let parsed = serde_json::from_slice::<Resp>(&body_bytes).map_err(|err| {
            CommunityShardWorkerError::MalformedResponse {
                path,
                detail: truncate_utf8_bytes(&err.to_string(), MAX_REASON_BYTES),
            }
        })?;
        Ok(HttpCallResult::Ok(parsed))
    }
}

fn spawn_blocking_with_execution_permit<F, T>(
    execution_permit: Option<WorkerExecutionPermit>,
    work: F,
) -> tokio::task::JoinHandle<T>
where
    F: FnOnce() -> T + Send + 'static,
    T: Send + 'static,
{
    tokio::task::spawn_blocking(move || {
        let _worker_execution_permit = execution_permit;
        work()
    })
}

enum BodyReadOutcome {
    Bytes(Vec<u8>),
    TransportError(String),
}

enum ExecutorFaultAction {
    IgnoreStale,
    Reacquire,
    Report(ShardStepOutcome),
}

fn classify_executor_error(
    err: ShardExecutorError,
) -> Result<ExecutorFaultAction, CommunityShardWorkerError> {
    match err {
        ShardExecutorError::StepDeadlineExpired | ShardExecutorError::SessionTombstoned => {
            Ok(ExecutorFaultAction::IgnoreStale)
        }
        ShardExecutorError::BootNonceMismatch
        | ShardExecutorError::LeaseIdMismatch
        | ShardExecutorError::LeaseEpochMismatch { .. }
        | ShardExecutorError::StaleLease
        | ShardExecutorError::ExecutorReleased => Ok(ExecutorFaultAction::Reacquire),
        ShardExecutorError::SessionCapacityReached
        | ShardExecutorError::KvCacheBudgetExceeded { .. }
        | ShardExecutorError::KvCacheAllocationFailed
        | ShardExecutorError::TombstoneCapacityReached => {
            Ok(ExecutorFaultAction::Report(ShardStepOutcome::Declined {
                reason: truncate_utf8_bytes(&err.to_string(), MAX_REASON_BYTES),
            }))
        }
        ShardExecutorError::ForwardFailed(_)
        | ShardExecutorError::InputKindMismatch { .. }
        | ShardExecutorError::TokenOutOfRange { .. }
        | ShardExecutorError::InvalidHiddenLength { .. }
        | ShardExecutorError::HiddenValueOutOfBounds
        | ShardExecutorError::HiddenHashMismatch
        | ShardExecutorError::ZeroDigest
        | ShardExecutorError::HistoryTooLong { .. }
        | ShardExecutorError::PositionOutOfRange { .. }
        | ShardExecutorError::StepDeadlineExceedsCap { .. }
        | ShardExecutorError::ConflictingStepRetry
        | ShardExecutorError::DuplicatePositionStepMismatch { .. }
        | ShardExecutorError::UnexpectedPosition { .. }
        | ShardExecutorError::NonMonotonicStepId { .. } => {
            Ok(ExecutorFaultAction::Report(ShardStepOutcome::Failed {
                reason: truncate_utf8_bytes(&err.to_string(), MAX_REASON_BYTES),
            }))
        }
        fatal => Err(CommunityShardWorkerError::Executor(fatal)),
    }
}

async fn read_bounded_success_body(
    mut response: reqwest::Response,
    path: &'static str,
    max_bytes: usize,
) -> Result<BodyReadOutcome, CommunityShardWorkerError> {
    if let Some(len) = response.content_length()
        && len > max_bytes as u64
    {
        return Err(CommunityShardWorkerError::ResponseTooLarge { path, max_bytes });
    }
    let mut buf = Vec::new();
    loop {
        match response.chunk().await {
            Ok(Some(chunk)) => {
                let next_len = buf
                    .len()
                    .checked_add(chunk.len())
                    .ok_or(CommunityShardWorkerError::ResponseTooLarge { path, max_bytes })?;
                if next_len > max_bytes {
                    return Err(CommunityShardWorkerError::ResponseTooLarge { path, max_bytes });
                }
                buf.extend_from_slice(&chunk);
            }
            Ok(None) => return Ok(BodyReadOutcome::Bytes(buf)),
            Err(err) => return Ok(BodyReadOutcome::TransportError(err.to_string())),
        }
    }
}

async fn read_bounded_error_snippet_until(
    mut response: reqwest::Response,
    deadline: Instant,
) -> String {
    let mut buf = Vec::new();
    while buf.len() < MAX_ERROR_BODY_BYTES {
        match tokio::time::timeout_at(deadline, response.chunk()).await {
            Ok(Ok(Some(chunk))) => {
                let remaining = MAX_ERROR_BODY_BYTES.saturating_sub(buf.len());
                let take = remaining.min(chunk.len());
                buf.extend_from_slice(&chunk[..take]);
            }
            Ok(Ok(None)) | Ok(Err(_)) | Err(_) => break,
        }
    }
    let lossy = String::from_utf8_lossy(&buf);
    truncate_utf8_bytes(lossy.trim(), MAX_ERROR_BODY_BYTES)
}

fn validate_model_spec(spec: &TrustedModelSpec) -> Result<(), CommunityShardWorkerError> {
    if spec.model_id == Hash256::ZERO {
        return Err(CommunityShardWorkerError::InvalidModelSpec(
            "model_id must be nonzero".to_string(),
        ));
    }
    if spec.execution_profile != CANONICAL_REWARD_INFERENCE_PROFILE {
        return Err(CommunityShardWorkerError::InvalidModelSpec(
            "execution_profile must match canonical reward inference profile".to_string(),
        ));
    }
    if spec.n_layers < 2 || spec.n_layers > MAX_MODEL_LAYERS_HARD_CAP {
        return Err(CommunityShardWorkerError::InvalidModelSpec(format!(
            "n_layers {} must be in 2..={MAX_MODEL_LAYERS_HARD_CAP}",
            spec.n_layers
        )));
    }
    if spec.d_model == 0 || spec.d_model > MAX_HIDDEN_LEN_HARD_CAP {
        return Err(CommunityShardWorkerError::InvalidModelSpec(format!(
            "d_model {} must be in 1..={MAX_HIDDEN_LEN_HARD_CAP}",
            spec.d_model
        )));
    }
    if spec.vocab_size == 0 {
        return Err(CommunityShardWorkerError::InvalidModelSpec(
            "vocab_size must be nonzero".to_string(),
        ));
    }
    if spec.max_seq == 0 || spec.max_seq > MAX_SEQUENCE_POSITIONS_HARD_CAP {
        return Err(CommunityShardWorkerError::InvalidModelSpec(format!(
            "max_seq {} must be in 1..={MAX_SEQUENCE_POSITIONS_HARD_CAP}",
            spec.max_seq
        )));
    }
    if spec.eos_tokens.len() > MAX_EOS_TOKENS {
        return Err(CommunityShardWorkerError::InvalidModelSpec(format!(
            "eos_tokens length exceeds {MAX_EOS_TOKENS}"
        )));
    }
    Ok(())
}

fn validate_executor_config(cfg: &ShardExecutorConfig) -> Result<(), CommunityShardWorkerError> {
    if cfg.max_active_sessions == 0 || cfg.max_active_sessions > MAX_ACTIVE_SESSIONS_HARD_CAP {
        return Err(CommunityShardWorkerError::InvalidConfig(
            "executor max_active_sessions out of bounds".to_string(),
        ));
    }
    if cfg.max_kv_cache_bytes == 0 || cfg.max_kv_cache_bytes > MAX_KV_CACHE_BYTES_HARD_CAP {
        return Err(CommunityShardWorkerError::InvalidConfig(
            "executor max_kv_cache_bytes out of bounds".to_string(),
        ));
    }
    if cfg.max_step_deadline_window_ms == 0
        || cfg.max_step_deadline_window_ms > MAX_STEP_DEADLINE_WINDOW_MS
    {
        return Err(CommunityShardWorkerError::InvalidConfig(
            "executor max_step_deadline_window_ms out of bounds".to_string(),
        ));
    }
    if cfg.session_ttl_ms == 0 || cfg.session_ttl_ms > MAX_SESSION_TTL_MS {
        return Err(CommunityShardWorkerError::InvalidConfig(
            "executor session_ttl_ms out of bounds".to_string(),
        ));
    }
    if cfg.tombstone_ttl_ms == 0 || cfg.tombstone_ttl_ms > MAX_SESSION_TTL_MS {
        return Err(CommunityShardWorkerError::InvalidConfig(
            "executor tombstone_ttl_ms out of bounds".to_string(),
        ));
    }
    if cfg.max_tombstones < cfg.max_active_sessions || cfg.max_tombstones > MAX_TOMBSTONES_HARD_CAP
    {
        return Err(CommunityShardWorkerError::InvalidConfig(
            "executor max_tombstones out of bounds".to_string(),
        ));
    }
    Ok(())
}

fn normalize_and_validate_coordinator_url(
    raw: &str,
    allow_insecure_for_loopback: bool,
) -> Result<String, CommunityShardWorkerError> {
    let bases = validate_community_rpc_bases(&[raw.to_string()], false)
        .map_err(CommunityShardWorkerError::InvalidConfig)?;
    let canonical = bases.into_iter().next().ok_or_else(|| {
        CommunityShardWorkerError::InvalidConfig("missing normalized coordinator URL".to_string())
    })?;
    let url = Url::parse(&canonical).map_err(|err| {
        CommunityShardWorkerError::InvalidConfig(format!("invalid coordinator URL: {err}"))
    })?;
    if url.cannot_be_a_base()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
        || !matches!(url.path(), "" | "/")
        || url.port() == Some(0)
    {
        return Err(CommunityShardWorkerError::InvalidConfig(
            "coordinator URL must be a bare origin without credentials, path, query, or fragment"
                .to_string(),
        ));
    }
    match url.scheme() {
        "https" => {}
        "http" => {
            if !allow_insecure_for_loopback {
                return Err(CommunityShardWorkerError::InvalidConfig(
                    "plaintext http requires explicit allow_insecure_for_loopback".to_string(),
                ));
            }
            let host = url.host_str().ok_or_else(|| {
                CommunityShardWorkerError::InvalidConfig("coordinator URL missing host".to_string())
            })?;
            let literal = host
                .strip_prefix('[')
                .and_then(|stripped| stripped.strip_suffix(']'))
                .unwrap_or(host);
            let is_literal_loopback = literal
                .parse::<IpAddr>()
                .ok()
                .is_some_and(|ip| ip.is_loopback());
            if !is_literal_loopback {
                return Err(CommunityShardWorkerError::InvalidConfig(
                    "plaintext http is permitted only for explicit literal loopback IP addresses"
                        .to_string(),
                ));
            }
        }
        scheme => {
            return Err(CommunityShardWorkerError::InvalidConfig(format!(
                "unsupported coordinator URL scheme {scheme:?}"
            )));
        }
    }
    Ok(canonical.trim_end_matches('/').to_string())
}

fn validate_lease_response_envelope(
    config: &CommunityShardWorkerConfig,
    response: &ShardLeaseResponse,
) -> Result<(), CommunityShardWorkerError> {
    if response.protocol_version != COMMUNITY_SHARD_PROTOCOL_VERSION {
        return Err(CommunityShardWorkerError::InvalidLease(format!(
            "unsupported protocol version {}, expected {COMMUNITY_SHARD_PROTOCOL_VERSION}",
            response.protocol_version
        )));
    }
    let min_cadence_ttl_ms = config.heartbeat_interval_ms.checked_mul(2).ok_or_else(|| {
        CommunityShardWorkerError::InvalidConfig("heartbeat interval overflow".to_string())
    })?;
    let min_request_ttl_ms = config
        .heartbeat_interval_ms
        .checked_add(config.request_timeout_ms)
        .ok_or_else(|| {
            CommunityShardWorkerError::InvalidConfig("request timeout overflow".to_string())
        })?;
    let min_connect_ttl_ms = config
        .heartbeat_interval_ms
        .checked_add(config.connect_timeout_ms)
        .ok_or_else(|| {
            CommunityShardWorkerError::InvalidConfig("connect timeout overflow".to_string())
        })?;
    let min_ready_ttl_ms = min_cadence_ttl_ms
        .max(min_request_ttl_ms)
        .max(min_connect_ttl_ms);
    if response.ready_ttl_ms == 0 || response.ready_ttl_ms < min_ready_ttl_ms {
        return Err(CommunityShardWorkerError::InvalidLease(format!(
            "ready_ttl_ms {} is too short for heartbeat {} and timeouts ({}/{})",
            response.ready_ttl_ms,
            config.heartbeat_interval_ms,
            config.connect_timeout_ms,
            config.request_timeout_ms
        )));
    }
    Ok(())
}

#[cfg(test)]
fn validate_lease_response(
    config: &CommunityShardWorkerConfig,
    model_spec: &TrustedModelSpec,
    response: &ShardLeaseResponse,
    now_ms: u64,
    fence: &LeaseFenceTracker,
) -> Result<(), CommunityShardWorkerError> {
    validate_lease_response_envelope(config, response)?;
    validate_lease_assignment(config, model_spec, &response.assignment, now_ms, fence)
}

fn validate_lease_assignment_shape(
    config: &CommunityShardWorkerConfig,
    model_spec: &TrustedModelSpec,
    assignment: &ShardLeaseAssignment,
) -> Result<(), CommunityShardWorkerError> {
    if assignment.lease_id.boot_nonce == Hash256::ZERO {
        return Err(CommunityShardWorkerError::InvalidLease(
            "boot_nonce must be nonzero".to_string(),
        ));
    }
    if assignment.lease_id.seq == 0 || assignment.lease_epoch == 0 {
        return Err(CommunityShardWorkerError::InvalidLease(
            "lease sequence and epoch must be nonzero".to_string(),
        ));
    }
    if assignment.model_id != model_spec.model_id {
        return Err(CommunityShardWorkerError::InvalidLease(
            "lease model_id mismatches pinned model_spec".to_string(),
        ));
    }
    if assignment.execution_profile != model_spec.execution_profile {
        return Err(CommunityShardWorkerError::InvalidLease(
            "lease execution_profile mismatches pinned model_spec".to_string(),
        ));
    }
    if !assignment.range.is_valid_for(model_spec.n_layers) {
        return Err(CommunityShardWorkerError::InvalidLease(
            "lease range is invalid for model layer count".to_string(),
        ));
    }
    if assignment.range.start_layer == 0 && assignment.range.end_layer == model_spec.n_layers {
        return Err(CommunityShardWorkerError::InvalidLease(
            "full-model layer range is rejected".to_string(),
        ));
    }
    let max_half = model_spec.n_layers.div_ceil(2);
    if assignment.range.len() > max_half {
        return Err(CommunityShardWorkerError::InvalidLease(
            "lease range exceeds half of model layers rounded up".to_string(),
        ));
    }
    if assignment.range.len() > config.max_resident_layers {
        return Err(CommunityShardWorkerError::InvalidLease(format!(
            "lease range length {} exceeds max_resident_layers {}",
            assignment.range.len(),
            config.max_resident_layers
        )));
    }
    Ok(())
}

fn validate_lease_assignment_progression(
    assignment: &ShardLeaseAssignment,
    now_ms: u64,
    fence: &LeaseFenceTracker,
) -> Result<(), CommunityShardWorkerError> {
    if assignment.provisioning_deadline_unix_ms <= now_ms {
        return Err(CommunityShardWorkerError::InvalidLease(
            "lease provisioning deadline is already expired".to_string(),
        ));
    }
    fence.validate_next(assignment)
}

fn validate_lease_assignment(
    config: &CommunityShardWorkerConfig,
    model_spec: &TrustedModelSpec,
    assignment: &ShardLeaseAssignment,
    now_ms: u64,
    fence: &LeaseFenceTracker,
) -> Result<(), CommunityShardWorkerError> {
    validate_lease_assignment_shape(config, model_spec, assignment)?;
    validate_lease_assignment_progression(assignment, now_ms, fence)
}

fn checked_now_unix_ms() -> Result<u64, CommunityShardWorkerError> {
    let millis = chrono::Utc::now().timestamp_millis();
    u64::try_from(millis).map_err(|_| CommunityShardWorkerError::InvalidSystemClock)
}

fn compute_backoff_ms(base_ms: u64, max_ms: u64, attempt: usize) -> u64 {
    let shift = attempt.saturating_sub(1).min(16) as u32;
    base_ms.saturating_mul(1u64 << shift).min(max_ms)
}

fn compute_transient_heartbeat_backoff_ms(
    config: &CommunityShardWorkerConfig,
    consecutive_transients: usize,
) -> u64 {
    let cap_ms = config
        .retry_max_backoff_ms
        .min(config.heartbeat_interval_ms)
        .max(1);
    let base_ms = config.retry_base_backoff_ms.min(cap_ms).max(1);
    compute_backoff_ms(base_ms, cap_ms, consecutive_transients)
}

fn is_auto_reconnectable(error: &CommunityShardWorkerError) -> bool {
    matches!(
        error,
        CommunityShardWorkerError::TransientBudgetExhausted { .. }
            | CommunityShardWorkerError::LeaseReacquireLimitExceeded { .. }
            | CommunityShardWorkerError::ReregisterLimitExceeded { .. }
    )
}

fn is_registration_missing_error(status: StatusCode, body: &str) -> bool {
    status == StatusCode::NOT_FOUND
        && (body.contains(COMMUNITY_REGISTER_PATH)
            || body.contains("not registered")
            || body.contains("registration expired"))
}

fn is_lease_not_found_error(status: StatusCode, body: &str) -> bool {
    status == StatusCode::NOT_FOUND && body.contains("worker lease not found")
}

fn is_fatal_model_or_profile_conflict(body: &str) -> bool {
    body.contains("does not match coordinator model")
        || body.contains("does not match coordinator profile")
        || body.contains("worker model specification mismatch")
        || body.contains("must not register whole-model")
        || body.contains("missing pinned model_id")
        || body.contains("registered model_id")
        || body.contains("missing canonical execution_profile")
        || body.contains("is not the canonical INT8 profile")
}

fn is_submit_terminal_step_stale(status: StatusCode, body: &str) -> bool {
    if status == StatusCode::GONE {
        return true;
    }
    if status == StatusCode::CONFLICT
        && body.contains("submitted step does not match expected session step")
    {
        return true;
    }
    status == StatusCode::NOT_FOUND
        && !is_registration_missing_error(status, body)
        && !body.contains("worker lease not found")
}

fn truncate_utf8_bytes(input: &str, max_bytes: usize) -> String {
    if input.len() <= max_bytes {
        return input.to_string();
    }
    let mut end = max_bytes;
    while end > 0 && !input.is_char_boundary(end) {
        end -= 1;
    }
    input[..end].to_string()
}

fn push_bounded_cancellation(
    queue: &mut VecDeque<SessionCancellationNotice>,
    notice: SessionCancellationNotice,
    active_session: SessionId,
) {
    if queue.iter().any(|existing| {
        existing.session_id == notice.session_id
            && existing.lease_id == notice.lease_id
            && existing.lease_epoch == notice.lease_epoch
    }) {
        return;
    }
    if queue.len() >= MAX_CANCELLATIONS_PER_WORKER_HARD_CAP {
        if notice.session_id != active_session {
            return;
        }
        if let Some(idx) = queue
            .iter()
            .position(|existing| existing.session_id != active_session)
        {
            queue.remove(idx);
        } else {
            return;
        }
    }
    queue.push_back(notice);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::community_sharding::{LeaseId, ShardLayerRange};
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn test_spec(n_layers: usize) -> TrustedModelSpec {
        TrustedModelSpec {
            model_id: arc_crypto::hash_bytes(b"unit-spec"),
            execution_profile: CANONICAL_REWARD_INFERENCE_PROFILE.to_string(),
            n_layers,
            d_model: 16,
            vocab_size: 32,
            max_seq: 64,
            eos_tokens: vec![128_001],
            bos_token: 1,
        }
    }

    fn base_config(url: &str) -> CommunityShardWorkerConfig {
        CommunityShardWorkerConfig::new(
            url,
            arc_crypto::hash_bytes(b"pinned-coordinator"),
            None,
            2,
            "worker-unit",
            "arc-4L-16d",
            "linux-x86_64",
        )
    }

    #[test]
    fn config_rejects_insecure_non_literal_loopback_urls_and_bad_layer_budget() {
        let spec = test_spec(4);
        assert!(
            base_config("https://coord.example.com:8443")
                .validate(&spec)
                .is_ok()
        );

        for bad_url in [
            "http://coord.example.com:8443",
            "http://127.0.0.1:8080",
            "http://localhost:8080",
            "https://user:pass@coord.example.com",
            "https://coord.example.com/?a=1",
            "https://coord.example.com/#frag",
            "https://coord.example.com/subpath",
        ] {
            assert!(
                base_config(bad_url).validate(&spec).is_err(),
                "expected {bad_url} to be rejected"
            );
        }

        let mut loopback_localhost = base_config("http://localhost:8080");
        loopback_localhost.allow_insecure_for_loopback = true;
        assert!(loopback_localhost.validate(&spec).is_err());

        let mut loopback_ip = base_config("http://127.0.0.1:8080/");
        loopback_ip.allow_insecure_for_loopback = true;
        assert_eq!(
            loopback_ip.validate(&spec).unwrap(),
            "http://127.0.0.1:8080"
        );

        let mut over_half = base_config("https://coord.example.com");
        over_half.max_resident_layers = 3;
        assert!(over_half.validate(&spec).is_err());

        let mut zero_layers = base_config("https://coord.example.com");
        zero_layers.max_resident_layers = 0;
        assert!(zero_layers.validate(&spec).is_err());

        let mut zero_coord = base_config("https://coord.example.com");
        zero_coord.coordinator_id = Hash256::ZERO;
        assert!(zero_coord.validate(&spec).is_err());
    }

    #[test]
    fn pre_loader_lease_validation_rejects_full_range_ttl_expiry_and_fenced_boot() {
        let spec = test_spec(4);
        let cfg = base_config("https://coord.example.com");
        let now_ms = 1_700_000_000_000u64;
        let boot1 = arc_crypto::hash_bytes(b"boot-1");
        let boot2 = arc_crypto::hash_bytes(b"boot-2");
        let mut fence = LeaseFenceTracker::default();

        let valid = ShardLeaseResponse {
            protocol_version: COMMUNITY_SHARD_PROTOCOL_VERSION,
            assignment: ShardLeaseAssignment {
                lease_id: LeaseId {
                    boot_nonce: boot1,
                    seq: 1,
                },
                lease_epoch: 1,
                model_id: spec.model_id,
                execution_profile: spec.execution_profile.clone(),
                range: ShardLayerRange {
                    start_layer: 0,
                    end_layer: 2,
                },
                provisioning_deadline_unix_ms: now_ms + 30_000,
            },
            ready_ttl_ms: 45_000,
        };
        assert!(validate_lease_response(&cfg, &spec, &valid, now_ms, &fence).is_ok());

        let mut full_range = valid.clone();
        full_range.assignment.range = ShardLayerRange {
            start_layer: 0,
            end_layer: 4,
        };
        assert!(validate_lease_response(&cfg, &spec, &full_range, now_ms, &fence).is_err());

        let mut zero_ttl = valid.clone();
        zero_ttl.ready_ttl_ms = 0;
        assert!(validate_lease_response(&cfg, &spec, &zero_ttl, now_ms, &fence).is_err());

        let mut short_ttl = valid.clone();
        short_ttl.ready_ttl_ms = cfg.heartbeat_interval_ms * 2 - 1;
        assert!(validate_lease_response(&cfg, &spec, &short_ttl, now_ms, &fence).is_err());

        let mut expired = valid.clone();
        expired.assignment.provisioning_deadline_unix_ms = now_ms;
        assert!(validate_lease_response(&cfg, &spec, &expired, now_ms, &fence).is_err());

        let mut bad_proto = valid.clone();
        bad_proto.protocol_version = COMMUNITY_SHARD_PROTOCOL_VERSION + 1;
        assert!(validate_lease_response(&cfg, &spec, &bad_proto, now_ms, &fence).is_err());

        fence.record(&valid.assignment);
        assert!(validate_lease_response(&cfg, &spec, &valid, now_ms, &fence).is_err());

        let mut next_boot = valid.clone();
        next_boot.assignment.lease_id.boot_nonce = boot2;
        assert!(validate_lease_response(&cfg, &spec, &next_boot, now_ms, &fence).is_ok());
        fence.record(&next_boot.assignment);

        let mut old_boot_replay = valid;
        old_boot_replay.assignment.lease_id.seq = 99;
        assert!(validate_lease_response(&cfg, &spec, &old_boot_replay, now_ms, &fence).is_err());
    }

    #[tokio::test]
    async fn elapsed_idle_wait_returns_before_overdue_heartbeat_but_shutdown_wins() {
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let loader: PartialModelLoader =
            Arc::new(|_| anyhow::bail!("idle wait must not load weights"));
        let mut runtime = WorkerRuntime::new(
            base_config("https://coord.example.com"),
            Arc::new(KeyPair::generate_ed25519()),
            test_spec(4),
            loader,
            shutdown_rx,
        )
        .unwrap();
        let mut heartbeat_deadline = Instant::now() - Duration::from_secs(1);
        assert!(matches!(
            runtime
                .wait_with_heartbeats(Duration::ZERO, &mut heartbeat_deadline, None)
                .await
                .unwrap(),
            WaitOutcome::Elapsed
        ));
        shutdown_tx.send(true).unwrap();
        assert!(matches!(
            runtime
                .wait_with_heartbeats(Duration::ZERO, &mut heartbeat_deadline, None)
                .await
                .unwrap(),
            WaitOutcome::Shutdown
        ));
    }

    #[tokio::test]
    async fn runtime_rejects_invalid_config_and_non_ed25519_before_loader() {
        let spec = test_spec(4);
        let calls = Arc::new(AtomicUsize::new(0));
        let calls_for_loader = Arc::clone(&calls);
        let loader: PartialModelLoader = Arc::new(move |_| {
            calls_for_loader.fetch_add(1, Ordering::SeqCst);
            anyhow::bail!("loader must not be invoked")
        });
        let (_tx, rx) = watch::channel(false);

        let err = run_community_shard_worker(
            base_config("http://127.0.0.1:8080"),
            Arc::new(KeyPair::generate_ed25519()),
            spec.clone(),
            Arc::clone(&loader),
            rx.clone(),
        )
        .await
        .unwrap_err();
        assert!(matches!(err, CommunityShardWorkerError::InvalidConfig(_)));

        let err = run_community_shard_worker(
            base_config("https://coord.example.com"),
            Arc::new(KeyPair::generate_secp256k1()),
            spec,
            loader,
            rx,
        )
        .await
        .unwrap_err();
        assert!(matches!(err, CommunityShardWorkerError::InvalidKeyPair(_)));
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn config_rejects_extreme_timings_and_reacquires_and_accepts_short_timings() {
        let spec = test_spec(4);
        let base = base_config("https://coord.example.com");
        assert!(base.validate(&spec).is_ok());

        let mut short = base.clone();
        short.heartbeat_interval_ms = MIN_HEARTBEAT_INTERVAL_MS;
        short.idle_poll_interval_ms = 1;
        short.connect_timeout_ms = 50;
        short.request_timeout_ms = 100;
        short.release_timeout_ms = 100;
        short.retry_base_backoff_ms = 10;
        short.retry_max_backoff_ms = 50;
        short.reacquire_base_backoff_ms = 10;
        short.reacquire_max_backoff_ms = 50;
        short.max_consecutive_lease_reacquires = 1;
        assert!(short.validate(&spec).is_ok());

        let bad_cases: [fn(&mut CommunityShardWorkerConfig); 13] = [
            |c| c.heartbeat_interval_ms = u64::MAX,
            |c| c.idle_poll_interval_ms = u64::MAX,
            |c| c.connect_timeout_ms = u64::MAX,
            |c| c.request_timeout_ms = u64::MAX,
            |c| c.release_timeout_ms = u64::MAX,
            |c| c.retry_max_backoff_ms = u64::MAX,
            |c| c.reacquire_max_backoff_ms = u64::MAX,
            |c| c.reconnect_max_backoff_ms = u64::MAX,
            |c| c.reconnect_base_backoff_ms = 0,
            |c| {
                c.reconnect_base_backoff_ms = 10_000;
                c.reconnect_max_backoff_ms = 5_000;
            },
            |c| c.max_consecutive_lease_reacquires = usize::MAX,
            |c| c.max_consecutive_lease_reacquires = MAX_CONSECUTIVE_LEASE_REACQUIRES_HARD_CAP + 1,
            |c| {
                c.connect_timeout_ms = 5_000;
                c.request_timeout_ms = 4_000;
            },
        ];
        for mutate in bad_cases {
            let mut cfg = base.clone();
            mutate(&mut cfg);
            assert!(cfg.validate(&spec).is_err());
        }
    }

    #[test]
    fn lease_ttl_validation_enforces_request_and_connect_timeout_headroom() {
        let spec = test_spec(4);
        let now_ms = 1_700_000_000_000u64;
        let fence = LeaseFenceTracker::default();
        let lease = ShardLeaseAssignment {
            lease_id: LeaseId {
                boot_nonce: arc_crypto::hash_bytes(b"ttl-headroom-boot"),
                seq: 1,
            },
            lease_epoch: 1,
            model_id: spec.model_id,
            execution_profile: spec.execution_profile.clone(),
            range: ShardLayerRange {
                start_layer: 0,
                end_layer: 2,
            },
            provisioning_deadline_unix_ms: now_ms + 30_000,
        };
        let make_resp = |ready_ttl_ms| ShardLeaseResponse {
            protocol_version: COMMUNITY_SHARD_PROTOCOL_VERSION,
            assignment: lease.clone(),
            ready_ttl_ms,
        };

        let defaults = base_config("https://coord.example.com");
        assert!(
            validate_lease_response(&defaults, &spec, &make_resp(24_999), now_ms, &fence).is_err()
        );
        assert!(
            validate_lease_response(&defaults, &spec, &make_resp(25_000), now_ms, &fence).is_ok()
        );
        assert!(
            validate_lease_response(&defaults, &spec, &make_resp(45_000), now_ms, &fence).is_ok()
        );

        let mut short = defaults;
        short.heartbeat_interval_ms = 50;
        short.connect_timeout_ms = 100;
        short.request_timeout_ms = 200;
        assert!(validate_lease_response(&short, &spec, &make_resp(249), now_ms, &fence).is_err());
        assert!(validate_lease_response(&short, &spec, &make_resp(250), now_ms, &fence).is_ok());
    }

    fn test_runtime_lease(spec: &TrustedModelSpec) -> ShardLeaseAssignment {
        ShardLeaseAssignment {
            lease_id: LeaseId {
                boot_nonce: arc_crypto::hash_bytes(b"worker-unit-hb-boot"),
                seq: 1,
            },
            lease_epoch: 1,
            model_id: spec.model_id,
            execution_profile: spec.execution_profile.clone(),
            range: ShardLayerRange {
                start_layer: 0,
                end_layer: 2,
            },
            provisioning_deadline_unix_ms: checked_now_unix_ms().expect("unix ms") + 60_000,
        }
    }

    async fn spawn_hb_router(app: axum::Router) -> (String, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind loopback");
        let addr = listener.local_addr().expect("local addr");
        let handle = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        (format!("http://{addr}"), handle)
    }

    fn phase_matched_hb_response(
        ready: bool,
        cancellations: Vec<SessionCancellationNotice>,
    ) -> ShardHeartbeatResponse {
        let now_ms = checked_now_unix_ms().expect("unix ms");
        let directive = if ready {
            HeartbeatDirective::Active {
                expires_at_unix_ms: now_ms + 45_000,
            }
        } else {
            HeartbeatDirective::Provisioning {
                deadline_unix_ms: now_ms + 60_000,
            }
        };
        ShardHeartbeatResponse {
            directive,
            cancellations,
        }
    }

    #[tokio::test]
    async fn transient_heartbeat_backoff_grows_and_resets_after_success() {
        use crate::rpc::CommunitySignedRequest;
        use crate::rpc::community_shard_http::SignedShardWorkerPayload;
        use axum::extract::State;
        use axum::routing::post;

        async fn flaky_hb_handler(
            State(calls): State<Arc<AtomicUsize>>,
            axum::Json(signed): axum::Json<
                CommunitySignedRequest<SignedShardWorkerPayload<ShardHeartbeatRequest>>,
            >,
        ) -> Result<axum::Json<ShardHeartbeatResponse>, (StatusCode, String)> {
            let idx = calls.fetch_add(1, Ordering::SeqCst);
            if idx == 3 {
                let ready = signed.payload.request.ready;
                return Ok(axum::Json(phase_matched_hb_response(ready, Vec::new())));
            }
            Err((
                StatusCode::SERVICE_UNAVAILABLE,
                "transient heartbeat failure".to_string(),
            ))
        }

        let calls = Arc::new(AtomicUsize::new(0));
        let app = axum::Router::new()
            .route(COMMUNITY_SHARD_HEARTBEAT_PATH, post(flaky_hb_handler))
            .with_state(Arc::clone(&calls));
        let (base_url, server) = spawn_hb_router(app).await;

        let spec = test_spec(4);
        let mut cfg = base_config(&base_url);
        cfg.allow_insecure_for_loopback = true;
        cfg.heartbeat_interval_ms = 200;
        cfg.retry_base_backoff_ms = 20;
        cfg.retry_max_backoff_ms = 120;
        let loader: PartialModelLoader = Arc::new(|_| anyhow::bail!("loader unused"));
        let (_tx, rx) = watch::channel(false);
        let mut runtime = WorkerRuntime::new(
            cfg,
            Arc::new(KeyPair::generate_ed25519()),
            spec.clone(),
            loader,
            rx,
        )
        .expect("runtime");
        let lease = test_runtime_lease(&spec);
        runtime.active_lease = Some(lease.clone());
        runtime.ready_ttl_ms = Some(45_000);

        let mut next_hb = Instant::now();
        for (expected_count, expected_ms) in [(1usize, 20u64), (2, 40), (3, 80)] {
            let out = runtime
                .send_due_heartbeat(&lease, &mut next_hb)
                .await
                .expect("transient heartbeat continues");
            assert!(matches!(out, StepCycleOutcome::Continue));
            assert_eq!(runtime.consecutive_heartbeat_transients, expected_count);
            assert_eq!(
                compute_transient_heartbeat_backoff_ms(&runtime.config, expected_count),
                expected_ms
            );
        }

        let recovered = runtime
            .send_due_heartbeat(&lease, &mut next_hb)
            .await
            .expect("recovered heartbeat succeeds");
        assert!(matches!(recovered, StepCycleOutcome::Continue));
        assert_eq!(runtime.consecutive_heartbeat_transients, 0);
        assert_eq!(runtime.summary.heartbeats_sent, 1);

        let post_reset = runtime
            .send_due_heartbeat(&lease, &mut next_hb)
            .await
            .expect("post-reset transient continues");
        assert!(matches!(post_reset, StepCycleOutcome::Continue));
        assert_eq!(runtime.consecutive_heartbeat_transients, 1);
        assert!(next_hb.saturating_duration_since(Instant::now()) <= Duration::from_millis(20));

        runtime.config.heartbeat_interval_ms = 60;
        assert_eq!(
            compute_transient_heartbeat_backoff_ms(&runtime.config, 4),
            60
        );
        assert_eq!(calls.load(Ordering::SeqCst), 5);
        server.abort();
    }

    #[tokio::test]
    async fn auto_reconnect_stops_during_backoff_without_extra_http_and_fails_closed_on_auth() {
        use axum::extract::State;
        use axum::routing::post;
        use tokio::sync::Notify;

        struct RegisterCounter {
            calls: AtomicUsize,
            status: StatusCode,
            notify: Notify,
        }

        async fn reg_handler(
            State(state): State<Arc<RegisterCounter>>,
            _body: axum::Json<serde_json::Value>,
        ) -> (StatusCode, String) {
            state.calls.fetch_add(1, Ordering::SeqCst);
            state.notify.notify_waiters();
            (state.status, "rejected".to_string())
        }

        let spec = test_spec(4);
        let loader: PartialModelLoader = Arc::new(|_| anyhow::bail!("loader unused"));

        let outage_state = Arc::new(RegisterCounter {
            calls: AtomicUsize::new(0),
            status: StatusCode::SERVICE_UNAVAILABLE,
            notify: Notify::new(),
        });
        let (outage_url, outage_srv) = spawn_hb_router(
            axum::Router::new()
                .route(COMMUNITY_REGISTER_PATH, post(reg_handler))
                .with_state(Arc::clone(&outage_state)),
        )
        .await;
        let mut outage_cfg = base_config(&outage_url);
        outage_cfg.allow_insecure_for_loopback = true;
        outage_cfg.auto_reconnect = true;
        outage_cfg.retry_base_backoff_ms = 5;
        outage_cfg.retry_max_backoff_ms = 5;
        outage_cfg.reconnect_base_backoff_ms = 5_000;
        outage_cfg.reconnect_max_backoff_ms = 5_000;
        let (shut_tx, shut_rx) = watch::channel(false);
        let mut runtime = WorkerRuntime::new(
            outage_cfg,
            Arc::new(KeyPair::generate_ed25519()),
            spec.clone(),
            Arc::clone(&loader),
            shut_rx,
        )
        .unwrap();
        let error = runtime.run().await.unwrap_err();
        assert!(is_auto_reconnectable(&error));
        assert_eq!(
            outage_state.calls.load(Ordering::SeqCst),
            MAX_REGISTER_TRANSIENT_ATTEMPTS
        );
        let mut reconnect = Box::pin(runtime.reconnect_after_transient(&error));
        std::future::poll_fn(|cx| {
            assert!(std::future::Future::poll(reconnect.as_mut(), cx).is_pending());
            std::task::Poll::Ready(())
        })
        .await;
        let stop_started = Instant::now();
        shut_tx.send(true).unwrap();
        assert!(
            !tokio::time::timeout(Duration::from_secs(1), reconnect)
                .await
                .unwrap()
        );
        runtime.cleanup_on_exit().await;
        outage_srv.abort();
        assert!(stop_started.elapsed() < Duration::from_secs(1));
        assert_eq!(
            outage_state.calls.load(Ordering::SeqCst),
            MAX_REGISTER_TRANSIENT_ATTEMPTS
        );
        assert_eq!(runtime.summary.reconnect_attempts, 1);

        let auth_state = Arc::new(RegisterCounter {
            calls: AtomicUsize::new(0),
            status: StatusCode::FORBIDDEN,
            notify: Notify::new(),
        });
        let (auth_url, auth_srv) = spawn_hb_router(
            axum::Router::new()
                .route(COMMUNITY_REGISTER_PATH, post(reg_handler))
                .with_state(Arc::clone(&auth_state)),
        )
        .await;
        let mut auth_cfg = base_config(&auth_url);
        auth_cfg.allow_insecure_for_loopback = true;
        auth_cfg.auto_reconnect = true;
        let (_tx, rx) = watch::channel(false);
        let err = run_community_shard_worker(
            auth_cfg,
            Arc::new(KeyPair::generate_ed25519()),
            spec,
            loader,
            rx,
        )
        .await
        .unwrap_err();
        auth_srv.abort();
        assert!(matches!(
            err,
            CommunityShardWorkerError::AuthenticationOrAuthorization { status: 403, .. }
        ));
        assert_eq!(auth_state.calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn auto_reconnect_drops_weights_and_retains_boot_fence_across_outage() {
        use crate::community_sharding::tests::{
            build_deterministic_integer_model, build_partial_integer_model, model_spec_from,
        };
        use crate::rpc::CommunitySignedRequest;
        use crate::rpc::community_shard_http::SignedShardWorkerPayload;
        use axum::extract::State;
        use axum::routing::post;
        use parking_lot::Mutex;
        use std::sync::Weak;
        use std::sync::atomic::AtomicBool;

        struct FenceServerState {
            spec: TrustedModelSpec,
            boot1: Hash256,
            boot2: Hash256,
            lease_calls: AtomicUsize,
            weights_dropped_on_reconnect: AtomicBool,
            prior_weaks: Arc<Mutex<Vec<Weak<CachedIntegerModel>>>>,
        }

        async fn reg_ok(body: axum::Json<serde_json::Value>) -> axum::Json<serde_json::Value> {
            let worker_id = body.0["payload"]["worker_id"]
                .as_str()
                .unwrap_or_default()
                .to_string();
            axum::Json(serde_json::json!({ "ok": true, "worker_id": worker_id }))
        }

        async fn lease_handler(
            State(state): State<Arc<FenceServerState>>,
            _body: axum::Json<serde_json::Value>,
        ) -> Result<axum::Json<ShardLeaseResponse>, (StatusCode, String)> {
            let idx = state.lease_calls.fetch_add(1, Ordering::SeqCst);
            if (2..8).contains(&idx) {
                return Err((StatusCode::SERVICE_UNAVAILABLE, "outage".to_string()));
            }
            if idx == 8 {
                let all_dropped = state
                    .prior_weaks
                    .lock()
                    .iter()
                    .all(|weak| weak.upgrade().is_none());
                state
                    .weights_dropped_on_reconnect
                    .store(all_dropped, Ordering::SeqCst);
            }
            let (boot_nonce, seq) = match idx {
                0 => (state.boot1, 1),
                1 => (state.boot2, 1),
                _ => (state.boot1, 99),
            };
            Ok(axum::Json(ShardLeaseResponse {
                protocol_version: COMMUNITY_SHARD_PROTOCOL_VERSION,
                assignment: ShardLeaseAssignment {
                    lease_id: LeaseId { boot_nonce, seq },
                    lease_epoch: 1,
                    model_id: state.spec.model_id,
                    execution_profile: state.spec.execution_profile.clone(),
                    range: ShardLayerRange {
                        start_layer: 0,
                        end_layer: 2,
                    },
                    provisioning_deadline_unix_ms: checked_now_unix_ms().unwrap() + 60_000,
                },
                ready_ttl_ms: 45_000,
            }))
        }

        async fn hb_ok(
            axum::Json(signed): axum::Json<
                CommunitySignedRequest<SignedShardWorkerPayload<ShardHeartbeatRequest>>,
            >,
        ) -> axum::Json<ShardHeartbeatResponse> {
            let ready = signed.payload.request.ready;
            axum::Json(phase_matched_hb_response(ready, Vec::new()))
        }

        async fn claim_boot_mismatch(_body: axum::Json<serde_json::Value>) -> (StatusCode, String) {
            (
                StatusCode::CONFLICT,
                "identifier boot_nonce does not match coordinator".to_string(),
            )
        }

        async fn release_ok(
            _body: axum::Json<serde_json::Value>,
        ) -> axum::Json<ShardReleaseResponse> {
            axum::Json(ShardReleaseResponse {
                released: true,
                aborted_sessions: Vec::new(),
            })
        }

        let full = Arc::new(build_deterministic_integer_model(
            32,
            16,
            2,
            32,
            4,
            64,
            vec![128_001],
        ));
        let spec = model_spec_from(arc_crypto::hash_bytes(b"fence-reconnect-spec"), &full);
        let prior_weaks: Arc<Mutex<Vec<Weak<CachedIntegerModel>>>> =
            Arc::new(Mutex::new(Vec::new()));
        let loader_calls = Arc::new(AtomicUsize::new(0));
        let weaks_for_loader = Arc::clone(&prior_weaks);
        let calls_for_loader = Arc::clone(&loader_calls);
        let full_for_loader = Arc::clone(&full);
        let loader: PartialModelLoader = Arc::new(move |assignment| {
            calls_for_loader.fetch_add(1, Ordering::SeqCst);
            let partial = Arc::new(build_partial_integer_model(
                &full_for_loader,
                assignment.range,
            ));
            weaks_for_loader.lock().push(Arc::downgrade(&partial));
            Ok(partial)
        });

        let state = Arc::new(FenceServerState {
            spec: spec.clone(),
            boot1: arc_crypto::hash_bytes(b"fence-boot-1"),
            boot2: arc_crypto::hash_bytes(b"fence-boot-2"),
            lease_calls: AtomicUsize::new(0),
            weights_dropped_on_reconnect: AtomicBool::new(false),
            prior_weaks,
        });
        let app = axum::Router::new()
            .route(COMMUNITY_REGISTER_PATH, post(reg_ok))
            .route(COMMUNITY_SHARD_LEASE_PATH, post(lease_handler))
            .route(COMMUNITY_SHARD_HEARTBEAT_PATH, post(hb_ok))
            .route(COMMUNITY_SHARD_CLAIM_STEP_PATH, post(claim_boot_mismatch))
            .route(COMMUNITY_SHARD_RELEASE_PATH, post(release_ok))
            .with_state(Arc::clone(&state));
        let (base_url, server) = spawn_hb_router(app).await;

        let mut cfg = base_config(&base_url);
        cfg.allow_insecure_for_loopback = true;
        cfg.auto_reconnect = true;
        cfg.heartbeat_interval_ms = 50;
        cfg.retry_base_backoff_ms = 5;
        cfg.retry_max_backoff_ms = 10;
        cfg.reacquire_base_backoff_ms = 10;
        cfg.reacquire_max_backoff_ms = 20;
        cfg.reconnect_base_backoff_ms = 20;
        cfg.reconnect_max_backoff_ms = 40;
        let (_tx, rx) = watch::channel(false);
        let err = run_community_shard_worker(
            cfg,
            Arc::new(KeyPair::generate_ed25519()),
            spec,
            loader,
            rx,
        )
        .await
        .unwrap_err();
        server.abort();

        assert!(matches!(
            err,
            CommunityShardWorkerError::InvalidLease(msg) if msg.contains("fenced")
        ));
        assert!(state.weights_dropped_on_reconnect.load(Ordering::SeqCst));
        assert_eq!(loader_calls.load(Ordering::SeqCst), 2);
        assert_eq!(state.lease_calls.load(Ordering::SeqCst), 9);
    }

    #[tokio::test]
    async fn duplicate_prior_lease_after_dropped_release_triggers_release_and_advances() {
        use crate::community_sharding::tests::{
            build_deterministic_integer_model, build_partial_integer_model, model_spec_from,
        };
        use crate::rpc::CommunitySignedRequest;
        use crate::rpc::community_shard_http::SignedShardWorkerPayload;
        use axum::extract::State;
        use axum::routing::post;
        use parking_lot::Mutex;
        use std::sync::atomic::AtomicBool;
        use tokio::sync::Notify;

        struct DuplicateLeaseServerState {
            lease_1: ShardLeaseAssignment,
            lease_2: ShardLeaseAssignment,
            prior_released: AtomicBool,
            lease_calls: AtomicUsize,
            release_calls: AtomicUsize,
            accepted_releases: Mutex<Vec<(u64, u64)>>,
            ready_seq2: Notify,
        }

        async fn reg_ok(body: axum::Json<serde_json::Value>) -> axum::Json<serde_json::Value> {
            let worker_id = body.0["payload"]["worker_id"]
                .as_str()
                .unwrap_or_default()
                .to_string();
            axum::Json(serde_json::json!({ "ok": true, "worker_id": worker_id }))
        }

        async fn lease_handler(
            State(state): State<Arc<DuplicateLeaseServerState>>,
            _body: axum::Json<serde_json::Value>,
        ) -> axum::Json<ShardLeaseResponse> {
            state.lease_calls.fetch_add(1, Ordering::SeqCst);
            let assignment = if state.prior_released.load(Ordering::SeqCst) {
                state.lease_2.clone()
            } else {
                state.lease_1.clone()
            };
            axum::Json(ShardLeaseResponse {
                protocol_version: COMMUNITY_SHARD_PROTOCOL_VERSION,
                assignment,
                ready_ttl_ms: 45_000,
            })
        }

        async fn hb_handler(
            State(state): State<Arc<DuplicateLeaseServerState>>,
            axum::Json(signed): axum::Json<
                CommunitySignedRequest<SignedShardWorkerPayload<ShardHeartbeatRequest>>,
            >,
        ) -> axum::Json<ShardHeartbeatResponse> {
            let req = signed.payload.request;
            if req.lease_id.seq == 2 && req.ready {
                state.ready_seq2.notify_one();
            }
            axum::Json(phase_matched_hb_response(req.ready, Vec::new()))
        }

        async fn claim_handler(
            axum::Json(signed): axum::Json<
                CommunitySignedRequest<SignedShardWorkerPayload<ShardClaimStepRequest>>,
            >,
        ) -> Result<axum::Json<ShardClaimStepResponse>, (StatusCode, String)> {
            if signed.payload.request.lease_id.seq == 1 {
                return Err((StatusCode::CONFLICT, "stale lease".to_string()));
            }
            Ok(axum::Json(ShardClaimStepResponse::Idle))
        }

        async fn release_handler(
            State(state): State<Arc<DuplicateLeaseServerState>>,
            axum::Json(signed): axum::Json<
                CommunitySignedRequest<SignedShardWorkerPayload<ShardReleaseRequest>>,
            >,
        ) -> Result<axum::Json<ShardReleaseResponse>, (StatusCode, String)> {
            let idx = state.release_calls.fetch_add(1, Ordering::SeqCst);
            if idx < MAX_RELEASE_TRANSIENT_ATTEMPTS {
                return Err((
                    StatusCode::SERVICE_UNAVAILABLE,
                    "dropped release".to_string(),
                ));
            }
            let req = signed.payload.request;
            if req.lease_id.seq == 1 && req.lease_epoch == 1 {
                state.prior_released.store(true, Ordering::SeqCst);
            }
            state
                .accepted_releases
                .lock()
                .push((req.lease_id.seq, req.lease_epoch));
            Ok(axum::Json(ShardReleaseResponse {
                released: true,
                aborted_sessions: Vec::new(),
            }))
        }

        let full = Arc::new(build_deterministic_integer_model(
            32,
            16,
            2,
            32,
            4,
            64,
            vec![128_001],
        ));
        let spec = model_spec_from(arc_crypto::hash_bytes(b"dup-lease-spec"), &full);
        let boot = arc_crypto::hash_bytes(b"dup-lease-boot");
        let deadline = checked_now_unix_ms().unwrap() + 60_000;
        let lease_1 = ShardLeaseAssignment {
            lease_id: LeaseId {
                boot_nonce: boot,
                seq: 1,
            },
            lease_epoch: 1,
            model_id: spec.model_id,
            execution_profile: spec.execution_profile.clone(),
            range: ShardLayerRange {
                start_layer: 0,
                end_layer: 2,
            },
            provisioning_deadline_unix_ms: deadline,
        };
        let lease_2 = ShardLeaseAssignment {
            lease_id: LeaseId {
                boot_nonce: boot,
                seq: 2,
            },
            lease_epoch: 1,
            model_id: spec.model_id,
            execution_profile: spec.execution_profile.clone(),
            range: ShardLayerRange {
                start_layer: 2,
                end_layer: 4,
            },
            provisioning_deadline_unix_ms: deadline + 10_000,
        };

        let loaded_leases: Arc<Mutex<Vec<(u64, u64, ShardLayerRange)>>> =
            Arc::new(Mutex::new(Vec::new()));
        let loaded_for_loader = Arc::clone(&loaded_leases);
        let loader: PartialModelLoader = Arc::new(move |assignment| {
            loaded_for_loader.lock().push((
                assignment.lease_id.seq,
                assignment.lease_epoch,
                assignment.range,
            ));
            let partial = build_partial_integer_model(&full, assignment.range);
            Ok(Arc::new(partial))
        });

        let state = Arc::new(DuplicateLeaseServerState {
            lease_1,
            lease_2,
            prior_released: AtomicBool::new(false),
            lease_calls: AtomicUsize::new(0),
            release_calls: AtomicUsize::new(0),
            accepted_releases: Mutex::new(Vec::new()),
            ready_seq2: Notify::new(),
        });
        let app = axum::Router::new()
            .route(COMMUNITY_REGISTER_PATH, post(reg_ok))
            .route(COMMUNITY_SHARD_LEASE_PATH, post(lease_handler))
            .route(COMMUNITY_SHARD_HEARTBEAT_PATH, post(hb_handler))
            .route(COMMUNITY_SHARD_CLAIM_STEP_PATH, post(claim_handler))
            .route(COMMUNITY_SHARD_RELEASE_PATH, post(release_handler))
            .with_state(Arc::clone(&state));
        let (base_url, server) = spawn_hb_router(app).await;

        let mut cfg = base_config(&base_url);
        cfg.allow_insecure_for_loopback = true;
        cfg.heartbeat_interval_ms = 50;
        cfg.idle_poll_interval_ms = 10;
        cfg.retry_base_backoff_ms = 5;
        cfg.retry_max_backoff_ms = 10;
        cfg.reacquire_base_backoff_ms = 10;
        cfg.reacquire_max_backoff_ms = 20;

        let (shut_tx, shut_rx) = watch::channel(false);
        let worker_task = tokio::spawn(run_community_shard_worker(
            cfg,
            Arc::new(KeyPair::generate_ed25519()),
            spec,
            loader,
            shut_rx,
        ));

        tokio::time::timeout(Duration::from_secs(10), state.ready_seq2.notified())
            .await
            .expect("worker must advance to seq 2 after releasing duplicate prior lease");
        shut_tx.send(true).unwrap();
        let summary = tokio::time::timeout(Duration::from_secs(10), worker_task)
            .await
            .expect("worker shutdown timeout")
            .unwrap()
            .expect("worker must exit cleanly");
        server.abort();

        assert_eq!(
            *loaded_leases.lock(),
            vec![
                (
                    1,
                    1,
                    ShardLayerRange {
                        start_layer: 0,
                        end_layer: 2,
                    },
                ),
                (
                    2,
                    1,
                    ShardLayerRange {
                        start_layer: 2,
                        end_layer: 4,
                    },
                ),
            ]
        );
        assert!(state.lease_calls.load(Ordering::SeqCst) >= 3);
        let accepted = state.accepted_releases.lock().clone();
        assert!(accepted.contains(&(1, 1)));
        assert!(accepted.contains(&(2, 1)));
        assert_eq!(summary.leases_acquired, 2);
        assert_eq!(summary.models_loaded, 2);
        assert!(summary.clean_release_sent);
    }

    #[tokio::test]
    async fn duplicate_lease_mismatched_fields_regressed_seq_or_fenced_boot_fail_closed() {
        use crate::community_sharding::tests::{
            build_deterministic_integer_model, build_partial_integer_model, model_spec_from,
        };
        use crate::rpc::CommunitySignedRequest;
        use crate::rpc::community_shard_http::SignedShardWorkerPayload;
        use axum::extract::State;
        use axum::routing::post;

        struct RejectCaseState {
            first: ShardLeaseAssignment,
            second: ShardLeaseAssignment,
            drop_initial_releases: bool,
            lease_calls: AtomicUsize,
            release_calls: AtomicUsize,
        }

        async fn reg_ok(body: axum::Json<serde_json::Value>) -> axum::Json<serde_json::Value> {
            let worker_id = body.0["payload"]["worker_id"]
                .as_str()
                .unwrap_or_default()
                .to_string();
            axum::Json(serde_json::json!({ "ok": true, "worker_id": worker_id }))
        }

        async fn lease_handler(
            State(state): State<Arc<RejectCaseState>>,
            _body: axum::Json<serde_json::Value>,
        ) -> axum::Json<ShardLeaseResponse> {
            let idx = state.lease_calls.fetch_add(1, Ordering::SeqCst);
            let assignment = if idx == 0 {
                state.first.clone()
            } else {
                state.second.clone()
            };
            axum::Json(ShardLeaseResponse {
                protocol_version: COMMUNITY_SHARD_PROTOCOL_VERSION,
                assignment,
                ready_ttl_ms: 45_000,
            })
        }

        async fn hb_ok(
            axum::Json(signed): axum::Json<
                CommunitySignedRequest<SignedShardWorkerPayload<ShardHeartbeatRequest>>,
            >,
        ) -> axum::Json<ShardHeartbeatResponse> {
            let ready = signed.payload.request.ready;
            axum::Json(phase_matched_hb_response(ready, Vec::new()))
        }

        async fn claim_conflict(_body: axum::Json<serde_json::Value>) -> (StatusCode, String) {
            (StatusCode::CONFLICT, "stale lease".to_string())
        }

        async fn release_handler(
            State(state): State<Arc<RejectCaseState>>,
            _body: axum::Json<serde_json::Value>,
        ) -> Result<axum::Json<ShardReleaseResponse>, (StatusCode, String)> {
            let idx = state.release_calls.fetch_add(1, Ordering::SeqCst);
            if state.drop_initial_releases && idx < MAX_RELEASE_TRANSIENT_ATTEMPTS {
                return Err((
                    StatusCode::SERVICE_UNAVAILABLE,
                    "dropped release".to_string(),
                ));
            }
            Ok(axum::Json(ShardReleaseResponse {
                released: true,
                aborted_sessions: Vec::new(),
            }))
        }

        let full = Arc::new(build_deterministic_integer_model(
            32,
            16,
            2,
            32,
            4,
            64,
            vec![128_001],
        ));
        let spec = model_spec_from(arc_crypto::hash_bytes(b"reject-dup-spec"), &full);
        let boot1 = arc_crypto::hash_bytes(b"reject-dup-boot-1");
        let boot2 = arc_crypto::hash_bytes(b"reject-dup-boot-2");
        let deadline = checked_now_unix_ms().unwrap() + 60_000;
        let base_first = ShardLeaseAssignment {
            lease_id: LeaseId {
                boot_nonce: boot1,
                seq: 2,
            },
            lease_epoch: 1,
            model_id: spec.model_id,
            execution_profile: spec.execution_profile.clone(),
            range: ShardLayerRange {
                start_layer: 0,
                end_layer: 2,
            },
            provisioning_deadline_unix_ms: deadline,
        };

        let mut mutated_range = base_first.clone();
        mutated_range.range = ShardLayerRange {
            start_layer: 2,
            end_layer: 4,
        };
        let mut mutated_deadline = base_first.clone();
        mutated_deadline.provisioning_deadline_unix_ms = deadline + 1_000;
        let mut regressed_seq = base_first.clone();
        regressed_seq.lease_id.seq = 1;

        let cases = [
            (mutated_range, true, MAX_RELEASE_TRANSIENT_ATTEMPTS + 1),
            (mutated_deadline, true, MAX_RELEASE_TRANSIENT_ATTEMPTS + 1),
            (base_first.clone(), false, 1),
            (regressed_seq, true, MAX_RELEASE_TRANSIENT_ATTEMPTS + 1),
        ];

        for (second, drop_initial_releases, expected_release_calls) in cases {
            let loader_calls = Arc::new(AtomicUsize::new(0));
            let loader_calls_inner = Arc::clone(&loader_calls);
            let full_for_loader = Arc::clone(&full);
            let loader: PartialModelLoader = Arc::new(move |assignment| {
                loader_calls_inner.fetch_add(1, Ordering::SeqCst);
                let partial = build_partial_integer_model(&full_for_loader, assignment.range);
                Ok(Arc::new(partial))
            });
            let state = Arc::new(RejectCaseState {
                first: base_first.clone(),
                second,
                drop_initial_releases,
                lease_calls: AtomicUsize::new(0),
                release_calls: AtomicUsize::new(0),
            });
            let app = axum::Router::new()
                .route(COMMUNITY_REGISTER_PATH, post(reg_ok))
                .route(COMMUNITY_SHARD_LEASE_PATH, post(lease_handler))
                .route(COMMUNITY_SHARD_HEARTBEAT_PATH, post(hb_ok))
                .route(COMMUNITY_SHARD_CLAIM_STEP_PATH, post(claim_conflict))
                .route(COMMUNITY_SHARD_RELEASE_PATH, post(release_handler))
                .with_state(Arc::clone(&state));
            let (base_url, server) = spawn_hb_router(app).await;

            let mut cfg = base_config(&base_url);
            cfg.allow_insecure_for_loopback = true;
            cfg.auto_reconnect = true;
            cfg.heartbeat_interval_ms = 50;
            cfg.retry_base_backoff_ms = 5;
            cfg.retry_max_backoff_ms = 10;
            cfg.reacquire_base_backoff_ms = 10;
            cfg.reacquire_max_backoff_ms = 20;

            let (_shut_tx, shut_rx) = watch::channel(false);
            let err = run_community_shard_worker(
                cfg,
                Arc::new(KeyPair::generate_ed25519()),
                spec.clone(),
                loader,
                shut_rx,
            )
            .await
            .unwrap_err();
            server.abort();

            assert!(matches!(err, CommunityShardWorkerError::InvalidLease(_)));
            assert_eq!(loader_calls.load(Ordering::SeqCst), 1);
            assert_eq!(state.lease_calls.load(Ordering::SeqCst), 2);
            assert_eq!(
                state.release_calls.load(Ordering::SeqCst),
                expected_release_calls
            );
        }

        let (_tx, rx) = watch::channel(false);
        let mut cfg = base_config("https://coord.example.com");
        cfg.auto_reconnect = true;
        let loader: PartialModelLoader = Arc::new(|_| anyhow::bail!("loader unused"));
        let mut runtime =
            WorkerRuntime::new(cfg, Arc::new(KeyPair::generate_ed25519()), spec, loader, rx)
                .unwrap();
        let expired_prior = ShardLeaseAssignment {
            provisioning_deadline_unix_ms: 1_000,
            ..base_first.clone()
        };
        runtime.fence.record(&expired_prior);
        runtime.unreleased_prior_lease = Some(expired_prior.clone());
        assert!(runtime.is_unreleased_prior_assignment(&expired_prior));
        assert!(
            validate_lease_assignment_progression(&expired_prior, 2_000, &runtime.fence).is_err()
        );
        let next_boot_assignment = ShardLeaseAssignment {
            lease_id: LeaseId {
                boot_nonce: boot2,
                seq: 1,
            },
            ..base_first
        };
        runtime.fence.record(&next_boot_assignment);
        assert!(!runtime.is_unreleased_prior_assignment(&expired_prior));
        assert!(
            validate_lease_assignment_progression(&expired_prior, 500, &runtime.fence).is_err()
        );
    }

    #[tokio::test]
    async fn auto_reconnect_recovers_reacquire_and_reregister_limits_and_cancels() {
        use crate::community_sharding::tests::{
            build_deterministic_integer_model, build_partial_integer_model, model_spec_from,
        };
        use crate::rpc::CommunitySignedRequest;
        use crate::rpc::community_shard_http::SignedShardWorkerPayload;
        use axum::extract::State;
        use axum::routing::post;
        use tokio::sync::Notify;

        struct ReacquireBudgetState {
            spec: TrustedModelSpec,
            boot: Hash256,
            fail_heartbeats: usize,
            lease_seq: AtomicUsize,
            hb_calls: AtomicUsize,
            ready_notify: Notify,
        }

        async fn reg_ok(body: axum::Json<serde_json::Value>) -> axum::Json<serde_json::Value> {
            let worker_id = body.0["payload"]["worker_id"]
                .as_str()
                .unwrap_or_default()
                .to_string();
            axum::Json(serde_json::json!({ "ok": true, "worker_id": worker_id }))
        }

        async fn lease_inc(
            State(state): State<Arc<ReacquireBudgetState>>,
            _body: axum::Json<serde_json::Value>,
        ) -> axum::Json<ShardLeaseResponse> {
            let seq = (state.lease_seq.fetch_add(1, Ordering::SeqCst) + 1) as u64;
            axum::Json(ShardLeaseResponse {
                protocol_version: COMMUNITY_SHARD_PROTOCOL_VERSION,
                assignment: ShardLeaseAssignment {
                    lease_id: LeaseId {
                        boot_nonce: state.boot,
                        seq,
                    },
                    lease_epoch: 1,
                    model_id: state.spec.model_id,
                    execution_profile: state.spec.execution_profile.clone(),
                    range: ShardLayerRange {
                        start_layer: 0,
                        end_layer: 2,
                    },
                    provisioning_deadline_unix_ms: checked_now_unix_ms().unwrap() + 60_000,
                },
                ready_ttl_ms: 45_000,
            })
        }

        async fn hb_flaky(
            State(state): State<Arc<ReacquireBudgetState>>,
            axum::Json(signed): axum::Json<
                CommunitySignedRequest<SignedShardWorkerPayload<ShardHeartbeatRequest>>,
            >,
        ) -> Result<axum::Json<ShardHeartbeatResponse>, (StatusCode, String)> {
            if !signed.payload.request.ready {
                return Ok(axum::Json(phase_matched_hb_response(false, Vec::new())));
            }
            let idx = state.hb_calls.fetch_add(1, Ordering::SeqCst);
            if idx < state.fail_heartbeats {
                return Err((StatusCode::CONFLICT, "lease not ready".to_string()));
            }
            state.ready_notify.notify_one();
            Ok(axum::Json(phase_matched_hb_response(true, Vec::new())))
        }

        async fn claim_idle(
            _body: axum::Json<serde_json::Value>,
        ) -> axum::Json<ShardClaimStepResponse> {
            axum::Json(ShardClaimStepResponse::Idle)
        }

        async fn release_ok(
            _body: axum::Json<serde_json::Value>,
        ) -> axum::Json<ShardReleaseResponse> {
            axum::Json(ShardReleaseResponse {
                released: true,
                aborted_sessions: Vec::new(),
            })
        }

        let full = Arc::new(build_deterministic_integer_model(
            32,
            16,
            2,
            32,
            4,
            64,
            vec![128_001],
        ));
        let spec = model_spec_from(arc_crypto::hash_bytes(b"outer-budget-spec"), &full);
        let loader: PartialModelLoader = Arc::new(move |assignment| {
            let partial = build_partial_integer_model(&full, assignment.range);
            Ok(Arc::new(partial))
        });

        let standalone_state = Arc::new(ReacquireBudgetState {
            spec: spec.clone(),
            boot: arc_crypto::hash_bytes(b"outer-budget-boot-1"),
            fail_heartbeats: 10,
            lease_seq: AtomicUsize::new(0),
            hb_calls: AtomicUsize::new(0),
            ready_notify: Notify::new(),
        });
        let (standalone_url, standalone_srv) = spawn_hb_router(
            axum::Router::new()
                .route(COMMUNITY_REGISTER_PATH, post(reg_ok))
                .route(COMMUNITY_SHARD_LEASE_PATH, post(lease_inc))
                .route(COMMUNITY_SHARD_HEARTBEAT_PATH, post(hb_flaky))
                .route(COMMUNITY_SHARD_CLAIM_STEP_PATH, post(claim_idle))
                .route(COMMUNITY_SHARD_RELEASE_PATH, post(release_ok))
                .with_state(standalone_state),
        )
        .await;
        let mut standalone_cfg = base_config(&standalone_url);
        standalone_cfg.allow_insecure_for_loopback = true;
        standalone_cfg.auto_reconnect = false;
        standalone_cfg.max_consecutive_lease_reacquires = 1;
        standalone_cfg.reacquire_base_backoff_ms = 5;
        standalone_cfg.reacquire_max_backoff_ms = 10;
        let (_tx, rx) = watch::channel(false);
        let standalone_err = run_community_shard_worker(
            standalone_cfg,
            Arc::new(KeyPair::generate_ed25519()),
            spec.clone(),
            Arc::clone(&loader),
            rx,
        )
        .await
        .unwrap_err();
        standalone_srv.abort();
        assert!(matches!(
            standalone_err,
            CommunityShardWorkerError::LeaseReacquireLimitExceeded { limit: 1 }
        ));

        let recover_state = Arc::new(ReacquireBudgetState {
            spec: spec.clone(),
            boot: arc_crypto::hash_bytes(b"outer-budget-boot-2"),
            fail_heartbeats: 2,
            lease_seq: AtomicUsize::new(0),
            hb_calls: AtomicUsize::new(0),
            ready_notify: Notify::new(),
        });
        let (recover_url, recover_srv) = spawn_hb_router(
            axum::Router::new()
                .route(COMMUNITY_REGISTER_PATH, post(reg_ok))
                .route(COMMUNITY_SHARD_LEASE_PATH, post(lease_inc))
                .route(COMMUNITY_SHARD_HEARTBEAT_PATH, post(hb_flaky))
                .route(COMMUNITY_SHARD_CLAIM_STEP_PATH, post(claim_idle))
                .route(COMMUNITY_SHARD_RELEASE_PATH, post(release_ok))
                .with_state(Arc::clone(&recover_state)),
        )
        .await;
        let mut recover_cfg = base_config(&recover_url);
        recover_cfg.allow_insecure_for_loopback = true;
        recover_cfg.auto_reconnect = true;
        recover_cfg.max_consecutive_lease_reacquires = 1;
        recover_cfg.reacquire_base_backoff_ms = 5;
        recover_cfg.reacquire_max_backoff_ms = 10;
        recover_cfg.reconnect_base_backoff_ms = 10;
        recover_cfg.reconnect_max_backoff_ms = 20;
        let (shut_tx, shut_rx) = watch::channel(false);
        let worker_task = tokio::spawn(run_community_shard_worker(
            recover_cfg,
            Arc::new(KeyPair::generate_ed25519()),
            spec.clone(),
            Arc::clone(&loader),
            shut_rx,
        ));
        let recover_ready = recover_state.ready_notify.notified();
        tokio::time::timeout(Duration::from_secs(10), recover_ready)
            .await
            .expect("auto_reconnect must recover after LeaseReacquireLimitExceeded");
        shut_tx.send(true).unwrap();
        let summary = tokio::time::timeout(Duration::from_secs(10), worker_task)
            .await
            .expect("worker shutdown timeout")
            .unwrap()
            .expect("worker must exit cleanly");
        recover_srv.abort();
        assert_eq!(summary.reconnect_attempts, 1);
        assert_eq!(summary.leases_acquired, 3);
        assert!(summary.clean_release_sent);

        struct ReregisterBudgetState {
            spec: TrustedModelSpec,
            boot: Hash256,
            miss_leases: usize,
            lease_calls: AtomicUsize,
            ready_notify: Notify,
        }

        async fn lease_reregister(
            State(state): State<Arc<ReregisterBudgetState>>,
            _body: axum::Json<serde_json::Value>,
        ) -> Result<axum::Json<ShardLeaseResponse>, (StatusCode, String)> {
            let idx = state.lease_calls.fetch_add(1, Ordering::SeqCst);
            if idx < state.miss_leases {
                return Err((
                    StatusCode::NOT_FOUND,
                    "worker_id not registered - call /community/register first".to_string(),
                ));
            }
            Ok(axum::Json(ShardLeaseResponse {
                protocol_version: COMMUNITY_SHARD_PROTOCOL_VERSION,
                assignment: ShardLeaseAssignment {
                    lease_id: LeaseId {
                        boot_nonce: state.boot,
                        seq: 1,
                    },
                    lease_epoch: 1,
                    model_id: state.spec.model_id,
                    execution_profile: state.spec.execution_profile.clone(),
                    range: ShardLayerRange {
                        start_layer: 0,
                        end_layer: 2,
                    },
                    provisioning_deadline_unix_ms: checked_now_unix_ms().unwrap() + 60_000,
                },
                ready_ttl_ms: 45_000,
            }))
        }

        async fn hb_ready(
            State(state): State<Arc<ReregisterBudgetState>>,
            axum::Json(signed): axum::Json<
                CommunitySignedRequest<SignedShardWorkerPayload<ShardHeartbeatRequest>>,
            >,
        ) -> axum::Json<ShardHeartbeatResponse> {
            let ready = signed.payload.request.ready;
            if ready {
                state.ready_notify.notify_one();
            }
            axum::Json(phase_matched_hb_response(ready, Vec::new()))
        }

        let rereg_state = Arc::new(ReregisterBudgetState {
            spec: spec.clone(),
            boot: arc_crypto::hash_bytes(b"outer-rereg-boot"),
            miss_leases: MAX_CONSECUTIVE_REREGISTERS + 1,
            lease_calls: AtomicUsize::new(0),
            ready_notify: Notify::new(),
        });
        let (rereg_url, rereg_srv) = spawn_hb_router(
            axum::Router::new()
                .route(COMMUNITY_REGISTER_PATH, post(reg_ok))
                .route(COMMUNITY_SHARD_LEASE_PATH, post(lease_reregister))
                .route(COMMUNITY_SHARD_HEARTBEAT_PATH, post(hb_ready))
                .route(COMMUNITY_SHARD_CLAIM_STEP_PATH, post(claim_idle))
                .route(COMMUNITY_SHARD_RELEASE_PATH, post(release_ok))
                .with_state(Arc::clone(&rereg_state)),
        )
        .await;
        let mut rereg_cfg = base_config(&rereg_url);
        rereg_cfg.allow_insecure_for_loopback = true;
        rereg_cfg.auto_reconnect = true;
        rereg_cfg.reacquire_base_backoff_ms = 5;
        rereg_cfg.reacquire_max_backoff_ms = 10;
        rereg_cfg.reconnect_base_backoff_ms = 10;
        rereg_cfg.reconnect_max_backoff_ms = 20;
        let (shut_tx, shut_rx) = watch::channel(false);
        let rereg_task = tokio::spawn(run_community_shard_worker(
            rereg_cfg,
            Arc::new(KeyPair::generate_ed25519()),
            spec.clone(),
            Arc::clone(&loader),
            shut_rx,
        ));
        let rereg_ready = rereg_state.ready_notify.notified();
        tokio::time::timeout(Duration::from_secs(10), rereg_ready)
            .await
            .expect("auto_reconnect must recover after ReregisterLimitExceeded");
        shut_tx.send(true).unwrap();
        let rereg_summary = tokio::time::timeout(Duration::from_secs(10), rereg_task)
            .await
            .expect("reregister worker shutdown timeout")
            .unwrap()
            .expect("reregister worker must exit cleanly");
        rereg_srv.abort();
        assert_eq!(rereg_summary.reconnect_attempts, 1);
        assert_eq!(rereg_summary.leases_acquired, 1);

        let cancel_state = Arc::new(ReacquireBudgetState {
            spec: spec.clone(),
            boot: arc_crypto::hash_bytes(b"outer-cancel-boot"),
            fail_heartbeats: 10,
            lease_seq: AtomicUsize::new(0),
            hb_calls: AtomicUsize::new(0),
            ready_notify: Notify::new(),
        });
        let (cancel_url, cancel_srv) = spawn_hb_router(
            axum::Router::new()
                .route(COMMUNITY_REGISTER_PATH, post(reg_ok))
                .route(COMMUNITY_SHARD_LEASE_PATH, post(lease_inc))
                .route(COMMUNITY_SHARD_HEARTBEAT_PATH, post(hb_flaky))
                .route(COMMUNITY_SHARD_CLAIM_STEP_PATH, post(claim_idle))
                .route(COMMUNITY_SHARD_RELEASE_PATH, post(release_ok))
                .with_state(Arc::clone(&cancel_state)),
        )
        .await;
        let mut cancel_cfg = base_config(&cancel_url);
        cancel_cfg.allow_insecure_for_loopback = true;
        cancel_cfg.auto_reconnect = true;
        cancel_cfg.max_consecutive_lease_reacquires = 1;
        cancel_cfg.reacquire_base_backoff_ms = 5;
        cancel_cfg.reacquire_max_backoff_ms = 10;
        cancel_cfg.reconnect_base_backoff_ms = 5_000;
        cancel_cfg.reconnect_max_backoff_ms = 5_000;
        let (cancel_tx, cancel_rx) = watch::channel(false);
        let mut runtime = WorkerRuntime::new(
            cancel_cfg,
            Arc::new(KeyPair::generate_ed25519()),
            spec,
            loader,
            cancel_rx,
        )
        .unwrap();
        let err = runtime.run().await.unwrap_err();
        assert!(matches!(
            err,
            CommunityShardWorkerError::LeaseReacquireLimitExceeded { limit: 1 }
        ));
        assert!(is_auto_reconnectable(&err));
        let leases_before_cancel = cancel_state.lease_seq.load(Ordering::SeqCst);
        let mut reconnect = Box::pin(runtime.reconnect_after_transient(&err));
        std::future::poll_fn(|cx| {
            assert!(std::future::Future::poll(reconnect.as_mut(), cx).is_pending());
            std::task::Poll::Ready(())
        })
        .await;
        let stop_started = Instant::now();
        cancel_tx.send(true).unwrap();
        assert!(
            !tokio::time::timeout(Duration::from_secs(1), reconnect)
                .await
                .unwrap()
        );
        runtime.cleanup_on_exit().await;
        cancel_srv.abort();
        assert!(stop_started.elapsed() < Duration::from_secs(1));
        assert_eq!(
            cancel_state.lease_seq.load(Ordering::SeqCst),
            leases_before_cancel
        );
    }

    #[tokio::test]
    async fn execution_gate_holds_claim_execute_submit_retries_then_quiesces_with_heartbeats() {
        use crate::community_sharding::tests::{
            build_deterministic_integer_model, build_partial_integer_model, model_spec_from,
        };
        use crate::community_sharding::{ShardStepInput, StepId};
        use crate::native_inference::WorkerQuiescenceResult;
        use crate::rpc::CommunitySignedRequest;
        use crate::rpc::community_shard_http::SignedShardWorkerPayload;
        use axum::extract::State;
        use axum::routing::post;
        use std::sync::atomic::AtomicBool;
        use tokio::sync::Notify;

        struct GateServerState {
            spec: TrustedModelSpec,
            boot: Hash256,
            claim_calls: AtomicUsize,
            submit_calls: AtomicUsize,
            claim1_entered: Notify,
            claim1_release: Notify,
            claim2_entered: Notify,
            claim2_release: Notify,
            submit1_entered: Notify,
            submit1_release: Notify,
            submit2_entered: Notify,
            submit2_release: Notify,
            quiesced_hb_flag: AtomicBool,
            quiesced_hb_count: AtomicUsize,
            quiesced_hb_target_reached: Notify,
            resumed_claim_flag: AtomicBool,
            resumed_claim_seen: Notify,
        }

        async fn reg_ok(body: axum::Json<serde_json::Value>) -> axum::Json<serde_json::Value> {
            let worker_id = body.0["payload"]["worker_id"]
                .as_str()
                .unwrap_or_default()
                .to_string();
            axum::Json(serde_json::json!({ "ok": true, "worker_id": worker_id }))
        }

        async fn lease_ok(
            State(state): State<Arc<GateServerState>>,
            _body: axum::Json<serde_json::Value>,
        ) -> axum::Json<ShardLeaseResponse> {
            axum::Json(ShardLeaseResponse {
                protocol_version: COMMUNITY_SHARD_PROTOCOL_VERSION,
                assignment: ShardLeaseAssignment {
                    lease_id: LeaseId {
                        boot_nonce: state.boot,
                        seq: 1,
                    },
                    lease_epoch: 1,
                    model_id: state.spec.model_id,
                    execution_profile: state.spec.execution_profile.clone(),
                    range: ShardLayerRange {
                        start_layer: 0,
                        end_layer: 2,
                    },
                    provisioning_deadline_unix_ms: checked_now_unix_ms().unwrap() + 60_000,
                },
                ready_ttl_ms: 45_000,
            })
        }

        async fn hb_handler(
            State(state): State<Arc<GateServerState>>,
            axum::Json(signed): axum::Json<
                CommunitySignedRequest<SignedShardWorkerPayload<ShardHeartbeatRequest>>,
            >,
        ) -> axum::Json<ShardHeartbeatResponse> {
            let ready = signed.payload.request.ready;
            if ready
                && state.quiesced_hb_flag.load(Ordering::SeqCst)
                && state.quiesced_hb_count.fetch_add(1, Ordering::SeqCst) + 1 == 2
            {
                state.quiesced_hb_target_reached.notify_one();
            }
            axum::Json(phase_matched_hb_response(ready, Vec::new()))
        }

        async fn claim_handler(
            State(state): State<Arc<GateServerState>>,
            _body: axum::Json<serde_json::Value>,
        ) -> axum::Json<ShardClaimStepResponse> {
            let idx = state.claim_calls.fetch_add(1, Ordering::SeqCst);
            if idx == 0 {
                state.claim1_entered.notify_one();
                state.claim1_release.notified().await;
                return axum::Json(ShardClaimStepResponse::Idle);
            }
            if idx == 1 {
                state.claim2_entered.notify_one();
                state.claim2_release.notified().await;
                return axum::Json(ShardClaimStepResponse::Step {
                    work: Box::new(ShardStepWork {
                        session_id: SessionId {
                            boot_nonce: state.boot,
                            seq: 1,
                        },
                        step_id: StepId {
                            boot_nonce: state.boot,
                            seq: 1,
                        },
                        lease_id: LeaseId {
                            boot_nonce: state.boot,
                            seq: 1,
                        },
                        lease_epoch: 1,
                        model_id: state.spec.model_id,
                        execution_profile: state.spec.execution_profile.clone(),
                        range: ShardLayerRange {
                            start_layer: 0,
                            end_layer: 2,
                        },
                        position: 0,
                        expected_hidden_len: state.spec.d_model,
                        expect_terminal: false,
                        last_token: false,
                        deadline_unix_ms: checked_now_unix_ms().unwrap() + 30_000,
                        input: ShardStepInput::Token {
                            token: state.spec.bos_token,
                        },
                        generated_tokens: Vec::new(),
                    }),
                });
            }
            if state.resumed_claim_flag.swap(false, Ordering::SeqCst) {
                state.resumed_claim_seen.notify_one();
            }
            axum::Json(ShardClaimStepResponse::Idle)
        }

        async fn submit_handler(
            State(state): State<Arc<GateServerState>>,
            _body: axum::Json<serde_json::Value>,
        ) -> Result<axum::Json<ShardStepSubmitResponse>, (StatusCode, String)> {
            let idx = state.submit_calls.fetch_add(1, Ordering::SeqCst);
            if idx == 0 {
                state.submit1_entered.notify_one();
                state.submit1_release.notified().await;
                return Err((
                    StatusCode::SERVICE_UNAVAILABLE,
                    "transient submit outage".to_string(),
                ));
            }
            state.submit2_entered.notify_one();
            state.submit2_release.notified().await;
            Ok(axum::Json(ShardStepSubmitResponse {
                accepted: true,
                session_completed: false,
                session_aborted: false,
            }))
        }

        async fn release_ok(
            _body: axum::Json<serde_json::Value>,
        ) -> axum::Json<ShardReleaseResponse> {
            axum::Json(ShardReleaseResponse {
                released: true,
                aborted_sessions: Vec::new(),
            })
        }

        let full = Arc::new(build_deterministic_integer_model(
            32,
            16,
            2,
            32,
            4,
            64,
            vec![128_001],
        ));
        let spec = model_spec_from(arc_crypto::hash_bytes(b"gate-step-spec"), &full);
        let loader: PartialModelLoader = Arc::new(move |assignment| {
            let partial = build_partial_integer_model(&full, assignment.range);
            Ok(Arc::new(partial))
        });

        let state = Arc::new(GateServerState {
            spec: spec.clone(),
            boot: arc_crypto::hash_bytes(b"gate-step-boot"),
            claim_calls: AtomicUsize::new(0),
            submit_calls: AtomicUsize::new(0),
            claim1_entered: Notify::new(),
            claim1_release: Notify::new(),
            claim2_entered: Notify::new(),
            claim2_release: Notify::new(),
            submit1_entered: Notify::new(),
            submit1_release: Notify::new(),
            submit2_entered: Notify::new(),
            submit2_release: Notify::new(),
            quiesced_hb_flag: AtomicBool::new(false),
            quiesced_hb_count: AtomicUsize::new(0),
            quiesced_hb_target_reached: Notify::new(),
            resumed_claim_flag: AtomicBool::new(false),
            resumed_claim_seen: Notify::new(),
        });
        let app = axum::Router::new()
            .route(COMMUNITY_REGISTER_PATH, post(reg_ok))
            .route(COMMUNITY_SHARD_LEASE_PATH, post(lease_ok))
            .route(COMMUNITY_SHARD_HEARTBEAT_PATH, post(hb_handler))
            .route(COMMUNITY_SHARD_CLAIM_STEP_PATH, post(claim_handler))
            .route(COMMUNITY_SHARD_SUBMIT_STEP_PATH, post(submit_handler))
            .route(COMMUNITY_SHARD_RELEASE_PATH, post(release_ok))
            .with_state(Arc::clone(&state));
        let (base_url, server) = spawn_hb_router(app).await;

        let gate = Arc::new(WorkerExecutionGate::default());
        let mut cfg = base_config(&base_url);
        cfg.allow_insecure_for_loopback = true;
        cfg.heartbeat_interval_ms = 40;
        cfg.idle_poll_interval_ms = 10;
        cfg.retry_base_backoff_ms = 10;
        cfg.retry_max_backoff_ms = 20;
        cfg.worker_execution_gate = Some(Arc::clone(&gate).into());

        let (shut_tx, shut_rx) = watch::channel(false);
        let worker_task = tokio::spawn(run_community_shard_worker(
            cfg,
            Arc::new(KeyPair::generate_ed25519()),
            spec,
            loader,
            shut_rx,
        ));

        let claim1 = state.claim1_entered.notified();
        tokio::time::timeout(Duration::from_secs(10), claim1)
            .await
            .expect("first claim must arrive");
        assert_eq!(gate.active_jobs(), 1);
        let idle_request_id = [0xA4; 32];
        let mut idle_quiesce = Box::pin(gate.quiesce(
            idle_request_id,
            Duration::from_secs(10),
            Instant::now() + Duration::from_secs(30),
        ));
        std::future::poll_fn(|cx| {
            assert!(std::future::Future::poll(idle_quiesce.as_mut(), cx).is_pending());
            std::task::Poll::Ready(())
        })
        .await;
        assert!(gate.is_closed());
        state.claim1_release.notify_one();
        let idle_quiesce_result = tokio::time::timeout(Duration::from_secs(10), idle_quiesce)
            .await
            .expect("idle claim must release execution gate before idle backoff");
        assert_eq!(
            idle_quiesce_result,
            WorkerQuiescenceResult::Quiesced { active_jobs: 0 }
        );
        assert_eq!(gate.resume(idle_request_id), Ok(0));

        let claim2 = state.claim2_entered.notified();
        tokio::time::timeout(Duration::from_secs(10), claim2)
            .await
            .expect("second claim must arrive");
        assert_eq!(gate.active_jobs(), 1);

        let request_id = [0xA5; 32];
        let mut quiesce_fut = Box::pin(gate.quiesce(
            request_id,
            Duration::from_secs(10),
            Instant::now() + Duration::from_secs(30),
        ));
        std::future::poll_fn(|cx| {
            assert!(std::future::Future::poll(quiesce_fut.as_mut(), cx).is_pending());
            std::task::Poll::Ready(())
        })
        .await;
        assert!(gate.is_closed());
        assert_eq!(gate.active_jobs(), 1);

        state.claim2_release.notify_one();
        let submit1 = state.submit1_entered.notified();
        tokio::time::timeout(Duration::from_secs(10), submit1)
            .await
            .expect("first submit attempt must arrive");
        assert_eq!(gate.active_jobs(), 1);
        std::future::poll_fn(|cx| {
            assert!(std::future::Future::poll(quiesce_fut.as_mut(), cx).is_pending());
            std::task::Poll::Ready(())
        })
        .await;

        state.submit1_release.notify_one();
        let submit2 = state.submit2_entered.notified();
        tokio::time::timeout(Duration::from_secs(10), submit2)
            .await
            .expect("submit retry must arrive");
        assert_eq!(gate.active_jobs(), 1);
        std::future::poll_fn(|cx| {
            assert!(std::future::Future::poll(quiesce_fut.as_mut(), cx).is_pending());
            std::task::Poll::Ready(())
        })
        .await;

        state.submit2_release.notify_one();
        let quiesce_result = tokio::time::timeout(Duration::from_secs(10), quiesce_fut)
            .await
            .expect("quiesce must complete after submit retry finishes");
        assert_eq!(
            quiesce_result,
            WorkerQuiescenceResult::Quiesced { active_jobs: 0 }
        );
        assert!(gate.is_closed());
        assert_eq!(gate.active_jobs(), 0);

        let claims_at_quiesce = state.claim_calls.load(Ordering::SeqCst);
        assert_eq!(claims_at_quiesce, 2);
        state.quiesced_hb_flag.store(true, Ordering::SeqCst);
        let hb_reached = state.quiesced_hb_target_reached.notified();
        tokio::time::timeout(Duration::from_secs(10), hb_reached)
            .await
            .expect("lease heartbeats must continue while gate is closed");
        let claims_after_hb = state.claim_calls.load(Ordering::SeqCst);
        assert_eq!(claims_after_hb, claims_at_quiesce);
        assert!(state.quiesced_hb_count.load(Ordering::SeqCst) >= 2);

        state.resumed_claim_flag.store(true, Ordering::SeqCst);
        assert_eq!(gate.resume(request_id), Ok(0));
        let resumed = state.resumed_claim_seen.notified();
        tokio::time::timeout(Duration::from_secs(10), resumed)
            .await
            .expect("claims must resume once gate reopens");
        assert!(state.claim_calls.load(Ordering::SeqCst) > claims_at_quiesce);

        shut_tx.send(true).unwrap();
        let summary = tokio::time::timeout(Duration::from_secs(10), worker_task)
            .await
            .expect("worker shutdown timeout")
            .unwrap()
            .expect("worker must exit cleanly");
        server.abort();

        assert_eq!(summary.steps_executed, 1);
        assert_eq!(summary.steps_submitted, 1);
        assert_eq!(summary.submit_retries, 1);
        assert!(summary.clean_release_sent);
        assert_eq!(gate.active_jobs(), 0);
    }

    #[tokio::test]
    async fn execution_gate_drops_permit_on_rebalance_and_quiesced_shutdown_stops_promptly() {
        use crate::community_sharding::tests::{
            build_deterministic_integer_model, build_partial_integer_model, model_spec_from,
        };
        use crate::native_inference::WorkerQuiescenceResult;
        use crate::rpc::CommunitySignedRequest;
        use crate::rpc::community_shard_http::SignedShardWorkerPayload;
        use axum::extract::State;
        use axum::routing::post;
        use tokio::sync::Notify;

        struct RebalanceGateState {
            spec: TrustedModelSpec,
            boot: Hash256,
            epoch2_claims: AtomicUsize,
            epoch2_ready: Notify,
        }

        async fn reg_ok(body: axum::Json<serde_json::Value>) -> axum::Json<serde_json::Value> {
            let worker_id = body.0["payload"]["worker_id"]
                .as_str()
                .unwrap_or_default()
                .to_string();
            axum::Json(serde_json::json!({ "ok": true, "worker_id": worker_id }))
        }

        async fn lease_ok(
            State(state): State<Arc<RebalanceGateState>>,
            _body: axum::Json<serde_json::Value>,
        ) -> axum::Json<ShardLeaseResponse> {
            axum::Json(ShardLeaseResponse {
                protocol_version: COMMUNITY_SHARD_PROTOCOL_VERSION,
                assignment: ShardLeaseAssignment {
                    lease_id: LeaseId {
                        boot_nonce: state.boot,
                        seq: 1,
                    },
                    lease_epoch: 1,
                    model_id: state.spec.model_id,
                    execution_profile: state.spec.execution_profile.clone(),
                    range: ShardLayerRange {
                        start_layer: 0,
                        end_layer: 2,
                    },
                    provisioning_deadline_unix_ms: checked_now_unix_ms().unwrap() + 60_000,
                },
                ready_ttl_ms: 45_000,
            })
        }

        async fn hb_handler(
            State(state): State<Arc<RebalanceGateState>>,
            axum::Json(signed): axum::Json<
                CommunitySignedRequest<SignedShardWorkerPayload<ShardHeartbeatRequest>>,
            >,
        ) -> axum::Json<ShardHeartbeatResponse> {
            let req = signed.payload.request;
            if req.lease_epoch == 2 && req.ready {
                state.epoch2_ready.notify_one();
            }
            axum::Json(phase_matched_hb_response(req.ready, Vec::new()))
        }

        async fn claim_handler(
            State(state): State<Arc<RebalanceGateState>>,
            axum::Json(signed): axum::Json<
                CommunitySignedRequest<SignedShardWorkerPayload<ShardClaimStepRequest>>,
            >,
        ) -> axum::Json<ShardClaimStepResponse> {
            if signed.payload.request.lease_epoch == 1 {
                return axum::Json(ShardClaimStepResponse::RebalanceRequired {
                    assignment: ShardLeaseAssignment {
                        lease_id: LeaseId {
                            boot_nonce: state.boot,
                            seq: 1,
                        },
                        lease_epoch: 2,
                        model_id: state.spec.model_id,
                        execution_profile: state.spec.execution_profile.clone(),
                        range: ShardLayerRange {
                            start_layer: 2,
                            end_layer: 4,
                        },
                        provisioning_deadline_unix_ms: checked_now_unix_ms().unwrap() + 60_000,
                    },
                });
            }
            state.epoch2_claims.fetch_add(1, Ordering::SeqCst);
            axum::Json(ShardClaimStepResponse::Idle)
        }

        async fn release_ok(
            _body: axum::Json<serde_json::Value>,
        ) -> axum::Json<ShardReleaseResponse> {
            axum::Json(ShardReleaseResponse {
                released: true,
                aborted_sessions: Vec::new(),
            })
        }

        let full = Arc::new(build_deterministic_integer_model(
            32,
            16,
            2,
            32,
            4,
            64,
            vec![128_001],
        ));
        let spec = model_spec_from(arc_crypto::hash_bytes(b"rebalance-gate-spec"), &full);
        let loader_epoch2_entered = Arc::new(Notify::new());
        let (loader_release_tx, loader_release_rx) = std::sync::mpsc::channel::<()>();
        let loader_release_rx = std::sync::Mutex::new(Some(loader_release_rx));
        let loader_entered_inner = Arc::clone(&loader_epoch2_entered);
        let loader: PartialModelLoader = Arc::new(move |assignment| {
            if assignment.lease_epoch == 2 {
                loader_entered_inner.notify_one();
                if let Some(rx) = loader_release_rx.lock().unwrap().take() {
                    rx.recv_timeout(Duration::from_secs(10))
                        .expect("loader release signal");
                }
            }
            let partial = build_partial_integer_model(&full, assignment.range);
            Ok(Arc::new(partial))
        });

        let state = Arc::new(RebalanceGateState {
            spec: spec.clone(),
            boot: arc_crypto::hash_bytes(b"rebalance-gate-boot"),
            epoch2_claims: AtomicUsize::new(0),
            epoch2_ready: Notify::new(),
        });
        let app = axum::Router::new()
            .route(COMMUNITY_REGISTER_PATH, post(reg_ok))
            .route(COMMUNITY_SHARD_LEASE_PATH, post(lease_ok))
            .route(COMMUNITY_SHARD_HEARTBEAT_PATH, post(hb_handler))
            .route(COMMUNITY_SHARD_CLAIM_STEP_PATH, post(claim_handler))
            .route(COMMUNITY_SHARD_RELEASE_PATH, post(release_ok))
            .with_state(Arc::clone(&state));
        let (base_url, server) = spawn_hb_router(app).await;

        let gate = Arc::new(WorkerExecutionGate::default());
        let mut cfg = base_config(&base_url);
        cfg.allow_insecure_for_loopback = true;
        cfg.heartbeat_interval_ms = 50;
        cfg.idle_poll_interval_ms = 10;
        cfg.worker_execution_gate = Some(Arc::clone(&gate).into());

        let (shut_tx, shut_rx) = watch::channel(false);
        let worker_task = tokio::spawn(run_community_shard_worker(
            cfg,
            Arc::new(KeyPair::generate_ed25519()),
            spec,
            loader,
            shut_rx,
        ));

        let loader_entered = loader_epoch2_entered.notified();
        tokio::time::timeout(Duration::from_secs(10), loader_entered)
            .await
            .expect("rebalance must enter loader for epoch 2");
        assert_eq!(gate.active_jobs(), 0);
        let result = gate
            .quiesce(
                [0xA6; 32],
                Duration::from_secs(5),
                Instant::now() + Duration::from_secs(30),
            )
            .await;
        assert_eq!(result, WorkerQuiescenceResult::Quiesced { active_jobs: 0 });
        loader_release_tx.send(()).unwrap();

        let epoch2_ready = state.epoch2_ready.notified();
        tokio::time::timeout(Duration::from_secs(10), epoch2_ready)
            .await
            .expect("rebalanced epoch 2 must report ready");
        gate.commit_shutdown();
        shut_tx.send(true).unwrap();
        let summary = tokio::time::timeout(Duration::from_secs(10), worker_task)
            .await
            .expect("quiesced worker must stop promptly on shutdown")
            .unwrap()
            .expect("worker must exit cleanly");
        server.abort();

        assert_eq!(summary.rebalances_completed, 1);
        assert!(summary.clean_release_sent);
        assert_eq!(state.epoch2_claims.load(Ordering::SeqCst), 0);
        assert_eq!(gate.active_jobs(), 0);
    }

    #[tokio::test]
    async fn duplicate_prior_lease_release_error_classification_and_recovery() {
        use crate::community_sharding::tests::{
            build_deterministic_integer_model, build_partial_integer_model, model_spec_from,
        };
        use crate::rpc::CommunitySignedRequest;
        use crate::rpc::community_shard_http::SignedShardWorkerPayload;
        use axum::extract::State;
        use axum::routing::post;
        use std::sync::atomic::AtomicBool;
        use tokio::sync::Notify;

        struct TerminalReleaseState {
            lease: ShardLeaseAssignment,
            terminal_status: StatusCode,
            terminal_body: &'static str,
            register_calls: AtomicUsize,
            lease_calls: AtomicUsize,
            release_calls: AtomicUsize,
        }

        async fn reg_terminal(
            State(state): State<Arc<TerminalReleaseState>>,
            body: axum::Json<serde_json::Value>,
        ) -> axum::Json<serde_json::Value> {
            state.register_calls.fetch_add(1, Ordering::SeqCst);
            let worker_id = body.0["payload"]["worker_id"]
                .as_str()
                .unwrap_or_default()
                .to_string();
            axum::Json(serde_json::json!({ "ok": true, "worker_id": worker_id }))
        }

        async fn lease_terminal(
            State(state): State<Arc<TerminalReleaseState>>,
            _body: axum::Json<serde_json::Value>,
        ) -> axum::Json<ShardLeaseResponse> {
            state.lease_calls.fetch_add(1, Ordering::SeqCst);
            axum::Json(ShardLeaseResponse {
                protocol_version: COMMUNITY_SHARD_PROTOCOL_VERSION,
                assignment: state.lease.clone(),
                ready_ttl_ms: 45_000,
            })
        }

        async fn hb_phase_ok(
            axum::Json(signed): axum::Json<
                CommunitySignedRequest<SignedShardWorkerPayload<ShardHeartbeatRequest>>,
            >,
        ) -> axum::Json<ShardHeartbeatResponse> {
            let ready = signed.payload.request.ready;
            axum::Json(phase_matched_hb_response(ready, Vec::new()))
        }

        async fn claim_stale(_body: axum::Json<serde_json::Value>) -> (StatusCode, String) {
            (StatusCode::CONFLICT, "stale lease".to_string())
        }

        async fn release_terminal(
            State(state): State<Arc<TerminalReleaseState>>,
            _body: axum::Json<serde_json::Value>,
        ) -> (StatusCode, String) {
            let idx = state.release_calls.fetch_add(1, Ordering::SeqCst);
            if idx < MAX_RELEASE_TRANSIENT_ATTEMPTS {
                let body = "dropped release".to_string();
                return (StatusCode::SERVICE_UNAVAILABLE, body);
            }
            (state.terminal_status, state.terminal_body.to_string())
        }

        let full = Arc::new(build_deterministic_integer_model(
            32,
            16,
            2,
            32,
            4,
            64,
            vec![128_001],
        ));
        let spec = model_spec_from(arc_crypto::hash_bytes(b"dup-release-err-spec"), &full);
        let boot = arc_crypto::hash_bytes(b"dup-release-err-boot");
        let deadline = checked_now_unix_ms().unwrap() + 60_000;
        let lease_1 = ShardLeaseAssignment {
            lease_id: LeaseId {
                boot_nonce: boot,
                seq: 1,
            },
            lease_epoch: 1,
            model_id: spec.model_id,
            execution_profile: spec.execution_profile.clone(),
            range: ShardLayerRange {
                start_layer: 0,
                end_layer: 2,
            },
            provisioning_deadline_unix_ms: deadline,
        };
        let lease_2 = ShardLeaseAssignment {
            lease_id: LeaseId {
                boot_nonce: boot,
                seq: 2,
            },
            lease_epoch: 1,
            model_id: spec.model_id,
            execution_profile: spec.execution_profile.clone(),
            range: ShardLayerRange {
                start_layer: 2,
                end_layer: 4,
            },
            provisioning_deadline_unix_ms: deadline + 10_000,
        };

        struct TerminalReleaseCase {
            status: StatusCode,
            body: &'static str,
            expect_auth: bool,
        }

        let terminal_cases = [
            TerminalReleaseCase {
                status: StatusCode::BAD_REQUEST,
                body: "worker identity must be nonzero",
                expect_auth: false,
            },
            TerminalReleaseCase {
                status: StatusCode::METHOD_NOT_ALLOWED,
                body: "method not allowed",
                expect_auth: false,
            },
            TerminalReleaseCase {
                status: StatusCode::NOT_FOUND,
                body: "route not found",
                expect_auth: false,
            },
            TerminalReleaseCase {
                status: StatusCode::CONFLICT,
                body: "registered worker model_id does not match coordinator model",
                expect_auth: false,
            },
            TerminalReleaseCase {
                status: StatusCode::CONFLICT,
                body: "invalid registered model_id hex",
                expect_auth: false,
            },
            TerminalReleaseCase {
                status: StatusCode::UNAUTHORIZED,
                body: "invalid worker signature",
                expect_auth: true,
            },
            TerminalReleaseCase {
                status: StatusCode::FORBIDDEN,
                body: "worker is currently quarantined",
                expect_auth: true,
            },
        ];

        for case in terminal_cases {
            let terminal_status = case.status;
            let terminal_body = case.body;
            let expect_auth = case.expect_auth;
            let loader_calls = Arc::new(AtomicUsize::new(0));
            let loader_calls_inner = Arc::clone(&loader_calls);
            let full_for_loader = Arc::clone(&full);
            let loader: PartialModelLoader = Arc::new(move |assignment| {
                loader_calls_inner.fetch_add(1, Ordering::SeqCst);
                let partial = build_partial_integer_model(&full_for_loader, assignment.range);
                Ok(Arc::new(partial))
            });
            let state = Arc::new(TerminalReleaseState {
                lease: lease_1.clone(),
                terminal_status,
                terminal_body,
                register_calls: AtomicUsize::new(0),
                lease_calls: AtomicUsize::new(0),
                release_calls: AtomicUsize::new(0),
            });
            let app = axum::Router::new()
                .route(COMMUNITY_REGISTER_PATH, post(reg_terminal))
                .route(COMMUNITY_SHARD_LEASE_PATH, post(lease_terminal))
                .route(COMMUNITY_SHARD_HEARTBEAT_PATH, post(hb_phase_ok))
                .route(COMMUNITY_SHARD_CLAIM_STEP_PATH, post(claim_stale))
                .route(COMMUNITY_SHARD_RELEASE_PATH, post(release_terminal))
                .with_state(Arc::clone(&state));
            let (base_url, server) = spawn_hb_router(app).await;

            let mut cfg = base_config(&base_url);
            cfg.allow_insecure_for_loopback = true;
            cfg.auto_reconnect = true;
            cfg.heartbeat_interval_ms = 50;
            cfg.retry_base_backoff_ms = 5;
            cfg.retry_max_backoff_ms = 10;
            cfg.reacquire_base_backoff_ms = 5;
            cfg.reacquire_max_backoff_ms = 10;

            let (_shut_tx, shut_rx) = watch::channel(false);
            let err = tokio::time::timeout(
                Duration::from_secs(10),
                run_community_shard_worker(
                    cfg,
                    Arc::new(KeyPair::generate_ed25519()),
                    spec.clone(),
                    loader,
                    shut_rx,
                ),
            )
            .await
            .expect("terminal duplicate release error must fail promptly")
            .unwrap_err();
            server.abort();

            if expect_auth {
                assert!(matches!(
                    err,
                    CommunityShardWorkerError::AuthenticationOrAuthorization {
                        path: COMMUNITY_SHARD_RELEASE_PATH,
                        status,
                        ..
                    } if status == terminal_status.as_u16()
                ));
            } else {
                assert!(matches!(
                    err,
                    CommunityShardWorkerError::ProtocolRejected {
                        path: COMMUNITY_SHARD_RELEASE_PATH,
                        status,
                        ..
                    } if status == terminal_status.as_u16()
                ));
            }
            assert_eq!(state.register_calls.load(Ordering::SeqCst), 1);
            assert_eq!(state.lease_calls.load(Ordering::SeqCst), 2);
            assert_eq!(
                state.release_calls.load(Ordering::SeqCst),
                MAX_RELEASE_TRANSIENT_ATTEMPTS + 1
            );
            assert_eq!(loader_calls.load(Ordering::SeqCst), 1);
        }

        #[derive(Clone, Copy)]
        enum RecoveryMode {
            MissingLeaseAdvances,
            MissingLeaseClearsPrior,
            ReregisterThenConflictAdvances,
        }

        struct RecoveryReleaseState {
            mode: RecoveryMode,
            lease_1: ShardLeaseAssignment,
            lease_2: ShardLeaseAssignment,
            advance_to_lease_2: AtomicBool,
            register_calls: AtomicUsize,
            lease_calls: AtomicUsize,
            release_calls: AtomicUsize,
            ready_seq2: Notify,
        }

        async fn reg_recovery(
            State(state): State<Arc<RecoveryReleaseState>>,
            body: axum::Json<serde_json::Value>,
        ) -> axum::Json<serde_json::Value> {
            state.register_calls.fetch_add(1, Ordering::SeqCst);
            let worker_id = body.0["payload"]["worker_id"]
                .as_str()
                .unwrap_or_default()
                .to_string();
            axum::Json(serde_json::json!({ "ok": true, "worker_id": worker_id }))
        }

        async fn lease_recovery(
            State(state): State<Arc<RecoveryReleaseState>>,
            _body: axum::Json<serde_json::Value>,
        ) -> axum::Json<ShardLeaseResponse> {
            state.lease_calls.fetch_add(1, Ordering::SeqCst);
            let assignment = if state.advance_to_lease_2.load(Ordering::SeqCst) {
                state.lease_2.clone()
            } else {
                state.lease_1.clone()
            };
            axum::Json(ShardLeaseResponse {
                protocol_version: COMMUNITY_SHARD_PROTOCOL_VERSION,
                assignment,
                ready_ttl_ms: 45_000,
            })
        }

        async fn hb_recovery(
            State(state): State<Arc<RecoveryReleaseState>>,
            axum::Json(signed): axum::Json<
                CommunitySignedRequest<SignedShardWorkerPayload<ShardHeartbeatRequest>>,
            >,
        ) -> axum::Json<ShardHeartbeatResponse> {
            let req = signed.payload.request;
            if req.lease_id.seq == 2 && req.ready {
                state.ready_seq2.notify_one();
            }
            axum::Json(phase_matched_hb_response(req.ready, Vec::new()))
        }

        async fn claim_recovery(
            axum::Json(signed): axum::Json<
                CommunitySignedRequest<SignedShardWorkerPayload<ShardClaimStepRequest>>,
            >,
        ) -> Result<axum::Json<ShardClaimStepResponse>, (StatusCode, String)> {
            if signed.payload.request.lease_id.seq == 1 {
                return Err((StatusCode::CONFLICT, "stale lease".to_string()));
            }
            Ok(axum::Json(ShardClaimStepResponse::Idle))
        }

        async fn release_recovery(
            State(state): State<Arc<RecoveryReleaseState>>,
            axum::Json(signed): axum::Json<
                CommunitySignedRequest<SignedShardWorkerPayload<ShardReleaseRequest>>,
            >,
        ) -> Result<axum::Json<ShardReleaseResponse>, (StatusCode, String)> {
            let idx = state.release_calls.fetch_add(1, Ordering::SeqCst);
            if idx < MAX_RELEASE_TRANSIENT_ATTEMPTS {
                return Err((
                    StatusCode::SERVICE_UNAVAILABLE,
                    "dropped release".to_string(),
                ));
            }
            if signed.payload.request.lease_id.seq == 2 {
                return Ok(axum::Json(ShardReleaseResponse {
                    released: true,
                    aborted_sessions: Vec::new(),
                }));
            }
            match state.mode {
                RecoveryMode::MissingLeaseAdvances => {
                    state.advance_to_lease_2.store(true, Ordering::SeqCst);
                    let body = "worker lease not found".to_string();
                    Err((StatusCode::NOT_FOUND, body))
                }
                RecoveryMode::MissingLeaseClearsPrior => {
                    let body = "worker lease not found".to_string();
                    Err((StatusCode::NOT_FOUND, body))
                }
                RecoveryMode::ReregisterThenConflictAdvances => {
                    if idx == MAX_RELEASE_TRANSIENT_ATTEMPTS {
                        return Err((
                            StatusCode::NOT_FOUND,
                            "worker_id not registered - call /community/register first".to_string(),
                        ));
                    }
                    state.advance_to_lease_2.store(true, Ordering::SeqCst);
                    Err((
                        StatusCode::CONFLICT,
                        "stale or mismatched lease_epoch 1, expected 2".to_string(),
                    ))
                }
            }
        }

        let loader: PartialModelLoader = Arc::new(move |assignment| {
            let partial = build_partial_integer_model(&full, assignment.range);
            Ok(Arc::new(partial))
        });

        for mode in [
            RecoveryMode::MissingLeaseAdvances,
            RecoveryMode::MissingLeaseClearsPrior,
            RecoveryMode::ReregisterThenConflictAdvances,
        ] {
            let state = Arc::new(RecoveryReleaseState {
                mode,
                lease_1: lease_1.clone(),
                lease_2: lease_2.clone(),
                advance_to_lease_2: AtomicBool::new(false),
                register_calls: AtomicUsize::new(0),
                lease_calls: AtomicUsize::new(0),
                release_calls: AtomicUsize::new(0),
                ready_seq2: Notify::new(),
            });
            let app = axum::Router::new()
                .route(COMMUNITY_REGISTER_PATH, post(reg_recovery))
                .route(COMMUNITY_SHARD_LEASE_PATH, post(lease_recovery))
                .route(COMMUNITY_SHARD_HEARTBEAT_PATH, post(hb_recovery))
                .route(COMMUNITY_SHARD_CLAIM_STEP_PATH, post(claim_recovery))
                .route(COMMUNITY_SHARD_RELEASE_PATH, post(release_recovery))
                .with_state(Arc::clone(&state));
            let (base_url, server) = spawn_hb_router(app).await;

            let mut cfg = base_config(&base_url);
            cfg.allow_insecure_for_loopback = true;
            cfg.auto_reconnect = true;
            cfg.heartbeat_interval_ms = 50;
            cfg.idle_poll_interval_ms = 10;
            cfg.retry_base_backoff_ms = 5;
            cfg.retry_max_backoff_ms = 10;
            cfg.reacquire_base_backoff_ms = 5;
            cfg.reacquire_max_backoff_ms = 10;

            let (shut_tx, shut_rx) = watch::channel(false);
            let worker_task = tokio::spawn(run_community_shard_worker(
                cfg,
                Arc::new(KeyPair::generate_ed25519()),
                spec.clone(),
                Arc::clone(&loader),
                shut_rx,
            ));

            match mode {
                RecoveryMode::MissingLeaseAdvances => {
                    tokio::time::timeout(Duration::from_secs(10), state.ready_seq2.notified())
                        .await
                        .expect("missing prior lease must clear and advance to seq 2");
                    shut_tx.send(true).unwrap();
                    let summary = tokio::time::timeout(Duration::from_secs(10), worker_task)
                        .await
                        .expect("worker shutdown timeout")
                        .unwrap()
                        .expect("worker must exit cleanly");
                    server.abort();
                    assert_eq!(summary.registrations, 1);
                    assert_eq!(summary.leases_acquired, 2);
                    assert_eq!(summary.models_loaded, 2);
                    assert_eq!(summary.reconnect_attempts, 0);
                    assert!(summary.clean_release_sent);
                }
                RecoveryMode::MissingLeaseClearsPrior => {
                    let err = tokio::time::timeout(Duration::from_secs(10), worker_task)
                        .await
                        .expect("re-returned lease after LeaseNotFound must fail promptly")
                        .unwrap()
                        .unwrap_err();
                    server.abort();
                    assert!(matches!(err, CommunityShardWorkerError::InvalidLease(_)));
                    assert_eq!(state.register_calls.load(Ordering::SeqCst), 1);
                    assert_eq!(state.lease_calls.load(Ordering::SeqCst), 3);
                    assert_eq!(
                        state.release_calls.load(Ordering::SeqCst),
                        MAX_RELEASE_TRANSIENT_ATTEMPTS + 1
                    );
                }
                RecoveryMode::ReregisterThenConflictAdvances => {
                    tokio::time::timeout(Duration::from_secs(10), state.ready_seq2.notified())
                        .await
                        .expect("reregister and conflict race on release must advance to seq 2");
                    shut_tx.send(true).unwrap();
                    let summary = tokio::time::timeout(Duration::from_secs(10), worker_task)
                        .await
                        .expect("worker shutdown timeout")
                        .unwrap()
                        .expect("worker must exit cleanly");
                    server.abort();
                    assert_eq!(summary.registrations, 2);
                    assert_eq!(summary.leases_acquired, 2);
                    assert_eq!(summary.models_loaded, 2);
                    assert_eq!(summary.reconnect_attempts, 0);
                    assert!(summary.clean_release_sent);
                }
            }
        }
    }

    #[tokio::test]
    async fn paused_execution_gate_applies_heartbeat_cancellations_without_claiming() {
        use crate::community_sharding::tests::{
            build_deterministic_integer_model, build_partial_integer_model, model_spec_from,
        };
        use crate::community_sharding::{SessionAbortReason, ShardStepInput, StepId};
        use crate::native_inference::WorkerQuiescenceResult;
        use crate::rpc::CommunitySignedRequest;
        use crate::rpc::community_shard_http::SignedShardWorkerPayload;
        use axum::extract::State;
        use axum::routing::post;
        use std::sync::atomic::AtomicBool;
        use tokio::sync::Notify;

        struct PausedCancelState {
            spec: TrustedModelSpec,
            boot: Hash256,
            claim_calls: AtomicUsize,
            submit_calls: AtomicUsize,
            submit1_entered: Notify,
            submit1_release: Notify,
            emit_cancel_while_paused: AtomicBool,
            cancel_sent: AtomicBool,
            post_cancel_hb_pending: AtomicBool,
            post_cancel_hb_seen: Notify,
            post_stale_claim_seen: Notify,
        }

        async fn reg_ok(body: axum::Json<serde_json::Value>) -> axum::Json<serde_json::Value> {
            let worker_id = body.0["payload"]["worker_id"]
                .as_str()
                .unwrap_or_default()
                .to_string();
            axum::Json(serde_json::json!({ "ok": true, "worker_id": worker_id }))
        }

        async fn lease_ok(
            State(state): State<Arc<PausedCancelState>>,
            _body: axum::Json<serde_json::Value>,
        ) -> axum::Json<ShardLeaseResponse> {
            axum::Json(ShardLeaseResponse {
                protocol_version: COMMUNITY_SHARD_PROTOCOL_VERSION,
                assignment: ShardLeaseAssignment {
                    lease_id: LeaseId {
                        boot_nonce: state.boot,
                        seq: 1,
                    },
                    lease_epoch: 1,
                    model_id: state.spec.model_id,
                    execution_profile: state.spec.execution_profile.clone(),
                    range: ShardLayerRange {
                        start_layer: 0,
                        end_layer: 2,
                    },
                    provisioning_deadline_unix_ms: checked_now_unix_ms().unwrap() + 60_000,
                },
                ready_ttl_ms: 45_000,
            })
        }

        async fn hb_handler(
            State(state): State<Arc<PausedCancelState>>,
            axum::Json(signed): axum::Json<
                CommunitySignedRequest<SignedShardWorkerPayload<ShardHeartbeatRequest>>,
            >,
        ) -> axum::Json<ShardHeartbeatResponse> {
            let ready = signed.payload.request.ready;
            let mut cancellations = Vec::new();
            if ready {
                if state.emit_cancel_while_paused.swap(false, Ordering::SeqCst) {
                    state.cancel_sent.store(true, Ordering::SeqCst);
                    cancellations.push(SessionCancellationNotice {
                        session_id: SessionId {
                            boot_nonce: state.boot,
                            seq: 1,
                        },
                        lease_id: LeaseId {
                            boot_nonce: state.boot,
                            seq: 1,
                        },
                        lease_epoch: 1,
                        reason: SessionAbortReason::CallerCancelled,
                        expires_at_unix_ms: checked_now_unix_ms().unwrap() + 60_000,
                    });
                } else if state.cancel_sent.load(Ordering::SeqCst)
                    && state.post_cancel_hb_pending.swap(false, Ordering::SeqCst)
                {
                    state.post_cancel_hb_seen.notify_one();
                }
            }
            axum::Json(phase_matched_hb_response(ready, cancellations))
        }

        async fn claim_handler(
            State(state): State<Arc<PausedCancelState>>,
            _body: axum::Json<serde_json::Value>,
        ) -> axum::Json<ShardClaimStepResponse> {
            let idx = state.claim_calls.fetch_add(1, Ordering::SeqCst);
            if idx == 0 || idx == 1 {
                return axum::Json(ShardClaimStepResponse::Step {
                    work: Box::new(ShardStepWork {
                        session_id: SessionId {
                            boot_nonce: state.boot,
                            seq: 1,
                        },
                        step_id: StepId {
                            boot_nonce: state.boot,
                            seq: (idx + 1) as u64,
                        },
                        lease_id: LeaseId {
                            boot_nonce: state.boot,
                            seq: 1,
                        },
                        lease_epoch: 1,
                        model_id: state.spec.model_id,
                        execution_profile: state.spec.execution_profile.clone(),
                        range: ShardLayerRange {
                            start_layer: 0,
                            end_layer: 2,
                        },
                        position: idx,
                        expected_hidden_len: state.spec.d_model,
                        expect_terminal: false,
                        last_token: false,
                        deadline_unix_ms: checked_now_unix_ms().unwrap() + 30_000,
                        input: ShardStepInput::Token {
                            token: state.spec.bos_token,
                        },
                        generated_tokens: Vec::new(),
                    }),
                });
            }
            if idx == 2 {
                state.post_stale_claim_seen.notify_one();
            }
            axum::Json(ShardClaimStepResponse::Idle)
        }

        async fn submit_handler(
            State(state): State<Arc<PausedCancelState>>,
            _body: axum::Json<serde_json::Value>,
        ) -> axum::Json<ShardStepSubmitResponse> {
            let idx = state.submit_calls.fetch_add(1, Ordering::SeqCst);
            if idx == 0 {
                state.submit1_entered.notify_one();
                state.submit1_release.notified().await;
            }
            axum::Json(ShardStepSubmitResponse {
                accepted: true,
                session_completed: false,
                session_aborted: false,
            })
        }

        async fn release_ok(
            _body: axum::Json<serde_json::Value>,
        ) -> axum::Json<ShardReleaseResponse> {
            axum::Json(ShardReleaseResponse {
                released: true,
                aborted_sessions: Vec::new(),
            })
        }

        let full = Arc::new(build_deterministic_integer_model(
            32,
            16,
            2,
            32,
            4,
            64,
            vec![128_001],
        ));
        let spec = model_spec_from(arc_crypto::hash_bytes(b"paused-cancel-spec"), &full);
        let loader: PartialModelLoader = Arc::new(move |assignment| {
            let partial = build_partial_integer_model(&full, assignment.range);
            Ok(Arc::new(partial))
        });

        let state = Arc::new(PausedCancelState {
            spec: spec.clone(),
            boot: arc_crypto::hash_bytes(b"paused-cancel-boot"),
            claim_calls: AtomicUsize::new(0),
            submit_calls: AtomicUsize::new(0),
            submit1_entered: Notify::new(),
            submit1_release: Notify::new(),
            emit_cancel_while_paused: AtomicBool::new(false),
            cancel_sent: AtomicBool::new(false),
            post_cancel_hb_pending: AtomicBool::new(true),
            post_cancel_hb_seen: Notify::new(),
            post_stale_claim_seen: Notify::new(),
        });
        let app = axum::Router::new()
            .route(COMMUNITY_REGISTER_PATH, post(reg_ok))
            .route(COMMUNITY_SHARD_LEASE_PATH, post(lease_ok))
            .route(COMMUNITY_SHARD_HEARTBEAT_PATH, post(hb_handler))
            .route(COMMUNITY_SHARD_CLAIM_STEP_PATH, post(claim_handler))
            .route(COMMUNITY_SHARD_SUBMIT_STEP_PATH, post(submit_handler))
            .route(COMMUNITY_SHARD_RELEASE_PATH, post(release_ok))
            .with_state(Arc::clone(&state));
        let (base_url, server) = spawn_hb_router(app).await;

        let keypair = Arc::new(KeyPair::generate_ed25519());
        let status = Arc::new(CommunityWorkerStatus::new_sharded(
            format!("0x{}", keypair.address().to_hex()),
            "worker-unit",
            "arc-4L-16d",
            &spec.model_id.to_hex(),
            spec.n_layers,
            false,
        ));
        let gate = Arc::new(WorkerExecutionGate::default());
        let mut cfg = base_config(&base_url);
        cfg.allow_insecure_for_loopback = true;
        cfg.heartbeat_interval_ms = 40;
        cfg.idle_poll_interval_ms = 10;
        cfg.telemetry = Some(Arc::clone(&status).into());
        cfg.worker_execution_gate = Some(Arc::clone(&gate).into());

        let (shut_tx, shut_rx) = watch::channel(false);
        let worker_task = tokio::spawn(run_community_shard_worker(
            cfg, keypair, spec, loader, shut_rx,
        ));

        tokio::time::timeout(Duration::from_secs(10), state.submit1_entered.notified())
            .await
            .expect("first step submit must arrive");
        assert_eq!(gate.active_jobs(), 1);

        let request_id = [0xA9; 32];
        let mut quiesce_fut = Box::pin(gate.quiesce(
            request_id,
            Duration::from_secs(10),
            Instant::now() + Duration::from_secs(30),
        ));
        std::future::poll_fn(|cx| {
            assert!(std::future::Future::poll(quiesce_fut.as_mut(), cx).is_pending());
            std::task::Poll::Ready(())
        })
        .await;
        assert!(gate.is_closed());

        state.submit1_release.notify_one();
        let quiesce_result = tokio::time::timeout(Duration::from_secs(10), quiesce_fut)
            .await
            .expect("quiesce must complete after first submit");
        assert_eq!(
            quiesce_result,
            WorkerQuiescenceResult::Quiesced { active_jobs: 0 }
        );
        assert!(gate.is_closed());
        assert_eq!(state.claim_calls.load(Ordering::SeqCst), 1);

        state.emit_cancel_while_paused.store(true, Ordering::SeqCst);
        let post_cancel_hb = state.post_cancel_hb_seen.notified();
        tokio::time::timeout(Duration::from_secs(10), post_cancel_hb)
            .await
            .expect("heartbeat after paused cancellation must arrive");
        assert!(gate.is_closed());
        assert_eq!(state.claim_calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            status
                .snapshot()
                .shard
                .expect("shard telemetry must exist")
                .cancellations_applied,
            1
        );

        assert_eq!(gate.resume(request_id), Ok(0));
        let post_stale_claim = state.post_stale_claim_seen.notified();
        tokio::time::timeout(Duration::from_secs(10), post_stale_claim)
            .await
            .expect("claims after resume must advance past tombstoned step");
        shut_tx.send(true).unwrap();
        let summary = tokio::time::timeout(Duration::from_secs(10), worker_task)
            .await
            .expect("worker shutdown timeout")
            .unwrap()
            .expect("worker must exit cleanly");
        server.abort();

        assert_eq!(summary.steps_executed, 1);
        assert_eq!(summary.steps_submitted, 1);
        assert_eq!(summary.cancellations_applied, 1);
        assert_eq!(state.submit_calls.load(Ordering::SeqCst), 1);
        assert!(summary.clean_release_sent);
        assert_eq!(gate.active_jobs(), 0);
    }

    #[tokio::test]
    async fn blocking_compute_wrapper_retains_permit_on_cancel_and_handoff() {
        use crate::native_inference::WorkerQuiescenceResult;
        use std::sync::atomic::AtomicBool;
        use tokio::sync::Notify;

        struct DropFlag(Arc<AtomicBool>);

        impl Drop for DropFlag {
            fn drop(&mut self) {
                self.0.store(true, Ordering::SeqCst);
            }
        }

        let gate = Arc::new(WorkerExecutionGate::default());

        let outer_permit = gate.try_enter().expect("open gate must admit permit");
        assert_eq!(gate.active_jobs(), 1);
        let entered = Arc::new(Notify::new());
        let entered_inner = Arc::clone(&entered);
        let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
        let completed = Arc::new(AtomicBool::new(false));
        let completed_inner = Arc::clone(&completed);
        let outer_dropped = Arc::new(AtomicBool::new(false));
        let outer_dropped_inner = Arc::clone(&outer_dropped);

        let mut async_step = Box::pin(async move {
            let _drop_flag = DropFlag(outer_dropped_inner);
            let compute_permit = Some(outer_permit.clone());
            let handle = spawn_blocking_with_execution_permit(compute_permit, move || {
                entered_inner.notify_one();
                release_rx
                    .recv_timeout(Duration::from_secs(10))
                    .expect("blocking compute release");
                completed_inner.store(true, Ordering::SeqCst);
                42usize
            });
            let value = handle.await.expect("blocking task join");
            drop(outer_permit);
            value
        });
        std::future::poll_fn(|cx| {
            assert!(std::future::Future::poll(async_step.as_mut(), cx).is_pending());
            std::task::Poll::Ready(())
        })
        .await;

        tokio::time::timeout(Duration::from_secs(10), entered.notified())
            .await
            .expect("blocking compute closure must enter");
        assert_eq!(gate.active_jobs(), 1);

        let cancel_request_id = [0xB1; 32];
        let mut cancel_quiesce = Box::pin(gate.quiesce(
            cancel_request_id,
            Duration::from_secs(10),
            Instant::now() + Duration::from_secs(30),
        ));
        std::future::poll_fn(|cx| {
            assert!(std::future::Future::poll(cancel_quiesce.as_mut(), cx).is_pending());
            std::task::Poll::Ready(())
        })
        .await;
        assert!(gate.is_closed());
        assert_eq!(gate.active_jobs(), 1);

        drop(async_step);
        assert!(outer_dropped.load(Ordering::SeqCst));
        assert!(!completed.load(Ordering::SeqCst));
        assert_eq!(gate.active_jobs(), 1);
        std::future::poll_fn(|cx| {
            assert!(std::future::Future::poll(cancel_quiesce.as_mut(), cx).is_pending());
            std::task::Poll::Ready(())
        })
        .await;

        release_tx.send(()).unwrap();
        let cancel_quiesce_result = tokio::time::timeout(Duration::from_secs(10), cancel_quiesce)
            .await
            .expect("quiesce must complete after blocking compute exits");
        assert_eq!(
            cancel_quiesce_result,
            WorkerQuiescenceResult::Quiesced { active_jobs: 0 }
        );
        assert!(completed.load(Ordering::SeqCst));
        assert_eq!(gate.active_jobs(), 0);
        assert_eq!(gate.resume(cancel_request_id), Ok(0));

        let outer_permit = gate.try_enter().expect("reopened gate must admit permit");
        assert_eq!(gate.active_jobs(), 1);
        let handoff_entered = Arc::new(Notify::new());
        let handoff_entered_inner = Arc::clone(&handoff_entered);
        let (handoff_release_tx, handoff_release_rx) = std::sync::mpsc::channel::<()>();
        let compute_permit = Some(outer_permit.clone());
        let handle = spawn_blocking_with_execution_permit(compute_permit, move || {
            handoff_entered_inner.notify_one();
            handoff_release_rx
                .recv_timeout(Duration::from_secs(10))
                .expect("handoff compute release");
            7usize
        });
        tokio::time::timeout(Duration::from_secs(10), handoff_entered.notified())
            .await
            .expect("handoff compute closure must enter");
        assert_eq!(gate.active_jobs(), 1);

        let handoff_request_id = [0xB2; 32];
        let mut handoff_quiesce = Box::pin(gate.quiesce(
            handoff_request_id,
            Duration::from_secs(10),
            Instant::now() + Duration::from_secs(30),
        ));
        std::future::poll_fn(|cx| {
            assert!(std::future::Future::poll(handoff_quiesce.as_mut(), cx).is_pending());
            std::task::Poll::Ready(())
        })
        .await;
        assert!(gate.is_closed());

        handoff_release_tx.send(()).unwrap();
        let value = tokio::time::timeout(Duration::from_secs(10), handle)
            .await
            .expect("handoff blocking task must finish")
            .expect("handoff blocking task join");
        assert_eq!(value, 7);
        assert_eq!(gate.active_jobs(), 1);
        std::future::poll_fn(|cx| {
            assert!(std::future::Future::poll(handoff_quiesce.as_mut(), cx).is_pending());
            std::task::Poll::Ready(())
        })
        .await;

        drop(outer_permit);
        let handoff_quiesce_result = tokio::time::timeout(Duration::from_secs(10), handoff_quiesce)
            .await
            .expect("quiesce must complete once outer permit drops after submit");
        assert_eq!(
            handoff_quiesce_result,
            WorkerQuiescenceResult::Quiesced { active_jobs: 0 }
        );
        assert_eq!(gate.active_jobs(), 0);
    }
}
