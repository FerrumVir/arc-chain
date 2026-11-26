use crate::community_sharding::hidden_values_within_bounds;
use crate::community_sharding::{
    DEFAULT_JOB_DEADLINE_MS, DEFAULT_MAX_RETAINED_RESULTS, DEFAULT_MAX_SESSIONS_PER_WORKER,
    DEFAULT_STEP_TIMEOUT_MS, LeaseId, MAX_ACTIVE_SESSIONS_HARD_CAP, MAX_EOS_TOKENS,
    MAX_HIDDEN_LEN_HARD_CAP, MAX_MODEL_LAYERS_HARD_CAP, MAX_SEQUENCE_POSITIONS_HARD_CAP,
    REPETITION_PENALTY_WINDOW, SessionCancellationNotice, SessionId, ShardLayerRange,
    ShardLeaseAssignment, ShardReleaseRequest, ShardStepInput, ShardStepOutcome, ShardStepOutput,
    ShardStepSubmitRequest, ShardStepWork, StepId, TrustedModelSpec, hidden_state_hash,
};
use arc_crypto::Hash256;
use arc_inference::cached_integer_model::{
    ArithmeticProfile, CANONICAL_REWARD_INFERENCE_PROFILE, CachedIntegerModel, CachedLayer,
    I8Weights, KVCache, ShardInput, ShardOutput,
};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::sync::Arc;

pub const DEFAULT_MAX_KV_CACHE_BYTES: usize = 256 * 1024 * 1024;
pub const MAX_KV_CACHE_BYTES_HARD_CAP: usize = 1024 * 1024 * 1024;
pub const MAX_STEP_DEADLINE_WINDOW_MS: u64 = DEFAULT_STEP_TIMEOUT_MS;
pub const MAX_SESSION_TTL_MS: u64 = DEFAULT_JOB_DEADLINE_MS;
pub const DEFAULT_MAX_TOMBSTONES: usize = DEFAULT_MAX_RETAINED_RESULTS;
pub const MAX_TOMBSTONES_HARD_CAP: usize = 256;
pub const MAX_FENCED_BOOT_NONCES_HARD_CAP: usize = 64;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShardExecutorConfig {
    pub max_active_sessions: usize,
    pub max_kv_cache_bytes: usize,
    pub max_step_deadline_window_ms: u64,
    pub session_ttl_ms: u64,
    pub tombstone_ttl_ms: u64,
    pub max_tombstones: usize,
}

