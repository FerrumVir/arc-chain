use arc_crypto::Hash256;
use arc_inference::cached_integer_model::CANONICAL_REWARD_INFERENCE_PROFILE;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fmt;

pub const COMMUNITY_SHARD_PROTOCOL_VERSION: u16 = 1;
pub const REPETITION_PENALTY_WINDOW: usize = 64;
pub const MAX_WORKERS_HARD_CAP: usize = 128;
pub const MAX_ACTIVE_SESSIONS_HARD_CAP: usize = 64;
pub const MAX_MODEL_LAYERS_HARD_CAP: usize = 1024;
pub const MAX_EOS_TOKENS: usize = 64;
pub const MAX_REASON_BYTES: usize = 256;
pub const MAX_HIDDEN_LEN_HARD_CAP: usize = 65_536;
pub const MAX_HIDDEN_ABS_Q16: i64 = 1 << 30;
pub const MAX_SEQUENCE_POSITIONS_HARD_CAP: usize = 32_768;
pub const MAX_GENERATED_TOKENS_HARD_CAP: u32 = 256;
pub const MAX_RETAINED_RESULTS_HARD_CAP: usize = 256;
pub const MAX_CANCELLATIONS_PER_WORKER_HARD_CAP: usize = 128;
pub const MAX_CANCELLATION_QUEUES_HARD_CAP: usize = 256;
pub const MAX_FAILED_WORKERS_HARD_CAP: usize = 256;
pub const DEFAULT_MAX_WORKERS: usize = 128;
pub const DEFAULT_MAX_ACTIVE_SESSIONS: usize = 24;
pub const DEFAULT_MAX_SESSIONS_PER_WORKER: usize = 1;
pub const DEFAULT_TARGET_PARTITIONS: usize = 6;
pub const DEFAULT_MAX_HIDDEN_LEN: usize = 16_384;
pub const DEFAULT_PROVISIONING_TTL_MS: u64 = 300_000;
pub const DEFAULT_READY_TTL_MS: u64 = 45_000;
pub const DEFAULT_STEP_TIMEOUT_MS: u64 = 60_000;
pub const DEFAULT_JOB_DEADLINE_MS: u64 = 300_000;
pub const DEFAULT_QUARANTINE_TTL_MS: u64 = 300_000;
pub const DEFAULT_MAX_RETAINED_RESULTS: usize = 64;
pub const DEFAULT_MAX_CANCELLATIONS_PER_WORKER: usize = 16;
pub const DEFAULT_MAX_CANCELLATION_QUEUES: usize = 256;
pub const DEFAULT_MAX_FAILED_WORKERS: usize = 256;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LeaseId {
    pub boot_nonce: Hash256,
    pub seq: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionId {
    pub boot_nonce: Hash256,
    pub seq: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StepId {
    pub boot_nonce: Hash256,
    pub seq: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ShardLayerRange {
    pub start_layer: usize,
    pub end_layer: usize,
}

impl ShardLayerRange {
    pub fn len(self) -> usize {
        self.end_layer.saturating_sub(self.start_layer)
    }

    pub fn is_empty(self) -> bool {
        self.start_layer >= self.end_layer
    }

    pub fn is_valid_for(self, total_layers: usize) -> bool {
        self.start_layer < self.end_layer && self.end_layer <= total_layers
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TrustedModelSpec {
    pub model_id: Hash256,
    pub execution_profile: String,
    pub n_layers: usize,
    pub d_model: usize,
    pub vocab_size: usize,
    pub max_seq: usize,
    pub eos_tokens: Vec<u32>,
    pub bos_token: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CommunityShardLimits {
    pub max_workers: usize,
    pub max_active_sessions: usize,
    pub max_sessions_per_worker: usize,
    pub target_partitions: usize,
    pub max_hidden_len: usize,
    pub provisioning_ttl_ms: u64,
    pub ready_ttl_ms: u64,
    pub step_timeout_ms: u64,
    pub job_deadline_ms: u64,
    pub quarantine_ttl_ms: u64,
    pub max_retained_results: usize,
    pub max_cancellations_per_worker: usize,
    pub max_cancellation_queues: usize,
    pub max_failed_workers: usize,
}

impl Default for CommunityShardLimits {
    fn default() -> Self {
        Self {
            max_workers: DEFAULT_MAX_WORKERS,
            max_active_sessions: DEFAULT_MAX_ACTIVE_SESSIONS,
            max_sessions_per_worker: DEFAULT_MAX_SESSIONS_PER_WORKER,
            target_partitions: DEFAULT_TARGET_PARTITIONS,
            max_hidden_len: DEFAULT_MAX_HIDDEN_LEN,
            provisioning_ttl_ms: DEFAULT_PROVISIONING_TTL_MS,
            ready_ttl_ms: DEFAULT_READY_TTL_MS,
            step_timeout_ms: DEFAULT_STEP_TIMEOUT_MS,
            job_deadline_ms: DEFAULT_JOB_DEADLINE_MS,
            quarantine_ttl_ms: DEFAULT_QUARANTINE_TTL_MS,
            max_retained_results: DEFAULT_MAX_RETAINED_RESULTS,
            max_cancellations_per_worker: DEFAULT_MAX_CANCELLATIONS_PER_WORKER,
            max_cancellation_queues: DEFAULT_MAX_CANCELLATION_QUEUES,
            max_failed_workers: DEFAULT_MAX_FAILED_WORKERS,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ShardCoordinatorError {
    InvalidConfig(&'static str),
    InvalidWorkerIdentity,
    WorkerIdentityMismatch,
    UnsupportedProtocolVersion {
        expected: u16,
        actual: u16,
    },
    ModelMismatch(&'static str),
    BootNonceMismatch,
    WorkerQuarantined,
    LeaseCapacityConflict {
        existing: usize,
        requested: usize,
    },
    WorkerCapacityReached,
    InsufficientLayerBudget {
        requested: usize,
        minimum_tile: usize,
    },
    LeaseNotFound,
    LeaseIdMismatch,
    LeaseEpochMismatch {
        expected: u64,
        actual: u64,
    },
    LeaseNotReady,
    LeaseRangeMismatch,
    ReadyStateRegression,
    SessionCapacityReached,
    InvalidPrompt(&'static str),
    InsufficientShardCoverage {
        missing_range: ShardLayerRange,
    },
    SessionNotFound,
    UnexpectedStep,
    StepDeadlineExpired,
    InvalidLayersProcessed {
        expected: usize,
        actual: usize,
    },
    TerminalModeMismatch {
        expected_terminal: bool,
    },
    InvalidHiddenLength {
        expected: usize,
        actual: usize,
    },
    HiddenValueOutOfBounds,
    HiddenHashMismatch,
    TokenOutOfRange {
        token_id: u32,
        vocab_size: usize,
    },
    ZeroDigest,
    ReasonTooLong,
    TimestampOverflow,
    CounterOverflow,
}

impl fmt::Display for ShardCoordinatorError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidConfig(msg) => write!(f, "invalid shard coordinator config: {msg}"),
            Self::InvalidWorkerIdentity => write!(f, "worker identity must be nonzero"),
            Self::WorkerIdentityMismatch => {
                write!(f, "payload worker_id does not match authenticated worker")
            }
            Self::UnsupportedProtocolVersion { expected, actual } => {
                write!(
                    f,
                    "unsupported protocol version {actual}, expected {expected}"
                )
            }
            Self::ModelMismatch(field) => write!(f, "worker model specification mismatch: {field}"),
            Self::BootNonceMismatch => {
                write!(f, "identifier boot_nonce does not match coordinator")
            }
            Self::WorkerQuarantined => write!(f, "worker is currently quarantined"),
            Self::LeaseCapacityConflict {
                existing,
                requested,
            } => write!(
                f,
                "active lease capacity {existing} conflicts with requested max_resident_layers {requested}"
            ),
            Self::WorkerCapacityReached => write!(f, "shard worker lease table is at capacity"),
            Self::InsufficientLayerBudget {
                requested,
                minimum_tile,
            } => write!(
                f,
                "worker layer budget {requested} is smaller than minimum assignable tile {minimum_tile}"
            ),
            Self::LeaseNotFound => write!(f, "worker lease not found"),
            Self::LeaseIdMismatch => write!(f, "lease_id does not match active worker lease"),
            Self::LeaseEpochMismatch { expected, actual } => {
                write!(
                    f,
                    "stale or mismatched lease_epoch {actual}, expected {expected}"
                )
            }
            Self::LeaseNotReady => write!(f, "worker lease has not acknowledged ready state"),
            Self::LeaseRangeMismatch => {
                write!(f, "reported layer range does not match assigned lease")
            }
            Self::ReadyStateRegression => {
                write!(f, "ready lease cannot regress to non-ready state")
            }
            Self::SessionCapacityReached => write!(f, "active sharded sessions at capacity"),
            Self::InvalidPrompt(msg) => write!(f, "invalid inference session input: {msg}"),
            Self::InsufficientShardCoverage { missing_range } => write!(
                f,
                "no eligible ready worker covers layer range [{}, {})",
                missing_range.start_layer, missing_range.end_layer
            ),
            Self::SessionNotFound => write!(f, "active shard session not found"),
            Self::UnexpectedStep => {
                write!(f, "submitted step does not match expected session step")
            }
            Self::StepDeadlineExpired => write!(f, "step or session deadline already expired"),
            Self::InvalidLayersProcessed { expected, actual } => {
                write!(
                    f,
                    "layers_processed {actual} does not match assigned range {expected}"
                )
            }
            Self::TerminalModeMismatch { expected_terminal } => {
                write!(
                    f,
                    "step output terminal mode mismatch (expected {expected_terminal})"
                )
            }
            Self::InvalidHiddenLength { expected, actual } => {
                write!(
                    f,
                    "hidden state length {actual} does not match model width {expected}"
                )
            }
            Self::HiddenValueOutOfBounds => write!(
                f,
                "hidden activation exceeds the bounded community shard numeric envelope"
            ),
            Self::HiddenHashMismatch => write!(f, "hidden state digest mismatch"),
            Self::TokenOutOfRange {
                token_id,
                vocab_size,
            } => write!(
                f,
                "token_id {token_id} out of range for vocab_size {vocab_size}"
            ),
            Self::ZeroDigest => write!(f, "digest must be nonzero"),
            Self::ReasonTooLong => write!(f, "reason exceeds maximum allowed length"),
            Self::TimestampOverflow => {
                write!(f, "deadline calculation overflowed timestamp bounds")
            }
            Self::CounterOverflow => write!(f, "monotonic identifier counter overflowed"),
        }
    }
}

impl std::error::Error for ShardCoordinatorError {}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ShardLeaseRequest {
    pub worker_id: Hash256,
    pub protocol_version: u16,
    pub model_id: Hash256,
    pub execution_profile: String,
    pub n_layers: usize,
    pub d_model: usize,
    pub vocab_size: usize,
    pub max_seq: usize,
    pub max_resident_layers: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ShardLeaseAssignment {
    pub lease_id: LeaseId,
    pub lease_epoch: u64,
    pub model_id: Hash256,
    pub execution_profile: String,
    pub range: ShardLayerRange,
    pub provisioning_deadline_unix_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ShardLeaseResponse {
    pub protocol_version: u16,
    pub assignment: ShardLeaseAssignment,
    pub ready_ttl_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ShardHeartbeatRequest {
    pub worker_id: Hash256,
    pub lease_id: LeaseId,
    pub lease_epoch: u64,
    pub model_id: Hash256,
    pub execution_profile: String,
    pub loaded_range: ShardLayerRange,
    pub ready: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "directive", rename_all = "snake_case", deny_unknown_fields)]
pub enum HeartbeatDirective {
    Provisioning { deadline_unix_ms: u64 },
    Active { expires_at_unix_ms: u64 },
    Rebalance { assignment: ShardLeaseAssignment },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ShardHeartbeatResponse {
    pub directive: HeartbeatDirective,
    pub cancellations: Vec<SessionCancellationNotice>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ShardClaimStepRequest {
    pub worker_id: Hash256,
    pub lease_id: LeaseId,
    pub lease_epoch: u64,
    pub model_id: Hash256,
    pub execution_profile: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ShardStepInput {
    Token {
        token: u32,
    },
    Hidden {
        hidden: Vec<i64>,
        hidden_hash: Hash256,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ShardStepWork {
    pub session_id: SessionId,
    pub step_id: StepId,
    pub lease_id: LeaseId,
    pub lease_epoch: u64,
    pub model_id: Hash256,
    pub execution_profile: String,
    pub range: ShardLayerRange,
    pub position: usize,
    pub expected_hidden_len: usize,
    pub expect_terminal: bool,
    pub last_token: bool,
    pub deadline_unix_ms: u64,
    pub input: ShardStepInput,
    pub generated_tokens: Vec<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case", deny_unknown_fields)]
pub enum ShardClaimStepResponse {
    Idle,
    Step {
        work: Box<ShardStepWork>,
    },
    CancelSessions {
        cancellations: Vec<SessionCancellationNotice>,
    },
    RebalanceRequired {
        assignment: ShardLeaseAssignment,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ShardStepOutput {
    Hidden {
        hidden: Vec<i64>,
        hidden_hash: Hash256,
    },
    Token {
        token_id: u32,
        logits_hash: Hash256,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case", deny_unknown_fields)]
pub enum ShardStepOutcome {
    Completed { output: ShardStepOutput },
    Declined { reason: String },
    Failed { reason: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ShardStepSubmitRequest {
    pub worker_id: Hash256,
    pub lease_id: LeaseId,
    pub lease_epoch: u64,
    pub session_id: SessionId,
    pub step_id: StepId,
    pub model_id: Hash256,
    pub execution_profile: String,
    pub range: ShardLayerRange,
    pub position: usize,
    pub layers_processed: usize,
    pub outcome: ShardStepOutcome,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ShardStepSubmitResponse {
    pub accepted: bool,
    pub session_completed: bool,
    pub session_aborted: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ShardReleaseRequest {
    pub worker_id: Hash256,
    pub lease_id: LeaseId,
    pub lease_epoch: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ShardReleaseResponse {
    pub released: bool,
    pub aborted_sessions: Vec<SessionId>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "reason", rename_all = "snake_case", deny_unknown_fields)]
pub enum SessionAbortReason {
    CallerCancelled,
    CompletedEarlyEos,
    WorkerReleased { worker_id: Hash256 },
    WorkerLeaseExpired { worker_id: Hash256 },
    WorkerQuarantined { worker_id: Hash256 },
    WorkerDeclined { worker_id: Hash256, detail: String },
    WorkerFailed { worker_id: Hash256, detail: String },
    StepDeadlineExceeded { worker_id: Hash256 },
    JobDeadlineExceeded,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "reason", rename_all = "snake_case", deny_unknown_fields)]
pub enum WorkerFaultReason {
    OutputVerificationMismatch,
    TamperedHiddenState,
    ProtocolViolation,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionCancellationNotice {
    pub session_id: SessionId,
    pub lease_id: LeaseId,
    pub lease_epoch: u64,
    pub reason: SessionAbortReason,
    pub expires_at_unix_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionParticipantRecord {
    pub worker_id: Hash256,
    pub lease_id: LeaseId,
    pub lease_epoch: u64,
    pub range: ShardLayerRange,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PendingVerificationResult {
    pub session_id: SessionId,
    pub model_id: Hash256,
    pub execution_profile: String,
    pub prompt_tokens: Vec<u32>,
    pub unverified_generated_tokens: Vec<u32>,
    pub unverified_output_digest: Hash256,
    pub terminal_logits_hashes: Vec<Hash256>,
    pub stopped_by_eos: bool,
    pub participants: Vec<SessionParticipantRecord>,
    pub verification_required: bool,
    pub reward_eligible: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AbortedSessionResult {
    pub session_id: SessionId,
    pub reason: SessionAbortReason,
    pub completed_tokens_before_abort: usize,
    pub participants: Vec<SessionParticipantRecord>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case", deny_unknown_fields)]
pub enum SessionOutcome {
    PendingVerification(Box<PendingVerificationResult>),
    Aborted(AbortedSessionResult),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExpirationSummary {
    pub expired_leases: usize,
    pub aborted_sessions: usize,
    pub pruned_cancellations: usize,
    pub pruned_quarantines: usize,
    pub rebalanced_workers: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TileCoverageStatus {
    pub range: ShardLayerRange,
    pub ready_workers: usize,
    pub provisioning_workers: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CoordinatorStatusSnapshot {
    pub boot_nonce: Hash256,
    pub model_id: Hash256,
    pub execution_profile: String,
    pub tiles: Vec<TileCoverageStatus>,
    pub fully_covered_ready: bool,
    pub ready_workers: usize,
    pub provisioning_workers: usize,
    pub active_sessions: usize,
    pub retained_results: usize,
    pub queued_cancellations: usize,
    pub quarantined_workers: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ShardPipelineCapacity {
    pub ready_complete_pipelines: usize,
    pub idle_complete_pipelines: usize,
    pub active_sessions: usize,
    pub max_active_sessions: usize,
    pub effective_session_capacity: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkerLeaseSnapshot {
    pub worker_id: Hash256,
    pub lease_id: LeaseId,
    pub lease_epoch: u64,
    pub range: ShardLayerRange,
    pub ready: bool,
    pub deadline_unix_ms: u64,
    pub active_sessions: usize,
    pub max_resident_layers: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionStatusSnapshot {
    pub session_id: SessionId,
    pub position: usize,
    pub hop_index: usize,
    pub generated_tokens_so_far: usize,
    pub deadline_unix_ms: u64,
    pub expected_worker: Hash256,
    pub expected_step_id: StepId,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LeasePhase {
    Provisioning { deadline_unix_ms: u64 },
    Ready { expires_at_unix_ms: u64 },
}

#[derive(Debug, Clone)]
struct WorkerLease {
    worker_id: Hash256,
    lease_id: LeaseId,
    lease_epoch: u64,
    range: ShardLayerRange,
    max_resident_layers: usize,
    provisioning_deadline_unix_ms: u64,
    phase: LeasePhase,
    pending_rebalance_from: Option<(u64, ShardLayerRange)>,
    active_sessions: BTreeSet<u64>,
    last_assigned_seq: u64,
}

#[derive(Debug, Clone)]
struct SessionHop {
    worker_id: Hash256,
    lease_id: LeaseId,
    lease_epoch: u64,
    range: ShardLayerRange,
}

#[derive(Debug, Clone)]
struct ShardSession {
    session_id: SessionId,
    deadline_unix_ms: u64,
    prompt_tokens: Vec<u32>,
    max_tokens: u32,
    generated_tokens: Vec<u32>,
    terminal_logits_hashes: Vec<Hash256>,
    hops: Vec<SessionHop>,
    position: usize,
    hop_index: usize,
    outstanding_step: ShardStepWork,
}

#[derive(Debug, Clone)]
struct QuarantineRecord {
    reason: WorkerFaultReason,
    expires_at_unix_ms: u64,
}

pub fn hidden_values_within_bounds(hidden: &[i64]) -> bool {
    hidden
        .iter()
        .all(|value| (-MAX_HIDDEN_ABS_Q16..=MAX_HIDDEN_ABS_Q16).contains(value))
}

pub fn hidden_state_hash(hidden: &[i64]) -> Hash256 {
    let mut bytes = Vec::with_capacity(hidden.len().saturating_mul(8));
    for value in hidden {
        bytes.extend_from_slice(&value.to_le_bytes());
    }
    arc_crypto::hash_bytes(&bytes)
}

pub fn unverified_tokens_digest(
    model_id: Hash256,
    execution_profile: &str,
    prompt_tokens: &[u32],
    generated_tokens: &[u32],
) -> Hash256 {
    let mut bytes = Vec::new();
    bytes.extend_from_slice(b"ARC-community-shard-unverified-v1");
    bytes.extend_from_slice(&model_id.0);
    bytes.extend_from_slice(&(execution_profile.len() as u64).to_le_bytes());
    bytes.extend_from_slice(execution_profile.as_bytes());
    bytes.extend_from_slice(&(prompt_tokens.len() as u64).to_le_bytes());
    for token in prompt_tokens {
        bytes.extend_from_slice(&token.to_le_bytes());
    }
    bytes.extend_from_slice(&(generated_tokens.len() as u64).to_le_bytes());
    for token in generated_tokens {
        bytes.extend_from_slice(&token.to_le_bytes());
    }
    arc_crypto::hash_bytes(&bytes)
}

fn validate_model_and_limits(
    boot_nonce: Hash256,
    model: &TrustedModelSpec,
    limits: &CommunityShardLimits,
) -> Result<Vec<ShardLayerRange>, ShardCoordinatorError> {
    if boot_nonce == Hash256::ZERO {
        return Err(ShardCoordinatorError::InvalidConfig(
            "boot_nonce must be nonzero",
        ));
    }
    if model.model_id == Hash256::ZERO {
        return Err(ShardCoordinatorError::InvalidConfig(
            "model_id must be nonzero",
        ));
    }
    if model.execution_profile != CANONICAL_REWARD_INFERENCE_PROFILE {
        return Err(ShardCoordinatorError::InvalidConfig(
            "execution_profile must match canonical reward inference profile",
        ));
    }
    if !(2..=MAX_MODEL_LAYERS_HARD_CAP).contains(&model.n_layers) {
        return Err(ShardCoordinatorError::InvalidConfig(
            "n_layers must be in 2..=1024",
        ));
    }
    if !(1..=MAX_HIDDEN_LEN_HARD_CAP).contains(&limits.max_hidden_len) {
        return Err(ShardCoordinatorError::InvalidConfig(
            "max_hidden_len out of bounds",
        ));
    }
    if !(1..=limits.max_hidden_len).contains(&model.d_model) {
        return Err(ShardCoordinatorError::InvalidConfig(
            "d_model must be nonzero and within max_hidden_len",
        ));
    }
    if !(1..=u32::MAX as usize).contains(&model.vocab_size) {
        return Err(ShardCoordinatorError::InvalidConfig(
            "vocab_size must be in 1..=u32::MAX",
        ));
    }
    if !(2..=MAX_SEQUENCE_POSITIONS_HARD_CAP).contains(&model.max_seq) {
        return Err(ShardCoordinatorError::InvalidConfig(
            "max_seq must be in 2..=32768",
        ));
    }
    if model.bos_token as usize >= model.vocab_size {
        return Err(ShardCoordinatorError::InvalidConfig(
            "bos_token is outside model vocabulary",
        ));
    }
    if model.eos_tokens.len() > MAX_EOS_TOKENS {
        return Err(ShardCoordinatorError::InvalidConfig("too many eos_tokens"));
    }
    if !(2..=MAX_WORKERS_HARD_CAP).contains(&limits.max_workers) {
        return Err(ShardCoordinatorError::InvalidConfig(
            "max_workers must be in 2..=128",
        ));
    }
    if !(1..=MAX_ACTIVE_SESSIONS_HARD_CAP).contains(&limits.max_active_sessions) {
        return Err(ShardCoordinatorError::InvalidConfig(
            "max_active_sessions must be in 1..=64",
        ));
    }
    if !(1..=limits.max_active_sessions).contains(&limits.max_sessions_per_worker) {
        return Err(ShardCoordinatorError::InvalidConfig(
            "max_sessions_per_worker must be in 1..=max_active_sessions",
        ));
    }
    if !(2..=MAX_MODEL_LAYERS_HARD_CAP).contains(&limits.target_partitions) {
        return Err(ShardCoordinatorError::InvalidConfig(
            "target_partitions must be in 2..=1024",
        ));
    }
    if limits.provisioning_ttl_ms == 0
        || limits.ready_ttl_ms == 0
        || limits.step_timeout_ms == 0
        || limits.job_deadline_ms == 0
        || limits.quarantine_ttl_ms == 0
    {
        return Err(ShardCoordinatorError::InvalidConfig(
            "all TTL/timeout bounds must be nonzero",
        ));
    }
    if limits.step_timeout_ms > limits.job_deadline_ms {
        return Err(ShardCoordinatorError::InvalidConfig(
            "step_timeout_ms must not exceed job_deadline_ms",
        ));
    }
    if !(1..=MAX_RETAINED_RESULTS_HARD_CAP).contains(&limits.max_retained_results) {
        return Err(ShardCoordinatorError::InvalidConfig(
            "max_retained_results must be in 1..=256",
        ));
    }
    if !(1..=MAX_CANCELLATIONS_PER_WORKER_HARD_CAP).contains(&limits.max_cancellations_per_worker) {
        return Err(ShardCoordinatorError::InvalidConfig(
            "max_cancellations_per_worker must be in 1..=128",
        ));
    }
    if !(1..=MAX_CANCELLATION_QUEUES_HARD_CAP).contains(&limits.max_cancellation_queues) {
        return Err(ShardCoordinatorError::InvalidConfig(
            "max_cancellation_queues must be in 1..=256",
        ));
    }
    if !(1..=MAX_FAILED_WORKERS_HARD_CAP).contains(&limits.max_failed_workers) {
        return Err(ShardCoordinatorError::InvalidConfig(
            "max_failed_workers must be in 1..=256",
        ));
    }

    let max_tile_layers = model.n_layers.div_ceil(2);
    let min_partitions = model.n_layers.div_ceil(max_tile_layers).max(2);
    let partitions = limits
        .target_partitions
        .max(min_partitions)
        .min(model.n_layers);
    let base = model.n_layers / partitions;
    let rem = model.n_layers % partitions;
    let mut tiles = Vec::with_capacity(partitions);
    let mut cursor = 0usize;
    for index in 0..partitions {
        let len = base + usize::from(index < rem);
        let end = cursor
            .checked_add(len)
            .ok_or(ShardCoordinatorError::InvalidConfig("tile range overflow"))?;
        if len == 0 || len > max_tile_layers || end > model.n_layers {
            return Err(ShardCoordinatorError::InvalidConfig(
                "fixed tiling violated partial-model bounds",
            ));
        }
        tiles.push(ShardLayerRange {
            start_layer: cursor,
            end_layer: end,
        });
        cursor = end;
    }
    if cursor != model.n_layers || tiles.len() < 2 {
        return Err(ShardCoordinatorError::InvalidConfig(
            "fixed tiling failed to partition model into >= 2 slices",
        ));
    }
    Ok(tiles)
}

pub struct CommunityShardCoordinator {
    boot_nonce: Hash256,
    model: TrustedModelSpec,
    limits: CommunityShardLimits,
    tiles: Vec<ShardLayerRange>,
    next_lease_seq: u64,
    next_session_seq: u64,
    next_step_seq: u64,
    next_assignment_seq: u64,
    leases: BTreeMap<[u8; 32], WorkerLease>,
    sessions: BTreeMap<u64, ShardSession>,
    completed_results: BTreeMap<u64, SessionOutcome>,
    completed_order: VecDeque<u64>,
    pending_cancellations: BTreeMap<[u8; 32], VecDeque<SessionCancellationNotice>>,
    cancellation_queue_order: VecDeque<[u8; 32]>,
    quarantined_workers: BTreeMap<[u8; 32], QuarantineRecord>,
    quarantine_order: VecDeque<[u8; 32]>,
}

impl CommunityShardCoordinator {
    pub fn new(
        boot_nonce: Hash256,
        model: TrustedModelSpec,
        limits: CommunityShardLimits,
    ) -> Result<Self, ShardCoordinatorError> {
        let tiles = validate_model_and_limits(boot_nonce, &model, &limits)?;
        Ok(Self {
            boot_nonce,
            model,
            limits,
            tiles,
            next_lease_seq: 0,
            next_session_seq: 0,
            next_step_seq: 0,
            next_assignment_seq: 0,
            leases: BTreeMap::new(),
            sessions: BTreeMap::new(),
            completed_results: BTreeMap::new(),
            completed_order: VecDeque::new(),
            pending_cancellations: BTreeMap::new(),
            cancellation_queue_order: VecDeque::new(),
            quarantined_workers: BTreeMap::new(),
            quarantine_order: VecDeque::new(),
        })
    }

    pub fn boot_nonce(&self) -> Hash256 {
        self.boot_nonce
    }

    pub fn model_spec(&self) -> &TrustedModelSpec {
        &self.model
    }

    pub fn limits(&self) -> &CommunityShardLimits {
        &self.limits
    }

    pub fn fixed_tiles(&self) -> &[ShardLayerRange] {
        &self.tiles
    }

    pub fn request_lease(
        &mut self,
        authenticated_worker: Hash256,
        now_ms: u64,
        req: ShardLeaseRequest,
    ) -> Result<ShardLeaseResponse, ShardCoordinatorError> {
        Self::check_worker_auth(authenticated_worker, req.worker_id)?;
        if req.protocol_version != COMMUNITY_SHARD_PROTOCOL_VERSION {
            return Err(ShardCoordinatorError::UnsupportedProtocolVersion {
                expected: COMMUNITY_SHARD_PROTOCOL_VERSION,
                actual: req.protocol_version,
            });
        }
        self.check_model_fields(req.model_id, &req.execution_profile)?;
        if req.n_layers != self.model.n_layers {
            return Err(ShardCoordinatorError::ModelMismatch("n_layers"));
        }
        if req.d_model != self.model.d_model {
            return Err(ShardCoordinatorError::ModelMismatch("d_model"));
        }
        if req.vocab_size != self.model.vocab_size {
            return Err(ShardCoordinatorError::ModelMismatch("vocab_size"));
        }
        if req.max_seq != self.model.max_seq {
            return Err(ShardCoordinatorError::ModelMismatch("max_seq"));
        }
        let min_tile = self
            .tiles
            .iter()
            .map(|tile| tile.len())
            .min()
            .unwrap_or(usize::MAX);
        if req.max_resident_layers == 0 || req.max_resident_layers < min_tile {
            return Err(ShardCoordinatorError::InsufficientLayerBudget {
                requested: req.max_resident_layers,
                minimum_tile: min_tile,
            });
        }
        let provisioning_deadline = now_ms
            .checked_add(self.limits.provisioning_ttl_ms)
            .ok_or(ShardCoordinatorError::TimestampOverflow)?;

        self.expire_stale(now_ms);

        if self.is_quarantined(authenticated_worker, now_ms) {
            return Err(ShardCoordinatorError::WorkerQuarantined);
        }
        if let Some(existing) = self.leases.get(&authenticated_worker.0) {
            if req.max_resident_layers != existing.max_resident_layers {
                return Err(ShardCoordinatorError::LeaseCapacityConflict {
                    existing: existing.max_resident_layers,
                    requested: req.max_resident_layers,
                });
            }
            return Ok(ShardLeaseResponse {
                protocol_version: COMMUNITY_SHARD_PROTOCOL_VERSION,
                assignment: ShardLeaseAssignment {
                    lease_id: existing.lease_id,
                    lease_epoch: existing.lease_epoch,
                    model_id: self.model.model_id,
                    execution_profile: self.model.execution_profile.clone(),
                    range: existing.range,
                    provisioning_deadline_unix_ms: existing.provisioning_deadline_unix_ms,
                },
                ready_ttl_ms: self.limits.ready_ttl_ms,
            });
        }
        if self.leases.len() >= self.limits.max_workers {
            return Err(ShardCoordinatorError::WorkerCapacityReached);
        }

        let assigned_range = self.select_tile_for_join(req.max_resident_layers).ok_or(
            ShardCoordinatorError::InsufficientLayerBudget {
                requested: req.max_resident_layers,
                minimum_tile: min_tile,
            },
        )?;
        let lease_seq = self
            .next_lease_seq
            .checked_add(1)
            .ok_or(ShardCoordinatorError::CounterOverflow)?;
        let lease_id = LeaseId {
            boot_nonce: self.boot_nonce,
            seq: lease_seq,
        };
        self.next_lease_seq = lease_seq;

        let lease = WorkerLease {
            worker_id: authenticated_worker,
            lease_id,
            lease_epoch: 1,
            range: assigned_range,
            max_resident_layers: req.max_resident_layers,
            provisioning_deadline_unix_ms: provisioning_deadline,
            phase: LeasePhase::Provisioning {
                deadline_unix_ms: provisioning_deadline,
            },
            pending_rebalance_from: None,
            active_sessions: BTreeSet::new(),
            last_assigned_seq: self.next_assignment_seq,
        };
        self.leases.insert(authenticated_worker.0, lease);

        Ok(ShardLeaseResponse {
            protocol_version: COMMUNITY_SHARD_PROTOCOL_VERSION,
            assignment: ShardLeaseAssignment {
                lease_id,
                lease_epoch: 1,
                model_id: self.model.model_id,
                execution_profile: self.model.execution_profile.clone(),
                range: assigned_range,
                provisioning_deadline_unix_ms: provisioning_deadline,
            },
            ready_ttl_ms: self.limits.ready_ttl_ms,
        })
    }

    pub fn heartbeat(
        &mut self,
        authenticated_worker: Hash256,
        now_ms: u64,
        req: ShardHeartbeatRequest,
    ) -> Result<ShardHeartbeatResponse, ShardCoordinatorError> {
        Self::check_worker_auth(authenticated_worker, req.worker_id)?;
        self.check_boot_nonce(req.lease_id.boot_nonce)?;
        self.check_model_fields(req.model_id, &req.execution_profile)?;
        if !req.loaded_range.is_valid_for(self.model.n_layers) {
            return Err(ShardCoordinatorError::LeaseRangeMismatch);
        }
        let next_ready_expiry = now_ms
            .checked_add(self.limits.ready_ttl_ms)
            .ok_or(ShardCoordinatorError::TimestampOverflow)?;

        self.expire_stale(now_ms);

        let lease = self
            .leases
            .get_mut(&authenticated_worker.0)
            .ok_or(ShardCoordinatorError::LeaseNotFound)?;
        if lease.lease_id != req.lease_id {
            return Err(ShardCoordinatorError::LeaseIdMismatch);
        }
        if lease
            .pending_rebalance_from
            .is_some_and(|(prev_epoch, _)| prev_epoch == req.lease_epoch)
        {
            let prev_range = lease
                .pending_rebalance_from
                .map(|(_, range)| range)
                .unwrap_or(lease.range);
            if req.loaded_range != prev_range {
                return Err(ShardCoordinatorError::LeaseRangeMismatch);
            }
            let assignment = ShardLeaseAssignment {
                lease_id: lease.lease_id,
                lease_epoch: lease.lease_epoch,
                model_id: self.model.model_id,
                execution_profile: self.model.execution_profile.clone(),
                range: lease.range,
                provisioning_deadline_unix_ms: lease.provisioning_deadline_unix_ms,
            };
            let cancellations = self.drain_cancellations_for(authenticated_worker);
            return Ok(ShardHeartbeatResponse {
                directive: HeartbeatDirective::Rebalance { assignment },
                cancellations,
            });
        }
        if lease.lease_epoch != req.lease_epoch {
            return Err(ShardCoordinatorError::LeaseEpochMismatch {
                expected: lease.lease_epoch,
                actual: req.lease_epoch,
            });
        }
        if req.loaded_range != lease.range {
            return Err(ShardCoordinatorError::LeaseRangeMismatch);
        }
        lease.pending_rebalance_from = None;

        let transitioned_to_ready = match lease.phase {
            LeasePhase::Provisioning { .. } => {
                if req.ready {
                    lease.phase = LeasePhase::Ready {
                        expires_at_unix_ms: next_ready_expiry,
                    };
                    true
                } else {
                    false
                }
            }
            LeasePhase::Ready { .. } => {
                if !req.ready {
                    return Err(ShardCoordinatorError::ReadyStateRegression);
                }
                lease.phase = LeasePhase::Ready {
                    expires_at_unix_ms: next_ready_expiry,
                };
                false
            }
        };

        if transitioned_to_ready {
            let _ = self.rebalance_idle_workers(now_ms);
        }

        let directive = {
            let lease = self
                .leases
                .get(&authenticated_worker.0)
                .ok_or(ShardCoordinatorError::LeaseNotFound)?;
            if lease.lease_epoch != req.lease_epoch {
                HeartbeatDirective::Rebalance {
                    assignment: ShardLeaseAssignment {
                        lease_id: lease.lease_id,
                        lease_epoch: lease.lease_epoch,
                        model_id: self.model.model_id,
                        execution_profile: self.model.execution_profile.clone(),
                        range: lease.range,
                        provisioning_deadline_unix_ms: lease.provisioning_deadline_unix_ms,
                    },
                }
            } else {
                match lease.phase {
                    LeasePhase::Provisioning { deadline_unix_ms } => {
                        HeartbeatDirective::Provisioning { deadline_unix_ms }
                    }
                    LeasePhase::Ready { expires_at_unix_ms } => {
                        HeartbeatDirective::Active { expires_at_unix_ms }
                    }
                }
            }
        };

        let cancellations = self.drain_cancellations_for(authenticated_worker);
        Ok(ShardHeartbeatResponse {
            directive,
            cancellations,
        })
    }

    pub fn release_lease(
        &mut self,
        authenticated_worker: Hash256,
        now_ms: u64,
        req: ShardReleaseRequest,
    ) -> Result<ShardReleaseResponse, ShardCoordinatorError> {
        Self::check_worker_auth(authenticated_worker, req.worker_id)?;
        self.check_boot_nonce(req.lease_id.boot_nonce)?;

        self.expire_stale(now_ms);

        let lease = self
            .leases
            .get(&authenticated_worker.0)
            .ok_or(ShardCoordinatorError::LeaseNotFound)?;
        if lease.lease_id != req.lease_id {
            return Err(ShardCoordinatorError::LeaseIdMismatch);
        }
        let matches_prev_epoch = lease
            .pending_rebalance_from
            .is_some_and(|(prev_epoch, _)| prev_epoch == req.lease_epoch);
        if lease.lease_epoch != req.lease_epoch && !matches_prev_epoch {
            return Err(ShardCoordinatorError::LeaseEpochMismatch {
                expected: lease.lease_epoch,
                actual: req.lease_epoch,
            });
        }

        let removed = self
            .leases
            .remove(&authenticated_worker.0)
            .ok_or(ShardCoordinatorError::LeaseNotFound)?;
        self.remove_cancellation_queue(&authenticated_worker.0);
        let mut aborted_sessions = Vec::new();
        for session_seq in removed.active_sessions {
            if let Some(session_id) = self.abort_session_internal(
                session_seq,
                now_ms,
                SessionAbortReason::WorkerReleased {
                    worker_id: authenticated_worker,
                },
                Some(authenticated_worker),
            ) {
                aborted_sessions.push(session_id);
            }
        }
        let _ = self.rebalance_idle_workers(now_ms);

        Ok(ShardReleaseResponse {
            released: true,
            aborted_sessions,
        })
    }

    pub fn open_session(
        &mut self,
        now_ms: u64,
        prompt_tokens: Vec<u32>,
        max_tokens: u32,
    ) -> Result<SessionId, ShardCoordinatorError> {
        if prompt_tokens.is_empty() {
            return Err(ShardCoordinatorError::InvalidPrompt(
                "prompt_tokens must be non-empty",
            ));
        }
        if max_tokens == 0 || max_tokens > MAX_GENERATED_TOKENS_HARD_CAP {
            return Err(ShardCoordinatorError::InvalidPrompt(
                "max_tokens must be in 1..=256",
            ));
        }
        let max_tokens_usize = usize::try_from(max_tokens)
            .map_err(|_| ShardCoordinatorError::InvalidPrompt("max_tokens overflow"))?;
        let total_positions = prompt_tokens
            .len()
            .checked_add(max_tokens_usize)
            .and_then(|positions| positions.checked_add(1))
            .ok_or(ShardCoordinatorError::InvalidPrompt(
                "total sequence length overflow",
            ))?;
        if total_positions > self.model.max_seq || total_positions > MAX_SEQUENCE_POSITIONS_HARD_CAP
        {
            return Err(ShardCoordinatorError::InvalidPrompt(
                "BOS + prompt_tokens + max_tokens exceeds sequence position cap",
            ));
        }
        for &token in &prompt_tokens {
            if (token as usize) >= self.model.vocab_size {
                return Err(ShardCoordinatorError::TokenOutOfRange {
                    token_id: token,
                    vocab_size: self.model.vocab_size,
                });
            }
        }
        let job_deadline = now_ms
            .checked_add(self.limits.job_deadline_ms)
            .ok_or(ShardCoordinatorError::TimestampOverflow)?;
        let step_deadline = now_ms
            .checked_add(self.limits.step_timeout_ms)
            .ok_or(ShardCoordinatorError::TimestampOverflow)?
            .min(job_deadline);

        self.expire_stale(now_ms);

        if self.sessions.len() >= self.limits.max_active_sessions {
            return Err(ShardCoordinatorError::SessionCapacityReached);
        }

        let hops = self.select_pipeline_hops(now_ms)?;
        let session_seq = self
            .next_session_seq
            .checked_add(1)
            .ok_or(ShardCoordinatorError::CounterOverflow)?;
        let step_seq = self
            .next_step_seq
            .checked_add(1)
            .ok_or(ShardCoordinatorError::CounterOverflow)?;
        let assignment_seq = self
            .next_assignment_seq
            .checked_add(1)
            .ok_or(ShardCoordinatorError::CounterOverflow)?;

        let session_id = SessionId {
            boot_nonce: self.boot_nonce,
            seq: session_seq,
        };
        let step_id = StepId {
            boot_nonce: self.boot_nonce,
            seq: step_seq,
        };
        let final_pos =
            total_positions
                .checked_sub(1)
                .ok_or(ShardCoordinatorError::InvalidPrompt(
                    "invalid final position",
                ))?;
        let first_hop = &hops[0];
        let first_step = ShardStepWork {
            session_id,
            step_id,
            lease_id: first_hop.lease_id,
            lease_epoch: first_hop.lease_epoch,
            model_id: self.model.model_id,
            execution_profile: self.model.execution_profile.clone(),
            range: first_hop.range,
            position: 0,
            expected_hidden_len: self.model.d_model,
            expect_terminal: hops.len() == 1,
            last_token: final_pos == 0,
            deadline_unix_ms: step_deadline,
            input: ShardStepInput::Token {
                token: self.model.bos_token,
            },
            generated_tokens: Vec::new(),
        };

        self.next_session_seq = session_seq;
        self.next_step_seq = step_seq;
        self.next_assignment_seq = assignment_seq;

        for hop in &hops {
            if let Some(lease) = self.leases.get_mut(&hop.worker_id.0) {
                lease.active_sessions.insert(session_seq);
                lease.last_assigned_seq = assignment_seq;
            }
        }
        self.sessions.insert(
            session_seq,
            ShardSession {
                session_id,
                deadline_unix_ms: job_deadline,
                prompt_tokens,
                max_tokens,
                generated_tokens: Vec::new(),
                terminal_logits_hashes: Vec::new(),
                hops,
                position: 0,
                hop_index: 0,
                outstanding_step: first_step,
            },
        );
        Ok(session_id)
    }

    pub fn cancel_session(
        &mut self,
        now_ms: u64,
        session_id: SessionId,
        reason: SessionAbortReason,
    ) -> Result<(), ShardCoordinatorError> {
        self.check_boot_nonce(session_id.boot_nonce)?;
        self.expire_stale(now_ms);
        if !self.sessions.contains_key(&session_id.seq) {
            return Err(ShardCoordinatorError::SessionNotFound);
        }
        let _ = self.abort_session_internal(session_id.seq, now_ms, reason, None);
        let _ = self.rebalance_idle_workers(now_ms);
        Ok(())
    }

    pub fn claim_step(
        &mut self,
        authenticated_worker: Hash256,
        now_ms: u64,
        req: ShardClaimStepRequest,
    ) -> Result<ShardClaimStepResponse, ShardCoordinatorError> {
        Self::check_worker_auth(authenticated_worker, req.worker_id)?;
        self.check_boot_nonce(req.lease_id.boot_nonce)?;
        self.check_model_fields(req.model_id, &req.execution_profile)?;

        self.expire_stale(now_ms);

        let lease = self
            .leases
            .get(&authenticated_worker.0)
            .ok_or(ShardCoordinatorError::LeaseNotFound)?;
        if lease.lease_id != req.lease_id {
            return Err(ShardCoordinatorError::LeaseIdMismatch);
        }
        if lease
            .pending_rebalance_from
            .is_some_and(|(prev_epoch, _)| prev_epoch == req.lease_epoch)
        {
            let assignment = ShardLeaseAssignment {
                lease_id: lease.lease_id,
                lease_epoch: lease.lease_epoch,
                model_id: self.model.model_id,
                execution_profile: self.model.execution_profile.clone(),
                range: lease.range,
                provisioning_deadline_unix_ms: lease.provisioning_deadline_unix_ms,
            };
            return Ok(ShardClaimStepResponse::RebalanceRequired { assignment });
        }
        if lease.lease_epoch != req.lease_epoch {
            return Err(ShardCoordinatorError::LeaseEpochMismatch {
                expected: lease.lease_epoch,
                actual: req.lease_epoch,
            });
        }
        if !matches!(lease.phase, LeasePhase::Ready { .. }) {
            return Err(ShardCoordinatorError::LeaseNotReady);
        }

        let cancellations = self.drain_cancellations_for(authenticated_worker);
        if !cancellations.is_empty() {
            return Ok(ShardClaimStepResponse::CancelSessions { cancellations });
        }

        for session in self.sessions.values() {
            let hop = &session.hops[session.hop_index];
            if hop.worker_id == authenticated_worker
                && hop.lease_id == req.lease_id
                && hop.lease_epoch == req.lease_epoch
            {
                return Ok(ShardClaimStepResponse::Step {
                    work: Box::new(session.outstanding_step.clone()),
                });
            }
        }
        Ok(ShardClaimStepResponse::Idle)
    }

    pub fn submit_step(
        &mut self,
        authenticated_worker: Hash256,
        now_ms: u64,
        req: ShardStepSubmitRequest,
    ) -> Result<ShardStepSubmitResponse, ShardCoordinatorError> {
        Self::check_worker_auth(authenticated_worker, req.worker_id)?;
        self.check_boot_nonce(req.lease_id.boot_nonce)?;
        self.check_boot_nonce(req.session_id.boot_nonce)?;
        self.check_boot_nonce(req.step_id.boot_nonce)?;
        self.check_model_fields(req.model_id, &req.execution_profile)?;
        if !req.range.is_valid_for(self.model.n_layers) {
            return Err(ShardCoordinatorError::LeaseRangeMismatch);
        }
        match &req.outcome {
            ShardStepOutcome::Declined { reason } | ShardStepOutcome::Failed { reason } => {
                if req.layers_processed > req.range.len() {
                    return Err(ShardCoordinatorError::InvalidLayersProcessed {
                        expected: req.range.len(),
                        actual: req.layers_processed,
                    });
                }
                if reason.len() > MAX_REASON_BYTES {
                    return Err(ShardCoordinatorError::ReasonTooLong);
                }
            }
            ShardStepOutcome::Completed { .. } => {
                if req.layers_processed != req.range.len() {
                    return Err(ShardCoordinatorError::InvalidLayersProcessed {
                        expected: req.range.len(),
                        actual: req.layers_processed,
                    });
                }
            }
        }

        let next_step_deadline_candidate = now_ms
            .checked_add(self.limits.step_timeout_ms)
            .ok_or(ShardCoordinatorError::TimestampOverflow)?;

        self.expire_stale(now_ms);

        let lease = self
            .leases
            .get(&authenticated_worker.0)
            .ok_or(ShardCoordinatorError::LeaseNotFound)?;
        if lease.lease_id != req.lease_id {
            return Err(ShardCoordinatorError::LeaseIdMismatch);
        }
        if lease.lease_epoch != req.lease_epoch {
            return Err(ShardCoordinatorError::LeaseEpochMismatch {
                expected: lease.lease_epoch,
                actual: req.lease_epoch,
            });
        }
        if !matches!(lease.phase, LeasePhase::Ready { .. }) {
            return Err(ShardCoordinatorError::LeaseNotReady);
        }
        if lease.range != req.range {
            return Err(ShardCoordinatorError::LeaseRangeMismatch);
        }

        let session = self
            .sessions
            .get(&req.session_id.seq)
            .ok_or(ShardCoordinatorError::SessionNotFound)?;
        let current_hop = &session.hops[session.hop_index];
        if current_hop.worker_id != authenticated_worker
            || current_hop.lease_id != req.lease_id
            || current_hop.lease_epoch != req.lease_epoch
            || session.outstanding_step.step_id != req.step_id
            || session.outstanding_step.position != req.position
            || session.outstanding_step.range != req.range
        {
            return Err(ShardCoordinatorError::UnexpectedStep);
        }
        if now_ms >= session.deadline_unix_ms || now_ms >= session.outstanding_step.deadline_unix_ms
        {
            return Err(ShardCoordinatorError::StepDeadlineExpired);
        }

        let expect_terminal = session.outstanding_step.expect_terminal;
        let validated_output = match req.outcome {
            ShardStepOutcome::Declined { reason } => {
                let _ = self.abort_session_internal(
                    req.session_id.seq,
                    now_ms,
                    SessionAbortReason::WorkerDeclined {
                        worker_id: authenticated_worker,
                        detail: reason,
                    },
                    None,
                );
                let _ = self.rebalance_idle_workers(now_ms);
                return Ok(ShardStepSubmitResponse {
                    accepted: true,
                    session_completed: false,
                    session_aborted: true,
                });
            }
            ShardStepOutcome::Failed { reason } => {
                let _ = self.abort_session_internal(
                    req.session_id.seq,
                    now_ms,
                    SessionAbortReason::WorkerFailed {
                        worker_id: authenticated_worker,
                        detail: reason,
                    },
                    None,
                );
                let _ = self.rebalance_idle_workers(now_ms);
                return Ok(ShardStepSubmitResponse {
                    accepted: true,
                    session_completed: false,
                    session_aborted: true,
                });
            }
            ShardStepOutcome::Completed { output } => match (expect_terminal, output) {
                (
                    false,
                    ShardStepOutput::Hidden {
                        hidden,
                        hidden_hash,
                    },
                ) => {
                    if hidden.len() != self.model.d_model {
                        return Err(ShardCoordinatorError::InvalidHiddenLength {
                            expected: self.model.d_model,
                            actual: hidden.len(),
                        });
                    }
                    if !hidden_values_within_bounds(&hidden) {
                        return Err(ShardCoordinatorError::HiddenValueOutOfBounds);
                    }
                    if hidden_hash == Hash256::ZERO {
                        return Err(ShardCoordinatorError::ZeroDigest);
                    }
                    if hidden_state_hash(&hidden) != hidden_hash {
                        return Err(ShardCoordinatorError::HiddenHashMismatch);
                    }
                    ShardStepOutput::Hidden {
                        hidden,
                        hidden_hash,
                    }
                }
                (
                    true,
                    ShardStepOutput::Token {
                        token_id,
                        logits_hash,
                    },
                ) => {
                    if (token_id as usize) >= self.model.vocab_size {
                        return Err(ShardCoordinatorError::TokenOutOfRange {
                            token_id,
                            vocab_size: self.model.vocab_size,
                        });
                    }
                    if logits_hash == Hash256::ZERO {
                        return Err(ShardCoordinatorError::ZeroDigest);
                    }
                    ShardStepOutput::Token {
                        token_id,
                        logits_hash,
                    }
                }
                (expected_terminal, _) => {
                    return Err(ShardCoordinatorError::TerminalModeMismatch { expected_terminal });
                }
            },
        };

        match validated_output {
            ShardStepOutput::Hidden {
                hidden,
                hidden_hash,
            } => {
                let next_step_seq = self
                    .next_step_seq
                    .checked_add(1)
                    .ok_or(ShardCoordinatorError::CounterOverflow)?;
                let session = self
                    .sessions
                    .get_mut(&req.session_id.seq)
                    .ok_or(ShardCoordinatorError::SessionNotFound)?;
                let next_hop_index = session
                    .hop_index
                    .checked_add(1)
                    .ok_or(ShardCoordinatorError::CounterOverflow)?;
                let next_hop = &session.hops[next_hop_index];
                let next_expect_terminal = next_hop_index + 1 == session.hops.len();
                let next_history = if next_expect_terminal {
                    let start = session
                        .generated_tokens
                        .len()
                        .saturating_sub(REPETITION_PENALTY_WINDOW);
                    session.generated_tokens[start..].to_vec()
                } else {
                    Vec::new()
                };
                let step_deadline = next_step_deadline_candidate.min(session.deadline_unix_ms);
                session.hop_index = next_hop_index;
                session.outstanding_step = ShardStepWork {
                    session_id: session.session_id,
                    step_id: StepId {
                        boot_nonce: self.boot_nonce,
                        seq: next_step_seq,
                    },
                    lease_id: next_hop.lease_id,
                    lease_epoch: next_hop.lease_epoch,
                    model_id: self.model.model_id,
                    execution_profile: self.model.execution_profile.clone(),
                    range: next_hop.range,
                    position: session.position,
                    expected_hidden_len: self.model.d_model,
                    expect_terminal: next_expect_terminal,
                    last_token: session.outstanding_step.last_token,
                    deadline_unix_ms: step_deadline,
                    input: ShardStepInput::Hidden {
                        hidden,
                        hidden_hash,
                    },
                    generated_tokens: next_history,
                };
                self.next_step_seq = next_step_seq;
                Ok(ShardStepSubmitResponse {
                    accepted: true,
                    session_completed: false,
                    session_aborted: false,
                })
            }
            ShardStepOutput::Token {
                token_id,
                logits_hash,
            } => {
                let (
                    finish_mode,
                    next_input_token,
                    next_position,
                    final_pos,
                    was_last_token,
                    push_token,
                ) = {
                    let session = self
                        .sessions
                        .get(&req.session_id.seq)
                        .ok_or(ShardCoordinatorError::SessionNotFound)?;
                    let total_positions = session
                        .prompt_tokens
                        .len()
                        .checked_add(session.max_tokens as usize)
                        .and_then(|positions| positions.checked_add(1))
                        .ok_or(ShardCoordinatorError::CounterOverflow)?;
                    let final_pos = total_positions
                        .checked_sub(1)
                        .ok_or(ShardCoordinatorError::CounterOverflow)?;
                    let was_last = session.outstanding_step.last_token;
                    let is_prefill_only = session.position <= session.prompt_tokens.len();
                    if is_prefill_only {
                        let next_pos = session
                            .position
                            .checked_add(1)
                            .ok_or(ShardCoordinatorError::CounterOverflow)?;
                        let next_tok = session.prompt_tokens
                            [session.position.min(session.prompt_tokens.len() - 1)];
                        (None, next_tok, next_pos, final_pos, was_last, false)
                    } else if self.model.eos_tokens.contains(&token_id) {
                        (Some(true), 0, session.position, final_pos, was_last, true)
                    } else {
                        let next_generated_len = session
                            .generated_tokens
                            .len()
                            .checked_add(1)
                            .ok_or(ShardCoordinatorError::CounterOverflow)?;
                        if next_generated_len >= session.max_tokens as usize {
                            (Some(false), 0, session.position, final_pos, was_last, true)
                        } else {
                            let next_pos = session
                                .position
                                .checked_add(1)
                                .ok_or(ShardCoordinatorError::CounterOverflow)?;
                            (None, token_id, next_pos, final_pos, was_last, true)
                        }
                    }
                };

                if let Some(stopped_by_eos) = finish_mode {
                    let mut completed = self
                        .sessions
                        .remove(&req.session_id.seq)
                        .ok_or(ShardCoordinatorError::SessionNotFound)?;
                    if push_token {
                        completed.generated_tokens.push(token_id);
                        completed.terminal_logits_hashes.push(logits_hash);
                    }
                    let participants = self.release_session_hops(
                        &completed,
                        now_ms,
                        if stopped_by_eos && !was_last_token {
                            Some(SessionAbortReason::CompletedEarlyEos)
                        } else {
                            None
                        },
                        None,
                    );
                    let digest = unverified_tokens_digest(
                        self.model.model_id,
                        &self.model.execution_profile,
                        &completed.prompt_tokens,
                        &completed.generated_tokens,
                    );
                    self.record_outcome(
                        completed.session_id.seq,
                        SessionOutcome::PendingVerification(Box::new(PendingVerificationResult {
                            session_id: completed.session_id,
                            model_id: self.model.model_id,
                            execution_profile: self.model.execution_profile.clone(),
                            prompt_tokens: completed.prompt_tokens,
                            unverified_generated_tokens: completed.generated_tokens,
                            unverified_output_digest: digest,
                            terminal_logits_hashes: completed.terminal_logits_hashes,
                            stopped_by_eos,
                            participants,
                            verification_required: true,
                            reward_eligible: false,
                        })),
                    );
                    let _ = self.rebalance_idle_workers(now_ms);
                    return Ok(ShardStepSubmitResponse {
                        accepted: true,
                        session_completed: true,
                        session_aborted: false,
                    });
                }

                let next_step_seq = self
                    .next_step_seq
                    .checked_add(1)
                    .ok_or(ShardCoordinatorError::CounterOverflow)?;
                let session = self
                    .sessions
                    .get_mut(&req.session_id.seq)
                    .ok_or(ShardCoordinatorError::SessionNotFound)?;
                if push_token {
                    session.generated_tokens.push(token_id);
                    session.terminal_logits_hashes.push(logits_hash);
                }
                let first_hop = &session.hops[0];
                let step_deadline = next_step_deadline_candidate.min(session.deadline_unix_ms);
                session.position = next_position;
                session.hop_index = 0;
                session.outstanding_step = ShardStepWork {
                    session_id: session.session_id,
                    step_id: StepId {
                        boot_nonce: self.boot_nonce,
                        seq: next_step_seq,
                    },
                    lease_id: first_hop.lease_id,
                    lease_epoch: first_hop.lease_epoch,
                    model_id: self.model.model_id,
                    execution_profile: self.model.execution_profile.clone(),
                    range: first_hop.range,
                    position: next_position,
                    expected_hidden_len: self.model.d_model,
                    expect_terminal: session.hops.len() == 1,
                    last_token: next_position == final_pos,
                    deadline_unix_ms: step_deadline,
                    input: ShardStepInput::Token {
                        token: next_input_token,
                    },
                    generated_tokens: Vec::new(),
                };
                self.next_step_seq = next_step_seq;
                Ok(ShardStepSubmitResponse {
                    accepted: true,
                    session_completed: false,
                    session_aborted: false,
                })
            }
        }
    }

    pub fn expire_stale(&mut self, now_ms: u64) -> ExpirationSummary {
        let mut pruned_quarantines = 0usize;
        let expired_quarantines: Vec<[u8; 32]> = self
            .quarantined_workers
            .iter()
            .filter(|(_, record)| now_ms >= record.expires_at_unix_ms)
            .map(|(key, _)| *key)
            .collect();
        for key in expired_quarantines {
            if self.quarantined_workers.remove(&key).is_some() {
                pruned_quarantines += 1;
            }
        }
        self.quarantine_order
            .retain(|key| self.quarantined_workers.contains_key(key));

        let mut pruned_cancellations = 0usize;
        let mut empty_queues = Vec::new();
        for (worker_key, queue) in &mut self.pending_cancellations {
            let before = queue.len();
            queue.retain(|notice| now_ms < notice.expires_at_unix_ms);
            pruned_cancellations += before - queue.len();
            if queue.is_empty() {
                empty_queues.push(*worker_key);
            }
        }
        for key in empty_queues {
            self.pending_cancellations.remove(&key);
        }
        self.cancellation_queue_order
            .retain(|key| self.pending_cancellations.contains_key(key));

        let expired_worker_keys: Vec<[u8; 32]> = self
            .leases
            .iter()
            .filter(|(_, lease)| match lease.phase {
                LeasePhase::Provisioning { deadline_unix_ms } => now_ms >= deadline_unix_ms,
                LeasePhase::Ready { expires_at_unix_ms } => now_ms >= expires_at_unix_ms,
            })
            .map(|(key, _)| *key)
            .collect();
        let mut expired_leases = 0usize;
        let mut aborted_sessions = 0usize;
        for key in expired_worker_keys {
            if let Some(removed) = self.leases.remove(&key) {
                expired_leases += 1;
                self.remove_cancellation_queue(&key);
                for session_seq in removed.active_sessions {
                    if self
                        .abort_session_internal(
                            session_seq,
                            now_ms,
                            SessionAbortReason::WorkerLeaseExpired {
                                worker_id: removed.worker_id,
                            },
                            Some(removed.worker_id),
                        )
                        .is_some()
                    {
                        aborted_sessions += 1;
                    }
                }
            }
        }

        let expired_session_specs: Vec<(u64, SessionAbortReason)> = self
            .sessions
            .iter()
            .filter_map(|(seq, session)| {
                if now_ms >= session.deadline_unix_ms {
                    Some((*seq, SessionAbortReason::JobDeadlineExceeded))
                } else if now_ms >= session.outstanding_step.deadline_unix_ms {
                    let worker_id = session.hops[session.hop_index].worker_id;
                    Some((*seq, SessionAbortReason::StepDeadlineExceeded { worker_id }))
                } else {
                    None
                }
            })
            .collect();
        for (seq, reason) in expired_session_specs {
            if self
                .abort_session_internal(seq, now_ms, reason, None)
                .is_some()
            {
                aborted_sessions += 1;
            }
        }

        let rebalanced_workers = if expired_leases > 0 || aborted_sessions > 0 {
            self.rebalance_idle_workers(now_ms)
        } else {
            0
        };

        ExpirationSummary {
            expired_leases,
            aborted_sessions,
            pruned_cancellations,
            pruned_quarantines,
            rebalanced_workers,
        }
    }

    pub fn quarantine_worker(
        &mut self,
        worker_id: Hash256,
        now_ms: u64,
        reason: WorkerFaultReason,
    ) -> Result<(), ShardCoordinatorError> {
        if worker_id == Hash256::ZERO {
            return Err(ShardCoordinatorError::InvalidWorkerIdentity);
        }
        let expires_at_unix_ms = now_ms
            .checked_add(self.limits.quarantine_ttl_ms)
            .ok_or(ShardCoordinatorError::TimestampOverflow)?;
        self.expire_stale(now_ms);

        if let Some(removed) = self.leases.remove(&worker_id.0) {
            self.remove_cancellation_queue(&worker_id.0);
            for session_seq in removed.active_sessions {
                let _ = self.abort_session_internal(
                    session_seq,
                    now_ms,
                    SessionAbortReason::WorkerQuarantined { worker_id },
                    Some(worker_id),
                );
            }
        }
        if self.quarantined_workers.contains_key(&worker_id.0) {
            self.quarantine_order.retain(|key| *key != worker_id.0);
        } else {
            while self.quarantined_workers.len() >= self.limits.max_failed_workers {
                let Some(oldest) = self.quarantine_order.pop_front() else {
                    break;
                };
                self.quarantined_workers.remove(&oldest);
            }
        }
        self.quarantine_order.push_back(worker_id.0);
        self.quarantined_workers.insert(
            worker_id.0,
            QuarantineRecord {
                reason,
                expires_at_unix_ms,
            },
        );
        let _ = self.rebalance_idle_workers(now_ms);
        Ok(())
    }

    pub fn take_result(&mut self, session_id: SessionId) -> Option<SessionOutcome> {
        if session_id.boot_nonce != self.boot_nonce {
            return None;
        }
        let outcome = self.completed_results.remove(&session_id.seq)?;
        self.completed_order.retain(|seq| *seq != session_id.seq);
        Some(outcome)
    }

    pub fn peek_result(&self, session_id: SessionId) -> Option<&SessionOutcome> {
        if session_id.boot_nonce != self.boot_nonce {
            return None;
        }
        self.completed_results.get(&session_id.seq)
    }

    pub fn worker_lease_snapshot(&self, worker_id: Hash256) -> Option<WorkerLeaseSnapshot> {
        let lease = self.leases.get(&worker_id.0)?;
        let (ready, deadline_unix_ms) = match lease.phase {
            LeasePhase::Provisioning { deadline_unix_ms } => (false, deadline_unix_ms),
            LeasePhase::Ready { expires_at_unix_ms } => (true, expires_at_unix_ms),
        };
        Some(WorkerLeaseSnapshot {
            worker_id: lease.worker_id,
            lease_id: lease.lease_id,
            lease_epoch: lease.lease_epoch,
            range: lease.range,
            ready,
            deadline_unix_ms,
            active_sessions: lease.active_sessions.len(),
            max_resident_layers: lease.max_resident_layers,
        })
    }

    pub fn worker_quarantine_reason(
        &self,
        worker_id: Hash256,
        now_ms: u64,
    ) -> Option<WorkerFaultReason> {
        self.quarantined_workers
            .get(&worker_id.0)
            .filter(|record| now_ms < record.expires_at_unix_ms)
            .map(|record| record.reason.clone())
    }

    pub fn session_status(&self, session_id: SessionId) -> Option<SessionStatusSnapshot> {
        if session_id.boot_nonce != self.boot_nonce {
            return None;
        }
        let session = self.sessions.get(&session_id.seq)?;
        Some(SessionStatusSnapshot {
            session_id: session.session_id,
            position: session.position,
            hop_index: session.hop_index,
            generated_tokens_so_far: session.generated_tokens.len(),
            deadline_unix_ms: session.deadline_unix_ms,
            expected_worker: session.hops[session.hop_index].worker_id,
            expected_step_id: session.outstanding_step.step_id,
        })
    }

    pub fn status_snapshot(&self, now_ms: u64) -> CoordinatorStatusSnapshot {
        let mut ready_workers = 0usize;
        let mut provisioning_workers = 0usize;
        let mut tile_counts: BTreeMap<ShardLayerRange, (usize, usize)> = self
            .tiles
            .iter()
            .map(|tile| (*tile, (0usize, 0usize)))
            .collect();
        for lease in self.leases.values() {
            match lease.phase {
                LeasePhase::Ready { expires_at_unix_ms } if now_ms < expires_at_unix_ms => {
                    ready_workers += 1;
                    if let Some(entry) = tile_counts.get_mut(&lease.range) {
                        entry.0 += 1;
                    }
                }
                LeasePhase::Provisioning { deadline_unix_ms } if now_ms < deadline_unix_ms => {
                    provisioning_workers += 1;
                    if let Some(entry) = tile_counts.get_mut(&lease.range) {
                        entry.1 += 1;
                    }
                }
                _ => {}
            }
        }
        let tiles: Vec<TileCoverageStatus> = self
            .tiles
            .iter()
            .map(|tile| {
                let (ready, provisioning) = tile_counts.get(tile).copied().unwrap_or((0, 0));
                TileCoverageStatus {
                    range: *tile,
                    ready_workers: ready,
                    provisioning_workers: provisioning,
                }
            })
            .collect();
        let fully_covered_ready = tiles.iter().all(|tile| tile.ready_workers > 0);
        let queued_cancellations = self
            .pending_cancellations
            .values()
            .map(|queue| {
                queue
                    .iter()
                    .filter(|n| now_ms < n.expires_at_unix_ms)
                    .count()
            })
            .sum();
        let quarantined_workers = self
            .quarantined_workers
            .values()
            .filter(|record| now_ms < record.expires_at_unix_ms)
            .count();
        CoordinatorStatusSnapshot {
            boot_nonce: self.boot_nonce,
            model_id: self.model.model_id,
            execution_profile: self.model.execution_profile.clone(),
            tiles,
            fully_covered_ready,
            ready_workers,
            provisioning_workers,
            active_sessions: self.sessions.len(),
            retained_results: self.completed_results.len(),
            queued_cancellations,
            quarantined_workers,
        }
    }

    pub fn pipeline_capacity(&self, now_ms: u64) -> ShardPipelineCapacity {
        let mut ready_slots_by_tile: BTreeMap<ShardLayerRange, usize> =
            self.tiles.iter().map(|tile| (*tile, 0usize)).collect();
        let mut idle_slots_by_tile: BTreeMap<ShardLayerRange, usize> =
            self.tiles.iter().map(|tile| (*tile, 0usize)).collect();
        for lease in self.leases.values() {
            let is_ready = matches!(
                lease.phase,
                LeasePhase::Ready { expires_at_unix_ms } if now_ms < expires_at_unix_ms
            );
            if !is_ready || self.is_quarantined(lease.worker_id, now_ms) {
                continue;
            }
            if let Some(ready_slots) = ready_slots_by_tile.get_mut(&lease.range) {
                *ready_slots = ready_slots.saturating_add(self.limits.max_sessions_per_worker);
            }
            let remaining_slots = self
                .limits
                .max_sessions_per_worker
                .saturating_sub(lease.active_sessions.len());
            if let Some(idle_slots) = idle_slots_by_tile.get_mut(&lease.range) {
                *idle_slots = idle_slots.saturating_add(remaining_slots);
            }
        }
        let ready_complete_pipelines = self
            .tiles
            .iter()
            .map(|tile| ready_slots_by_tile.get(tile).copied().unwrap_or(0))
            .min()
            .unwrap_or(0);
        let idle_complete_pipelines = self
            .tiles
            .iter()
            .map(|tile| idle_slots_by_tile.get(tile).copied().unwrap_or(0))
            .min()
            .unwrap_or(0);
        let effective_session_capacity = ready_complete_pipelines
            .min(self.limits.max_active_sessions)
            .min(MAX_ACTIVE_SESSIONS_HARD_CAP);
        ShardPipelineCapacity {
            ready_complete_pipelines,
            idle_complete_pipelines,
            active_sessions: self.sessions.len(),
            max_active_sessions: self.limits.max_active_sessions,
            effective_session_capacity,
        }
    }

    fn check_worker_auth(
        authenticated_worker: Hash256,
        payload_worker: Hash256,
    ) -> Result<(), ShardCoordinatorError> {
        if authenticated_worker == Hash256::ZERO || payload_worker == Hash256::ZERO {
            return Err(ShardCoordinatorError::InvalidWorkerIdentity);
        }
        if authenticated_worker != payload_worker {
            return Err(ShardCoordinatorError::WorkerIdentityMismatch);
        }
        Ok(())
    }

    fn check_boot_nonce(&self, boot_nonce: Hash256) -> Result<(), ShardCoordinatorError> {
        if boot_nonce != self.boot_nonce {
            return Err(ShardCoordinatorError::BootNonceMismatch);
        }
        Ok(())
    }

    fn check_model_fields(
        &self,
        model_id: Hash256,
        execution_profile: &str,
    ) -> Result<(), ShardCoordinatorError> {
        if model_id != self.model.model_id {
            return Err(ShardCoordinatorError::ModelMismatch("model_id"));
        }
        if execution_profile != self.model.execution_profile {
            return Err(ShardCoordinatorError::ModelMismatch("execution_profile"));
        }
        Ok(())
    }

    fn is_quarantined(&self, worker_id: Hash256, now_ms: u64) -> bool {
        self.quarantined_workers
            .get(&worker_id.0)
            .is_some_and(|record| now_ms < record.expires_at_unix_ms)
    }

    fn select_tile_for_join(&self, max_resident_layers: usize) -> Option<ShardLayerRange> {
        let mut counts: BTreeMap<ShardLayerRange, usize> =
            self.tiles.iter().map(|tile| (*tile, 0usize)).collect();
        for lease in self.leases.values() {
            if let Some(count) = counts.get_mut(&lease.range) {
                *count += 1;
            }
        }
        self.tiles
            .iter()
            .copied()
            .filter(|tile| tile.len() <= max_resident_layers)
            .min_by_key(|tile| {
                let holders = counts.get(tile).copied().unwrap_or(0);
                (holders, tile.start_layer, tile.end_layer)
            })
    }

    fn select_pipeline_hops(&self, now_ms: u64) -> Result<Vec<SessionHop>, ShardCoordinatorError> {
        let mut hops = Vec::with_capacity(self.tiles.len());
        let mut used_workers: BTreeSet<[u8; 32]> = BTreeSet::new();
        for &tile in &self.tiles {
            let chosen = self
                .leases
                .values()
                .filter(|lease| {
                    lease.range == tile
                        && matches!(lease.phase, LeasePhase::Ready { expires_at_unix_ms } if now_ms < expires_at_unix_ms)
                        && lease.active_sessions.len() < self.limits.max_sessions_per_worker
                        && !used_workers.contains(&lease.worker_id.0)
                        && !self.is_quarantined(lease.worker_id, now_ms)
                })
                .min_by_key(|lease| {
                    (
                        lease.active_sessions.len(),
                        lease.last_assigned_seq,
                        lease.worker_id.0,
                    )
                })
                .ok_or(ShardCoordinatorError::InsufficientShardCoverage {
                    missing_range: tile,
                })?;
            used_workers.insert(chosen.worker_id.0);
            hops.push(SessionHop {
                worker_id: chosen.worker_id,
                lease_id: chosen.lease_id,
                lease_epoch: chosen.lease_epoch,
                range: chosen.range,
            });
        }
        Ok(hops)
    }

    fn select_rebalance_move(
        &self,
        now_ms: u64,
        total_by_tile: &BTreeMap<ShardLayerRange, usize>,
        ready_by_tile: &BTreeMap<ShardLayerRange, usize>,
    ) -> Option<(ShardLayerRange, [u8; 32])> {
        let mut target_tiles = self.tiles.clone();
        target_tiles.sort_by_key(|tile| {
            (
                total_by_tile.get(tile).copied().unwrap_or(0),
                ready_by_tile.get(tile).copied().unwrap_or(0),
                tile.start_layer,
                tile.end_layer,
            )
        });
        let idle_ready_leases: Vec<&WorkerLease> = self
            .leases
            .values()
            .filter(|lease| {
                matches!(
                    lease.phase,
                    LeasePhase::Ready { expires_at_unix_ms } if now_ms < expires_at_unix_ms
                ) && lease.active_sessions.is_empty()
                    && lease.lease_epoch.checked_add(1).is_some()
            })
            .collect();

        for target_tile in target_tiles {
            let target_total = total_by_tile.get(&target_tile).copied().unwrap_or(0);
            let target_ready = ready_by_tile.get(&target_tile).copied().unwrap_or(0);
            let mut visited = BTreeSet::new();
            visited.insert(target_tile);
            let mut frontier = vec![target_tile];

            while !frontier.is_empty() {
                let mut best_move: Option<(ShardLayerRange, &WorkerLease)> = None;
                for &dest_tile in &frontier {
                    for &lease in &idle_ready_leases {
                        if visited.contains(&lease.range)
                            || lease.max_resident_layers < dest_tile.len()
                        {
                            continue;
                        }
                        let source_total = total_by_tile.get(&lease.range).copied().unwrap_or(0);
                        let source_ready = ready_by_tile.get(&lease.range).copied().unwrap_or(0);
                        if source_ready < 2
                            || source_total < target_total.saturating_add(2)
                            || source_ready <= target_ready
                        {
                            continue;
                        }
                        let replace = match best_move {
                            None => true,
                            Some((best_dest, best_lease)) => {
                                let best_ready =
                                    ready_by_tile.get(&best_lease.range).copied().unwrap_or(0);
                                let best_total =
                                    total_by_tile.get(&best_lease.range).copied().unwrap_or(0);
                                let dest_total =
                                    total_by_tile.get(&dest_tile).copied().unwrap_or(0);
                                let best_dest_total =
                                    total_by_tile.get(&best_dest).copied().unwrap_or(0);
                                source_ready
                                    .cmp(&best_ready)
                                    .then_with(|| source_total.cmp(&best_total))
                                    .then_with(|| best_dest_total.cmp(&dest_total))
                                    .then_with(|| dest_tile.len().cmp(&best_dest.len()))
                                    .then_with(|| {
                                        best_lease
                                            .max_resident_layers
                                            .cmp(&lease.max_resident_layers)
                                    })
                                    .then_with(|| best_lease.worker_id.0.cmp(&lease.worker_id.0))
                                    .then_with(|| best_dest.cmp(&dest_tile))
                                    .is_gt()
                            }
                        };
                        if replace {
                            best_move = Some((dest_tile, lease));
                        }
                    }
                }
                if let Some((dest_tile, lease)) = best_move {
                    return Some((dest_tile, lease.worker_id.0));
                }

                let mut next_frontier = Vec::new();
                for &dest_tile in &frontier {
                    for &lease in &idle_ready_leases {
                        if visited.contains(&lease.range)
                            || lease.max_resident_layers < dest_tile.len()
                        {
                            continue;
                        }
                        let tile_total = total_by_tile.get(&lease.range).copied().unwrap_or(0);
                        if tile_total <= target_total.saturating_add(1)
                            && visited.insert(lease.range)
                        {
                            next_frontier.push(lease.range);
                        }
                    }
                }
                next_frontier.sort();
                frontier = next_frontier;
            }
        }
        None
    }

    fn rebalance_idle_workers(&mut self, now_ms: u64) -> usize {
        let Some(provisioning_deadline) = now_ms.checked_add(self.limits.provisioning_ttl_ms)
        else {
            return 0;
        };
        let mut rebalanced = 0usize;
        loop {
            let mut total_by_tile: BTreeMap<ShardLayerRange, usize> =
                self.tiles.iter().map(|tile| (*tile, 0usize)).collect();
            let mut ready_by_tile: BTreeMap<ShardLayerRange, usize> =
                self.tiles.iter().map(|tile| (*tile, 0usize)).collect();
            for lease in self.leases.values() {
                if let Some(count) = total_by_tile.get_mut(&lease.range) {
                    *count += 1;
                }
                match (lease.phase, ready_by_tile.get_mut(&lease.range)) {
                    (LeasePhase::Ready { expires_at_unix_ms }, Some(count))
                        if now_ms < expires_at_unix_ms =>
                    {
                        *count += 1;
                    }
                    _ => {}
                }
            }
            let Some((target_tile, candidate_key)) =
                self.select_rebalance_move(now_ms, &total_by_tile, &ready_by_tile)
            else {
                break;
            };
            let Some(lease) = self.leases.get_mut(&candidate_key) else {
                break;
            };
            let Some(next_epoch) = lease.lease_epoch.checked_add(1) else {
                break;
            };
            let previous_epoch = lease.lease_epoch;
            let previous_range = lease.range;
            lease.lease_epoch = next_epoch;
            lease.range = target_tile;
            lease.provisioning_deadline_unix_ms = provisioning_deadline;
            lease.phase = LeasePhase::Provisioning {
                deadline_unix_ms: provisioning_deadline,
            };
            lease.pending_rebalance_from = Some((previous_epoch, previous_range));
            rebalanced += 1;
        }
        rebalanced
    }

    fn abort_session_internal(
        &mut self,
        session_seq: u64,
        now_ms: u64,
        reason: SessionAbortReason,
        exclude_cancel_worker: Option<Hash256>,
    ) -> Option<SessionId> {
        let session = self.sessions.remove(&session_seq)?;
        let participants = self.release_session_hops(
            &session,
            now_ms,
            Some(reason.clone()),
            exclude_cancel_worker,
        );
        self.record_outcome(
            session.session_id.seq,
            SessionOutcome::Aborted(AbortedSessionResult {
                session_id: session.session_id,
                reason,
                completed_tokens_before_abort: session.generated_tokens.len(),
                participants,
            }),
        );
        Some(session.session_id)
    }

    fn release_session_hops(
        &mut self,
        session: &ShardSession,
        now_ms: u64,
        cancel_reason: Option<SessionAbortReason>,
        exclude_cancel_worker: Option<Hash256>,
    ) -> Vec<SessionParticipantRecord> {
        let notice_expiry = now_ms.saturating_add(self.limits.ready_ttl_ms.saturating_mul(2));
        let mut participants = Vec::with_capacity(session.hops.len());
        for hop in &session.hops {
            participants.push(SessionParticipantRecord {
                worker_id: hop.worker_id,
                lease_id: hop.lease_id,
                lease_epoch: hop.lease_epoch,
                range: hop.range,
            });
            if let Some(lease) = self.leases.get_mut(&hop.worker_id.0) {
                lease.active_sessions.remove(&session.session_id.seq);
            }
            if let Some(reason) = cancel_reason
                .as_ref()
                .filter(|_| exclude_cancel_worker != Some(hop.worker_id))
            {
                self.push_cancellation_notice(
                    hop.worker_id,
                    SessionCancellationNotice {
                        session_id: session.session_id,
                        lease_id: hop.lease_id,
                        lease_epoch: hop.lease_epoch,
                        reason: reason.clone(),
                        expires_at_unix_ms: notice_expiry,
                    },
                );
            }
        }
        participants
    }

    fn remove_cancellation_queue(&mut self, worker_key: &[u8; 32]) {
        self.pending_cancellations.remove(worker_key);
        self.cancellation_queue_order
            .retain(|key| key != worker_key);
    }

    fn push_cancellation_notice(&mut self, worker_id: Hash256, notice: SessionCancellationNotice) {
        if !self.pending_cancellations.contains_key(&worker_id.0) {
            while self.pending_cancellations.len() >= self.limits.max_cancellation_queues {
                let Some(oldest) = self.cancellation_queue_order.pop_front() else {
                    break;
                };
                self.pending_cancellations.remove(&oldest);
            }
            self.cancellation_queue_order.push_back(worker_id.0);
        }
        let queue = self.pending_cancellations.entry(worker_id.0).or_default();
        while queue.len() >= self.limits.max_cancellations_per_worker {
            queue.pop_front();
        }
        queue.push_back(notice);
    }

    fn drain_cancellations_for(&mut self, worker_id: Hash256) -> Vec<SessionCancellationNotice> {
        let notices = self
            .pending_cancellations
            .remove(&worker_id.0)
            .map(|queue| queue.into_iter().collect())
            .unwrap_or_default();
        self.cancellation_queue_order
            .retain(|key| *key != worker_id.0);
        notices
    }

    fn record_outcome(&mut self, session_seq: u64, outcome: SessionOutcome) {
        if !self.completed_results.contains_key(&session_seq) {
            while self.completed_results.len() >= self.limits.max_retained_results {
                let Some(oldest) = self.completed_order.pop_front() else {
                    break;
                };
                self.completed_results.remove(&oldest);
            }
            self.completed_order.push_back(session_seq);
        }
        self.completed_results.insert(session_seq, outcome);
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use arc_inference::cached_integer_model::{
        ArithmeticProfile, CachedIntegerModel, CachedLayer, I8Weights, KVCache, ModelConfig,
        ShardInput, ShardOutput,
    };

    const Q16_ONE: i64 = 1i64 << 16;

    pub(crate) fn test_hash(label: &[u8]) -> Hash256 {
        arc_crypto::hash_bytes(label)
    }

    fn test_i8_weights(rng: &mut u64, n_rows: usize, n_cols: usize) -> I8Weights {
        let mut next = || -> u64 {
            *rng = rng
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            *rng
        };
        let mut data = Vec::with_capacity(n_rows * n_cols);
        for _ in 0..(n_rows * n_cols) {
            let val = ((next() >> 32) % 121) as i64 - 60;
            data.push(val as i8);
        }
        let mut scales = Vec::with_capacity(n_rows);
        for _ in 0..n_rows {
            let scale = ((next() >> 32) % 64) as i64 + 32;
            scales.push(scale);
        }
        I8Weights {
            data,
            scales,
            n_rows,
            n_cols,
        }
    }

    fn clone_i8_weights(weights: &I8Weights) -> I8Weights {
        I8Weights {
            data: weights.data.clone(),
            scales: weights.scales.clone(),
            n_rows: weights.n_rows,
            n_cols: weights.n_cols,
        }
    }

    fn clone_layer(layer: &CachedLayer) -> CachedLayer {
        CachedLayer {
            wq: clone_i8_weights(&layer.wq),
            wk: clone_i8_weights(&layer.wk),
            wv: clone_i8_weights(&layer.wv),
            wo: clone_i8_weights(&layer.wo),
            w_gate: clone_i8_weights(&layer.w_gate),
            w_up: clone_i8_weights(&layer.w_up),
            w_down: clone_i8_weights(&layer.w_down),
            attn_norm: layer.attn_norm.clone(),
            ffn_norm: layer.ffn_norm.clone(),
        }
    }

    fn clone_model_config(cfg: &ModelConfig) -> ModelConfig {
        ModelConfig {
            n_layers: cfg.n_layers,
            d_model: cfg.d_model,
            n_heads: cfg.n_heads,
            n_kv_heads: cfg.n_kv_heads,
            d_ff: cfg.d_ff,
            d_head: cfg.d_head,
            d_kv: cfg.d_kv,
            vocab_size: cfg.vocab_size,
            attn_scale: cfg.attn_scale,
            rope_cos: cfg.rope_cos.clone(),
            rope_sin: cfg.rope_sin.clone(),
            max_seq: cfg.max_seq,
            eos_tokens: cfg.eos_tokens.clone(),
            bos_token: cfg.bos_token,
            chat_template: cfg.chat_template.clone(),
            arithmetic_profile: cfg.arithmetic_profile,
        }
    }

    pub(crate) fn build_deterministic_integer_model(
        vocab_size: usize,
        d_model: usize,
        n_heads: usize,
        d_ff: usize,
        n_layers: usize,
        max_seq: usize,
        eos_tokens: Vec<u32>,
    ) -> CachedIntegerModel {
        let d_head = d_model / n_heads;
        let d_kv = d_head * n_heads;
        let mut rng: u64 = 0x5EED_CAFE_1234_5678;

        let embedding_i8 = test_i8_weights(&mut rng, vocab_size, d_model);
        let mut embedding_q16 = Vec::with_capacity(vocab_size * d_model);
        for row in 0..vocab_size {
            let scale = embedding_i8.scales[row];
            for col in 0..d_model {
                embedding_q16.push(i64::from(embedding_i8.data[row * d_model + col]) * scale);
            }
        }
        let output_weight = test_i8_weights(&mut rng, vocab_size, d_model);
        let mut layers = Vec::with_capacity(n_layers);
        for _ in 0..n_layers {
            layers.push(CachedLayer {
                wq: test_i8_weights(&mut rng, d_model, d_model),
                wk: test_i8_weights(&mut rng, d_kv, d_model),
                wv: test_i8_weights(&mut rng, d_kv, d_model),
                wo: test_i8_weights(&mut rng, d_model, d_model),
                w_gate: test_i8_weights(&mut rng, d_ff, d_model),
                w_up: test_i8_weights(&mut rng, d_ff, d_model),
                w_down: test_i8_weights(&mut rng, d_model, d_ff),
                attn_norm: vec![Q16_ONE; d_model],
                ffn_norm: vec![Q16_ONE; d_model],
            });
        }

        let quarter_turns = [
            (Q16_ONE, 0i64),
            (0i64, Q16_ONE),
            (-Q16_ONE, 0i64),
            (0i64, -Q16_ONE),
        ];
        let mut rope_cos = Vec::with_capacity(max_seq * d_head);
        let mut rope_sin = Vec::with_capacity(max_seq * d_head);
        for pos in 0..max_seq {
            for idx in 0..d_head {
                let (c, s) = quarter_turns[(pos + idx) % 4];
                rope_cos.push(c);
                rope_sin.push(s);
            }
        }

        CachedIntegerModel {
            config: ModelConfig {
                n_layers,
                d_model,
                n_heads,
                n_kv_heads: n_heads,
                d_ff,
                d_head,
                d_kv,
                vocab_size,
                attn_scale: 23_170,
                rope_cos,
                rope_sin,
                max_seq,
                eos_tokens,
                bos_token: 1,
                chat_template: String::new(),
                arithmetic_profile: ArithmeticProfile::LegacySplitHalfV0,
            },
            embedding_q16,
            embedding_i8,
            layers,
            final_norm: vec![Q16_ONE; d_model],
            output_weight,
            vocab: (0..vocab_size).map(|i| format!("tok_{i}")).collect(),
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
        }
    }

    pub(crate) fn build_partial_integer_model(
        full: &CachedIntegerModel,
        range: ShardLayerRange,
    ) -> CachedIntegerModel {
        let is_first = range.start_layer == 0;
        let is_last = range.end_layer == full.config.n_layers;
        let layers = (0..full.config.n_layers)
            .map(|idx| {
                if (range.start_layer..range.end_layer).contains(&idx) {
                    clone_layer(&full.layers[idx])
                } else {
                    CachedLayer::placeholder()
                }
            })
            .collect();
        CachedIntegerModel {
            config: clone_model_config(&full.config),
            embedding_q16: if is_first {
                full.embedding_q16.clone()
            } else {
                Vec::new()
            },
            embedding_i8: if is_first {
                clone_i8_weights(&full.embedding_i8)
            } else {
                I8Weights::empty()
            },
            layers,
            final_norm: if is_last {
                full.final_norm.clone()
            } else {
                Vec::new()
            },
            output_weight: if is_last {
                clone_i8_weights(&full.output_weight)
            } else {
                I8Weights::empty()
            },
            vocab: full.vocab.clone(),
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
        }
    }

    pub(crate) fn model_spec_from(
        model_id: Hash256,
        model: &CachedIntegerModel,
    ) -> TrustedModelSpec {
        TrustedModelSpec {
            model_id,
            execution_profile: CANONICAL_REWARD_INFERENCE_PROFILE.to_string(),
            n_layers: model.config.n_layers,
            d_model: model.config.d_model,
            vocab_size: model.config.vocab_size,
            max_seq: model.config.max_seq,
            eos_tokens: model.config.eos_tokens.clone(),
            bos_token: model.config.bos_token,
        }
    }

    pub(crate) fn lease_req(
        worker_id: Hash256,
        spec: &TrustedModelSpec,
        max_resident_layers: usize,
    ) -> ShardLeaseRequest {
        ShardLeaseRequest {
            worker_id,
            protocol_version: COMMUNITY_SHARD_PROTOCOL_VERSION,
            model_id: spec.model_id,
            execution_profile: spec.execution_profile.clone(),
            n_layers: spec.n_layers,
            d_model: spec.d_model,
            vocab_size: spec.vocab_size,
            max_seq: spec.max_seq,
            max_resident_layers,
        }
    }

    pub(crate) fn ready_hb(
        worker_id: Hash256,
        assignment: &ShardLeaseAssignment,
        ready: bool,
    ) -> ShardHeartbeatRequest {
        ShardHeartbeatRequest {
            worker_id,
            lease_id: assignment.lease_id,
            lease_epoch: assignment.lease_epoch,
            model_id: assignment.model_id,
            execution_profile: assignment.execution_profile.clone(),
            loaded_range: assignment.range,
            ready,
        }
    }

    pub(crate) fn claim_req(
        worker_id: Hash256,
        assignment: &ShardLeaseAssignment,
    ) -> ShardClaimStepRequest {
        ShardClaimStepRequest {
            worker_id,
            lease_id: assignment.lease_id,
            lease_epoch: assignment.lease_epoch,
            model_id: assignment.model_id,
            execution_profile: assignment.execution_profile.clone(),
        }
    }

    fn execute_work_on_partial_model(
        worker_id: Hash256,
        model: &CachedIntegerModel,
        caches: &mut BTreeMap<u64, KVCache>,
        work: &ShardStepWork,
    ) -> ShardStepSubmitRequest {
        let input = match &work.input {
            ShardStepInput::Token { token } => ShardInput::Token(*token),
            ShardStepInput::Hidden {
                hidden,
                hidden_hash,
            } => {
                assert_eq!(hidden_state_hash(hidden), *hidden_hash);
                ShardInput::Hidden(hidden.clone())
            }
        };
        let cache = caches
            .entry(work.session_id.seq)
            .or_insert_with(|| KVCache::new(model.config.n_layers));
        let shard_out = model
            .forward_shard_token_with_history(
                input,
                cache,
                work.range.start_layer,
                work.range.end_layer,
                work.position,
                &work.generated_tokens,
            )
            .expect("partial shard execution must succeed");
        if work.last_token {
            caches.remove(&work.session_id.seq);
        }
        let output = match shard_out {
            ShardOutput::Hidden(hidden) => {
                let hidden_hash = hidden_state_hash(&hidden);
                ShardStepOutput::Hidden {
                    hidden,
                    hidden_hash,
                }
            }
            ShardOutput::Token { id, logits_hash } => ShardStepOutput::Token {
                token_id: id,
                logits_hash,
            },
        };
        ShardStepSubmitRequest {
            worker_id,
            lease_id: work.lease_id,
            lease_epoch: work.lease_epoch,
            session_id: work.session_id,
            step_id: work.step_id,
            model_id: work.model_id,
            execution_profile: work.execution_profile.clone(),
            range: work.range,
            position: work.position,
            layers_processed: work.range.len(),
            outcome: ShardStepOutcome::Completed { output },
        }
    }

    pub(crate) fn make_two_tile_coordinator(spec: &TrustedModelSpec) -> CommunityShardCoordinator {
        let limits = CommunityShardLimits {
            target_partitions: 2,
            ..CommunityShardLimits::default()
        };
        CommunityShardCoordinator::new(test_hash(b"boot-1"), spec.clone(), limits)
            .expect("valid two-tile coordinator")
    }

    #[test]
    fn community_sharding_real_integer_model_pipeline_matches_try_generate() {
        let mut full =
            build_deterministic_integer_model(32, 16, 2, 32, 4, 64, vec![128_001, 128_009]);
        full.config.bos_token = 17;
        assert!(full.has_all_transformer_layers());
        assert!(full.has_canonical_i8_profile());

        let spec = model_spec_from(test_hash(b"tiny-int8-model"), &full);
        let mut coord = make_two_tile_coordinator(&spec);
        let w1 = test_hash(b"worker-1");
        let w2 = test_hash(b"worker-2");
        let mut now = 1_000u64;

        let lease1 = coord
            .request_lease(w1, now, lease_req(w1, &spec, 4))
            .expect("worker 1 lease")
            .assignment;
        let lease2 = coord
            .request_lease(w2, now, lease_req(w2, &spec, 4))
            .expect("worker 2 lease")
            .assignment;
        assert_eq!(
            lease1.range,
            ShardLayerRange {
                start_layer: 0,
                end_layer: 2
            }
        );
        assert_eq!(
            lease2.range,
            ShardLayerRange {
                start_layer: 2,
                end_layer: 4
            }
        );

        let partial1 = build_partial_integer_model(&full, lease1.range);
        let partial2 = build_partial_integer_model(&full, lease2.range);
        assert!(!partial1.has_all_transformer_layers());
        assert!(!partial2.has_all_transformer_layers());
        assert!(partial1.has_canonical_i8_profile());
        assert!(partial2.has_canonical_i8_profile());
        assert!(!partial1.embedding_q16.is_empty());
        assert_eq!(partial1.output_weight.n_rows, 0);
        assert!(partial2.embedding_q16.is_empty());
        assert_eq!(partial2.output_weight.n_rows, 32);

        coord
            .heartbeat(w1, now, ready_hb(w1, &lease1, true))
            .expect("worker 1 ready");
        coord
            .heartbeat(w2, now, ready_hb(w2, &lease2, true))
            .expect("worker 2 ready");

        let prompt = vec![1u32, 5, 11];
        let max_tokens = 6u32;
        let session_id = coord
            .open_session(now, prompt.clone(), max_tokens)
            .expect("open real inference session");

        let mut caches1 = BTreeMap::new();
        let mut caches2 = BTreeMap::new();
        let mut completed = false;
        for _ in 0..64 {
            now += 100;
            for (worker_id, lease, model, caches) in [
                (w1, &lease1, &partial1, &mut caches1),
                (w2, &lease2, &partial2, &mut caches2),
            ] {
                match coord
                    .claim_step(worker_id, now, claim_req(worker_id, lease))
                    .expect("claim step")
                {
                    ShardClaimStepResponse::Step { work } => {
                        let sub = execute_work_on_partial_model(worker_id, model, caches, &work);
                        let resp = coord
                            .submit_step(worker_id, now, sub)
                            .expect("submit real shard step");
                        assert!(resp.accepted);
                        if resp.session_completed {
                            completed = true;
                        }
                    }
                    ShardClaimStepResponse::Idle => {}
                    other => panic!("unexpected claim response: {other:?}"),
                }
            }
            if completed {
                break;
            }
        }
        assert!(completed);
        assert!(caches1.is_empty());
        assert!(caches2.is_empty());

        let outcome = coord.take_result(session_id).expect("session result");
        let pending = match outcome {
            SessionOutcome::PendingVerification(pending) => pending,
            SessionOutcome::Aborted(aborted) => panic!("unexpected abort: {aborted:?}"),
        };
        assert!(pending.verification_required);
        assert!(!pending.reward_eligible);
        assert!(!pending.stopped_by_eos);
        assert_eq!(pending.participants.len(), 2);
        assert_ne!(
            pending.participants[0].worker_id,
            pending.participants[1].worker_id
        );

        let (expected_tokens, _) = full
            .try_generate(&prompt, max_tokens, &full.config.eos_tokens)
            .expect("whole model generation");
        assert_eq!(pending.unverified_generated_tokens, expected_tokens);
        assert_eq!(
            pending.unverified_generated_tokens.len(),
            max_tokens as usize
        );

        let eos_target = expected_tokens[2];
        let mut full_eos =
            build_deterministic_integer_model(32, 16, 2, 32, 4, 64, vec![eos_target]);
        full_eos.config.bos_token = full.config.bos_token;
        let spec_eos = model_spec_from(test_hash(b"tiny-int8-eos"), &full_eos);
        let mut coord_eos = make_two_tile_coordinator(&spec_eos);
        let l1 = coord_eos
            .request_lease(w1, 2_000, lease_req(w1, &spec_eos, 2))
            .unwrap()
            .assignment;
        let l2 = coord_eos
            .request_lease(w2, 2_000, lease_req(w2, &spec_eos, 2))
            .unwrap()
            .assignment;
        coord_eos
            .heartbeat(w1, 2_000, ready_hb(w1, &l1, true))
            .unwrap();
        coord_eos
            .heartbeat(w2, 2_000, ready_hb(w2, &l2, true))
            .unwrap();
        let p1 = build_partial_integer_model(&full_eos, l1.range);
        let p2 = build_partial_integer_model(&full_eos, l2.range);
        let sid_eos = coord_eos
            .open_session(2_000, prompt.clone(), max_tokens)
            .unwrap();
        let mut c1 = BTreeMap::new();
        let mut c2 = BTreeMap::new();
        let mut t = 2_000u64;
        let mut done_eos = false;
        for _ in 0..64 {
            t += 50;
            for (worker_id, lease, model, caches) in
                [(w1, &l1, &p1, &mut c1), (w2, &l2, &p2, &mut c2)]
            {
                if let ShardClaimStepResponse::Step { work } = coord_eos
                    .claim_step(worker_id, t, claim_req(worker_id, lease))
                    .unwrap()
                {
                    let sub = execute_work_on_partial_model(worker_id, model, caches, &work);
                    if coord_eos
                        .submit_step(worker_id, t, sub)
                        .unwrap()
                        .session_completed
                    {
                        done_eos = true;
                    }
                }
            }
            if done_eos {
                break;
            }
        }
        assert!(done_eos);
        let pending_eos = match coord_eos.take_result(sid_eos).unwrap() {
            SessionOutcome::PendingVerification(p) => p,
            other => panic!("expected pending verification, got {other:?}"),
        };
        let (expected_eos_tokens, _) = full_eos
            .try_generate(&prompt, max_tokens, &full_eos.config.eos_tokens)
            .unwrap();
        assert!(pending_eos.stopped_by_eos);
        assert_eq!(pending_eos.unverified_generated_tokens, expected_eos_tokens);
        match coord_eos
            .claim_step(w1, t + 10, claim_req(w1, &l1))
            .unwrap()
        {
            ShardClaimStepResponse::CancelSessions { cancellations } => {
                assert_eq!(cancellations.len(), 1);
                assert_eq!(cancellations[0].session_id, sid_eos);
                assert_eq!(
                    cancellations[0].reason,
                    SessionAbortReason::CompletedEarlyEos
                );
            }
            other => panic!("expected cancel notice on early eos, got {other:?}"),
        }
    }

    #[test]
    fn community_sharding_hundred_workers_execute_fair_partial_pipelines() {
        const WORKERS: usize = 100;
        const TILES: usize = 6;
        const TILE_LAYERS: usize = 2;
        const CONCURRENT: usize = 16;
        const BATCHES: usize = 2;
        const PROMPT_TOKENS: usize = 2;
        const GENERATED_TOKENS: u32 = 2;
        const STEPS_PER_HOP: usize = 1 + PROMPT_TOKENS + GENERATED_TOKENS as usize;
        struct SimWorker {
            id: Hash256,
            assignment: ShardLeaseAssignment,
            model: CachedIntegerModel,
            caches: BTreeMap<u64, KVCache>,
            steps: usize,
            sessions: BTreeSet<u64>,
        }

        let full = build_deterministic_integer_model(32, 16, 2, 32, 12, 64, vec![128_001, 128_009]);
        assert!(full.has_all_transformer_layers() && full.has_canonical_i8_profile());
        let spec = model_spec_from(test_hash(b"model-12l-100w"), &full);
        let limits = CommunityShardLimits {
            max_workers: WORKERS,
            max_active_sessions: CONCURRENT,
            max_sessions_per_worker: 1,
            target_partitions: TILES,
            ..CommunityShardLimits::default()
        };
        let mut coord =
            CommunityShardCoordinator::new(test_hash(b"boot-100-workers"), spec.clone(), limits)
                .expect("valid coordinator");
        let expected_tiles: Vec<ShardLayerRange> = (0..TILES)
            .map(|i| ShardLayerRange {
                start_layer: i * TILE_LAYERS,
                end_layer: (i + 1) * TILE_LAYERS,
            })
            .collect();
        assert_eq!(coord.fixed_tiles(), expected_tiles.as_slice());

        let start_ms: u64 = 1_000_000;
        let mut now_ms = start_ms;
        let mut workers = Vec::with_capacity(WORKERS);
        let mut worker_idx = BTreeMap::new();
        for idx in 0..WORKERS {
            let id = test_hash(format!("community-worker-{idx:03}").as_bytes());
            assert_ne!(id.0, [0u8; 32]);
            assert!(worker_idx.insert(id.0, idx).is_none());
            let lease = coord
                .request_lease(id, now_ms, lease_req(id, &spec, TILE_LAYERS))
                .expect("lease must succeed");
            assert_eq!(lease.protocol_version, COMMUNITY_SHARD_PROTOCOL_VERSION);
            assert_eq!(lease.ready_ttl_ms, DEFAULT_READY_TTL_MS);
            assert_eq!(lease.assignment.range.len(), TILE_LAYERS);
            assert!(lease.assignment.range.len() < full.config.n_layers);
            let model = build_partial_integer_model(&full, lease.assignment.range);
            assert!(!model.has_all_transformer_layers());
            let hb = coord
                .heartbeat(id, now_ms, ready_hb(id, &lease.assignment, true))
                .expect("ready heartbeat must succeed");
            assert!(matches!(
                hb.directive,
                HeartbeatDirective::Active { expires_at_unix_ms }
                    if expires_at_unix_ms == now_ms + DEFAULT_READY_TTL_MS
            ));
            assert!(hb.cancellations.is_empty());
            let snap = coord.worker_lease_snapshot(id).expect("lease snapshot");
            assert!(snap.ready && snap.range == lease.assignment.range);
            assert_eq!(snap.max_resident_layers, TILE_LAYERS);
            workers.push(SimWorker {
                id,
                assignment: lease.assignment,
                model,
                caches: BTreeMap::new(),
                steps: 0,
                sessions: BTreeSet::new(),
            });
        }

        assert_eq!(worker_idx.len(), WORKERS);
        let status = coord.status_snapshot(now_ms);
        assert!(status.fully_covered_ready && status.provisioning_workers == 0);
        assert_eq!((status.ready_workers, status.tiles.len()), (WORKERS, TILES));
        assert_eq!(
            status.tiles.iter().map(|t| t.ready_workers).min(),
            Some(CONCURRENT)
        );
        assert_eq!(
            status.tiles.iter().map(|t| t.ready_workers).max(),
            Some(CONCURRENT + 1)
        );
        assert_eq!(
            status.tiles.iter().map(|t| t.ready_workers).sum::<usize>(),
            WORKERS
        );

        for batch in 0..BATCHES {
            now_ms += 100;
            let mut sessions = Vec::with_capacity(CONCURRENT);
            for slot in 0..CONCURRENT {
                let n = batch * CONCURRENT + slot;
                let t0 = ((n * 3 + 1) % 32) as u32;
                let t1 = ((t0 as usize + 5 + (n % 7)) % 32) as u32;
                let prompt = vec![t0, t1];
                let (exp_tokens, canonical_digest) = full
                    .try_generate(&prompt, GENERATED_TOKENS, &full.config.eos_tokens)
                    .expect("try_generate");
                assert_eq!(prompt.len(), PROMPT_TOKENS);
                assert_eq!(exp_tokens.len(), GENERATED_TOKENS as usize);
                assert_ne!(canonical_digest.0, [0u8; 32]);
                let exp_digest = unverified_tokens_digest(
                    spec.model_id,
                    &spec.execution_profile,
                    &prompt,
                    &exp_tokens,
                );
                let sid = coord
                    .open_session(now_ms, prompt.clone(), GENERATED_TOKENS)
                    .expect("open_session");
                sessions.push((sid, prompt, exp_tokens, exp_digest));
            }
            assert_eq!(coord.status_snapshot(now_ms).active_sessions, CONCURRENT);
            assert_eq!(
                coord.open_session(now_ms, vec![1, 2], 2),
                Err(ShardCoordinatorError::SessionCapacityReached)
            );
            let mut completed = 0usize;
            let mut rounds = 0usize;
            while completed < CONCURRENT {
                assert!(rounds < 32);
                rounds += 1;
                now_ms += 50;
                let mut progressed = false;
                for w in &mut workers {
                    match coord
                        .claim_step(w.id, now_ms, claim_req(w.id, &w.assignment))
                        .expect("claim_step")
                    {
                        ShardClaimStepResponse::Idle => {}
                        ShardClaimStepResponse::Step { work } => {
                            assert_eq!(work.range, w.assignment.range);
                            assert!(work.range.len() < full.config.n_layers);
                            let req =
                                execute_work_on_partial_model(w.id, &w.model, &mut w.caches, &work);
                            let resp = coord.submit_step(w.id, now_ms, req).expect("submit_step");
                            assert!(resp.accepted && !resp.session_aborted);
                            w.steps += 1;
                            w.sessions.insert(work.session_id.seq);
                            progressed = true;
                            if resp.session_completed {
                                completed += 1;
                            }
                        }
                        other => panic!("unexpected claim_step response: {other:?}"),
                    }
                }
                assert!(progressed);
            }
            for (sid, prompt, exp_tokens, exp_digest) in sessions {
                let pending = match coord.take_result(sid).expect("retained result") {
                    SessionOutcome::PendingVerification(p) => p,
                    other => panic!("expected PendingVerification, got {other:?}"),
                };
                assert_eq!((pending.session_id, pending.model_id), (sid, spec.model_id));
                assert_eq!(pending.execution_profile, spec.execution_profile);
                assert_eq!(pending.prompt_tokens, prompt);
                assert_eq!(pending.unverified_generated_tokens, exp_tokens);
                assert_eq!(pending.unverified_output_digest, exp_digest);
                assert!(pending.terminal_logits_hashes.len() >= exp_tokens.len());
                assert!(
                    pending
                        .terminal_logits_hashes
                        .iter()
                        .all(|h| h.0 != [0u8; 32])
                );
                assert!(
                    !pending.stopped_by_eos
                        && pending.verification_required
                        && !pending.reward_eligible
                );
                let ranges: Vec<ShardLayerRange> =
                    pending.participants.iter().map(|p| p.range).collect();
                assert_eq!(ranges, expected_tiles);
                let mut seen = BTreeSet::new();
                for p in &pending.participants {
                    assert!(seen.insert(p.worker_id.0));
                    assert!(
                        workers[worker_idx[&p.worker_id.0]]
                            .sessions
                            .contains(&sid.seq)
                    );
                }
            }
            if batch == 0 {
                assert_eq!(
                    workers.iter().filter(|w| w.steps > 0).count(),
                    CONCURRENT * TILES
                );
            }
        }

        assert!(now_ms - start_ms < DEFAULT_READY_TTL_MS);
        let end = coord.status_snapshot(now_ms);
        assert!(end.fully_covered_ready && end.ready_workers == WORKERS);
        assert_eq!(
            end.active_sessions
                + end.retained_results
                + end.quarantined_workers
                + end.queued_cancellations,
            0
        );
        assert_eq!(workers.iter().filter(|w| w.steps > 0).count(), WORKERS);
        assert_eq!(workers.iter().map(|w| w.sessions.len()).min(), Some(1));
        assert_eq!(workers.iter().map(|w| w.sessions.len()).max(), Some(2));
        let total_steps: usize = workers.iter().map(|w| w.steps).sum();
        assert_eq!(total_steps, BATCHES * CONCURRENT * TILES * STEPS_PER_HOP);
        for w in &workers {
            assert_eq!(w.steps, w.sessions.len() * STEPS_PER_HOP);
            assert!(w.caches.is_empty());
        }
    }

    #[test]
    fn community_sharding_join_gap_fails_closed_and_caps_partial_range() {
        let model = build_deterministic_integer_model(32, 16, 2, 32, 4, 64, vec![]);
        let spec = model_spec_from(test_hash(b"model-gap"), &model);
        let mut coord = make_two_tile_coordinator(&spec);
        let w1 = test_hash(b"gap-w1");
        let w2 = test_hash(b"gap-w2");

        assert!(matches!(
            coord.open_session(100, vec![1], 2),
            Err(ShardCoordinatorError::InsufficientShardCoverage { .. })
        ));

        let l1 = coord
            .request_lease(w1, 100, lease_req(w1, &spec, 4))
            .unwrap()
            .assignment;
        assert!(l1.range.len() <= spec.n_layers.div_ceil(2));
        assert!(l1.range.len() < spec.n_layers);

        assert!(matches!(
            coord.open_session(100, vec![1], 2),
            Err(ShardCoordinatorError::InsufficientShardCoverage { .. })
        ));
        coord.heartbeat(w1, 100, ready_hb(w1, &l1, true)).unwrap();
        assert!(matches!(
            coord.open_session(100, vec![1], 2),
            Err(ShardCoordinatorError::InsufficientShardCoverage {
                missing_range: ShardLayerRange {
                    start_layer: 2,
                    end_layer: 4
                }
            })
        ));

        let l2 = coord
            .request_lease(w2, 100, lease_req(w2, &spec, 2))
            .unwrap()
            .assignment;
        assert!(matches!(
            coord.open_session(100, vec![1], 2),
            Err(ShardCoordinatorError::InsufficientShardCoverage { .. })
        ));
        coord.heartbeat(w2, 100, ready_hb(w2, &l2, true)).unwrap();
        assert!(coord.open_session(100, vec![1], 2).is_ok());
    }

    #[test]
    fn community_sharding_replicate_join_and_leave_idle_rebalance() {
        let model = build_deterministic_integer_model(32, 16, 2, 32, 4, 64, vec![]);
        let spec = model_spec_from(test_hash(b"model-rebalance"), &model);
        let mut coord = make_two_tile_coordinator(&spec);
        let w1 = test_hash(b"reb-w1");
        let w2 = test_hash(b"reb-w2");
        let w3 = test_hash(b"reb-w3");

        let l1 = coord
            .request_lease(w1, 1_000, lease_req(w1, &spec, 2))
            .unwrap()
            .assignment;
        let l2 = coord
            .request_lease(w2, 1_000, lease_req(w2, &spec, 2))
            .unwrap()
            .assignment;
        let l3 = coord
            .request_lease(w3, 1_000, lease_req(w3, &spec, 2))
            .unwrap()
            .assignment;
        assert_eq!(l1.range, l3.range);
        assert_ne!(l1.range, l2.range);

        coord.heartbeat(w1, 1_000, ready_hb(w1, &l1, true)).unwrap();
        coord.heartbeat(w2, 1_000, ready_hb(w2, &l2, true)).unwrap();
        coord.heartbeat(w3, 1_000, ready_hb(w3, &l3, true)).unwrap();

        let rel = coord
            .release_lease(
                w2,
                2_000,
                ShardReleaseRequest {
                    worker_id: w2,
                    lease_id: l2.lease_id,
                    lease_epoch: l2.lease_epoch,
                },
            )
            .unwrap();
        assert!(rel.released);

        let moved_worker = if coord.worker_lease_snapshot(w1).unwrap().range == l2.range {
            (w1, &l1)
        } else {
            (w3, &l3)
        };
        let moved_snap = coord.worker_lease_snapshot(moved_worker.0).unwrap();
        assert_eq!(moved_snap.lease_epoch, 2);
        assert_eq!(moved_snap.range, l2.range);
        assert!(!moved_snap.ready);

        assert!(matches!(
            coord.open_session(2_000, vec![1], 2),
            Err(ShardCoordinatorError::InsufficientShardCoverage { .. })
        ));

        let hb_resp = coord
            .heartbeat(
                moved_worker.0,
                2_500,
                ready_hb(moved_worker.0, moved_worker.1, true),
            )
            .unwrap();
        let new_assignment = match hb_resp.directive {
            HeartbeatDirective::Rebalance { assignment } => assignment,
            other => panic!("expected rebalance directive, got {other:?}"),
        };
        assert_eq!(new_assignment.lease_epoch, 2);
        assert_eq!(new_assignment.range, l2.range);

        assert!(matches!(
            coord.claim_step(
                moved_worker.0,
                2_500,
                claim_req(moved_worker.0, &new_assignment)
            ),
            Err(ShardCoordinatorError::LeaseNotReady)
        ));

        let ack = coord
            .heartbeat(
                moved_worker.0,
                3_000,
                ready_hb(moved_worker.0, &new_assignment, true),
            )
            .unwrap();
        assert!(matches!(ack.directive, HeartbeatDirective::Active { .. }));
        assert!(coord.open_session(3_000, vec![1], 2).is_ok());
    }

    #[test]
    fn community_sharding_mid_session_expiry_and_provisioning_hard_deadline() {
        let model = build_deterministic_integer_model(32, 16, 2, 32, 4, 64, vec![]);
        let spec = model_spec_from(test_hash(b"model-expiry"), &model);
        let mut coord = make_two_tile_coordinator(&spec);
        let w1 = test_hash(b"exp-w1");
        let w2 = test_hash(b"exp-w2");

        let l1 = coord
            .request_lease(w1, 1_000, lease_req(w1, &spec, 2))
            .unwrap()
            .assignment;
        let hb_prov = coord
            .heartbeat(w1, 150_000, ready_hb(w1, &l1, false))
            .unwrap();
        assert_eq!(
            hb_prov.directive,
            HeartbeatDirective::Provisioning {
                deadline_unix_ms: 1_000 + DEFAULT_PROVISIONING_TTL_MS
            }
        );
        let summary_prov = coord.expire_stale(1_000 + DEFAULT_PROVISIONING_TTL_MS);
        assert_eq!(summary_prov.expired_leases, 1);
        assert!(coord.worker_lease_snapshot(w1).is_none());

        let t0 = 500_000u64;
        let l1 = coord
            .request_lease(w1, t0, lease_req(w1, &spec, 2))
            .unwrap()
            .assignment;
        let l2 = coord
            .request_lease(w2, t0, lease_req(w2, &spec, 2))
            .unwrap()
            .assignment;
        coord.heartbeat(w1, t0, ready_hb(w1, &l1, true)).unwrap();
        coord.heartbeat(w2, t0, ready_hb(w2, &l2, true)).unwrap();

        let sid = coord.open_session(t0, vec![1, 2], 3).unwrap();
        coord
            .heartbeat(w1, t0 + 30_000, ready_hb(w1, &l1, true))
            .unwrap();
        let summary = coord.expire_stale(t0 + DEFAULT_READY_TTL_MS);
        assert_eq!(summary.expired_leases, 1);
        assert_eq!(summary.aborted_sessions, 1);
        assert!(matches!(
            coord.take_result(sid),
            Some(SessionOutcome::Aborted(AbortedSessionResult {
                reason: SessionAbortReason::WorkerLeaseExpired { worker_id },
                ..
            })) if worker_id == w2
        ));
        match coord
            .claim_step(w1, t0 + DEFAULT_READY_TTL_MS + 1, claim_req(w1, &l1))
            .unwrap()
        {
            ShardClaimStepResponse::CancelSessions { cancellations } => {
                assert_eq!(cancellations.len(), 1);
                assert_eq!(cancellations[0].session_id, sid);
            }
            other => panic!("expected cancellation for surviving worker, got {other:?}"),
        }
    }

    #[test]
    fn community_sharding_output_tamper_wrong_hash_and_bounds_rejected() {
        let model = build_deterministic_integer_model(32, 16, 2, 32, 4, 64, vec![]);
        let spec = model_spec_from(test_hash(b"model-tamper"), &model);
        let mut coord = make_two_tile_coordinator(&spec);
        let w1 = test_hash(b"tmp-w1");
        let w2 = test_hash(b"tmp-w2");
        let l1 = coord
            .request_lease(w1, 100, lease_req(w1, &spec, 2))
            .unwrap()
            .assignment;
        let l2 = coord
            .request_lease(w2, 100, lease_req(w2, &spec, 2))
            .unwrap()
            .assignment;
        coord.heartbeat(w1, 100, ready_hb(w1, &l1, true)).unwrap();
        coord.heartbeat(w2, 100, ready_hb(w2, &l2, true)).unwrap();
        let sid = coord.open_session(100, vec![1], 2).unwrap();

        let work1 = match coord.claim_step(w1, 100, claim_req(w1, &l1)).unwrap() {
            ShardClaimStepResponse::Step { work } => work,
            other => panic!("expected step, got {other:?}"),
        };

        let mut hidden = vec![123i64; spec.d_model];
        let good_hash = hidden_state_hash(&hidden);
        hidden[0] ^= 1;
        let bad_hash_sub = ShardStepSubmitRequest {
            worker_id: w1,
            lease_id: l1.lease_id,
            lease_epoch: l1.lease_epoch,
            session_id: sid,
            step_id: work1.step_id,
            model_id: spec.model_id,
            execution_profile: spec.execution_profile.clone(),
            range: l1.range,
            position: 0,
            layers_processed: 2,
            outcome: ShardStepOutcome::Completed {
                output: ShardStepOutput::Hidden {
                    hidden: hidden.clone(),
                    hidden_hash: good_hash,
                },
            },
        };
        assert_eq!(
            coord.submit_step(w1, 101, bad_hash_sub),
            Err(ShardCoordinatorError::HiddenHashMismatch)
        );
        assert_eq!(
            coord.session_status(sid).unwrap().expected_step_id,
            work1.step_id
        );

        for value in [
            i64::MIN,
            -MAX_HIDDEN_ABS_Q16 - 1,
            MAX_HIDDEN_ABS_Q16 + 1,
            i64::MAX,
        ] {
            let hidden = vec![value; spec.d_model];
            let invalid = ShardStepSubmitRequest {
                worker_id: w1,
                lease_id: l1.lease_id,
                lease_epoch: l1.lease_epoch,
                session_id: sid,
                step_id: work1.step_id,
                model_id: spec.model_id,
                execution_profile: spec.execution_profile.clone(),
                range: l1.range,
                position: 0,
                layers_processed: 2,
                outcome: ShardStepOutcome::Completed {
                    output: ShardStepOutput::Hidden {
                        hidden_hash: hidden_state_hash(&hidden),
                        hidden,
                    },
                },
            };
            assert_eq!(
                coord.submit_step(w1, 101, invalid),
                Err(ShardCoordinatorError::HiddenValueOutOfBounds)
            );
            assert_eq!(
                coord.session_status(sid).unwrap().expected_step_id,
                work1.step_id
            );
        }

        let short_hidden = vec![1i64; spec.d_model - 1];
        let short_hidden_hash = hidden_state_hash(&short_hidden);
        let bad_len_sub = ShardStepSubmitRequest {
            worker_id: w1,
            lease_id: l1.lease_id,
            lease_epoch: l1.lease_epoch,
            session_id: sid,
            step_id: work1.step_id,
            model_id: spec.model_id,
            execution_profile: spec.execution_profile.clone(),
            range: l1.range,
            position: 0,
            layers_processed: 2,
            outcome: ShardStepOutcome::Completed {
                output: ShardStepOutput::Hidden {
                    hidden: short_hidden,
                    hidden_hash: short_hidden_hash,
                },
            },
        };
        assert!(matches!(
            coord.submit_step(w1, 102, bad_len_sub),
            Err(ShardCoordinatorError::InvalidHiddenLength { .. })
        ));

        let wrong_mode_sub = ShardStepSubmitRequest {
            worker_id: w1,
            lease_id: l1.lease_id,
            lease_epoch: l1.lease_epoch,
            session_id: sid,
            step_id: work1.step_id,
            model_id: spec.model_id,
            execution_profile: spec.execution_profile.clone(),
            range: l1.range,
            position: 0,
            layers_processed: 2,
            outcome: ShardStepOutcome::Completed {
                output: ShardStepOutput::Token {
                    token_id: 1,
                    logits_hash: test_hash(b"logits"),
                },
            },
        };
        assert_eq!(
            coord.submit_step(w1, 103, wrong_mode_sub),
            Err(ShardCoordinatorError::TerminalModeMismatch {
                expected_terminal: false
            })
        );

        let valid_hidden = vec![7i64; spec.d_model];
        let valid_sub1 = ShardStepSubmitRequest {
            worker_id: w1,
            lease_id: l1.lease_id,
            lease_epoch: l1.lease_epoch,
            session_id: sid,
            step_id: work1.step_id,
            model_id: spec.model_id,
            execution_profile: spec.execution_profile.clone(),
            range: l1.range,
            position: 0,
            layers_processed: 2,
            outcome: ShardStepOutcome::Completed {
                output: ShardStepOutput::Hidden {
                    hidden_hash: hidden_state_hash(&valid_hidden),
                    hidden: valid_hidden,
                },
            },
        };
        assert!(coord.submit_step(w1, 104, valid_sub1).unwrap().accepted);

        let work2 = match coord.claim_step(w2, 105, claim_req(w2, &l2)).unwrap() {
            ShardClaimStepResponse::Step { work } => work,
            other => panic!("expected terminal step, got {other:?}"),
        };
        let oob_token_sub = ShardStepSubmitRequest {
            worker_id: w2,
            lease_id: l2.lease_id,
            lease_epoch: l2.lease_epoch,
            session_id: sid,
            step_id: work2.step_id,
            model_id: spec.model_id,
            execution_profile: spec.execution_profile.clone(),
            range: l2.range,
            position: 0,
            layers_processed: 2,
            outcome: ShardStepOutcome::Completed {
                output: ShardStepOutput::Token {
                    token_id: spec.vocab_size as u32,
                    logits_hash: test_hash(b"logits"),
                },
            },
        };
        assert!(matches!(
            coord.submit_step(w2, 106, oob_token_sub),
            Err(ShardCoordinatorError::TokenOutOfRange { .. })
        ));
    }

    #[test]
    fn community_sharding_identity_epoch_model_and_serde_mismatch_rejected() {
        let model = build_deterministic_integer_model(32, 16, 2, 32, 4, 64, vec![]);
        let spec = model_spec_from(test_hash(b"model-auth"), &model);
        let mut coord = make_two_tile_coordinator(&spec);
        let w1 = test_hash(b"auth-w1");
        let w2 = test_hash(b"auth-w2");

        assert_eq!(
            coord.request_lease(w2, 100, lease_req(w1, &spec, 2)),
            Err(ShardCoordinatorError::WorkerIdentityMismatch)
        );
        let mut bad_shape = lease_req(w1, &spec, 2);
        bad_shape.d_model = 999;
        assert_eq!(
            coord.request_lease(w1, 100, bad_shape),
            Err(ShardCoordinatorError::ModelMismatch("d_model"))
        );
        let mut bad_profile = lease_req(w1, &spec, 2);
        bad_profile.execution_profile = "wrong-profile".to_string();
        assert_eq!(
            coord.request_lease(w1, 100, bad_profile),
            Err(ShardCoordinatorError::ModelMismatch("execution_profile"))
        );

        let l1 = coord
            .request_lease(w1, 100, lease_req(w1, &spec, 2))
            .unwrap()
            .assignment;
        let mut wrong_epoch = ready_hb(w1, &l1, true);
        wrong_epoch.lease_epoch = 99;
        assert!(matches!(
            coord.heartbeat(w1, 100, wrong_epoch),
            Err(ShardCoordinatorError::LeaseEpochMismatch { .. })
        ));
        let mut wrong_boot = ready_hb(w1, &l1, true);
        wrong_boot.lease_id.boot_nonce = test_hash(b"other-boot");
        assert_eq!(
            coord.heartbeat(w1, 100, wrong_boot),
            Err(ShardCoordinatorError::BootNonceMismatch)
        );
        let mut wrong_range = ready_hb(w1, &l1, true);
        wrong_range.loaded_range = ShardLayerRange {
            start_layer: 2,
            end_layer: 4,
        };
        assert_eq!(
            coord.heartbeat(w1, 100, wrong_range),
            Err(ShardCoordinatorError::LeaseRangeMismatch)
        );

        let mut json_val = serde_json::to_value(lease_req(w1, &spec, 2)).unwrap();
        json_val
            .as_object_mut()
            .unwrap()
            .insert("unknown_extra".to_string(), serde_json::json!(1));
        assert!(serde_json::from_value::<ShardLeaseRequest>(json_val).is_err());
    }

    #[test]
    fn community_sharding_no_duplicate_position_or_step_accepted() {
        let model = build_deterministic_integer_model(32, 16, 2, 32, 4, 64, vec![]);
        let spec = model_spec_from(test_hash(b"model-dup"), &model);
        let mut coord = make_two_tile_coordinator(&spec);
        let w1 = test_hash(b"dup-w1");
        let w2 = test_hash(b"dup-w2");
        let l1 = coord
            .request_lease(w1, 100, lease_req(w1, &spec, 2))
            .unwrap()
            .assignment;
        let l2 = coord
            .request_lease(w2, 100, lease_req(w2, &spec, 2))
            .unwrap()
            .assignment;
        coord.heartbeat(w1, 100, ready_hb(w1, &l1, true)).unwrap();
        coord.heartbeat(w2, 100, ready_hb(w2, &l2, true)).unwrap();
        let sid = coord.open_session(100, vec![1], 2).unwrap();

        let poll1 = coord.claim_step(w1, 100, claim_req(w1, &l1)).unwrap();
        let poll2 = coord.claim_step(w1, 101, claim_req(w1, &l1)).unwrap();
        assert_eq!(poll1, poll2);
        let work1 = match poll1 {
            ShardClaimStepResponse::Step { work } => work,
            other => panic!("expected step, got {other:?}"),
        };

        let hidden = vec![9i64; spec.d_model];
        let sub = ShardStepSubmitRequest {
            worker_id: w1,
            lease_id: l1.lease_id,
            lease_epoch: l1.lease_epoch,
            session_id: sid,
            step_id: work1.step_id,
            model_id: spec.model_id,
            execution_profile: spec.execution_profile.clone(),
            range: l1.range,
            position: 0,
            layers_processed: 2,
            outcome: ShardStepOutcome::Completed {
                output: ShardStepOutput::Hidden {
                    hidden_hash: hidden_state_hash(&hidden),
                    hidden,
                },
            },
        };
        assert!(coord.submit_step(w1, 102, sub.clone()).unwrap().accepted);
        assert_eq!(
            coord.submit_step(w1, 103, sub),
            Err(ShardCoordinatorError::UnexpectedStep)
        );
    }

    #[test]
    fn community_sharding_bounds_exhaustion_and_retention_caps() {
        let model = build_deterministic_integer_model(32, 16, 2, 32, 4, 64, vec![]);
        let spec = model_spec_from(test_hash(b"model-bounds"), &model);
        let limits = CommunityShardLimits {
            max_workers: 2,
            max_active_sessions: 1,
            max_sessions_per_worker: 1,
            target_partitions: 2,
            max_retained_results: 2,
            max_cancellations_per_worker: 2,
            max_cancellation_queues: 2,
            max_failed_workers: 2,
            ..CommunityShardLimits::default()
        };
        let mut coord =
            CommunityShardCoordinator::new(test_hash(b"boot-b"), spec.clone(), limits).unwrap();
        let w1 = test_hash(b"bnd-w1");
        let w2 = test_hash(b"bnd-w2");
        let w3 = test_hash(b"bnd-w3");
        let l1 = coord
            .request_lease(w1, 100, lease_req(w1, &spec, 2))
            .unwrap()
            .assignment;
        let l2 = coord
            .request_lease(w2, 100, lease_req(w2, &spec, 2))
            .unwrap()
            .assignment;
        assert_eq!(
            coord.request_lease(w3, 100, lease_req(w3, &spec, 2)),
            Err(ShardCoordinatorError::WorkerCapacityReached)
        );
        coord.heartbeat(w1, 100, ready_hb(w1, &l1, true)).unwrap();
        coord.heartbeat(w2, 100, ready_hb(w2, &l2, true)).unwrap();

        let s1 = coord.open_session(100, vec![1], 2).unwrap();
        assert_eq!(
            coord.open_session(100, vec![1], 2),
            Err(ShardCoordinatorError::SessionCapacityReached)
        );
        coord
            .cancel_session(101, s1, SessionAbortReason::CallerCancelled)
            .unwrap();
        let s2 = coord.open_session(102, vec![1], 2).unwrap();
        coord
            .cancel_session(103, s2, SessionAbortReason::CallerCancelled)
            .unwrap();
        let s3 = coord.open_session(104, vec![1], 2).unwrap();
        coord
            .cancel_session(105, s3, SessionAbortReason::CallerCancelled)
            .unwrap();

        let snap = coord.status_snapshot(106);
        assert_eq!(snap.retained_results, 2);
        assert!(coord.peek_result(s1).is_none());
        assert!(coord.peek_result(s2).is_some());
        assert!(coord.peek_result(s3).is_some());
        assert!(snap.queued_cancellations <= 4);

        coord
            .quarantine_worker(test_hash(b"q1"), 200, WorkerFaultReason::ProtocolViolation)
            .unwrap();
        coord
            .quarantine_worker(
                test_hash(b"q2"),
                200,
                WorkerFaultReason::TamperedHiddenState,
            )
            .unwrap();
        coord
            .quarantine_worker(
                test_hash(b"q3"),
                200,
                WorkerFaultReason::OutputVerificationMismatch,
            )
            .unwrap();
        assert_eq!(coord.status_snapshot(201).quarantined_workers, 2);
    }

    #[test]
    fn community_sharding_zero_and_overflow_params_fail() {
        let model = build_deterministic_integer_model(32, 16, 2, 32, 4, 64, vec![]);
        let valid_spec = model_spec_from(test_hash(b"model-zero"), &model);
        let valid_limits = CommunityShardLimits {
            target_partitions: 2,
            ..CommunityShardLimits::default()
        };

        assert!(
            CommunityShardCoordinator::new(Hash256::ZERO, valid_spec.clone(), valid_limits.clone())
                .is_err()
        );

        let mut single_layer = valid_spec.clone();
        single_layer.n_layers = 1;
        assert!(
            CommunityShardCoordinator::new(
                test_hash(b"boot-z"),
                single_layer,
                valid_limits.clone()
            )
            .is_err()
        );

        let mut zero_vocab = valid_spec.clone();
        zero_vocab.vocab_size = 0;
        assert!(
            CommunityShardCoordinator::new(test_hash(b"boot-z"), zero_vocab, valid_limits.clone())
                .is_err()
        );

        let mut too_many_workers = valid_limits.clone();
        too_many_workers.max_workers = MAX_WORKERS_HARD_CAP + 1;
        assert!(
            CommunityShardCoordinator::new(
                test_hash(b"boot-z"),
                valid_spec.clone(),
                too_many_workers
            )
            .is_err()
        );

        let mut zero_ttl = valid_limits.clone();
        zero_ttl.step_timeout_ms = 0;
        assert!(
            CommunityShardCoordinator::new(test_hash(b"boot-z"), valid_spec.clone(), zero_ttl)
                .is_err()
        );

        let mut coord =
            CommunityShardCoordinator::new(test_hash(b"boot-z"), valid_spec.clone(), valid_limits)
                .unwrap();
        let w1 = test_hash(b"ovf-w1");
        let w2 = test_hash(b"ovf-w2");
        assert!(matches!(
            coord.request_lease(w1, 100, lease_req(w1, &valid_spec, 0)),
            Err(ShardCoordinatorError::InsufficientLayerBudget { .. })
        ));
        assert_eq!(
            coord.request_lease(w1, u64::MAX, lease_req(w1, &valid_spec, 2)),
            Err(ShardCoordinatorError::TimestampOverflow)
        );
        let l1 = coord
            .request_lease(w1, 100, lease_req(w1, &valid_spec, 2))
            .unwrap()
            .assignment;
        let l2 = coord
            .request_lease(w2, 100, lease_req(w2, &valid_spec, 2))
            .unwrap()
            .assignment;
        coord.heartbeat(w1, 100, ready_hb(w1, &l1, true)).unwrap();
        coord.heartbeat(w2, 100, ready_hb(w2, &l2, true)).unwrap();

        assert!(matches!(
            coord.open_session(100, vec![], 2),
            Err(ShardCoordinatorError::InvalidPrompt(_))
        ));
        assert!(matches!(
            coord.open_session(100, vec![1], 0),
            Err(ShardCoordinatorError::InvalidPrompt(_))
        ));
        assert!(matches!(
            coord.open_session(100, vec![1; 64], 1),
            Err(ShardCoordinatorError::InvalidPrompt(_))
        ));
        assert_eq!(
            coord.open_session(u64::MAX, vec![1], 1),
            Err(ShardCoordinatorError::TimestampOverflow)
        );
    }

    #[test]
    fn community_sharding_hard_caps_enforced_for_sequence_tokens_and_retention() {
        let model = build_deterministic_integer_model(32, 16, 2, 32, 4, 64, vec![]);
        let base_spec = model_spec_from(test_hash(b"model-caps"), &model);
        let base_limits = CommunityShardLimits {
            target_partitions: 2,
            ..CommunityShardLimits::default()
        };

        for bad_seq in [0usize, 1, MAX_SEQUENCE_POSITIONS_HARD_CAP + 1] {
            let mut spec = base_spec.clone();
            spec.max_seq = bad_seq;
            assert!(matches!(
                CommunityShardCoordinator::new(test_hash(b"boot-c"), spec, base_limits.clone()),
                Err(ShardCoordinatorError::InvalidConfig(_))
            ));
        }

        let mut bad_results = base_limits.clone();
        bad_results.max_retained_results = MAX_RETAINED_RESULTS_HARD_CAP + 1;
        assert!(matches!(
            CommunityShardCoordinator::new(test_hash(b"boot-c"), base_spec.clone(), bad_results),
            Err(ShardCoordinatorError::InvalidConfig(_))
        ));

        let mut bad_cancels = base_limits.clone();
        bad_cancels.max_cancellations_per_worker = MAX_CANCELLATIONS_PER_WORKER_HARD_CAP + 1;
        assert!(matches!(
            CommunityShardCoordinator::new(test_hash(b"boot-c"), base_spec.clone(), bad_cancels),
            Err(ShardCoordinatorError::InvalidConfig(_))
        ));

        let mut bad_queues = base_limits.clone();
        bad_queues.max_cancellation_queues = MAX_CANCELLATION_QUEUES_HARD_CAP + 1;
        assert!(matches!(
            CommunityShardCoordinator::new(test_hash(b"boot-c"), base_spec.clone(), bad_queues),
            Err(ShardCoordinatorError::InvalidConfig(_))
        ));

        let mut bad_failed = base_limits.clone();
        bad_failed.max_failed_workers = MAX_FAILED_WORKERS_HARD_CAP + 1;
        assert!(matches!(
            CommunityShardCoordinator::new(test_hash(b"boot-c"), base_spec.clone(), bad_failed),
            Err(ShardCoordinatorError::InvalidConfig(_))
        ));

        let mut large_seq_spec = base_spec;
        large_seq_spec.max_seq = MAX_SEQUENCE_POSITIONS_HARD_CAP;
        let mut coord = CommunityShardCoordinator::new(
            test_hash(b"boot-c"),
            large_seq_spec.clone(),
            base_limits,
        )
        .unwrap();
        let w1 = test_hash(b"cap-w1");
        let w2 = test_hash(b"cap-w2");
        let l1 = coord
            .request_lease(w1, 100, lease_req(w1, &large_seq_spec, 2))
            .unwrap()
            .assignment;
        let l2 = coord
            .request_lease(w2, 100, lease_req(w2, &large_seq_spec, 2))
            .unwrap()
            .assignment;
        coord.heartbeat(w1, 100, ready_hb(w1, &l1, true)).unwrap();
        coord.heartbeat(w2, 100, ready_hb(w2, &l2, true)).unwrap();

        assert!(matches!(
            coord.open_session(100, vec![1, 2], MAX_GENERATED_TOKENS_HARD_CAP + 1),
            Err(ShardCoordinatorError::InvalidPrompt(_))
        ));
        assert!(matches!(
            coord.open_session(100, vec![1; MAX_SEQUENCE_POSITIONS_HARD_CAP], 1),
            Err(ShardCoordinatorError::InvalidPrompt(_))
        ));
        assert!(
            coord
                .open_session(100, vec![1, 2], MAX_GENERATED_TOKENS_HARD_CAP)
                .is_ok()
        );
    }

    #[test]
    fn community_sharding_request_lease_idempotent_retry_and_rebalance_epoch_fence() {
        let model = build_deterministic_integer_model(32, 16, 2, 32, 4, 64, vec![]);
        let spec = model_spec_from(test_hash(b"model-idem"), &model);
        let mut coord = make_two_tile_coordinator(&spec);
        let w1 = test_hash(b"idem-w1");
        let w2 = test_hash(b"idem-w2");
        let w3 = test_hash(b"idem-w3");

        let resp1 = coord
            .request_lease(w1, 1_000, lease_req(w1, &spec, 2))
            .unwrap();
        let retry_prov = coord
            .request_lease(w1, 50_000, lease_req(w1, &spec, 2))
            .unwrap();
        assert_eq!(retry_prov, resp1);
        assert_eq!(
            coord.worker_lease_snapshot(w1).unwrap().deadline_unix_ms,
            1_000 + DEFAULT_PROVISIONING_TTL_MS
        );
        assert_eq!(
            coord.request_lease(w1, 50_000, lease_req(w1, &spec, 4)),
            Err(ShardCoordinatorError::LeaseCapacityConflict {
                existing: 2,
                requested: 4
            })
        );

        let hb_ready = coord
            .heartbeat(w1, 60_000, ready_hb(w1, &resp1.assignment, true))
            .unwrap();
        assert_eq!(
            hb_ready.directive,
            HeartbeatDirective::Active {
                expires_at_unix_ms: 60_000 + DEFAULT_READY_TTL_MS
            }
        );
        let retry_ready = coord
            .request_lease(w1, 70_000, lease_req(w1, &spec, 2))
            .unwrap();
        assert_eq!(retry_ready, resp1);
        let snap_after_retry = coord.worker_lease_snapshot(w1).unwrap();
        assert!(snap_after_retry.ready);
        assert_eq!(snap_after_retry.lease_epoch, 1);
        assert_eq!(
            snap_after_retry.deadline_unix_ms,
            60_000 + DEFAULT_READY_TTL_MS
        );

        let l2 = coord
            .request_lease(w2, 70_000, lease_req(w2, &spec, 2))
            .unwrap()
            .assignment;
        let l3 = coord
            .request_lease(w3, 70_000, lease_req(w3, &spec, 2))
            .unwrap()
            .assignment;
        coord
            .heartbeat(w2, 70_000, ready_hb(w2, &l2, true))
            .unwrap();
        coord
            .heartbeat(w3, 70_000, ready_hb(w3, &l3, true))
            .unwrap();

        let reb_time = 80_000u64;
        coord
            .release_lease(
                w2,
                reb_time,
                ShardReleaseRequest {
                    worker_id: w2,
                    lease_id: l2.lease_id,
                    lease_epoch: l2.lease_epoch,
                },
            )
            .unwrap();

        let (moved_w, old_assign) = if coord.worker_lease_snapshot(w1).unwrap().range == l2.range {
            (w1, resp1.assignment.clone())
        } else {
            (w3, l3.clone())
        };
        let retry_reb = coord
            .request_lease(moved_w, reb_time + 5_000, lease_req(moved_w, &spec, 2))
            .unwrap();
        assert_eq!(retry_reb.assignment.lease_epoch, 2);
        assert_eq!(retry_reb.assignment.range, l2.range);
        assert_eq!(
            retry_reb.assignment.provisioning_deadline_unix_ms,
            reb_time + DEFAULT_PROVISIONING_TTL_MS
        );

        let mut wrong_old_range = ready_hb(moved_w, &old_assign, true);
        wrong_old_range.loaded_range = l2.range;
        assert_eq!(
            coord.heartbeat(moved_w, reb_time + 6_000, wrong_old_range),
            Err(ShardCoordinatorError::LeaseRangeMismatch)
        );

        let old_hb = coord
            .heartbeat(
                moved_w,
                reb_time + 7_000,
                ready_hb(moved_w, &old_assign, true),
            )
            .unwrap();
        assert_eq!(
            old_hb.directive,
            HeartbeatDirective::Rebalance {
                assignment: retry_reb.assignment.clone()
            }
        );
        assert!(!coord.worker_lease_snapshot(moved_w).unwrap().ready);

        let new_prov_hb = coord
            .heartbeat(
                moved_w,
                reb_time + 8_000,
                ready_hb(moved_w, &retry_reb.assignment, false),
            )
            .unwrap();
        assert_eq!(
            new_prov_hb.directive,
            HeartbeatDirective::Provisioning {
                deadline_unix_ms: reb_time + DEFAULT_PROVISIONING_TTL_MS
            }
        );

        assert_eq!(
            coord.heartbeat(
                moved_w,
                reb_time + 9_000,
                ready_hb(moved_w, &old_assign, true)
            ),
            Err(ShardCoordinatorError::LeaseEpochMismatch {
                expected: 2,
                actual: 1
            })
        );
        assert_eq!(
            coord.claim_step(moved_w, reb_time + 9_000, claim_req(moved_w, &old_assign)),
            Err(ShardCoordinatorError::LeaseEpochMismatch {
                expected: 2,
                actual: 1
            })
        );

        let exp = coord.expire_stale(reb_time + DEFAULT_PROVISIONING_TTL_MS);
        assert!(exp.expired_leases >= 1);
        assert!(coord.worker_lease_snapshot(moved_w).is_none());
    }

    #[test]
    fn community_sharding_rebalance_skips_unfillable_gap_and_overflowed_epoch() {
        let model = build_deterministic_integer_model(32, 16, 2, 32, 7, 64, vec![]);
        let spec = model_spec_from(test_hash(b"model-unequal"), &model);
        let limits = CommunityShardLimits {
            target_partitions: 3,
            ..CommunityShardLimits::default()
        };
        let mut coord =
            CommunityShardCoordinator::new(test_hash(b"boot-u1"), spec.clone(), limits.clone())
                .unwrap();
        assert_eq!(
            coord.fixed_tiles(),
            &[
                ShardLayerRange {
                    start_layer: 0,
                    end_layer: 3
                },
                ShardLayerRange {
                    start_layer: 3,
                    end_layer: 5
                },
                ShardLayerRange {
                    start_layer: 5,
                    end_layer: 7
                },
            ]
        );

        let w1 = test_hash(b"uneq-w1");
        let w2 = test_hash(b"uneq-w2");
        let w3 = test_hash(b"uneq-w3");
        let w4 = test_hash(b"uneq-w4");
        let l1 = coord
            .request_lease(w1, 100, lease_req(w1, &spec, 3))
            .unwrap()
            .assignment;
        let l2 = coord
            .request_lease(w2, 100, lease_req(w2, &spec, 2))
            .unwrap()
            .assignment;
        let l3 = coord
            .request_lease(w3, 100, lease_req(w3, &spec, 2))
            .unwrap()
            .assignment;
        let l4 = coord
            .request_lease(w4, 100, lease_req(w4, &spec, 2))
            .unwrap()
            .assignment;
        assert_eq!(l1.range, coord.fixed_tiles()[0]);
        assert_eq!(l2.range, coord.fixed_tiles()[1]);
        assert_eq!(l3.range, coord.fixed_tiles()[2]);
        assert_eq!(l4.range, coord.fixed_tiles()[1]);
        for (w, l) in [(w1, &l1), (w2, &l2), (w3, &l3), (w4, &l4)] {
            coord.heartbeat(w, 100, ready_hb(w, l, true)).unwrap();
        }

        coord
            .release_lease(
                w1,
                200,
                ShardReleaseRequest {
                    worker_id: w1,
                    lease_id: l1.lease_id,
                    lease_epoch: l1.lease_epoch,
                },
            )
            .unwrap();
        assert_eq!(coord.worker_lease_snapshot(w2).unwrap().range, l2.range);
        assert_eq!(coord.worker_lease_snapshot(w4).unwrap().range, l4.range);

        coord
            .release_lease(
                w3,
                300,
                ShardReleaseRequest {
                    worker_id: w3,
                    lease_id: l3.lease_id,
                    lease_epoch: l3.lease_epoch,
                },
            )
            .unwrap();
        let snap2 = coord.worker_lease_snapshot(w2).unwrap();
        let snap4 = coord.worker_lease_snapshot(w4).unwrap();
        let moved_to_t2 = [snap2.clone(), snap4.clone()]
            .into_iter()
            .filter(|s| s.range == l3.range && s.lease_epoch == 2 && !s.ready)
            .count();
        let stayed_on_t1 = [snap2, snap4]
            .into_iter()
            .filter(|s| s.range == l2.range && s.lease_epoch == 1 && s.ready)
            .count();
        assert_eq!(moved_to_t2, 1);
        assert_eq!(stayed_on_t1, 1);

        let mut coord2 =
            CommunityShardCoordinator::new(test_hash(b"boot-u2"), spec.clone(), limits).unwrap();
        let w5 = test_hash(b"uneq-w5");
        let a1 = coord2
            .request_lease(w1, 100, lease_req(w1, &spec, 3))
            .unwrap()
            .assignment;
        let a2 = coord2
            .request_lease(w2, 100, lease_req(w2, &spec, 2))
            .unwrap()
            .assignment;
        let a3 = coord2
            .request_lease(w3, 100, lease_req(w3, &spec, 2))
            .unwrap()
            .assignment;
        let a4 = coord2
            .request_lease(w4, 100, lease_req(w4, &spec, 2))
            .unwrap()
            .assignment;
        let a5 = coord2
            .request_lease(w5, 100, lease_req(w5, &spec, 2))
            .unwrap()
            .assignment;
        for (w, a) in [(w1, &a1), (w2, &a2), (w3, &a3), (w4, &a4), (w5, &a5)] {
            coord2.heartbeat(w, 100, ready_hb(w, a, true)).unwrap();
        }
        let first_t1 = if w2.0 < w4.0 { w2 } else { w4 };
        let other_t1 = if w2.0 < w4.0 { w4 } else { w2 };
        coord2.leases.get_mut(&first_t1.0).unwrap().lease_epoch = u64::MAX;
        coord2
            .release_lease(
                w1,
                200,
                ShardReleaseRequest {
                    worker_id: w1,
                    lease_id: a1.lease_id,
                    lease_epoch: a1.lease_epoch,
                },
            )
            .unwrap();
        coord2
            .release_lease(
                w5,
                250,
                ShardReleaseRequest {
                    worker_id: w5,
                    lease_id: a5.lease_id,
                    lease_epoch: a5.lease_epoch,
                },
            )
            .unwrap();
        coord2
            .release_lease(
                w3,
                300,
                ShardReleaseRequest {
                    worker_id: w3,
                    lease_id: a3.lease_id,
                    lease_epoch: a3.lease_epoch,
                },
            )
            .unwrap();
        let max_snap = coord2.worker_lease_snapshot(first_t1).unwrap();
        let moved_snap = coord2.worker_lease_snapshot(other_t1).unwrap();
        assert_eq!(max_snap.lease_epoch, u64::MAX);
        assert_eq!(max_snap.range, a2.range);
        assert_eq!(moved_snap.lease_epoch, 2);
        assert_eq!(moved_snap.range, a3.range);
        assert!(!moved_snap.ready);
    }

    #[test]
    fn community_sharding_decline_zero_layers_and_cancel_queue_churn_bounded() {
        let model = build_deterministic_integer_model(32, 16, 2, 32, 4, 64, vec![]);
        let spec = model_spec_from(test_hash(b"model-decline"), &model);
        let mut coord = make_two_tile_coordinator(&spec);
        let w1 = test_hash(b"dec-w1");
        let w2 = test_hash(b"dec-w2");

        let l1 = coord
            .request_lease(w1, 100, lease_req(w1, &spec, 2))
            .unwrap()
            .assignment;
        let l2 = coord
            .request_lease(w2, 100, lease_req(w2, &spec, 2))
            .unwrap()
            .assignment;
        coord.heartbeat(w1, 100, ready_hb(w1, &l1, true)).unwrap();
        coord.heartbeat(w2, 100, ready_hb(w2, &l2, true)).unwrap();

        let sid = coord.open_session(100, vec![1], 2).unwrap();
        let work = match coord.claim_step(w1, 100, claim_req(w1, &l1)).unwrap() {
            ShardClaimStepResponse::Step { work } => work,
            other => panic!("expected step, got {other:?}"),
        };

        let bad_decline = ShardStepSubmitRequest {
            worker_id: w1,
            lease_id: l1.lease_id,
            lease_epoch: l1.lease_epoch,
            session_id: sid,
            step_id: work.step_id,
            model_id: spec.model_id,
            execution_profile: spec.execution_profile.clone(),
            range: l1.range,
            position: 0,
            layers_processed: 3,
            outcome: ShardStepOutcome::Declined {
                reason: "busy".to_string(),
            },
        };
        assert!(matches!(
            coord.submit_step(w1, 101, bad_decline),
            Err(ShardCoordinatorError::InvalidLayersProcessed { .. })
        ));

        let ok_decline = ShardStepSubmitRequest {
            worker_id: w1,
            lease_id: l1.lease_id,
            lease_epoch: l1.lease_epoch,
            session_id: sid,
            step_id: work.step_id,
            model_id: spec.model_id,
            execution_profile: spec.execution_profile.clone(),
            range: l1.range,
            position: 0,
            layers_processed: 0,
            outcome: ShardStepOutcome::Declined {
                reason: "quiescing".to_string(),
            },
        };
        let resp = coord.submit_step(w1, 102, ok_decline).unwrap();
        assert!(resp.accepted);
        assert!(resp.session_aborted);
        assert_eq!(
            coord.cancellation_queue_order.len(),
            coord.pending_cancellations.len()
        );

        coord
            .release_lease(
                w2,
                103,
                ShardReleaseRequest {
                    worker_id: w2,
                    lease_id: l2.lease_id,
                    lease_epoch: l2.lease_epoch,
                },
            )
            .unwrap();
        assert_eq!(
            coord.cancellation_queue_order.len(),
            coord.pending_cancellations.len()
        );

        coord
            .quarantine_worker(w1, 200, WorkerFaultReason::TamperedHiddenState)
            .unwrap();
        assert_eq!(
            coord.worker_quarantine_reason(w1, 201),
            Some(WorkerFaultReason::TamperedHiddenState)
        );
        assert_eq!(
            coord.worker_quarantine_reason(w1, 200 + DEFAULT_QUARANTINE_TTL_MS),
            None
        );
    }
    fn register_ready_capacity_worker(
        coord: &mut CommunityShardCoordinator,
        model: &TrustedModelSpec,
        now_ms: u64,
        index: usize,
        max_resident_layers: usize,
    ) -> (Hash256, ShardLeaseAssignment) {
        let worker_id = test_hash(format!("capacity-worker-{index}").as_bytes());
        let assignment = coord
            .request_lease(
                worker_id,
                now_ms,
                ShardLeaseRequest {
                    worker_id,
                    protocol_version: COMMUNITY_SHARD_PROTOCOL_VERSION,
                    model_id: model.model_id,
                    execution_profile: model.execution_profile.clone(),
                    n_layers: model.n_layers,
                    d_model: model.d_model,
                    vocab_size: model.vocab_size,
                    max_seq: model.max_seq,
                    max_resident_layers,
                },
            )
            .unwrap()
            .assignment;
        let hb = coord
            .heartbeat(
                worker_id,
                now_ms,
                ShardHeartbeatRequest {
                    worker_id,
                    lease_id: assignment.lease_id,
                    lease_epoch: assignment.lease_epoch,
                    model_id: model.model_id,
                    execution_profile: model.execution_profile.clone(),
                    loaded_range: assignment.range,
                    ready: true,
                },
            )
            .unwrap();
        assert!(matches!(hb.directive, HeartbeatDirective::Active { .. }));
        (worker_id, assignment)
    }

    #[test]
    fn pipeline_capacity_scales_across_6_12_24_and_100_workers_and_tracks_churn() {
        let model = TrustedModelSpec {
            model_id: test_hash(b"capacity-model-24l"),
            execution_profile: CANONICAL_REWARD_INFERENCE_PROFILE.to_string(),
            n_layers: 24,
            d_model: 16,
            vocab_size: 32,
            max_seq: 64,
            eos_tokens: vec![2],
            bos_token: 0,
        };
        let limits = CommunityShardLimits::default();
        assert_eq!(limits.max_active_sessions, 24);
        assert_eq!(limits.max_sessions_per_worker, 1);
        assert_eq!(limits.target_partitions, 6);

        let now_ms = 10_000u64;
        let mut coord =
            CommunityShardCoordinator::new(test_hash(b"capacity-boot"), model.clone(), limits)
                .unwrap();
        assert_eq!(coord.fixed_tiles().len(), 6);

        let mut registered = Vec::new();
        for index in 0..5 {
            registered.push(register_ready_capacity_worker(
                &mut coord, &model, now_ms, index, 4,
            ));
        }
        let partial = coord.pipeline_capacity(now_ms);
        assert_eq!(partial.ready_complete_pipelines, 0);
        assert_eq!(partial.idle_complete_pipelines, 0);
        assert_eq!(partial.effective_session_capacity, 0);

        for (target_workers, expected_pipelines) in [(6usize, 1usize), (12, 2), (24, 4), (100, 16)]
        {
            while registered.len() < target_workers {
                let next_index = registered.len();
                registered.push(register_ready_capacity_worker(
                    &mut coord, &model, now_ms, next_index, 4,
                ));
            }
            let cap = coord.pipeline_capacity(now_ms);
            assert_eq!(cap.ready_complete_pipelines, expected_pipelines);
            assert_eq!(cap.idle_complete_pipelines, expected_pipelines);
            assert_eq!(cap.effective_session_capacity, expected_pipelines);
            assert_eq!(cap.active_sessions, 0);

            let mut open_ids = Vec::with_capacity(expected_pipelines);
            for _ in 0..expected_pipelines {
                open_ids.push(coord.open_session(now_ms, vec![1], 2).unwrap());
            }
            let busy = coord.pipeline_capacity(now_ms);
            assert_eq!(busy.ready_complete_pipelines, expected_pipelines);
            assert_eq!(busy.idle_complete_pipelines, 0);
            assert_eq!(busy.active_sessions, expected_pipelines);
            assert!(coord.open_session(now_ms, vec![1], 2).is_err());

            for session_id in open_ids {
                coord
                    .cancel_session(now_ms, session_id, SessionAbortReason::CallerCancelled)
                    .unwrap();
            }
            let restored = coord.pipeline_capacity(now_ms);
            assert_eq!(restored.active_sessions, 0);
            assert_eq!(restored.idle_complete_pipelines, expected_pipelines);
        }

        let target_tile = coord.fixed_tiles()[4];
        let tile4_workers: Vec<(Hash256, ShardLeaseAssignment)> = registered
            .iter()
            .filter(|(_, assignment)| assignment.range == target_tile)
            .cloned()
            .collect();
        assert_eq!(tile4_workers.len(), 16);

        let (leaving_worker, leaving_assignment) = &tile4_workers[0];
        coord
            .release_lease(
                *leaving_worker,
                now_ms,
                ShardReleaseRequest {
                    worker_id: *leaving_worker,
                    lease_id: leaving_assignment.lease_id,
                    lease_epoch: leaving_assignment.lease_epoch,
                },
            )
            .unwrap();
        let after_leave = coord.pipeline_capacity(now_ms);
        assert_eq!(after_leave.ready_complete_pipelines, 15);
        assert_eq!(after_leave.effective_session_capacity, 15);

        coord
            .quarantine_worker(
                tile4_workers[1].0,
                now_ms,
                WorkerFaultReason::OutputVerificationMismatch,
            )
            .unwrap();
        let after_quarantine = coord.pipeline_capacity(now_ms);
        assert_eq!(after_quarantine.ready_complete_pipelines, 14);
        assert_eq!(after_quarantine.effective_session_capacity, 14);

        for (worker_id, assignment) in &tile4_workers[2..] {
            coord
                .release_lease(
                    *worker_id,
                    now_ms,
                    ShardReleaseRequest {
                        worker_id: *worker_id,
                        lease_id: assignment.lease_id,
                        lease_epoch: assignment.lease_epoch,
                    },
                )
                .unwrap();
        }
        let uncovered = coord.pipeline_capacity(now_ms);
        assert_eq!(uncovered.ready_complete_pipelines, 0);
        assert_eq!(uncovered.effective_session_capacity, 0);
        assert_eq!(coord.status_snapshot(now_ms).tiles[4].ready_workers, 0);

        let mut expiry_coord = CommunityShardCoordinator::new(
            test_hash(b"capacity-expiry-boot"),
            model.clone(),
            CommunityShardLimits::default(),
        )
        .unwrap();
        for index in 0..6 {
            let _ = register_ready_capacity_worker(&mut expiry_coord, &model, now_ms, index, 4);
        }
        assert_eq!(
            expiry_coord
                .pipeline_capacity(now_ms)
                .ready_complete_pipelines,
            1
        );
        let expired_ms = now_ms + DEFAULT_READY_TTL_MS;
        let expired = expiry_coord.pipeline_capacity(expired_ms);
        assert_eq!(expired.ready_complete_pipelines, 0);
        assert_eq!(expired.effective_session_capacity, 0);
    }

    #[test]
    fn pipeline_capacity_respects_default_24_and_hard_cap_64() {
        let model = TrustedModelSpec {
            model_id: test_hash(b"capacity-model-hardcap"),
            execution_profile: CANONICAL_REWARD_INFERENCE_PROFILE.to_string(),
            n_layers: 24,
            d_model: 16,
            vocab_size: 32,
            max_seq: 64,
            eos_tokens: vec![2],
            bos_token: 0,
        };
        let now_ms = 20_000u64;

        let default_limits = CommunityShardLimits {
            target_partitions: 2,
            ..CommunityShardLimits::default()
        };
        let mut default_coord = CommunityShardCoordinator::new(
            test_hash(b"capacity-default-cap"),
            model.clone(),
            default_limits,
        )
        .unwrap();
        for index in 0..MAX_WORKERS_HARD_CAP {
            let _ = register_ready_capacity_worker(&mut default_coord, &model, now_ms, index, 12);
        }
        let default_cap = default_coord.pipeline_capacity(now_ms);
        assert_eq!(default_cap.ready_complete_pipelines, 64);
        assert_eq!(default_cap.effective_session_capacity, 24);
        for _ in 0..24 {
            let _ = default_coord.open_session(now_ms, vec![1], 2).unwrap();
        }
        assert_eq!(
            default_coord.open_session(now_ms, vec![1], 2),
            Err(ShardCoordinatorError::SessionCapacityReached)
        );

        let hard_limits = CommunityShardLimits {
            target_partitions: 2,
            max_active_sessions: MAX_ACTIVE_SESSIONS_HARD_CAP,
            ..CommunityShardLimits::default()
        };
        let mut hard_coord = CommunityShardCoordinator::new(
            test_hash(b"capacity-hard-cap"),
            model.clone(),
            hard_limits,
        )
        .unwrap();
        for index in 0..MAX_WORKERS_HARD_CAP {
            let _ = register_ready_capacity_worker(&mut hard_coord, &model, now_ms, index, 12);
        }
        let hard_cap = hard_coord.pipeline_capacity(now_ms);
        assert_eq!(hard_cap.ready_complete_pipelines, 64);
        assert_eq!(hard_cap.effective_session_capacity, 64);
        for _ in 0..MAX_ACTIVE_SESSIONS_HARD_CAP {
            let _ = hard_coord.open_session(now_ms, vec![1], 2).unwrap();
        }
        assert_eq!(
            hard_coord.pipeline_capacity(now_ms).idle_complete_pipelines,
            0
        );
        assert_eq!(
            hard_coord.open_session(now_ms, vec![1], 2),
            Err(ShardCoordinatorError::SessionCapacityReached)
        );
    }

    #[test]
    fn heartbeat_ready_transition_heals_coverage_gap_without_deadlock() {
        let model = TrustedModelSpec {
            model_id: test_hash(b"liveness-gap-model-24l"),
            execution_profile: CANONICAL_REWARD_INFERENCE_PROFILE.to_string(),
            n_layers: 24,
            d_model: 16,
            vocab_size: 32,
            max_seq: 64,
            eos_tokens: vec![2],
            bos_token: 0,
        };
        let mut now_ms = 10_000u64;
        let mut coord = CommunityShardCoordinator::new(
            test_hash(b"liveness-gap-boot"),
            model.clone(),
            CommunityShardLimits::default(),
        )
        .unwrap();

        let mut workers: Vec<(Hash256, ShardLeaseAssignment)> = (0..7)
            .map(|idx| {
                let id = test_hash(format!("liveness-worker-{idx:02}").as_bytes());
                let resp = coord
                    .request_lease(id, now_ms, lease_req(id, &model, 4))
                    .unwrap();
                (id, resp.assignment)
            })
            .collect();

        for (id, assignment) in &workers[..6] {
            let hb = coord
                .heartbeat(*id, now_ms, ready_hb(*id, assignment, true))
                .unwrap();
            assert!(matches!(hb.directive, HeartbeatDirective::Active { .. }));
        }

        now_ms += 1_000;
        let (leaving_id, leaving_assignment) = workers.remove(5);
        let missing_tile = leaving_assignment.range;
        coord
            .release_lease(
                leaving_id,
                now_ms,
                ShardReleaseRequest {
                    worker_id: leaving_id,
                    lease_id: leaving_assignment.lease_id,
                    lease_epoch: leaving_assignment.lease_epoch,
                },
            )
            .unwrap();
        assert_eq!(coord.pipeline_capacity(now_ms).ready_complete_pipelines, 0);
        assert_eq!(
            coord.status_snapshot(now_ms).tiles[5].provisioning_workers,
            0
        );

        now_ms += 1_000;
        let (late_id, late_assignment) = workers[5].clone();
        let late_hb = coord
            .heartbeat(late_id, now_ms, ready_hb(late_id, &late_assignment, true))
            .unwrap();
        if let HeartbeatDirective::Rebalance { assignment } = late_hb.directive {
            workers[5].1 = assignment;
        }

        now_ms += 1_000;
        for (id, assignment) in &mut workers {
            let hb = coord
                .heartbeat(*id, now_ms, ready_hb(*id, assignment, true))
                .unwrap();
            if let HeartbeatDirective::Rebalance {
                assignment: rebalanced,
            } = hb.directive
            {
                assert_eq!(rebalanced.range, missing_tile);
                let ready = coord
                    .heartbeat(*id, now_ms, ready_hb(*id, &rebalanced, true))
                    .unwrap();
                assert!(matches!(ready.directive, HeartbeatDirective::Active { .. }));
                *assignment = rebalanced;
            }
        }

        let status = coord.status_snapshot(now_ms);
        assert!(status.fully_covered_ready);
        assert_eq!(coord.pipeline_capacity(now_ms).ready_complete_pipelines, 1);
        assert!(coord.open_session(now_ms, vec![1], 2).is_ok());
    }

    #[test]
    fn rebalance_recovers_post_churn_throughput_and_heterogeneous_tile_balance() {
        let model_24 = TrustedModelSpec {
            model_id: test_hash(b"skew-model-24l"),
            execution_profile: CANONICAL_REWARD_INFERENCE_PROFILE.to_string(),
            n_layers: 24,
            d_model: 16,
            vocab_size: 32,
            max_seq: 64,
            eos_tokens: vec![2],
            bos_token: 0,
        };
        let mut now_ms = 20_000u64;
        let mut coord = CommunityShardCoordinator::new(
            test_hash(b"skew-boot-24l"),
            model_24.clone(),
            CommunityShardLimits::default(),
        )
        .unwrap();

        let mut workers: Vec<(Hash256, ShardLeaseAssignment)> = (0..100)
            .map(|idx| register_ready_capacity_worker(&mut coord, &model_24, now_ms, idx, 4))
            .collect();
        assert_eq!(coord.pipeline_capacity(now_ms).ready_complete_pipelines, 16);

        let tile4 = coord.fixed_tiles()[4];
        let leaving: Vec<(Hash256, ShardLeaseAssignment)> = workers
            .iter()
            .filter(|(_, a)| a.range == tile4)
            .take(15)
            .cloned()
            .collect();
        for (id, assignment) in &leaving {
            coord
                .release_lease(
                    *id,
                    now_ms,
                    ShardReleaseRequest {
                        worker_id: *id,
                        lease_id: assignment.lease_id,
                        lease_epoch: assignment.lease_epoch,
                    },
                )
                .unwrap();
            assert!(coord.pipeline_capacity(now_ms).ready_complete_pipelines >= 1);
        }
        workers.retain(|(id, _)| !leaving.iter().any(|(left, _)| left == id));

        now_ms += 1_000;
        for (id, assignment) in &mut workers {
            let hb = coord
                .heartbeat(*id, now_ms, ready_hb(*id, assignment, true))
                .unwrap();
            if let HeartbeatDirective::Rebalance {
                assignment: rebalanced,
            } = hb.directive
            {
                let ready = coord
                    .heartbeat(*id, now_ms, ready_hb(*id, &rebalanced, true))
                    .unwrap();
                assert!(matches!(ready.directive, HeartbeatDirective::Active { .. }));
                *assignment = rebalanced;
            }
        }
        assert_eq!(coord.pipeline_capacity(now_ms).ready_complete_pipelines, 14);

        let model_32 = TrustedModelSpec {
            model_id: test_hash(b"hetero-model-32l"),
            execution_profile: CANONICAL_REWARD_INFERENCE_PROFILE.to_string(),
            n_layers: 32,
            d_model: 16,
            vocab_size: 32,
            max_seq: 64,
            eos_tokens: vec![2],
            bos_token: 0,
        };
        let mut hetero_ms = 30_000u64;
        let mut hetero_coord = CommunityShardCoordinator::new(
            test_hash(b"hetero-boot-32l"),
            model_32.clone(),
            CommunityShardLimits::default(),
        )
        .unwrap();
        assert_eq!(hetero_coord.fixed_tiles()[0].len(), 6);
        assert_eq!(hetero_coord.fixed_tiles()[1].len(), 6);
        assert_eq!(hetero_coord.fixed_tiles()[2].len(), 5);

        let mut hetero_workers = Vec::with_capacity(100);
        for idx in 0..20 {
            hetero_workers.push(register_ready_capacity_worker(
                &mut hetero_coord,
                &model_32,
                hetero_ms,
                idx,
                6,
            ));
        }
        for idx in 20..100 {
            let worker_id = test_hash(format!("capacity-worker-{idx}").as_bytes());
            let mut assignment = hetero_coord
                .request_lease(worker_id, hetero_ms, lease_req(worker_id, &model_32, 5))
                .unwrap()
                .assignment;
            let hb = hetero_coord
                .heartbeat(worker_id, hetero_ms, ready_hb(worker_id, &assignment, true))
                .unwrap();
            if let HeartbeatDirective::Rebalance {
                assignment: rebalanced,
            } = hb.directive
            {
                assignment = rebalanced;
            }
            hetero_workers.push((worker_id, assignment));
        }

        for round in 0..8 {
            hetero_ms += 100;
            let mut moved = false;
            for (id, assignment) in &mut hetero_workers {
                let hb = hetero_coord
                    .heartbeat(*id, hetero_ms, ready_hb(*id, assignment, true))
                    .unwrap();
                if let HeartbeatDirective::Rebalance {
                    assignment: rebalanced,
                } = hb.directive
                {
                    *assignment = rebalanced;
                    moved = true;
                }
            }
            if !moved && hetero_coord.status_snapshot(hetero_ms).provisioning_workers == 0 {
                break;
            }
            assert!(round < 7);
        }
        let hetero_cap = hetero_coord.pipeline_capacity(hetero_ms);
        assert_eq!(hetero_cap.ready_complete_pipelines, 10);
        let hetero_status = hetero_coord.status_snapshot(hetero_ms);
        assert_eq!(hetero_status.tiles[0].ready_workers, 10);
        assert_eq!(hetero_status.tiles[1].ready_workers, 10);
    }

    #[test]
    fn rejoining_worker_cannot_jump_lru_fairness_queue() {
        let model = TrustedModelSpec {
            model_id: test_hash(b"fairness-model-12l"),
            execution_profile: CANONICAL_REWARD_INFERENCE_PROFILE.to_string(),
            n_layers: 12,
            d_model: 16,
            vocab_size: 32,
            max_seq: 64,
            eos_tokens: vec![2],
            bos_token: 0,
        };
        let mut now_ms = 40_000u64;
        let mut coord = CommunityShardCoordinator::new(
            test_hash(b"fairness-boot"),
            model.clone(),
            CommunityShardLimits::default(),
        )
        .unwrap();

        let workers: Vec<(Hash256, ShardLeaseAssignment)> = (0..12)
            .map(|idx| register_ready_capacity_worker(&mut coord, &model, now_ms, idx, 2))
            .collect();
        let tile5 = coord.fixed_tiles()[5];
        let tile5_workers: Vec<(Hash256, ShardLeaseAssignment)> = workers
            .iter()
            .filter(|(_, a)| a.range == tile5)
            .cloned()
            .collect();
        assert_eq!(tile5_workers.len(), 2);

        let s1 = coord.open_session(now_ms, vec![1], 2).unwrap();
        let chosen_first = tile5_workers
            .iter()
            .find(|(id, _)| coord.worker_lease_snapshot(*id).unwrap().active_sessions == 1)
            .cloned()
            .unwrap();
        let other = tile5_workers
            .iter()
            .find(|(id, _)| *id != chosen_first.0)
            .cloned()
            .unwrap();
        coord
            .cancel_session(now_ms, s1, SessionAbortReason::CallerCancelled)
            .unwrap();

        let s2 = coord.open_session(now_ms, vec![1], 2).unwrap();
        assert_eq!(
            coord
                .worker_lease_snapshot(other.0)
                .unwrap()
                .active_sessions,
            1
        );
        coord
            .cancel_session(now_ms, s2, SessionAbortReason::CallerCancelled)
            .unwrap();

        let s3 = coord.open_session(now_ms, vec![1], 2).unwrap();
        assert_eq!(
            coord
                .worker_lease_snapshot(chosen_first.0)
                .unwrap()
                .active_sessions,
            1
        );
        coord
            .cancel_session(now_ms, s3, SessionAbortReason::CallerCancelled)
            .unwrap();

        now_ms += 100;
        coord
            .release_lease(
                chosen_first.0,
                now_ms,
                ShardReleaseRequest {
                    worker_id: chosen_first.0,
                    lease_id: chosen_first.1.lease_id,
                    lease_epoch: chosen_first.1.lease_epoch,
                },
            )
            .unwrap();
        let rejoined = coord
            .request_lease(chosen_first.0, now_ms, lease_req(chosen_first.0, &model, 2))
            .unwrap();
        assert_eq!(rejoined.assignment.range, tile5);
        let hb = coord
            .heartbeat(
                chosen_first.0,
                now_ms,
                ready_hb(chosen_first.0, &rejoined.assignment, true),
            )
            .unwrap();
        assert!(matches!(hb.directive, HeartbeatDirective::Active { .. }));

        let s4 = coord.open_session(now_ms, vec![1], 2).unwrap();
        assert_eq!(
            coord
                .worker_lease_snapshot(other.0)
                .unwrap()
                .active_sessions,
            1
        );
        assert_eq!(
            coord
                .worker_lease_snapshot(chosen_first.0)
                .unwrap()
                .active_sessions,
            0
        );
        coord
            .cancel_session(now_ms, s4, SessionAbortReason::CallerCancelled)
            .unwrap();
    }

    fn state_model_spec(label: &[u8], n_layers: usize) -> TrustedModelSpec {
        TrustedModelSpec {
            model_id: test_hash(label),
            execution_profile: CANONICAL_REWARD_INFERENCE_PROFILE.to_string(),
            n_layers,
            d_model: 16,
            vocab_size: 32,
            max_seq: 64,
            eos_tokens: vec![2],
            bos_token: 0,
        }
    }

    fn optimal_feasible_pipelines(tiles: &[ShardLayerRange], budgets: &[usize]) -> usize {
        tiles
            .iter()
            .map(|tile| {
                let req = tile.len();
                let needed = tiles.iter().filter(|t| t.len() >= req).count();
                let eligible = budgets.iter().filter(|&&b| b >= req).count();
                eligible / needed
            })
            .min()
            .unwrap_or(0)
    }

    fn seed_state_leases(
        coord: &mut CommunityShardCoordinator,
        spec: &TrustedModelSpec,
        now_ms: u64,
        prefix: &str,
        entries: &[(usize, usize, bool)],
    ) -> Vec<(Hash256, ShardLeaseAssignment, usize)> {
        let tiles = coord.fixed_tiles().to_vec();
        let prov_ttl = coord.limits().provisioning_ttl_ms;
        let ready_ttl = coord.limits().ready_ttl_ms;
        let boot_nonce = coord.boot_nonce();
        entries
            .iter()
            .enumerate()
            .map(|(idx, &(tile_idx, max_resident_layers, ready))| {
                let range = tiles[tile_idx];
                assert!(max_resident_layers >= range.len());
                let worker_id = test_hash(format!("{prefix}-w{idx:02}").as_bytes());
                coord.next_lease_seq += 1;
                let lease_id = LeaseId {
                    boot_nonce,
                    seq: coord.next_lease_seq,
                };
                let prov_deadline = now_ms + prov_ttl;
                let phase = if ready {
                    LeasePhase::Ready {
                        expires_at_unix_ms: now_ms + ready_ttl,
                    }
                } else {
                    LeasePhase::Provisioning {
                        deadline_unix_ms: prov_deadline,
                    }
                };
                coord.leases.insert(
                    worker_id.0,
                    WorkerLease {
                        worker_id,
                        lease_id,
                        lease_epoch: 1,
                        range,
                        max_resident_layers,
                        provisioning_deadline_unix_ms: prov_deadline,
                        phase,
                        pending_rebalance_from: None,
                        active_sessions: BTreeSet::new(),
                        last_assigned_seq: 0,
                    },
                );
                (
                    worker_id,
                    ShardLeaseAssignment {
                        lease_id,
                        lease_epoch: 1,
                        model_id: spec.model_id,
                        execution_profile: spec.execution_profile.clone(),
                        range,
                        provisioning_deadline_unix_ms: prov_deadline,
                    },
                    max_resident_layers,
                )
            })
            .collect()
    }

    fn settle_and_assert_invariants(
        coord: &mut CommunityShardCoordinator,
        workers: &mut [(Hash256, ShardLeaseAssignment, usize)],
        now_ms: &mut u64,
        floor_before: usize,
        pinned_busy: &[(Hash256, u64, ShardLayerRange)],
        reverse_ack: bool,
        label: &str,
    ) {
        let mut rounds = 0usize;
        loop {
            assert!(
                rounds < 24,
                "failed to converge within 24 rounds for {label}"
            );
            rounds += 1;
            *now_ms += 100;
            let mut progressed = false;
            let order: Vec<usize> = if reverse_ack {
                (0..workers.len()).rev().collect()
            } else {
                (0..workers.len()).collect()
            };
            for idx in order {
                let (id, assignment, max_layers) = &mut workers[idx];
                let step_floor = coord.pipeline_capacity(*now_ms).ready_complete_pipelines;
                let hb = coord
                    .heartbeat(*id, *now_ms, ready_hb(*id, assignment, true))
                    .unwrap_or_else(|e| panic!("heartbeat failed for {label}: {e}"));
                let after_floor = coord.pipeline_capacity(*now_ms).ready_complete_pipelines;
                assert!(
                    after_floor >= step_floor && after_floor >= floor_before,
                    "floor dropped step={step_floor}/init={floor_before} -> {after_floor} for {label}"
                );
                if let HeartbeatDirective::Rebalance { assignment: next } = hb.directive {
                    assert!(
                        next.range.len() <= *max_layers
                            && next.lease_epoch == assignment.lease_epoch + 1,
                        "invalid rebalance assignment for {label}: {next:?}"
                    );
                    *assignment = next;
                    progressed = true;
                }
            }
            for &(busy_id, epoch, range) in pinned_busy {
                let snap = coord.worker_lease_snapshot(busy_id).unwrap();
                assert_eq!(
                    (
                        snap.lease_epoch,
                        snap.range,
                        snap.ready,
                        snap.active_sessions
                    ),
                    (epoch, range, true, 1),
                    "busy worker migrated during settle for {label}"
                );
            }
            if !progressed && coord.status_snapshot(*now_ms).provisioning_workers == 0 {
                break;
            }
        }
        let settled: Vec<(u64, ShardLayerRange)> = workers
            .iter()
            .map(|(id, assignment, max_layers)| {
                let snap = coord.worker_lease_snapshot(*id).unwrap();
                assert!(
                    snap.ready
                        && snap.range == assignment.range
                        && snap.lease_epoch == assignment.lease_epoch
                        && snap.range.len() <= *max_layers,
                    "invalid settled lease for {label}: {snap:?}"
                );
                (snap.lease_epoch, snap.range)
            })
            .collect();
        *now_ms += 100;
        for ((id, assignment, _), &(epoch, range)) in workers.iter().zip(settled.iter()) {
            let hb = coord
                .heartbeat(*id, *now_ms, ready_hb(*id, assignment, true))
                .unwrap();
            assert!(
                matches!(hb.directive, HeartbeatDirective::Active { .. }),
                "oscillation after convergence for {label}: {:?}",
                hb.directive
            );
            let snap = coord.worker_lease_snapshot(*id).unwrap();
            assert_eq!((snap.lease_epoch, snap.range), (epoch, range));
        }
    }

    fn run_state_case(
        spec: &TrustedModelSpec,
        limits: &CommunityShardLimits,
        entries: &[(usize, usize, bool)],
        open_sessions: usize,
        reverse_ack: bool,
        label: &str,
    ) {
        let mut now_ms = 50_000u64;
        let mut coord = CommunityShardCoordinator::new(
            test_hash(label.as_bytes()),
            spec.clone(),
            limits.clone(),
        )
        .unwrap();
        let mut workers = seed_state_leases(&mut coord, spec, now_ms, label, entries);
        let budgets: Vec<usize> = entries.iter().map(|&(_, b, _)| b).collect();
        let expected = optimal_feasible_pipelines(coord.fixed_tiles(), &budgets);

        let mut sessions = Vec::with_capacity(open_sessions);
        for _ in 0..open_sessions {
            sessions.push(coord.open_session(now_ms, vec![1], 2).unwrap());
        }
        let pinned: Vec<(Hash256, u64, ShardLayerRange)> = workers
            .iter()
            .filter_map(|(id, _, _)| {
                let snap = coord.worker_lease_snapshot(*id)?;
                (snap.active_sessions > 0).then_some((*id, snap.lease_epoch, snap.range))
            })
            .collect();
        assert_eq!(pinned.len(), open_sessions * coord.fixed_tiles().len());

        let floor_before = coord.pipeline_capacity(now_ms).ready_complete_pipelines;
        settle_and_assert_invariants(
            &mut coord,
            &mut workers,
            &mut now_ms,
            floor_before,
            &pinned,
            reverse_ack,
            label,
        );
        for sid in sessions {
            let pre = coord.pipeline_capacity(now_ms).ready_complete_pipelines;
            coord
                .cancel_session(now_ms, sid, SessionAbortReason::CallerCancelled)
                .unwrap();
            assert!(coord.pipeline_capacity(now_ms).ready_complete_pipelines >= pre);
        }
        if open_sessions > 0 {
            let post = coord.pipeline_capacity(now_ms).ready_complete_pipelines;
            settle_and_assert_invariants(
                &mut coord,
                &mut workers,
                &mut now_ms,
                post,
                &[],
                reverse_ack,
                label,
            );
        }

        let status = coord.status_snapshot(now_ms);
        let cap = coord.pipeline_capacity(now_ms);
        assert_eq!(
            cap.ready_complete_pipelines, expected,
            "settled pipelines mismatch for {label}: tiles={:?}",
            status.tiles
        );
        assert_eq!(
            status.fully_covered_ready,
            expected >= 1,
            "settled coverage mismatch for {label}: tiles={:?}",
            status.tiles
        );
        if expected >= 1 {
            let sid = coord.open_session(now_ms, vec![1], 2).unwrap();
            coord
                .cancel_session(now_ms, sid, SessionAbortReason::CallerCancelled)
                .unwrap();
        }
    }

    #[test]
    fn rebalance_state_model_preserves_invariants_across_homogeneous_heterogeneous_and_ack_retries()
    {
        let homo = state_model_spec(b"model-state-homo-8l", 8);
        let hetero = state_model_spec(b"model-state-hetero-10l", 10);
        let limits = CommunityShardLimits {
            target_partitions: 4,
            ..CommunityShardLimits::default()
        };

        for c0 in 0..=3usize {
            for c1 in 0..=3usize {
                for c2 in 0..=3usize {
                    for c3 in 0..=3usize {
                        let counts = [c0, c1, c2, c3];
                        let mut entries = Vec::new();
                        for (t, &c) in counts.iter().enumerate() {
                            for _ in 0..c {
                                entries.push((t, 2usize, true));
                            }
                        }
                        if let Some(last) = entries.last_mut() {
                            last.2 = false;
                        }
                        run_state_case(
                            &homo,
                            &limits,
                            &entries,
                            0,
                            (c0 + c1 + c2 + c3) % 2 == 1,
                            &format!("homo[{c0},{c1},{c2},{c3}]"),
                        );
                        let min_c = *counts.iter().min().unwrap();
                        if min_c >= 1 {
                            let mut busy_entries = Vec::new();
                            for (t, &c) in counts.iter().enumerate() {
                                for _ in 0..c {
                                    busy_entries.push((t, 2usize, true));
                                }
                            }
                            busy_entries.push((0, 2, false));
                            run_state_case(
                                &homo,
                                &limits,
                                &busy_entries,
                                min_c,
                                false,
                                &format!("busy[{c0},{c1},{c2},{c3}]"),
                            );
                        }
                    }
                }
            }
        }

        let build_hetero = |s: [usize; 4], w: [usize; 2]| {
            let mut entries = Vec::new();
            for (t, &c) in s.iter().enumerate() {
                for _ in 0..c {
                    entries.push((t, 3usize, true));
                }
            }
            for (offset, &c) in w.iter().enumerate() {
                for _ in 0..c {
                    entries.push((offset + 2, 2usize, true));
                }
            }
            if let Some(last) = entries.last_mut() {
                last.2 = false;
            }
            entries
        };
        for &(s, w) in &[
            ([1, 0, 1, 0], [0, 2]),
            ([0, 0, 2, 0], [0, 2]),
            ([2, 1, 1, 0], [1, 3]),
            ([0, 0, 1, 1], [2, 0]),
        ] {
            let entries = build_hetero(s, w);
            for reverse_ack in [false, true] {
                run_state_case(
                    &hetero,
                    &limits,
                    &entries,
                    0,
                    reverse_ack,
                    &format!("augment[{s:?},{w:?},rev={reverse_ack}]"),
                );
            }
        }
        for s0 in 0..=2usize {
            for s1 in 0..=2usize {
                for s2 in 0..=2usize {
                    for s3 in 0..=2usize {
                        for w2 in 0..=2usize {
                            for w3 in 0..=2usize {
                                let entries = build_hetero([s0, s1, s2, s3], [w2, w3]);
                                run_state_case(
                                    &hetero,
                                    &limits,
                                    &entries,
                                    0,
                                    (s0 + s1 + s2 + s3 + w2 + w3) % 2 == 1,
                                    &format!("hetero[{s0},{s1},{s2},{s3};{w2},{w3}]"),
                                );
                            }
                        }
                    }
                }
            }
        }

        let id_a = test_hash(b"drop-ack-a");
        let id_b = test_hash(b"drop-ack-b");
        let (smaller_id, larger_id) = if id_a.0 < id_b.0 {
            (id_a, id_b)
        } else {
            (id_b, id_a)
        };
        let peer1_id = test_hash(b"drop-ack-p1");
        let peer2_id = test_hash(b"drop-ack-p2");
        let departing_id = test_hash(b"drop-ack-dep");
        for release_old in [false, true] {
            let mut now_ms = 90_000u64;
            let mut coord = CommunityShardCoordinator::new(
                test_hash(if release_old {
                    b"drop-ack-rel".as_slice()
                } else {
                    b"drop-ack-rec".as_slice()
                }),
                homo.clone(),
                limits.clone(),
            )
            .unwrap();
            let larger = coord
                .request_lease(larger_id, now_ms, lease_req(larger_id, &homo, 2))
                .unwrap()
                .assignment;
            let p1 = coord
                .request_lease(peer1_id, now_ms, lease_req(peer1_id, &homo, 2))
                .unwrap()
                .assignment;
            let p2 = coord
                .request_lease(peer2_id, now_ms, lease_req(peer2_id, &homo, 2))
                .unwrap()
                .assignment;
            let dep = coord
                .request_lease(departing_id, now_ms, lease_req(departing_id, &homo, 2))
                .unwrap()
                .assignment;
            let smaller = coord
                .request_lease(smaller_id, now_ms, lease_req(smaller_id, &homo, 2))
                .unwrap()
                .assignment;
            for (id, a) in [
                (larger_id, &larger),
                (peer1_id, &p1),
                (peer2_id, &p2),
                (departing_id, &dep),
            ] {
                coord.heartbeat(id, now_ms, ready_hb(id, a, true)).unwrap();
            }
            now_ms += 1_000;
            coord
                .release_lease(
                    departing_id,
                    now_ms,
                    ShardReleaseRequest {
                        worker_id: departing_id,
                        lease_id: dep.lease_id,
                        lease_epoch: dep.lease_epoch,
                    },
                )
                .unwrap();
            now_ms += 1_000;
            let rebalanced = match coord
                .heartbeat(smaller_id, now_ms, ready_hb(smaller_id, &smaller, true))
                .unwrap()
                .directive
            {
                HeartbeatDirective::Rebalance { assignment } => assignment,
                other => panic!("expected direct self-rebalance, got {other:?}"),
            };
            assert_eq!((rebalanced.lease_epoch, rebalanced.range), (2, dep.range));
            now_ms += 100;
            assert_eq!(
                coord
                    .heartbeat(smaller_id, now_ms, ready_hb(smaller_id, &smaller, true))
                    .unwrap()
                    .directive,
                HeartbeatDirective::Rebalance {
                    assignment: rebalanced.clone(),
                }
            );
            assert_eq!(
                coord
                    .claim_step(smaller_id, now_ms, claim_req(smaller_id, &smaller))
                    .unwrap(),
                ShardClaimStepResponse::RebalanceRequired {
                    assignment: rebalanced.clone(),
                }
            );
            let mut bad_range = ready_hb(smaller_id, &smaller, true);
            bad_range.loaded_range = rebalanced.range;
            assert_eq!(
                coord.heartbeat(smaller_id, now_ms, bad_range),
                Err(ShardCoordinatorError::LeaseRangeMismatch)
            );
            if release_old {
                let rel = coord
                    .release_lease(
                        smaller_id,
                        now_ms + 100,
                        ShardReleaseRequest {
                            worker_id: smaller_id,
                            lease_id: smaller.lease_id,
                            lease_epoch: smaller.lease_epoch,
                        },
                    )
                    .unwrap();
                assert!(rel.released && coord.worker_lease_snapshot(smaller_id).is_none());
            } else {
                let prov = coord
                    .heartbeat(
                        smaller_id,
                        now_ms + 100,
                        ready_hb(smaller_id, &rebalanced, false),
                    )
                    .unwrap();
                assert!(matches!(
                    prov.directive,
                    HeartbeatDirective::Provisioning { .. }
                ));
                assert_eq!(
                    coord.heartbeat(
                        smaller_id,
                        now_ms + 100,
                        ready_hb(smaller_id, &smaller, true)
                    ),
                    Err(ShardCoordinatorError::LeaseEpochMismatch {
                        expected: 2,
                        actual: 1,
                    })
                );
                let active = coord
                    .heartbeat(
                        smaller_id,
                        now_ms + 200,
                        ready_hb(smaller_id, &rebalanced, true),
                    )
                    .unwrap();
                assert!(matches!(
                    active.directive,
                    HeartbeatDirective::Active { .. }
                ));
                assert_eq!(
                    coord
                        .pipeline_capacity(now_ms + 200)
                        .ready_complete_pipelines,
                    1
                );
            }
        }
    }
}