impl Default for ShardExecutorConfig {
    fn default() -> Self {
        Self {
            max_active_sessions: DEFAULT_MAX_SESSIONS_PER_WORKER,
            max_kv_cache_bytes: DEFAULT_MAX_KV_CACHE_BYTES,
            max_step_deadline_window_ms: MAX_STEP_DEADLINE_WINDOW_MS,
            session_ttl_ms: MAX_SESSION_TTL_MS,
            tombstone_ttl_ms: MAX_SESSION_TTL_MS,
            max_tombstones: DEFAULT_MAX_TOMBSTONES,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecutorSweepSummary {
    pub expired_sessions: usize,
    pub pruned_tombstones: usize,
    pub pruned_terminal_replies: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShardExecutorStatus {
    pub bound: bool,
    pub active_sessions: usize,
    pub reserved_kv_bytes: usize,
    pub tombstones: usize,
    pub terminal_reply_tombstones: usize,
    pub fenced_boot_nonces: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ShardExecutorError {
    InvalidConfig(&'static str),
    InvalidIdentity(&'static str),
    InvalidModelSpec(&'static str),
    InvalidLease(&'static str),
    FullModelNotAllowed,
    InvalidPartialModel(&'static str),
    CoordinatorMismatch,
    ExecutorReleased,
    BootNonceFenced,
    FencedBootNonceCapacityReached,
    StaleLease,
    BootNonceMismatch,
    LeaseIdMismatch,
    LeaseEpochMismatch {
        expected: u64,
        actual: u64,
    },
    ModelMismatch,
    ProfileMismatch,
    RangeMismatch {
        expected: ShardLayerRange,
        actual: ShardLayerRange,
    },
    TerminalModeMismatch {
        expected_terminal: bool,
        actual_terminal: bool,
    },
    InputKindMismatch {
        expected_token: bool,
    },
    TokenOutOfRange {
        token_id: u32,
        vocab_size: usize,
    },
    InvalidHiddenLength {
        expected: usize,
        actual: usize,
    },
    HiddenValueOutOfBounds,
    HiddenHashMismatch,
    ZeroDigest,
    HistoryTooLong {
        max_len: usize,
        actual: usize,
    },
    PositionOutOfRange {
        position: usize,
        max_seq: usize,
    },
    StepDeadlineExpired,
    StepDeadlineExceedsCap {
        max_deadline_unix_ms: u64,
        actual_deadline_unix_ms: u64,
    },
    TimestampOverflow,
    SessionTombstoned,
    SessionCapacityReached,
    TombstoneCapacityReached,
    ConflictingStepRetry,
    DuplicatePositionStepMismatch {
        position: usize,
        expected_step_id: StepId,
        actual_step_id: StepId,
    },
    UnexpectedPosition {
        expected: usize,
        actual: usize,
    },
    NonMonotonicStepId {
        previous_seq: u64,
        actual_seq: u64,
    },
    KvCacheBudgetExceeded {
        requested_bytes: usize,
        limit_bytes: usize,
    },
    KvCacheAllocationFailed,
    KvCacheCorrupted,
    ForwardFailed(String),
}

impl fmt::Display for ShardExecutorError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidConfig(msg) => write!(f, "invalid shard executor config: {msg}"),
            Self::InvalidIdentity(msg) => write!(f, "invalid shard executor identity: {msg}"),
            Self::InvalidModelSpec(msg) => write!(f, "invalid trusted model spec: {msg}"),
            Self::InvalidLease(msg) => write!(f, "invalid shard lease assignment: {msg}"),
            Self::FullModelNotAllowed => {
                write!(
                    f,
                    "full-model resident weights or full layer range rejected"
                )
            }
            Self::InvalidPartialModel(msg) => write!(f, "invalid partial model: {msg}"),
            Self::CoordinatorMismatch => write!(f, "coordinator identity does not match pinned id"),
            Self::ExecutorReleased => write!(f, "executor lease has been released"),
            Self::BootNonceFenced => write!(f, "boot_nonce has been permanently fenced"),
            Self::FencedBootNonceCapacityReached => {
                write!(f, "fenced boot_nonce table is at capacity")
            }
            Self::StaleLease => write!(f, "rebind lease must strictly advance lease progression"),
            Self::BootNonceMismatch => write!(f, "boot_nonce does not match active lease"),
            Self::LeaseIdMismatch => write!(f, "lease_id does not match active lease"),
            Self::LeaseEpochMismatch { expected, actual } => {
                write!(f, "lease_epoch {actual} does not match expected {expected}")
            }
            Self::ModelMismatch => write!(f, "model_id does not match bound model spec"),
            Self::ProfileMismatch => {
                write!(f, "execution_profile does not match canonical profile")
            }
            Self::RangeMismatch { expected, actual } => write!(
                f,
                "layer range [{}, {}) does not match bound [{}, {})",
                actual.start_layer, actual.end_layer, expected.start_layer, expected.end_layer
            ),
            Self::TerminalModeMismatch {
                expected_terminal,
                actual_terminal,
            } => write!(
                f,
                "terminal mode {actual_terminal} does not match expected {expected_terminal}"
            ),
            Self::InputKindMismatch { expected_token } => {
                write!(
                    f,
                    "step input kind mismatch (expected_token={expected_token})"
                )
            }
            Self::TokenOutOfRange {
                token_id,
                vocab_size,
            } => write!(
                f,
                "token {token_id} out of range for vocab_size {vocab_size}"
            ),
            Self::InvalidHiddenLength { expected, actual } => {
                write!(
                    f,
                    "hidden length {actual} does not match expected {expected}"
                )
            }
            Self::HiddenValueOutOfBounds => write!(
                f,
                "hidden activation exceeds the bounded community shard numeric envelope"
            ),
            Self::HiddenHashMismatch => write!(f, "hidden state hash mismatch"),
            Self::ZeroDigest => write!(f, "digest must be nonzero"),
            Self::HistoryTooLong { max_len, actual } => {
                write!(
                    f,
                    "generated_tokens length {actual} exceeds limit {max_len}"
                )
            }
            Self::PositionOutOfRange { position, max_seq } => {
                write!(f, "position {position} out of range for max_seq {max_seq}")
            }
            Self::StepDeadlineExpired => write!(f, "step deadline already expired"),
            Self::StepDeadlineExceedsCap {
                max_deadline_unix_ms,
                actual_deadline_unix_ms,
            } => write!(
                f,
                "step deadline {actual_deadline_unix_ms} exceeds cap {max_deadline_unix_ms}"
            ),
            Self::TimestampOverflow => write!(f, "timestamp arithmetic overflowed"),
            Self::SessionTombstoned => write!(f, "session is tombstoned and cannot be reused"),
            Self::SessionCapacityReached => write!(f, "active session limit reached"),
            Self::TombstoneCapacityReached => write!(f, "session tombstone capacity reached"),
            Self::ConflictingStepRetry => {
                write!(f, "duplicate step_id received with altered payload")
            }
            Self::DuplicatePositionStepMismatch {
                position,
                expected_step_id,
                actual_step_id,
            } => write!(
                f,
                "position {position} already completed with step seq {}, got seq {}",
                expected_step_id.seq, actual_step_id.seq
            ),
            Self::UnexpectedPosition { expected, actual } => {
                write!(f, "unexpected position {actual}, expected {expected}")
            }
            Self::NonMonotonicStepId {
                previous_seq,
                actual_seq,
            } => write!(
                f,
                "step_id seq {actual_seq} must be greater than previous seq {previous_seq}"
            ),
            Self::KvCacheBudgetExceeded {
                requested_bytes,
                limit_bytes,
            } => write!(
                f,
                "kv cache reservation {requested_bytes} bytes exceeds cap {limit_bytes} bytes"
            ),
            Self::KvCacheAllocationFailed => write!(f, "kv cache reservation allocation failed"),
            Self::KvCacheCorrupted => write!(f, "kv cache length invariant violated"),
            Self::ForwardFailed(detail) => write!(f, "partial shard forward failed: {detail}"),
        }
    }
}

impl std::error::Error for ShardExecutorError {}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct SessionKey {
    boot_nonce: [u8; 32],
    seq: u64,
}

impl SessionKey {
    fn from_id(id: SessionId) -> Self {
        Self {
            boot_nonce: id.boot_nonce.0,
            seq: id.seq,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum StepInputFingerprint {
    Token(u32),
    Hidden { len: usize, hidden_hash: Hash256 },
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct StepPayloadFingerprint {
    session_id: SessionId,
    step_id: StepId,
    lease_id: LeaseId,
    lease_epoch: u64,
    model_id: Hash256,
    execution_profile: String,
    range: ShardLayerRange,
    position: usize,
    expected_hidden_len: usize,
    expect_terminal: bool,
    last_token: bool,
    deadline_unix_ms: u64,
    input: StepInputFingerprint,
    generated_tokens: Vec<u32>,
}

impl StepPayloadFingerprint {
    fn from_work(work: &ShardStepWork) -> Self {
        let input = match &work.input {
            ShardStepInput::Token { token } => StepInputFingerprint::Token(*token),
            ShardStepInput::Hidden {
                hidden,
                hidden_hash,
            } => StepInputFingerprint::Hidden {
                len: hidden.len(),
                hidden_hash: *hidden_hash,
            },
        };
        Self {
            session_id: work.session_id,
            step_id: work.step_id,
            lease_id: work.lease_id,
            lease_epoch: work.lease_epoch,
            model_id: work.model_id,
            execution_profile: work.execution_profile.clone(),
            range: work.range,
            position: work.position,
            expected_hidden_len: work.expected_hidden_len,
            expect_terminal: work.expect_terminal,
            last_token: work.last_token,
            deadline_unix_ms: work.deadline_unix_ms,
            input,
            generated_tokens: work.generated_tokens.clone(),
        }
    }
}

#[derive(Debug, Clone)]
struct CachedStepReply {
    fingerprint: StepPayloadFingerprint,
    response: ShardStepSubmitRequest,
}

struct ActiveShardSession {
    expires_at_unix_ms: u64,
    next_position: usize,
    reserved_kv_bytes: usize,
    kv_cache: KVCache,
    last_completed: CachedStepReply,
}

struct TombstoneEntry {
    expires_at_unix_ms: u64,
    terminal_reply: Option<CachedStepReply>,
}

struct BoundShard {
    model_spec: TrustedModelSpec,
    lease: ShardLeaseAssignment,
    model: Arc<CachedIntegerModel>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct HighWaterLease {
    boot_nonce: Hash256,
    lease_seq: u64,
    lease_epoch: u64,
}

pub struct CommunityShardExecutor {
    worker_id: Hash256,
    coordinator_id: Hash256,
    config: ShardExecutorConfig,
    bound: Option<BoundShard>,
    high_water_lease: HighWaterLease,
    fenced_boot_nonces: BTreeSet<[u8; 32]>,
    active_sessions: BTreeMap<SessionKey, ActiveShardSession>,
    tombstones: BTreeMap<SessionKey, TombstoneEntry>,
    reserved_kv_bytes: usize,
}

pub fn estimate_kv_cache_bytes(layer_count: usize, d_kv: usize, positions: usize) -> Option<usize> {
    layer_count
        .checked_mul(d_kv)?
        .checked_mul(positions)?
        .checked_mul(2)?
        .checked_mul(std::mem::size_of::<i64>())
}

fn is_zero_hash(hash: Hash256) -> bool {
    hash == Hash256::default()
}

fn validate_config(config: &ShardExecutorConfig) -> Result<(), ShardExecutorError> {
    if config.max_active_sessions == 0 || config.max_active_sessions > MAX_ACTIVE_SESSIONS_HARD_CAP
    {
        return Err(ShardExecutorError::InvalidConfig(
            "max_active_sessions out of bounds",
        ));
    }
    if config.max_kv_cache_bytes == 0 || config.max_kv_cache_bytes > MAX_KV_CACHE_BYTES_HARD_CAP {
        return Err(ShardExecutorError::InvalidConfig(
            "max_kv_cache_bytes out of bounds",
        ));
    }
    if config.max_step_deadline_window_ms == 0
        || config.max_step_deadline_window_ms > MAX_STEP_DEADLINE_WINDOW_MS
    {
        return Err(ShardExecutorError::InvalidConfig(
            "max_step_deadline_window_ms out of bounds",
        ));
    }
    if config.session_ttl_ms == 0 || config.session_ttl_ms > MAX_SESSION_TTL_MS {
        return Err(ShardExecutorError::InvalidConfig(
            "session_ttl_ms out of bounds",
        ));
    }
    if config.tombstone_ttl_ms < config.max_step_deadline_window_ms
        || config.tombstone_ttl_ms > MAX_SESSION_TTL_MS
    {
        return Err(ShardExecutorError::InvalidConfig(
            "tombstone_ttl_ms must cover the full step replay window and stay within the session TTL cap",
        ));
    }
    if config.max_tombstones < config.max_active_sessions
        || config.max_tombstones > MAX_TOMBSTONES_HARD_CAP
    {
        return Err(ShardExecutorError::InvalidConfig(
            "max_tombstones out of bounds",
        ));
    }
    Ok(())
}

fn validate_model_spec(spec: &TrustedModelSpec) -> Result<(), ShardExecutorError> {
    if is_zero_hash(spec.model_id) {
        return Err(ShardExecutorError::InvalidModelSpec(
            "model_id must be nonzero",
        ));
    }
    if spec.execution_profile != CANONICAL_REWARD_INFERENCE_PROFILE {
        return Err(ShardExecutorError::InvalidModelSpec(
            "execution_profile must be canonical reward profile",
        ));
    }
    if spec.n_layers < 2 || spec.n_layers > MAX_MODEL_LAYERS_HARD_CAP {
        return Err(ShardExecutorError::InvalidModelSpec(
            "n_layers out of bounds",
        ));
    }
    if spec.d_model == 0 || spec.d_model > MAX_HIDDEN_LEN_HARD_CAP {
        return Err(ShardExecutorError::InvalidModelSpec(
            "d_model out of bounds",
        ));
    }
    if spec.vocab_size == 0 || u32::try_from(spec.vocab_size - 1).is_err() {
        return Err(ShardExecutorError::InvalidModelSpec(
            "vocab_size out of bounds",
        ));
    }
    if !(2..=MAX_SEQUENCE_POSITIONS_HARD_CAP).contains(&spec.max_seq) {
        return Err(ShardExecutorError::InvalidModelSpec(
            "max_seq must be 2..=32768",
        ));
    }
    if spec.bos_token as usize >= spec.vocab_size {
        return Err(ShardExecutorError::InvalidModelSpec(
            "bos_token is outside model vocabulary",
        ));
    }
    if spec.eos_tokens.len() > MAX_EOS_TOKENS {
        return Err(ShardExecutorError::InvalidModelSpec(
            "eos_tokens exceeds limit",
        ));
    }
    Ok(())
}

fn validate_lease(
    spec: &TrustedModelSpec,
    lease: &ShardLeaseAssignment,
) -> Result<(), ShardExecutorError> {
    if is_zero_hash(lease.lease_id.boot_nonce) {
        return Err(ShardExecutorError::InvalidLease(
            "boot_nonce must be nonzero",
        ));
    }
    if lease.lease_id.seq == 0 || lease.lease_epoch == 0 {
        return Err(ShardExecutorError::InvalidLease(
            "lease sequence and epoch must be nonzero",
        ));
    }
    if lease.model_id != spec.model_id {
        return Err(ShardExecutorError::InvalidLease("model_id mismatch"));
    }
    if lease.execution_profile != spec.execution_profile {
        return Err(ShardExecutorError::InvalidLease(
            "execution_profile mismatch",
        ));
    }
    if !lease.range.is_valid_for(spec.n_layers) {
        return Err(ShardExecutorError::InvalidLease(
            "layer range invalid for model",
        ));
    }
    if lease.range.start_layer == 0 && lease.range.end_layer == spec.n_layers {
        return Err(ShardExecutorError::FullModelNotAllowed);
    }
    if lease.range.len() > spec.n_layers.div_ceil(2) {
        return Err(ShardExecutorError::InvalidLease(
            "partial ownership cannot exceed half the layers rounded up",
        ));
    }
    Ok(())
}

fn is_empty_i8_weights(weights: &I8Weights) -> bool {
    weights.n_rows == 0
        && weights.n_cols == 0
        && weights.data.is_empty()
        && weights.scales.is_empty()
}

fn is_complete_i8_weights(weights: &I8Weights, expected_rows: usize, expected_cols: usize) -> bool {
    let Some(expected_len) = expected_rows.checked_mul(expected_cols) else {
        return false;
    };
    weights.n_rows == expected_rows
        && weights.n_cols == expected_cols
        && weights.data.len() == expected_len
        && weights.scales.len() == expected_rows
}

fn is_placeholder_layer(layer: &CachedLayer) -> bool {
    is_empty_i8_weights(&layer.wq)
        && is_empty_i8_weights(&layer.wk)
        && is_empty_i8_weights(&layer.wv)
        && is_empty_i8_weights(&layer.wo)
        && is_empty_i8_weights(&layer.w_gate)
        && is_empty_i8_weights(&layer.w_up)
        && is_empty_i8_weights(&layer.w_down)
        && layer.attn_norm.is_empty()
        && layer.ffn_norm.is_empty()
}

fn layer_matches_shape(layer: &CachedLayer, d_model: usize, d_kv: usize, d_ff: usize) -> bool {
    is_complete_i8_weights(&layer.wq, d_model, d_model)
        && is_complete_i8_weights(&layer.wk, d_kv, d_model)
        && is_complete_i8_weights(&layer.wv, d_kv, d_model)
        && is_complete_i8_weights(&layer.wo, d_model, d_model)
        && is_complete_i8_weights(&layer.w_gate, d_ff, d_model)
        && is_complete_i8_weights(&layer.w_up, d_ff, d_model)
        && is_complete_i8_weights(&layer.w_down, d_model, d_ff)
        && layer.attn_norm.len() == d_model
        && layer.ffn_norm.len() == d_model
}

fn validate_partial_model(
    spec: &TrustedModelSpec,
    lease: &ShardLeaseAssignment,
    model: &CachedIntegerModel,
) -> Result<(), ShardExecutorError> {
    if model.has_all_transformer_layers() {
        return Err(ShardExecutorError::FullModelNotAllowed);
    }
    if !model.has_canonical_i8_profile()
        || model.canonical_execution_profile() != Some(CANONICAL_REWARD_INFERENCE_PROFILE)
        || model.config.arithmetic_profile != ArithmeticProfile::LegacySplitHalfV0
    {
        return Err(ShardExecutorError::InvalidPartialModel(
            "model must satisfy canonical per-row INT8 profile",
        ));
    }

    let cfg = &model.config;
    if cfg.n_layers != spec.n_layers
        || cfg.d_model != spec.d_model
        || cfg.vocab_size != spec.vocab_size
        || cfg.max_seq != spec.max_seq
        || cfg.eos_tokens != spec.eos_tokens
        || cfg.bos_token != spec.bos_token
    {
        return Err(ShardExecutorError::InvalidPartialModel(
            "model config does not match trusted spec",
        ));
    }
    if model.layers.len() != cfg.n_layers {
        return Err(ShardExecutorError::InvalidPartialModel(
            "layers vector length does not match n_layers",
        ));
    }
    if cfg.n_heads == 0
        || cfg.n_kv_heads == 0
        || cfg.d_head == 0
        || cfg.d_kv == 0
        || cfg.d_ff == 0
        || !cfg.d_head.is_multiple_of(2)
        || !cfg.n_heads.is_multiple_of(cfg.n_kv_heads)
        || cfg.n_heads.checked_mul(cfg.d_head) != Some(cfg.d_model)
        || cfg.n_kv_heads.checked_mul(cfg.d_head) != Some(cfg.d_kv)
    {
        return Err(ShardExecutorError::InvalidPartialModel(
            "invalid attention or feed-forward geometry",
        ));
    }
    let min_rope =
        cfg.max_seq
            .checked_mul(cfg.d_head / 2)
            .ok_or(ShardExecutorError::InvalidPartialModel(
                "rope table size overflow",
            ))?;
    if cfg.rope_cos.len() != cfg.rope_sin.len() || cfg.rope_cos.len() < min_rope {
        return Err(ShardExecutorError::InvalidPartialModel(
            "rope tables missing or shorter than max_seq",
        ));
    }

    for (idx, layer) in model.layers.iter().enumerate() {
        let owned = idx >= lease.range.start_layer && idx < lease.range.end_layer;
        if owned {
            if !layer.is_loaded() || !layer_matches_shape(layer, cfg.d_model, cfg.d_kv, cfg.d_ff) {
                return Err(ShardExecutorError::InvalidPartialModel(
                    "owned layer missing weights or invalid shape",
                ));
            }
        } else if !is_placeholder_layer(layer) {
            return Err(ShardExecutorError::InvalidPartialModel(
                "non-owned layer must be an empty placeholder",
            ));
        }
    }

    let is_first = lease.range.start_layer == 0;
    let is_last = lease.range.end_layer == cfg.n_layers;
    if is_first {
        let expected_emb_len = cfg.vocab_size.checked_mul(cfg.d_model).ok_or(
            ShardExecutorError::InvalidPartialModel("embedding size overflow"),
        )?;
        if model.embedding_q16.len() != expected_emb_len {
            return Err(ShardExecutorError::InvalidPartialModel(
                "first shard missing or malformed embedding_q16",
            ));
        }
        if !is_empty_i8_weights(&model.embedding_i8)
            && !is_complete_i8_weights(&model.embedding_i8, cfg.vocab_size, cfg.d_model)
        {
            return Err(ShardExecutorError::InvalidPartialModel(
                "first shard malformed embedding_i8",
            ));
        }
    } else if !model.embedding_q16.is_empty() || !is_empty_i8_weights(&model.embedding_i8) {
        return Err(ShardExecutorError::InvalidPartialModel(
            "non-first shard must not retain embeddings",
        ));
    }

    if is_last {
        if model.final_norm.len() != cfg.d_model {
            return Err(ShardExecutorError::InvalidPartialModel(
                "last shard missing or malformed final_norm",
            ));
        }
        if !is_complete_i8_weights(&model.output_weight, cfg.vocab_size, cfg.d_model) {
            return Err(ShardExecutorError::InvalidPartialModel(
                "last shard missing or malformed output_weight",
            ));
        }
    } else if !model.final_norm.is_empty() || !is_empty_i8_weights(&model.output_weight) {
        return Err(ShardExecutorError::InvalidPartialModel(
            "non-last shard must not retain output head",
        ));
    }

    Ok(())
}

fn validate_work(
    bound: &BoundShard,
    now_ms: u64,
    max_deadline_unix_ms: u64,
    work: &ShardStepWork,
) -> Result<(), ShardExecutorError> {
    let expected_boot = bound.lease.lease_id.boot_nonce;
    if work.lease_id.boot_nonce != expected_boot
        || work.session_id.boot_nonce != expected_boot
        || work.step_id.boot_nonce != expected_boot
    {
        return Err(ShardExecutorError::BootNonceMismatch);
    }
    if work.lease_id != bound.lease.lease_id {
        return Err(ShardExecutorError::LeaseIdMismatch);
    }
    if work.lease_epoch != bound.lease.lease_epoch {
        return Err(ShardExecutorError::LeaseEpochMismatch {
            expected: bound.lease.lease_epoch,
            actual: work.lease_epoch,
        });
    }
    if work.model_id != bound.model_spec.model_id {
        return Err(ShardExecutorError::ModelMismatch);
    }
    if work.execution_profile != bound.model_spec.execution_profile {
        return Err(ShardExecutorError::ProfileMismatch);
    }
    if work.range != bound.lease.range {
        return Err(ShardExecutorError::RangeMismatch {
            expected: bound.lease.range,
            actual: work.range,
        });
    }
    if work.expected_hidden_len != bound.model_spec.d_model {
        return Err(ShardExecutorError::InvalidHiddenLength {
            expected: bound.model_spec.d_model,
            actual: work.expected_hidden_len,
        });
    }
    let is_first = bound.lease.range.start_layer == 0;
    let is_last = bound.lease.range.end_layer == bound.model_spec.n_layers;
    if work.expect_terminal != is_last {
        return Err(ShardExecutorError::TerminalModeMismatch {
            expected_terminal: is_last,
            actual_terminal: work.expect_terminal,
        });
    }
    if work.position >= bound.model_spec.max_seq {
        return Err(ShardExecutorError::PositionOutOfRange {
            position: work.position,
            max_seq: bound.model_spec.max_seq,
        });
    }
    if work.deadline_unix_ms <= now_ms {
        return Err(ShardExecutorError::StepDeadlineExpired);
    }
    if work.deadline_unix_ms > max_deadline_unix_ms {
        return Err(ShardExecutorError::StepDeadlineExceedsCap {
            max_deadline_unix_ms,
            actual_deadline_unix_ms: work.deadline_unix_ms,
        });
    }
    let max_history = REPETITION_PENALTY_WINDOW.min(work.position);
    if work.generated_tokens.len() > max_history {
        return Err(ShardExecutorError::HistoryTooLong {
            max_len: max_history,
            actual: work.generated_tokens.len(),
        });
    }
    let vocab_size = bound.model_spec.vocab_size;
    for &tok in &work.generated_tokens {
        if usize::try_from(tok).map_or(true, |idx| idx >= vocab_size) {
            return Err(ShardExecutorError::TokenOutOfRange {
                token_id: tok,
                vocab_size,
            });
        }
    }
    match &work.input {
        ShardStepInput::Token { token } => {
            if !is_first {
                return Err(ShardExecutorError::InputKindMismatch {
                    expected_token: false,
                });
            }
            if usize::try_from(*token).map_or(true, |idx| idx >= vocab_size) {
                return Err(ShardExecutorError::TokenOutOfRange {
                    token_id: *token,
                    vocab_size,
                });
            }
        }
        ShardStepInput::Hidden {
            hidden,
            hidden_hash,
        } => {
            if is_first {
                return Err(ShardExecutorError::InputKindMismatch {
                    expected_token: true,
                });
            }
            if hidden.len() != bound.model_spec.d_model {
                return Err(ShardExecutorError::InvalidHiddenLength {
                    expected: bound.model_spec.d_model,
                    actual: hidden.len(),
                });
            }
            if !hidden_values_within_bounds(hidden) {
                return Err(ShardExecutorError::HiddenValueOutOfBounds);
            }
            if is_zero_hash(*hidden_hash) {
                return Err(ShardExecutorError::ZeroDigest);
            }
            if hidden_state_hash(hidden) != *hidden_hash {
                return Err(ShardExecutorError::HiddenHashMismatch);
            }
        }
    }
    Ok(())
}

fn make_shard_input(input: &ShardStepInput) -> ShardInput {
    match input {
        ShardStepInput::Token { token } => ShardInput::Token(*token),
        ShardStepInput::Hidden { hidden, .. } => ShardInput::Hidden(hidden.clone()),
    }
}

fn verify_kv_cache_positions(
    cache: &KVCache,
    n_layers: usize,
    range: ShardLayerRange,
    d_kv: usize,
    expected_positions: usize,
) -> Result<(), ShardExecutorError> {
    if cache.k_data.len() != n_layers || cache.v_data.len() != n_layers {
        return Err(ShardExecutorError::KvCacheCorrupted);
    }
    let expected_elems = expected_positions
        .checked_mul(d_kv)
        .ok_or(ShardExecutorError::KvCacheCorrupted)?;
    for layer_idx in 0..n_layers {
        let want = if layer_idx >= range.start_layer && layer_idx < range.end_layer {
            expected_elems
        } else {
            0
        };
        if cache.k_data[layer_idx].len() != want || cache.v_data[layer_idx].len() != want {
            return Err(ShardExecutorError::KvCacheCorrupted);
        }
    }
    Ok(())
}

fn try_reserve_kv_step(
    cache: &mut KVCache,
    range: ShardLayerRange,
    d_kv: usize,
) -> Result<(), ShardExecutorError> {
    for layer_idx in range.start_layer..range.end_layer {
        cache.k_data[layer_idx]
            .try_reserve_exact(d_kv)
            .map_err(|_| ShardExecutorError::KvCacheAllocationFailed)?;
        cache.v_data[layer_idx]
            .try_reserve_exact(d_kv)
            .map_err(|_| ShardExecutorError::KvCacheAllocationFailed)?;
    }
    Ok(())
}

fn convert_shard_output(
    output: ShardOutput,
    expect_terminal: bool,
    expected_hidden_len: usize,
    vocab_size: usize,
) -> Result<ShardStepOutput, ShardExecutorError> {
    match output {
        ShardOutput::Hidden(hidden) => {
            if expect_terminal {
                return Err(ShardExecutorError::TerminalModeMismatch {
                    expected_terminal: true,
                    actual_terminal: false,
                });
            }
            if hidden.len() != expected_hidden_len {
                return Err(ShardExecutorError::InvalidHiddenLength {
                    expected: expected_hidden_len,
                    actual: hidden.len(),
                });
            }
            if !hidden_values_within_bounds(&hidden) {
                return Err(ShardExecutorError::HiddenValueOutOfBounds);
            }
            let hidden_hash = hidden_state_hash(&hidden);
            if is_zero_hash(hidden_hash) {
                return Err(ShardExecutorError::ZeroDigest);
            }
            Ok(ShardStepOutput::Hidden {
                hidden,
                hidden_hash,
            })
        }
        ShardOutput::Token {
            id: token_id,
            logits_hash,
        } => {
            if !expect_terminal {
                return Err(ShardExecutorError::TerminalModeMismatch {
                    expected_terminal: false,
                    actual_terminal: true,
                });
            }
            if usize::try_from(token_id).map_or(true, |idx| idx >= vocab_size) {
                return Err(ShardExecutorError::TokenOutOfRange {
                    token_id,
                    vocab_size,
                });
            }
            if is_zero_hash(logits_hash) {
                return Err(ShardExecutorError::ZeroDigest);
            }
            Ok(ShardStepOutput::Token {
                token_id,
                logits_hash,
            })
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn execute_forward_step(
    model: &CachedIntegerModel,
    kv_cache: &mut KVCache,
    n_layers: usize,
    range: ShardLayerRange,
    d_model: usize,
    d_kv: usize,
    vocab_size: usize,
    is_last: bool,
    position: usize,
    shard_input: ShardInput,
    generated_tokens: &[u32],
) -> Result<ShardStepOutput, ShardExecutorError> {
    verify_kv_cache_positions(kv_cache, n_layers, range, d_kv, position)?;
    try_reserve_kv_step(kv_cache, range, d_kv)?;
    let raw_output = model
        .forward_shard_token_with_history(
            shard_input,
            kv_cache,
            range.start_layer,
            range.end_layer,
            position,
            generated_tokens,
        )
        .map_err(|err| ShardExecutorError::ForwardFailed(format!("{err:?}")))?;
    let next_positions = position
        .checked_add(1)
        .ok_or(ShardExecutorError::KvCacheCorrupted)?;
    verify_kv_cache_positions(kv_cache, n_layers, range, d_kv, next_positions)?;
    kv_cache.seq_len = next_positions;
    convert_shard_output(raw_output, is_last, d_model, vocab_size)
}

impl CommunityShardExecutor {
    pub fn new(
        worker_id: Hash256,
        coordinator_id: Hash256,
        model_spec: TrustedModelSpec,
        lease: ShardLeaseAssignment,
        model: Arc<CachedIntegerModel>,
        config: ShardExecutorConfig,
    ) -> Result<Self, ShardExecutorError> {
        if is_zero_hash(worker_id) {
            return Err(ShardExecutorError::InvalidIdentity(
                "worker_id must be nonzero",
            ));
        }
        if is_zero_hash(coordinator_id) {
            return Err(ShardExecutorError::InvalidIdentity(
                "coordinator_id must be nonzero",
            ));
        }
        validate_config(&config)?;
        validate_model_spec(&model_spec)?;
        validate_lease(&model_spec, &lease)?;
        validate_partial_model(&model_spec, &lease, &model)?;

        let high_water_lease = HighWaterLease {
            boot_nonce: lease.lease_id.boot_nonce,
            lease_seq: lease.lease_id.seq,
            lease_epoch: lease.lease_epoch,
        };
        Ok(Self {
            worker_id,
            coordinator_id,
            config,
            bound: Some(BoundShard {
                model_spec,
                lease,
                model,
            }),
            high_water_lease,
            fenced_boot_nonces: BTreeSet::new(),
            active_sessions: BTreeMap::new(),
            tombstones: BTreeMap::new(),
            reserved_kv_bytes: 0,
        })
    }

    pub fn worker_id(&self) -> Hash256 {
        self.worker_id
    }

    pub fn coordinator_id(&self) -> Hash256 {
        self.coordinator_id
    }

    pub fn config(&self) -> &ShardExecutorConfig {
        &self.config
    }

    pub fn lease(&self) -> Option<&ShardLeaseAssignment> {
        self.bound.as_ref().map(|bound| &bound.lease)
    }

    pub fn model_spec(&self) -> Option<&TrustedModelSpec> {
        self.bound.as_ref().map(|bound| &bound.model_spec)
    }

    pub fn status(&self) -> ShardExecutorStatus {
        let terminal_reply_tombstones = self
            .tombstones
            .values()
            .filter(|entry| entry.terminal_reply.is_some())
            .count();
        ShardExecutorStatus {
            bound: self.bound.is_some(),
            active_sessions: self.active_sessions.len(),
            reserved_kv_bytes: self.reserved_kv_bytes,
            tombstones: self.tombstones.len(),
            terminal_reply_tombstones,
            fenced_boot_nonces: self.fenced_boot_nonces.len(),
        }
    }

    pub fn sweep_expired(&mut self, now_ms: u64) -> ExecutorSweepSummary {
        let mut pruned_tombstones = 0usize;
        let mut pruned_terminal_replies = 0usize;
        self.tombstones.retain(|_, entry| {
            if now_ms >= entry.expires_at_unix_ms {
                pruned_tombstones += 1;
                if entry.terminal_reply.is_some() {
                    pruned_terminal_replies += 1;
                }
                false
            } else {
                true
            }
        });

        let expired_keys: Vec<SessionKey> = self
            .active_sessions
            .iter()
            .filter_map(|(&key, session)| (now_ms >= session.expires_at_unix_ms).then_some(key))
            .collect();
        let tombstone_expires_at = now_ms.saturating_add(self.config.tombstone_ttl_ms);
        let expired_sessions = expired_keys.len();
        for key in expired_keys {
            if let Some(session) = self.active_sessions.remove(&key) {
                self.reserved_kv_bytes = self
                    .reserved_kv_bytes
                    .saturating_sub(session.reserved_kv_bytes);
                self.tombstones.insert(
                    key,
                    TombstoneEntry {
                        expires_at_unix_ms: tombstone_expires_at,
                        terminal_reply: None,
                    },
                );
            }
        }

        ExecutorSweepSummary {
            expired_sessions,
            pruned_tombstones,
            pruned_terminal_replies,
        }
    }

    pub fn apply_cancellation(
        &mut self,
        coordinator_id: Hash256,
        now_ms: u64,
        notice: &SessionCancellationNotice,
    ) -> Result<bool, ShardExecutorError> {
        if coordinator_id != self.coordinator_id {
            return Err(ShardExecutorError::CoordinatorMismatch);
        }
        let tombstone_expires_at = now_ms
            .checked_add(self.config.tombstone_ttl_ms)
            .ok_or(ShardExecutorError::TimestampOverflow)?;
        let bound = self
            .bound
            .as_ref()
            .ok_or(ShardExecutorError::ExecutorReleased)?;
        let expected_boot = bound.lease.lease_id.boot_nonce;
        if notice.lease_id.boot_nonce != expected_boot
            || notice.session_id.boot_nonce != expected_boot
        {
            return Err(ShardExecutorError::BootNonceMismatch);
        }
        if notice.lease_id != bound.lease.lease_id {
            return Err(ShardExecutorError::LeaseIdMismatch);
        }
        if notice.lease_epoch != bound.lease.lease_epoch {
            return Err(ShardExecutorError::LeaseEpochMismatch {
                expected: bound.lease.lease_epoch,
                actual: notice.lease_epoch,
            });
        }

        self.sweep_expired(now_ms);
        let key = SessionKey::from_id(notice.session_id);
        if let Some(session) = self.active_sessions.remove(&key) {
            self.reserved_kv_bytes = self
                .reserved_kv_bytes
                .saturating_sub(session.reserved_kv_bytes);
            self.tombstones.insert(
                key,
                TombstoneEntry {
                    expires_at_unix_ms: tombstone_expires_at,
                    terminal_reply: None,
                },
            );
            return Ok(true);
        }
        if let Some(entry) = self.tombstones.get_mut(&key) {
            entry.terminal_reply = None;
            entry.expires_at_unix_ms = entry.expires_at_unix_ms.max(tombstone_expires_at);
            return Ok(false);
        }
        if self
            .active_sessions
            .len()
            .saturating_add(self.tombstones.len())
            >= self.config.max_tombstones
        {
            return Err(ShardExecutorError::TombstoneCapacityReached);
        }
        self.tombstones.insert(
            key,
            TombstoneEntry {
                expires_at_unix_ms: tombstone_expires_at,
                terminal_reply: None,
            },
        );
        Ok(false)
    }

    pub fn release(&mut self, now_ms: u64) -> Result<ShardReleaseRequest, ShardExecutorError> {
        let tombstone_expires_at = now_ms
            .checked_add(self.config.tombstone_ttl_ms)
            .ok_or(ShardExecutorError::TimestampOverflow)?;
        if self.bound.is_none() {
            return Err(ShardExecutorError::ExecutorReleased);
        }
        self.sweep_expired(now_ms);
        let bound = self
            .bound
            .take()
            .ok_or(ShardExecutorError::ExecutorReleased)?;
        self.evict_all_active_into_tombstones(tombstone_expires_at);
        Ok(ShardReleaseRequest {
            worker_id: self.worker_id,
            lease_id: bound.lease.lease_id,
            lease_epoch: bound.lease.lease_epoch,
        })
    }

    pub fn rebind(
        &mut self,
        coordinator_id: Hash256,
        now_ms: u64,
        model_spec: TrustedModelSpec,
        lease: ShardLeaseAssignment,
        model: Arc<CachedIntegerModel>,
    ) -> Result<(), ShardExecutorError> {
        if coordinator_id != self.coordinator_id {
            return Err(ShardExecutorError::CoordinatorMismatch);
        }
        let tombstone_expires_at = now_ms
            .checked_add(self.config.tombstone_ttl_ms)
            .ok_or(ShardExecutorError::TimestampOverflow)?;
        validate_model_spec(&model_spec)?;
        validate_lease(&model_spec, &lease)?;
        validate_partial_model(&model_spec, &lease, &model)?;

        let new_boot_nonce = lease.lease_id.boot_nonce;
        if self.fenced_boot_nonces.contains(&new_boot_nonce.0) {
            return Err(ShardExecutorError::BootNonceFenced);
        }
        let boot_nonce_changed = new_boot_nonce != self.high_water_lease.boot_nonce;
        if boot_nonce_changed {
            if self.fenced_boot_nonces.len() >= MAX_FENCED_BOOT_NONCES_HARD_CAP {
                return Err(ShardExecutorError::FencedBootNonceCapacityReached);
            }
        } else {
            let progresses = lease.lease_id.seq > self.high_water_lease.lease_seq
                || (lease.lease_id.seq == self.high_water_lease.lease_seq
                    && lease.lease_epoch > self.high_water_lease.lease_epoch);
            if !progresses {
                return Err(ShardExecutorError::StaleLease);
            }
        }

        self.sweep_expired(now_ms);
        if boot_nonce_changed {
            self.fenced_boot_nonces
                .insert(self.high_water_lease.boot_nonce.0);
            self.active_sessions.clear();
            self.tombstones.clear();
            self.reserved_kv_bytes = 0;
        } else {
            self.evict_all_active_into_tombstones(tombstone_expires_at);
        }

        self.high_water_lease = HighWaterLease {
            boot_nonce: new_boot_nonce,
            lease_seq: lease.lease_id.seq,
            lease_epoch: lease.lease_epoch,
        };
        self.bound = Some(BoundShard {
            model_spec,
            lease,
            model,
        });
        Ok(())
    }

    pub fn execute_step(
        &mut self,
        coordinator_id: Hash256,
        now_ms: u64,
        work: &ShardStepWork,
    ) -> Result<ShardStepSubmitRequest, ShardExecutorError> {
        if coordinator_id != self.coordinator_id {
            return Err(ShardExecutorError::CoordinatorMismatch);
        }
        let max_deadline_unix_ms = now_ms
            .checked_add(self.config.max_step_deadline_window_ms)
            .ok_or(ShardExecutorError::TimestampOverflow)?;
        let session_expires_at = now_ms
            .checked_add(self.config.session_ttl_ms)
            .ok_or(ShardExecutorError::TimestampOverflow)?;
        let tombstone_expires_at = now_ms
            .checked_add(self.config.tombstone_ttl_ms)
            .ok_or(ShardExecutorError::TimestampOverflow)?;
        if self.bound.is_none() {
            return Err(ShardExecutorError::ExecutorReleased);
        }

        self.sweep_expired(now_ms);

        let (
            lease_id,
            lease_epoch,
            model_id,
            execution_profile,
            range,
            n_layers,
            d_model,
            d_kv,
            vocab_size,
            is_last,
            model,
        ) = {
            let bound = self
                .bound
                .as_ref()
                .ok_or(ShardExecutorError::ExecutorReleased)?;
            validate_work(bound, now_ms, max_deadline_unix_ms, work)?;
            (
                bound.lease.lease_id,
                bound.lease.lease_epoch,
                bound.model_spec.model_id,
                bound.model_spec.execution_profile.clone(),
                bound.lease.range,
                bound.model_spec.n_layers,
                bound.model_spec.d_model,
                bound.model.config.d_kv,
                bound.model_spec.vocab_size,
                bound.lease.range.end_layer == bound.model_spec.n_layers,
                Arc::clone(&bound.model),
            )
        };

        let session_key = SessionKey::from_id(work.session_id);
        let fingerprint = StepPayloadFingerprint::from_work(work);

        if let Some(entry) = self.tombstones.get(&session_key) {
            if let Some(cached) = &entry.terminal_reply {
                if work.step_id == cached.fingerprint.step_id {
                    if fingerprint == cached.fingerprint {
                        return Ok(cached.response.clone());
                    }
                    return Err(ShardExecutorError::ConflictingStepRetry);
                }
                if work.position == cached.fingerprint.position {
                    return Err(ShardExecutorError::DuplicatePositionStepMismatch {
                        position: work.position,
                        expected_step_id: cached.fingerprint.step_id,
                        actual_step_id: work.step_id,
                    });
                }
            }
            return Err(ShardExecutorError::SessionTombstoned);
        }

        if let Some(session) = self.active_sessions.get(&session_key) {
            if work.step_id == session.last_completed.fingerprint.step_id {
                if fingerprint == session.last_completed.fingerprint {
                    return Ok(session.last_completed.response.clone());
                }
                return Err(ShardExecutorError::ConflictingStepRetry);
            }
            if work.position == session.last_completed.fingerprint.position {
                return Err(ShardExecutorError::DuplicatePositionStepMismatch {
                    position: work.position,
                    expected_step_id: session.last_completed.fingerprint.step_id,
                    actual_step_id: work.step_id,
                });
            }
            if work.position != session.next_position {
                return Err(ShardExecutorError::UnexpectedPosition {
                    expected: session.next_position,
                    actual: work.position,
                });
            }
            if work.step_id.seq <= session.last_completed.fingerprint.step_id.seq {
                return Err(ShardExecutorError::NonMonotonicStepId {
                    previous_seq: session.last_completed.fingerprint.step_id.seq,
                    actual_seq: work.step_id.seq,
                });
            }
        }

        if let Some(mut session) = self.active_sessions.remove(&session_key) {
            self.reserved_kv_bytes = self
                .reserved_kv_bytes
                .saturating_sub(session.reserved_kv_bytes);
            let next_positions = match work.position.checked_add(1) {
                Some(positions) => positions,
                None => {
                    self.insert_tombstone(session_key, tombstone_expires_at, None);
                    return Err(ShardExecutorError::KvCacheBudgetExceeded {
                        requested_bytes: usize::MAX,
                        limit_bytes: self.config.max_kv_cache_bytes,
                    });
                }
            };
            let required_session_bytes =
                match estimate_kv_cache_bytes(range.len(), d_kv, next_positions) {
                    Some(bytes) => bytes,
                    None => {
                        self.insert_tombstone(session_key, tombstone_expires_at, None);
                        return Err(ShardExecutorError::KvCacheBudgetExceeded {
                            requested_bytes: usize::MAX,
                            limit_bytes: self.config.max_kv_cache_bytes,
                        });
                    }
                };
            let new_total_bytes = match self.reserved_kv_bytes.checked_add(required_session_bytes) {
                Some(total) if total <= self.config.max_kv_cache_bytes => total,
                Some(total) => {
                    self.insert_tombstone(session_key, tombstone_expires_at, None);
                    return Err(ShardExecutorError::KvCacheBudgetExceeded {
                        requested_bytes: total,
                        limit_bytes: self.config.max_kv_cache_bytes,
                    });
                }
                None => {
                    self.insert_tombstone(session_key, tombstone_expires_at, None);
                    return Err(ShardExecutorError::KvCacheBudgetExceeded {
                        requested_bytes: usize::MAX,
                        limit_bytes: self.config.max_kv_cache_bytes,
                    });
                }
            };

            let shard_input = make_shard_input(&work.input);
            let output = match execute_forward_step(
                &model,
                &mut session.kv_cache,
                n_layers,
                range,
                d_model,
                d_kv,
                vocab_size,
                is_last,
                work.position,
                shard_input,
                &work.generated_tokens,
            ) {
                Ok(output) => output,
                Err(err) => {
                    self.insert_tombstone(session_key, tombstone_expires_at, None);
                    return Err(err);
                }
            };

            let response = ShardStepSubmitRequest {
                worker_id: self.worker_id,
                lease_id,
                lease_epoch,
                session_id: work.session_id,
                step_id: work.step_id,
                model_id,
                execution_profile,
                range,
                position: work.position,
                layers_processed: range.len(),
                outcome: ShardStepOutcome::Completed { output },
            };
            let reply = CachedStepReply {
                fingerprint,
                response: response.clone(),
            };
            if work.last_token {
                self.insert_tombstone(session_key, tombstone_expires_at, Some(reply));
            } else {
                session.next_position = next_positions;
                session.reserved_kv_bytes = required_session_bytes;
                session.last_completed = reply;
                self.reserved_kv_bytes = new_total_bytes;
                self.active_sessions.insert(session_key, session);
            }
            return Ok(response);
        }

        if work.position != 0 {
            return Err(ShardExecutorError::UnexpectedPosition {
                expected: 0,
                actual: work.position,
            });
        }
        if self.active_sessions.len() >= self.config.max_active_sessions {
            return Err(ShardExecutorError::SessionCapacityReached);
        }
        if self
            .active_sessions
            .len()
            .saturating_add(self.tombstones.len())
            >= self.config.max_tombstones
        {
            return Err(ShardExecutorError::TombstoneCapacityReached);
        }
        let required_session_bytes = estimate_kv_cache_bytes(range.len(), d_kv, 1).ok_or(
            ShardExecutorError::KvCacheBudgetExceeded {
                requested_bytes: usize::MAX,
                limit_bytes: self.config.max_kv_cache_bytes,
            },
        )?;
        let new_total_bytes = self
            .reserved_kv_bytes
            .checked_add(required_session_bytes)
            .ok_or(ShardExecutorError::KvCacheBudgetExceeded {
                requested_bytes: usize::MAX,
                limit_bytes: self.config.max_kv_cache_bytes,
            })?;
        if new_total_bytes > self.config.max_kv_cache_bytes {
            return Err(ShardExecutorError::KvCacheBudgetExceeded {
                requested_bytes: new_total_bytes,
                limit_bytes: self.config.max_kv_cache_bytes,
            });
        }

        let mut kv_cache = KVCache::new(n_layers);
        let shard_input = make_shard_input(&work.input);
        let output = match execute_forward_step(
            &model,
            &mut kv_cache,
            n_layers,
            range,
            d_model,
            d_kv,
            vocab_size,
            is_last,
            0,
            shard_input,
            &work.generated_tokens,
        ) {
            Ok(output) => output,
            Err(err) => {
                self.insert_tombstone(session_key, tombstone_expires_at, None);
                return Err(err);
            }
        };

        let response = ShardStepSubmitRequest {
            worker_id: self.worker_id,
            lease_id,
            lease_epoch,
            session_id: work.session_id,
            step_id: work.step_id,
            model_id,
            execution_profile,
            range,
            position: 0,
            layers_processed: range.len(),
            outcome: ShardStepOutcome::Completed { output },
        };
        let reply = CachedStepReply {
            fingerprint,
            response: response.clone(),
        };
        if work.last_token {
            self.insert_tombstone(session_key, tombstone_expires_at, Some(reply));
        } else {
            self.reserved_kv_bytes = new_total_bytes;
            self.active_sessions.insert(
                session_key,
                ActiveShardSession {
                    expires_at_unix_ms: session_expires_at,
                    next_position: 1,
                    reserved_kv_bytes: required_session_bytes,
                    kv_cache,
                    last_completed: reply,
                },
            );
        }
        Ok(response)
    }

    fn insert_tombstone(
        &mut self,
        key: SessionKey,
        expires_at_unix_ms: u64,
        terminal_reply: Option<CachedStepReply>,
    ) {
        self.tombstones.insert(
            key,
            TombstoneEntry {
                expires_at_unix_ms,
                terminal_reply,
            },
        );
    }

    fn evict_all_active_into_tombstones(&mut self, tombstone_expires_at: u64) {
        for entry in self.tombstones.values_mut() {
            entry.terminal_reply = None;
        }
        let evicted = std::mem::take(&mut self.active_sessions);
        for (key, _session) in evicted {
            self.tombstones.insert(
                key,
                TombstoneEntry {
                    expires_at_unix_ms: tombstone_expires_at,
                    terminal_reply: None,
                },
            );
        }
        self.reserved_kv_bytes = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::community_sharding::tests::{
        build_deterministic_integer_model, build_partial_integer_model, claim_req, lease_req,
        make_two_tile_coordinator, model_spec_from, ready_hb, test_hash,
    };
    use crate::community_sharding::{SessionAbortReason, SessionOutcome, ShardClaimStepResponse};

    struct TestFixture {
        full: CachedIntegerModel,
        spec: TrustedModelSpec,
        coord_id: Hash256,
        w1: Hash256,
        w2: Hash256,
        lease1: ShardLeaseAssignment,
        lease2: ShardLeaseAssignment,
        partial1: Arc<CachedIntegerModel>,
        partial2: Arc<CachedIntegerModel>,
    }

    fn build_two_worker_fixture() -> (
        TestFixture,
        crate::community_sharding::CommunityShardCoordinator,
        u64,
    ) {
        let full = build_deterministic_integer_model(32, 16, 2, 32, 4, 64, vec![128_001, 128_009]);
        let spec = model_spec_from(test_hash(b"executor-tiny-int8-model"), &full);
        let mut coord = make_two_tile_coordinator(&spec);
        let coord_id = test_hash(b"pinned-coordinator");
        let w1 = test_hash(b"exec-worker-1");
        let w2 = test_hash(b"exec-worker-2");
        let now = 1_000u64;

        let lease1 = coord
            .request_lease(w1, now, lease_req(w1, &spec, 4))
            .expect("worker 1 lease")
            .assignment;
        let lease2 = coord
            .request_lease(w2, now, lease_req(w2, &spec, 4))
            .expect("worker 2 lease")
            .assignment;
        let partial1 = Arc::new(build_partial_integer_model(&full, lease1.range));
        let partial2 = Arc::new(build_partial_integer_model(&full, lease2.range));

        coord
            .heartbeat(w1, now, ready_hb(w1, &lease1, true))
            .expect("worker 1 ready");
        coord
            .heartbeat(w2, now, ready_hb(w2, &lease2, true))
            .expect("worker 2 ready");

        (
            TestFixture {
                full,
                spec,
                coord_id,
                w1,
                w2,
                lease1,
                lease2,
                partial1,
                partial2,
            },
            coord,
            now,
        )
    }

    fn build_executors(
        fx: &TestFixture,
        cfg1: ShardExecutorConfig,
        cfg2: ShardExecutorConfig,
    ) -> (CommunityShardExecutor, CommunityShardExecutor) {
        let exec1 = CommunityShardExecutor::new(
            fx.w1,
            fx.coord_id,
            fx.spec.clone(),
            fx.lease1.clone(),
            Arc::clone(&fx.partial1),
            cfg1,
        )
        .expect("executor 1");
        let exec2 = CommunityShardExecutor::new(
            fx.w2,
            fx.coord_id,
            fx.spec.clone(),
            fx.lease2.clone(),
            Arc::clone(&fx.partial2),
            cfg2,
        )
        .expect("executor 2");
        (exec1, exec2)
    }

    fn claim_step_work(
        coord: &mut crate::community_sharding::CommunityShardCoordinator,
        worker_id: Hash256,
        now: u64,
        lease: &ShardLeaseAssignment,
    ) -> ShardStepWork {
        match coord
            .claim_step(worker_id, now, claim_req(worker_id, lease))
            .expect("claim step")
        {
            ShardClaimStepResponse::Step { work } => *work,
            other => panic!("expected step work, got {other:?}"),
        }
    }

    #[test]
    fn full_pipeline_two_partial_executors_matches_try_generate_and_allows_terminal_retry() {
        let (fx, mut coord, mut now) = build_two_worker_fixture();
        let (mut exec1, mut exec2) = build_executors(
            &fx,
            ShardExecutorConfig::default(),
            ShardExecutorConfig::default(),
        );
        assert_eq!(exec1.worker_id(), fx.w1);
        assert_eq!(exec1.coordinator_id(), fx.coord_id);
        assert_eq!(exec1.lease(), Some(&fx.lease1));
        assert_eq!(exec1.model_spec(), Some(&fx.spec));
        assert_eq!(exec1.config(), &ShardExecutorConfig::default());

        let prompt = vec![1u32, 5, 11];
        let max_tokens = 6u32;
        let session_id = coord
            .open_session(now, prompt.clone(), max_tokens)
            .expect("open session");

        let mut last_work1: Option<(ShardStepWork, ShardStepSubmitRequest)> = None;
        let mut last_work2: Option<(ShardStepWork, ShardStepSubmitRequest)> = None;
        let mut completed = false;
        for _ in 0..64 {
            now += 100;
            for (worker_id, lease, exec, last_slot) in [
                (fx.w1, &fx.lease1, &mut exec1, &mut last_work1),
                (fx.w2, &fx.lease2, &mut exec2, &mut last_work2),
            ] {
                match coord
                    .claim_step(worker_id, now, claim_req(worker_id, lease))
                    .expect("claim step")
                {
                    ShardClaimStepResponse::Step { work } => {
                        let sub = exec
                            .execute_step(fx.coord_id, now, &work)
                            .expect("execute partial shard step");
                        if work.last_token {
                            *last_slot = Some((*work, sub.clone()));
                        }
                        let resp = coord
                            .submit_step(worker_id, now, sub)
                            .expect("submit shard step");
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

        let st1 = exec1.status();
        let st2 = exec2.status();
        assert_eq!(st1.active_sessions, 0);
        assert_eq!(st1.reserved_kv_bytes, 0);
        assert_eq!(st1.terminal_reply_tombstones, 1);
        assert_eq!(st2.active_sessions, 0);
        assert_eq!(st2.reserved_kv_bytes, 0);
        assert_eq!(st2.terminal_reply_tombstones, 1);

        let (w1_last, r1_last) = last_work1.expect("worker 1 last work");
        let (w2_last, r2_last) = last_work2.expect("worker 2 last work");
        assert_eq!(
            exec1
                .execute_step(fx.coord_id, now, &w1_last)
                .expect("worker 1 terminal retry"),
            r1_last
        );
        assert_eq!(
            exec2
                .execute_step(fx.coord_id, now, &w2_last)
                .expect("worker 2 terminal retry"),
            r2_last
        );
        assert_eq!(exec1.status().active_sessions, 0);
        assert_eq!(exec1.status().reserved_kv_bytes, 0);

        let mut altered_last = w1_last.clone();
        altered_last.deadline_unix_ms -= 1;
        assert_eq!(
            exec1.execute_step(fx.coord_id, now, &altered_last),
            Err(ShardExecutorError::ConflictingStepRetry)
        );
        let mut wrong_step_last = w1_last.clone();
        wrong_step_last.step_id.seq += 1;
        assert!(matches!(
            exec1.execute_step(fx.coord_id, now, &wrong_step_last),
            Err(ShardExecutorError::DuplicatePositionStepMismatch { .. })
        ));

        let outcome = coord.take_result(session_id).expect("session outcome");
        let pending = match outcome {
            SessionOutcome::PendingVerification(pending) => pending,
            other => panic!("expected pending verification, got {other:?}"),
        };
        let (expected_tokens, _) = fx
            .full
            .try_generate(&prompt, max_tokens, &fx.full.config.eos_tokens)
            .expect("full try_generate");
        assert_eq!(pending.unverified_generated_tokens, expected_tokens);
    }

    #[test]
    fn constructor_rejects_full_model_bad_range_shape_profile_and_config() {
        let (fx, _coord, _now) = build_two_worker_fixture();
        for variant in 0..3 {
            let mut invalid_lease = fx.lease1.clone();
            match variant {
                0 => invalid_lease.lease_id.seq = 0,
                1 => invalid_lease.lease_epoch = 0,
                _ => invalid_lease.range.end_layer = fx.spec.n_layers - 1,
            }
            assert!(matches!(
                CommunityShardExecutor::new(
                    fx.w1,
                    fx.coord_id,
                    fx.spec.clone(),
                    invalid_lease,
                    Arc::clone(&fx.partial1),
                    ShardExecutorConfig::default(),
                ),
                Err(ShardExecutorError::InvalidLease(_))
            ));
        }

        assert_eq!(
            CommunityShardExecutor::new(
                Hash256::default(),
                fx.coord_id,
                fx.spec.clone(),
                fx.lease1.clone(),
                Arc::clone(&fx.partial1),
                ShardExecutorConfig::default(),
            )
            .err(),
            Some(ShardExecutorError::InvalidIdentity(
                "worker_id must be nonzero"
            ))
        );
        assert_eq!(
            CommunityShardExecutor::new(
                fx.w1,
                Hash256::default(),
                fx.spec.clone(),
                fx.lease1.clone(),
                Arc::clone(&fx.partial1),
                ShardExecutorConfig::default(),
            )
            .err(),
            Some(ShardExecutorError::InvalidIdentity(
                "coordinator_id must be nonzero"
            ))
        );

        let full_arc = Arc::new(build_deterministic_integer_model(
            32,
            16,
            2,
            32,
            4,
            64,
            vec![128_001, 128_009],
        ));
        assert_eq!(
            CommunityShardExecutor::new(
                fx.w1,
                fx.coord_id,
                fx.spec.clone(),
                fx.lease1.clone(),
                full_arc,
                ShardExecutorConfig::default(),
            )
            .err(),
            Some(ShardExecutorError::FullModelNotAllowed)
        );

        let mut full_lease = fx.lease1.clone();
        full_lease.range = ShardLayerRange {
            start_layer: 0,
            end_layer: 4,
        };
        assert_eq!(
            CommunityShardExecutor::new(
                fx.w1,
                fx.coord_id,
                fx.spec.clone(),
                full_lease,
                Arc::clone(&fx.partial1),
                ShardExecutorConfig::default(),
            )
            .err(),
            Some(ShardExecutorError::FullModelNotAllowed)
        );

        assert!(matches!(
            CommunityShardExecutor::new(
                fx.w1,
                fx.coord_id,
                fx.spec.clone(),
                fx.lease1.clone(),
                Arc::clone(&fx.partial2),
                ShardExecutorConfig::default(),
            ),
            Err(ShardExecutorError::InvalidPartialModel(_))
        ));

        let mut dirty_non_owned = build_partial_integer_model(&fx.full, fx.lease1.range);
        dirty_non_owned.layers[2].attn_norm.push(1);
        assert!(matches!(
            CommunityShardExecutor::new(
                fx.w1,
                fx.coord_id,
                fx.spec.clone(),
                fx.lease1.clone(),
                Arc::new(dirty_non_owned),
                ShardExecutorConfig::default(),
            ),
            Err(ShardExecutorError::InvalidPartialModel(_))
        ));

        let mut missing_emb = build_partial_integer_model(&fx.full, fx.lease1.range);
        missing_emb.embedding_q16.clear();
        assert!(matches!(
            CommunityShardExecutor::new(
                fx.w1,
                fx.coord_id,
                fx.spec.clone(),
                fx.lease1.clone(),
                Arc::new(missing_emb),
                ShardExecutorConfig::default(),
            ),
            Err(ShardExecutorError::InvalidPartialModel(_))
        ));

        let mut extra_emb = build_partial_integer_model(&fx.full, fx.lease2.range);
        extra_emb.embedding_q16 = fx.full.embedding_q16.clone();
        assert!(matches!(
            CommunityShardExecutor::new(
                fx.w2,
                fx.coord_id,
                fx.spec.clone(),
                fx.lease2.clone(),
                Arc::new(extra_emb),
                ShardExecutorConfig::default(),
            ),
            Err(ShardExecutorError::InvalidPartialModel(_))
        ));

        let mut missing_out = build_partial_integer_model(&fx.full, fx.lease2.range);
        missing_out.output_weight = I8Weights::empty();
        assert!(matches!(
            CommunityShardExecutor::new(
                fx.w2,
                fx.coord_id,
                fx.spec.clone(),
                fx.lease2.clone(),
                Arc::new(missing_out),
                ShardExecutorConfig::default(),
            ),
            Err(ShardExecutorError::InvalidPartialModel(_))
        ));

        let mut bad_shape = build_partial_integer_model(&fx.full, fx.lease1.range);
        bad_shape.layers[0].attn_norm.pop();
        assert!(matches!(
            CommunityShardExecutor::new(
                fx.w1,
                fx.coord_id,
                fx.spec.clone(),
                fx.lease1.clone(),
                Arc::new(bad_shape),
                ShardExecutorConfig::default(),
            ),
            Err(ShardExecutorError::InvalidPartialModel(_))
        ));

        let mut non_canonical_q4 = build_partial_integer_model(&fx.full, fx.lease1.range);
        non_canonical_q4.q4_layers = Some(Vec::new());
        assert!(matches!(
            CommunityShardExecutor::new(
                fx.w1,
                fx.coord_id,
                fx.spec.clone(),
                fx.lease1.clone(),
                Arc::new(non_canonical_q4),
                ShardExecutorConfig::default(),
            ),
            Err(ShardExecutorError::InvalidPartialModel(_))
        ));

        let mut non_canonical_rope = build_partial_integer_model(&fx.full, fx.lease1.range);
        non_canonical_rope.config.arithmetic_profile = ArithmeticProfile::GgufInterleavedRowsV1;
        assert!(matches!(
            CommunityShardExecutor::new(
                fx.w1,
                fx.coord_id,
                fx.spec.clone(),
                fx.lease1.clone(),
                Arc::new(non_canonical_rope),
                ShardExecutorConfig::default(),
            ),
            Err(ShardExecutorError::InvalidPartialModel(_))
        ));

        let bad_cfg = ShardExecutorConfig {
            max_kv_cache_bytes: MAX_KV_CACHE_BYTES_HARD_CAP + 1,
            ..ShardExecutorConfig::default()
        };
        assert!(matches!(
            CommunityShardExecutor::new(
                fx.w1,
                fx.coord_id,
                fx.spec.clone(),
                fx.lease1.clone(),
                Arc::clone(&fx.partial1),
                bad_cfg,
            ),
            Err(ShardExecutorError::InvalidConfig(_))
        ));

        let bad_sessions_cfg = ShardExecutorConfig {
            max_active_sessions: MAX_ACTIVE_SESSIONS_HARD_CAP + 1,
            ..ShardExecutorConfig::default()
        };
        assert!(matches!(
            CommunityShardExecutor::new(
                fx.w1,
                fx.coord_id,
                fx.spec.clone(),
                fx.lease1.clone(),
                Arc::clone(&fx.partial1),
                bad_sessions_cfg,
            ),
            Err(ShardExecutorError::InvalidConfig(_))
        ));

        let bad_deadline_cfg = ShardExecutorConfig {
            max_step_deadline_window_ms: MAX_STEP_DEADLINE_WINDOW_MS + 1,
            ..ShardExecutorConfig::default()
        };
        assert!(matches!(
            CommunityShardExecutor::new(
                fx.w1,
                fx.coord_id,
                fx.spec.clone(),
                fx.lease1.clone(),
                Arc::clone(&fx.partial1),
                bad_deadline_cfg,
            ),
            Err(ShardExecutorError::InvalidConfig(_))
        ));

        let bad_ttl_cfg = ShardExecutorConfig {
            session_ttl_ms: MAX_SESSION_TTL_MS + 1,
            ..ShardExecutorConfig::default()
        };
        assert!(matches!(
            CommunityShardExecutor::new(
                fx.w1,
                fx.coord_id,
                fx.spec.clone(),
                fx.lease1.clone(),
                Arc::clone(&fx.partial1),
                bad_ttl_cfg,
            ),
            Err(ShardExecutorError::InvalidConfig(_))
        ));

        let short_tombstone_cfg = ShardExecutorConfig {
            tombstone_ttl_ms: MAX_STEP_DEADLINE_WINDOW_MS - 1,
            ..ShardExecutorConfig::default()
        };
        assert!(matches!(
            CommunityShardExecutor::new(
                fx.w1,
                fx.coord_id,
                fx.spec.clone(),
                fx.lease1.clone(),
                Arc::clone(&fx.partial1),
                short_tombstone_cfg,
            ),
            Err(ShardExecutorError::InvalidConfig(_))
        ));

        let bad_tomb_cap_cfg = ShardExecutorConfig {
            max_tombstones: MAX_TOMBSTONES_HARD_CAP + 1,
            ..ShardExecutorConfig::default()
        };
        assert!(matches!(
            CommunityShardExecutor::new(
                fx.w1,
                fx.coord_id,
                fx.spec.clone(),
                fx.lease1.clone(),
                Arc::clone(&fx.partial1),
                bad_tomb_cap_cfg,
            ),
            Err(ShardExecutorError::InvalidConfig(_))
        ));

        for bad_max_seq in [0usize, 1, 32_769] {
            let mut bad_spec = fx.spec.clone();
            bad_spec.max_seq = bad_max_seq;
            assert!(matches!(
                CommunityShardExecutor::new(
                    fx.w1,
                    fx.coord_id,
                    bad_spec,
                    fx.lease1.clone(),
                    Arc::clone(&fx.partial1),
                    ShardExecutorConfig::default(),
                ),
                Err(ShardExecutorError::InvalidModelSpec(_))
            ));
        }

        let required_rope_len = fx.spec.max_seq * (fx.full.config.d_head / 2);
        for short_len in [0, required_rope_len - 1] {
            let mut short_rope = build_partial_integer_model(&fx.full, fx.lease1.range);
            short_rope.config.rope_cos.truncate(short_len);
            short_rope.config.rope_sin.truncate(short_len);
            assert!(matches!(
                CommunityShardExecutor::new(
                    fx.w1,
                    fx.coord_id,
                    fx.spec.clone(),
                    fx.lease1.clone(),
                    Arc::new(short_rope),
                    ShardExecutorConfig::default(),
                ),
                Err(ShardExecutorError::InvalidPartialModel(_))
            ));
        }
    }

    #[test]
    fn step_retry_preserves_single_kv_advance_and_rejects_conflicts_and_order_violations() {
        let (fx, mut coord, now) = build_two_worker_fixture();
        let (mut exec1, mut exec2) = build_executors(
            &fx,
            ShardExecutorConfig::default(),
            ShardExecutorConfig::default(),
        );
        coord
            .open_session(now, vec![1u32, 5, 11], 3)
            .expect("open session");

        let work0 = claim_step_work(&mut coord, fx.w1, now, &fx.lease1);
        let res0 = exec1
            .execute_step(fx.coord_id, now, &work0)
            .expect("pos 0 execute");
        let one_pos_bytes = estimate_kv_cache_bytes(2, 16, 1).expect("kv bytes");
        assert_eq!(exec1.status().active_sessions, 1);
        assert_eq!(exec1.status().reserved_kv_bytes, one_pos_bytes);

        let retry0 = exec1
            .execute_step(fx.coord_id, now + 10, &work0)
            .expect("identical retry");
        assert_eq!(retry0, res0);
        assert_eq!(exec1.status().reserved_kv_bytes, one_pos_bytes);

        let mut altered_tok = work0.clone();
        altered_tok.input = ShardStepInput::Token { token: 2 };
        assert_eq!(
            exec1.execute_step(fx.coord_id, now + 10, &altered_tok),
            Err(ShardExecutorError::ConflictingStepRetry)
        );
        let mut altered_dl = work0.clone();
        altered_dl.deadline_unix_ms -= 1;
        assert_eq!(
            exec1.execute_step(fx.coord_id, now + 10, &altered_dl),
            Err(ShardExecutorError::ConflictingStepRetry)
        );

        let mut dup_pos = work0.clone();
        dup_pos.step_id.seq += 1;
        assert!(matches!(
            exec1.execute_step(fx.coord_id, now + 10, &dup_pos),
            Err(ShardExecutorError::DuplicatePositionStepMismatch { position: 0, .. })
        ));

        let mut skip_pos = work0.clone();
        skip_pos.position = 2;
        skip_pos.step_id.seq += 2;
        assert_eq!(
            exec1.execute_step(fx.coord_id, now + 10, &skip_pos),
            Err(ShardExecutorError::UnexpectedPosition {
                expected: 1,
                actual: 2,
            })
        );

        let mut new_nonzero = work0.clone();
        new_nonzero.session_id.seq += 99;
        new_nonzero.position = 1;
        new_nonzero.step_id.seq += 1;
        assert_eq!(
            exec1.execute_step(fx.coord_id, now + 10, &new_nonzero),
            Err(ShardExecutorError::UnexpectedPosition {
                expected: 0,
                actual: 1,
            })
        );

        assert!(
            coord
                .submit_step(fx.w1, now + 20, res0)
                .expect("submit w1 pos 0")
                .accepted
        );
        let work0_w2 = claim_step_work(&mut coord, fx.w2, now + 20, &fx.lease2);
        let res0_w2 = exec2
            .execute_step(fx.coord_id, now + 30, &work0_w2)
            .expect("w2 pos 0");
        assert!(
            coord
                .submit_step(fx.w2, now + 40, res0_w2)
                .expect("submit w2 pos 0")
                .accepted
        );

        let work1 = claim_step_work(&mut coord, fx.w1, now + 50, &fx.lease1);
        assert_eq!(work1.position, 1);
        let res1 = exec1
            .execute_step(fx.coord_id, now + 50, &work1)
            .expect("pos 1 after retry");
        assert_eq!(exec1.status().reserved_kv_bytes, one_pos_bytes * 2);
        assert_eq!(
            exec1
                .execute_step(fx.coord_id, now + 55, &work1)
                .expect("pos 1 retry"),
            res1
        );
        assert_eq!(exec1.status().reserved_kv_bytes, one_pos_bytes * 2);

        let mut non_mono = work1.clone();
        non_mono.position = 2;
        non_mono.step_id.seq = work0.step_id.seq;
        assert_eq!(
            exec1.execute_step(fx.coord_id, now + 55, &non_mono),
            Err(ShardExecutorError::NonMonotonicStepId {
                previous_seq: work1.step_id.seq,
                actual_seq: work0.step_id.seq,
            })
        );
        assert_eq!(
            exec1.execute_step(fx.coord_id, now + 55, &work0),
            Err(ShardExecutorError::UnexpectedPosition {
                expected: 2,
                actual: 0,
            })
        );
    }

    #[test]
    fn rejects_invalid_work_auth_binding_inputs_history_terminal_and_deadlines() {
        let (fx, mut coord, now) = build_two_worker_fixture();
        let (mut exec1, mut exec2) = build_executors(
            &fx,
            ShardExecutorConfig::default(),
            ShardExecutorConfig::default(),
        );
        coord
            .open_session(now, vec![1u32, 5], 2)
            .expect("open session");
        let work1 = claim_step_work(&mut coord, fx.w1, now, &fx.lease1);

        assert_eq!(
            exec1.execute_step(test_hash(b"wrong-coord"), now, &work1),
            Err(ShardExecutorError::CoordinatorMismatch)
        );
        let mut bad_boot = work1.clone();
        bad_boot.session_id.boot_nonce = test_hash(b"other-boot");
        assert_eq!(
            exec1.execute_step(fx.coord_id, now, &bad_boot),
            Err(ShardExecutorError::BootNonceMismatch)
        );
        let mut bad_lease = work1.clone();
        bad_lease.lease_id.seq += 1;
        assert_eq!(
            exec1.execute_step(fx.coord_id, now, &bad_lease),
            Err(ShardExecutorError::LeaseIdMismatch)
        );
        let mut bad_epoch = work1.clone();
        bad_epoch.lease_epoch += 1;
        assert!(matches!(
            exec1.execute_step(fx.coord_id, now, &bad_epoch),
            Err(ShardExecutorError::LeaseEpochMismatch { .. })
        ));
        let mut bad_model = work1.clone();
        bad_model.model_id = test_hash(b"other-model");
        assert_eq!(
            exec1.execute_step(fx.coord_id, now, &bad_model),
            Err(ShardExecutorError::ModelMismatch)
        );
        let mut bad_profile = work1.clone();
        bad_profile.execution_profile = "bad-profile".to_string();
        assert_eq!(
            exec1.execute_step(fx.coord_id, now, &bad_profile),
            Err(ShardExecutorError::ProfileMismatch)
        );
        let mut bad_range = work1.clone();
        bad_range.range = fx.lease2.range;
        assert!(matches!(
            exec1.execute_step(fx.coord_id, now, &bad_range),
            Err(ShardExecutorError::RangeMismatch { .. })
        ));
        let mut bad_hlen = work1.clone();
        bad_hlen.expected_hidden_len = fx.spec.d_model - 1;
        assert!(matches!(
            exec1.execute_step(fx.coord_id, now, &bad_hlen),
            Err(ShardExecutorError::InvalidHiddenLength { .. })
        ));
        let mut bad_term = work1.clone();
        bad_term.expect_terminal = true;
        assert_eq!(
            exec1.execute_step(fx.coord_id, now, &bad_term),
            Err(ShardExecutorError::TerminalModeMismatch {
                expected_terminal: false,
                actual_terminal: true,
            })
        );
        let mut bad_pos = work1.clone();
        bad_pos.position = fx.spec.max_seq;
        assert!(matches!(
            exec1.execute_step(fx.coord_id, now, &bad_pos),
            Err(ShardExecutorError::PositionOutOfRange { .. })
        ));
        let mut expired = work1.clone();
        expired.deadline_unix_ms = now;
        assert_eq!(
            exec1.execute_step(fx.coord_id, now, &expired),
            Err(ShardExecutorError::StepDeadlineExpired)
        );
        let mut over_cap = work1.clone();
        over_cap.deadline_unix_ms = now + MAX_STEP_DEADLINE_WINDOW_MS + 1;
        assert!(matches!(
            exec1.execute_step(fx.coord_id, now, &over_cap),
            Err(ShardExecutorError::StepDeadlineExceedsCap { .. })
        ));
        assert_eq!(
            exec1.execute_step(fx.coord_id, u64::MAX, &work1),
            Err(ShardExecutorError::TimestampOverflow)
        );
        let mut long_hist = work1.clone();
        long_hist.generated_tokens = vec![1];
        assert_eq!(
            exec1.execute_step(fx.coord_id, now, &long_hist),
            Err(ShardExecutorError::HistoryTooLong {
                max_len: 0,
                actual: 1,
            })
        );
        let mut oob_hist = work1.clone();
        oob_hist.position = 1;
        oob_hist.generated_tokens = vec![fx.spec.vocab_size as u32];
        assert!(matches!(
            exec1.execute_step(fx.coord_id, now, &oob_hist),
            Err(ShardExecutorError::TokenOutOfRange { .. })
        ));
        let mut oob_tok = work1.clone();
        oob_tok.input = ShardStepInput::Token {
            token: fx.spec.vocab_size as u32,
        };
        assert!(matches!(
            exec1.execute_step(fx.coord_id, now, &oob_tok),
            Err(ShardExecutorError::TokenOutOfRange { .. })
        ));
        let zeros = vec![0i64; fx.spec.d_model];
        let mut wrong_in1 = work1.clone();
        wrong_in1.input = ShardStepInput::Hidden {
            hidden_hash: hidden_state_hash(&zeros),
            hidden: zeros,
        };
        assert_eq!(
            exec1.execute_step(fx.coord_id, now, &wrong_in1),
            Err(ShardExecutorError::InputKindMismatch {
                expected_token: true,
            })
        );
        assert_eq!(exec1.status().active_sessions, 0);

        let sub1 = exec1
            .execute_step(fx.coord_id, now, &work1)
            .expect("valid w1");
        assert!(
            coord
                .submit_step(fx.w1, now + 10, sub1)
                .expect("submit w1")
                .accepted
        );
        let work2 = claim_step_work(&mut coord, fx.w2, now + 10, &fx.lease2);

        let mut non_term2 = work2.clone();
        non_term2.expect_terminal = false;
        assert_eq!(
            exec2.execute_step(fx.coord_id, now + 10, &non_term2),
            Err(ShardExecutorError::TerminalModeMismatch {
                expected_terminal: true,
                actual_terminal: false,
            })
        );
        let mut tok_in2 = work2.clone();
        tok_in2.input = ShardStepInput::Token { token: 1 };
        assert_eq!(
            exec2.execute_step(fx.coord_id, now + 10, &tok_in2),
            Err(ShardExecutorError::InputKindMismatch {
                expected_token: false,
            })
        );
        let short_h = vec![1i64; fx.spec.d_model - 1];
        let mut short_in2 = work2.clone();
        short_in2.input = ShardStepInput::Hidden {
            hidden_hash: hidden_state_hash(&short_h),
            hidden: short_h,
        };
        assert!(matches!(
            exec2.execute_step(fx.coord_id, now + 10, &short_in2),
            Err(ShardExecutorError::InvalidHiddenLength { .. })
        ));
        for value in [
            i64::MIN,
            i64::MAX,
            crate::community_sharding::MAX_HIDDEN_ABS_Q16 + 1,
        ] {
            let hidden = vec![value; fx.spec.d_model];
            let mut invalid = work2.clone();
            invalid.input = ShardStepInput::Hidden {
                hidden_hash: hidden_state_hash(&hidden),
                hidden,
            };
            assert_eq!(
                exec2.execute_step(fx.coord_id, now + 10, &invalid),
                Err(ShardExecutorError::HiddenValueOutOfBounds)
            );
            assert_eq!(exec2.status().active_sessions, 0);
            assert_eq!(exec2.status().reserved_kv_bytes, 0);
        }
        let mut zero_hash2 = work2.clone();
        if let ShardStepInput::Hidden { hidden_hash, .. } = &mut zero_hash2.input {
            *hidden_hash = Hash256::ZERO;
        }
        assert_eq!(
            exec2.execute_step(fx.coord_id, now + 10, &zero_hash2),
            Err(ShardExecutorError::ZeroDigest)
        );
        let mut tampered2 = work2.clone();
        if let ShardStepInput::Hidden { hidden, .. } = &mut tampered2.input {
            hidden[0] ^= 1;
        }
        assert_eq!(
            exec2.execute_step(fx.coord_id, now + 10, &tampered2),
            Err(ShardExecutorError::HiddenHashMismatch)
        );
        assert_eq!(exec2.status().active_sessions, 0);
        assert_eq!(exec2.status().reserved_kv_bytes, 0);

        let sub2 = exec2
            .execute_step(fx.coord_id, now + 10, &work2)
            .expect("valid terminal w2");
        assert!(matches!(
            sub2.outcome,
            ShardStepOutcome::Completed {
                output: ShardStepOutput::Token { .. }
            }
        ));
    }

    #[test]
    fn cancellation_expiry_release_and_rebind_prevent_resurrection_and_fence_old_boots() {
        let (fx, mut coord, now) = build_two_worker_fixture();
        let cfg = ShardExecutorConfig {
            max_step_deadline_window_ms: MAX_STEP_DEADLINE_WINDOW_MS,
            session_ttl_ms: 120_000,
            tombstone_ttl_ms: MAX_STEP_DEADLINE_WINDOW_MS,
            ..ShardExecutorConfig::default()
        };
        let mut exec1 = CommunityShardExecutor::new(
            fx.w1,
            fx.coord_id,
            fx.spec.clone(),
            fx.lease1.clone(),
            Arc::clone(&fx.partial1),
            cfg,
        )
        .expect("exec1");

        let s1 = coord
            .open_session(now, vec![1u32, 5, 11], 3)
            .expect("open s1");
        let work_s1 = claim_step_work(&mut coord, fx.w1, now, &fx.lease1);
        assert_eq!(work_s1.deadline_unix_ms, now + MAX_STEP_DEADLINE_WINDOW_MS);
        exec1
            .execute_step(fx.coord_id, now, &work_s1)
            .expect("s1 pos 0");
        assert_eq!(exec1.status().active_sessions, 1);
        assert!(exec1.status().reserved_kv_bytes > 0);

        let notice = SessionCancellationNotice {
            session_id: s1,
            lease_id: fx.lease1.lease_id,
            lease_epoch: fx.lease1.lease_epoch,
            reason: SessionAbortReason::CallerCancelled,
            expires_at_unix_ms: now + MAX_STEP_DEADLINE_WINDOW_MS,
        };
        assert_eq!(
            exec1.apply_cancellation(test_hash(b"wrong-coord"), now + 100, &notice),
            Err(ShardExecutorError::CoordinatorMismatch)
        );
        let mut bad_notice = notice.clone();
        bad_notice.lease_epoch += 1;
        assert!(matches!(
            exec1.apply_cancellation(fx.coord_id, now + 100, &bad_notice),
            Err(ShardExecutorError::LeaseEpochMismatch { .. })
        ));

        let cancel_t = now + 500;
        assert_eq!(
            exec1.apply_cancellation(fx.coord_id, cancel_t, &notice),
            Ok(true)
        );
        assert_eq!(exec1.status().active_sessions, 0);
        assert_eq!(exec1.status().reserved_kv_bytes, 0);
        assert_eq!(exec1.status().tombstones, 1);

        assert_eq!(
            exec1.execute_step(fx.coord_id, work_s1.deadline_unix_ms - 1, &work_s1),
            Err(ShardExecutorError::SessionTombstoned)
        );
        let prune_t = cancel_t + MAX_STEP_DEADLINE_WINDOW_MS;
        let sweep1 = exec1.sweep_expired(prune_t);
        assert_eq!(sweep1.pruned_tombstones, 1);
        assert_eq!(
            exec1.execute_step(fx.coord_id, prune_t, &work_s1),
            Err(ShardExecutorError::StepDeadlineExpired)
        );
        assert_eq!(exec1.status().active_sessions, 0);

        let t2 = prune_t + 1_000;
        let mut work_s2 = work_s1.clone();
        work_s2.session_id.seq = s1.seq + 1;
        work_s2.step_id.seq = work_s1.step_id.seq + 10;
        work_s2.deadline_unix_ms = t2 + 10_000;
        exec1
            .execute_step(fx.coord_id, t2, &work_s2)
            .expect("s2 pos 0");
        let exp_t = t2 + 120_000;
        let sweep2 = exec1.sweep_expired(exp_t);
        assert_eq!(sweep2.expired_sessions, 1);
        assert_eq!(exec1.status().active_sessions, 0);
        assert_eq!(exec1.status().reserved_kv_bytes, 0);
        let mut retry_s2 = work_s2.clone();
        retry_s2.deadline_unix_ms = exp_t + 5_000;
        assert_eq!(
            exec1.execute_step(fx.coord_id, exp_t, &retry_s2),
            Err(ShardExecutorError::SessionTombstoned)
        );

        let t3 = exp_t + 1_000;
        let mut work_s3 = work_s1.clone();
        work_s3.session_id.seq = s1.seq + 2;
        work_s3.step_id.seq = work_s1.step_id.seq + 20;
        work_s3.deadline_unix_ms = t3 + 10_000;
        exec1
            .execute_step(fx.coord_id, t3, &work_s3)
            .expect("s3 pos 0");

        let rel = exec1.release(t3 + 10).expect("release");
        assert_eq!(rel.worker_id, fx.w1);
        assert_eq!(rel.lease_id, fx.lease1.lease_id);
        assert_eq!(rel.lease_epoch, fx.lease1.lease_epoch);
        assert!(!exec1.status().bound);
        assert_eq!(exec1.status().active_sessions, 0);
        assert_eq!(exec1.status().reserved_kv_bytes, 0);
        assert_eq!(
            exec1.execute_step(fx.coord_id, t3 + 20, &work_s3),
            Err(ShardExecutorError::ExecutorReleased)
        );
        assert_eq!(
            exec1.rebind(
                fx.coord_id,
                t3 + 20,
                fx.spec.clone(),
                fx.lease1.clone(),
                Arc::clone(&fx.partial1),
            ),
            Err(ShardExecutorError::StaleLease)
        );

        let mut lease_e2 = fx.lease1.clone();
        lease_e2.lease_epoch += 1;
        lease_e2.range = fx.lease2.range;
        exec1
            .rebind(
                fx.coord_id,
                t3 + 30,
                fx.spec.clone(),
                lease_e2.clone(),
                Arc::clone(&fx.partial2),
            )
            .expect("rebind epoch 2");
        assert!(matches!(
            exec1.execute_step(fx.coord_id, t3 + 35, &work_s3),
            Err(ShardExecutorError::LeaseEpochMismatch { .. })
        ));
        let hidden_vec = vec![1i64; fx.spec.d_model];
        let resurrect_s3 = ShardStepWork {
            session_id: work_s3.session_id,
            step_id: StepId {
                boot_nonce: lease_e2.lease_id.boot_nonce,
                seq: work_s3.step_id.seq + 1,
            },
            lease_id: lease_e2.lease_id,
            lease_epoch: lease_e2.lease_epoch,
            model_id: fx.spec.model_id,
            execution_profile: fx.spec.execution_profile.clone(),
            range: lease_e2.range,
            position: 0,
            expected_hidden_len: fx.spec.d_model,
            expect_terminal: true,
            last_token: false,
            deadline_unix_ms: t3 + 5_000,
            input: ShardStepInput::Hidden {
                hidden_hash: hidden_state_hash(&hidden_vec),
                hidden: hidden_vec,
            },
            generated_tokens: Vec::new(),
        };
        assert_eq!(
            exec1.execute_step(fx.coord_id, t3 + 35, &resurrect_s3),
            Err(ShardExecutorError::SessionTombstoned)
        );

        let mut lease_b2 = fx.lease1.clone();
        lease_b2.lease_id = LeaseId {
            boot_nonce: test_hash(b"exec-boot-2"),
            seq: 1,
        };
        lease_b2.lease_epoch = 1;
        exec1
            .rebind(
                fx.coord_id,
                t3 + 40,
                fx.spec.clone(),
                lease_b2,
                Arc::clone(&fx.partial1),
            )
            .expect("rebind new boot");
        assert_eq!(exec1.status().fenced_boot_nonces, 1);
        assert_eq!(exec1.status().tombstones, 0);

        let mut old_boot_high = fx.lease1.clone();
        old_boot_high.lease_id.seq = 999;
        old_boot_high.lease_epoch = 999;
        assert_eq!(
            exec1.rebind(
                fx.coord_id,
                t3 + 50,
                fx.spec.clone(),
                old_boot_high,
                Arc::clone(&fx.partial1),
            ),
            Err(ShardExecutorError::BootNonceFenced)
        );
        assert_eq!(
            exec1.execute_step(fx.coord_id, t3 + 50, &work_s3),
            Err(ShardExecutorError::BootNonceMismatch)
        );
    }

    #[test]
    fn enforces_kv_budget_session_cap_tombstone_cap_and_fenced_boot_cap() {
        assert_eq!(estimate_kv_cache_bytes(2, 16, 1), Some(512));
        assert_eq!(estimate_kv_cache_bytes(2, 16, 2), Some(1024));
        assert_eq!(estimate_kv_cache_bytes(usize::MAX, 16, 1), None);

        let (fx, mut coord, now) = build_two_worker_fixture();
        let s1 = coord
            .open_session(now, vec![1u32, 5, 11], 3)
            .expect("open s1");
        let work0 = claim_step_work(&mut coord, fx.w1, now, &fx.lease1);

        let small_cfg = ShardExecutorConfig {
            max_kv_cache_bytes: 511,
            ..ShardExecutorConfig::default()
        };
        let mut exec_small = CommunityShardExecutor::new(
            fx.w1,
            fx.coord_id,
            fx.spec.clone(),
            fx.lease1.clone(),
            Arc::clone(&fx.partial1),
            small_cfg,
        )
        .expect("exec_small");
        assert_eq!(
            exec_small.execute_step(fx.coord_id, now, &work0),
            Err(ShardExecutorError::KvCacheBudgetExceeded {
                requested_bytes: 512,
                limit_bytes: 511,
            })
        );
        assert_eq!(exec_small.status().active_sessions, 0);
        assert_eq!(exec_small.status().reserved_kv_bytes, 0);
        assert_eq!(exec_small.status().tombstones, 0);

        let one_pos_cfg = ShardExecutorConfig {
            max_kv_cache_bytes: 512,
            ..ShardExecutorConfig::default()
        };
        let mut exec_one = CommunityShardExecutor::new(
            fx.w1,
            fx.coord_id,
            fx.spec.clone(),
            fx.lease1.clone(),
            Arc::clone(&fx.partial1),
            one_pos_cfg,
        )
        .expect("exec_one");
        exec_one
            .execute_step(fx.coord_id, now, &work0)
            .expect("pos 0 within 512");
        let mut work1 = work0.clone();
        work1.position = 1;
        work1.step_id.seq += 1;
        assert_eq!(
            exec_one.execute_step(fx.coord_id, now + 10, &work1),
            Err(ShardExecutorError::KvCacheBudgetExceeded {
                requested_bytes: 1024,
                limit_bytes: 512,
            })
        );
        assert_eq!(exec_one.status().active_sessions, 0);
        assert_eq!(exec_one.status().reserved_kv_bytes, 0);
        assert_eq!(exec_one.status().tombstones, 1);
        assert_eq!(
            exec_one.execute_step(fx.coord_id, now + 20, &work0),
            Err(ShardExecutorError::SessionTombstoned)
        );

        let cap_cfg = ShardExecutorConfig {
            max_active_sessions: 1,
            max_tombstones: 1,
            tombstone_ttl_ms: MAX_STEP_DEADLINE_WINDOW_MS,
            ..ShardExecutorConfig::default()
        };
        let mut exec_cap = CommunityShardExecutor::new(
            fx.w1,
            fx.coord_id,
            fx.spec.clone(),
            fx.lease1.clone(),
            Arc::clone(&fx.partial1),
            cap_cfg,
        )
        .expect("exec_cap");
        exec_cap
            .execute_step(fx.coord_id, now, &work0)
            .expect("s1 start");
        let mut work_s2 = work0.clone();
        work_s2.session_id.seq = s1.seq + 1;
        work_s2.step_id.seq += 10;
        assert_eq!(
            exec_cap.execute_step(fx.coord_id, now, &work_s2),
            Err(ShardExecutorError::SessionCapacityReached)
        );
        let cancel_s1 = SessionCancellationNotice {
            session_id: s1,
            lease_id: fx.lease1.lease_id,
            lease_epoch: fx.lease1.lease_epoch,
            reason: SessionAbortReason::CallerCancelled,
            expires_at_unix_ms: now + MAX_STEP_DEADLINE_WINDOW_MS,
        };
        assert_eq!(
            exec_cap.apply_cancellation(fx.coord_id, now + 10, &cancel_s1),
            Ok(true)
        );
        assert_eq!(
            exec_cap.execute_step(fx.coord_id, now + 20, &work_s2),
            Err(ShardExecutorError::TombstoneCapacityReached)
        );
        let mut cancel_s3 = cancel_s1.clone();
        cancel_s3.session_id.seq = s1.seq + 2;
        assert_eq!(
            exec_cap.apply_cancellation(fx.coord_id, now + 20, &cancel_s3),
            Err(ShardExecutorError::TombstoneCapacityReached)
        );
        assert_eq!(
            exec_cap.execute_step(fx.coord_id, now + 20, &work0),
            Err(ShardExecutorError::SessionTombstoned)
        );
        let after_tomb = now + 10 + MAX_STEP_DEADLINE_WINDOW_MS;
        assert_eq!(exec_cap.sweep_expired(after_tomb).pruned_tombstones, 1);
        work_s2.deadline_unix_ms = after_tomb + 5_000;
        exec_cap
            .execute_step(fx.coord_id, after_tomb, &work_s2)
            .expect("s2 after tombstone prune");

        let mut exec_fence = CommunityShardExecutor::new(
            fx.w1,
            fx.coord_id,
            fx.spec.clone(),
            fx.lease1.clone(),
            Arc::clone(&fx.partial1),
            ShardExecutorConfig::default(),
        )
        .expect("exec_fence");
        for i in 0..MAX_FENCED_BOOT_NONCES_HARD_CAP {
            let mut next_lease = fx.lease1.clone();
            next_lease.lease_id = LeaseId {
                boot_nonce: test_hash(format!("fence-boot-{i}").as_bytes()),
                seq: 1,
            };
            next_lease.lease_epoch = 1;
            exec_fence
                .rebind(
                    fx.coord_id,
                    now + i as u64 + 1,
                    fx.spec.clone(),
                    next_lease,
                    Arc::clone(&fx.partial1),
                )
                .expect("fence rebind");
        }
        assert_eq!(
            exec_fence.status().fenced_boot_nonces,
            MAX_FENCED_BOOT_NONCES_HARD_CAP
        );
        let mut overflow_lease = fx.lease1.clone();
        overflow_lease.lease_id = LeaseId {
            boot_nonce: test_hash(b"fence-boot-overflow"),
            seq: 1,
        };
        overflow_lease.lease_epoch = 1;
        assert_eq!(
            exec_fence.rebind(
                fx.coord_id,
                now + 1_000,
                fx.spec.clone(),
                overflow_lease,
                Arc::clone(&fx.partial1),
            ),
            Err(ShardExecutorError::FencedBootNonceCapacityReached)
        );
        assert_eq!(
            exec_fence.rebind(
                fx.coord_id,
                now + 1_001,
                fx.spec.clone(),
                fx.lease1.clone(),
                Arc::clone(&fx.partial1),
            ),
            Err(ShardExecutorError::BootNonceFenced)
        );
    }
}
