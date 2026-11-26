use crate::community_sharding::{
    COMMUNITY_SHARD_PROTOCOL_VERSION, CommunityShardCoordinator, CommunityShardLimits,
    CoordinatorStatusSnapshot, ShardClaimStepRequest, ShardClaimStepResponse,
    ShardCoordinatorError, ShardHeartbeatRequest, ShardHeartbeatResponse, ShardLeaseRequest,
    ShardLeaseResponse, ShardReleaseRequest, ShardReleaseResponse, ShardStepSubmitRequest,
    ShardStepSubmitResponse, TrustedModelSpec,
};
use arc_crypto::Hash256;
use arc_inference::cached_integer_model::CANONICAL_REWARD_INFERENCE_PROFILE;
use axum::{
    Json, Router,
    extract::{DefaultBodyLimit, State as AxumState},
    http::StatusCode,
    routing::{get, post},
};
use serde::{Deserialize, Serialize};
use std::time::Duration;

pub const COMMUNITY_SHARD_CAPABILITY: &str = "shard-inference-v1";
pub const COMMUNITY_SHARD_LEASE_PATH: &str = "/community/shards/lease";
pub const COMMUNITY_SHARD_HEARTBEAT_PATH: &str = "/community/shards/heartbeat";
pub const COMMUNITY_SHARD_CLAIM_STEP_PATH: &str = "/community/shards/claim_step";
pub const COMMUNITY_SHARD_SUBMIT_STEP_PATH: &str = "/community/shards/submit_step";
pub const COMMUNITY_SHARD_RELEASE_PATH: &str = "/community/shards/release";
pub const COMMUNITY_SHARD_STATUS_PATH: &str = "/community/shards/status";

pub const COMMUNITY_SHARD_METADATA_BODY_LIMIT_BYTES: usize = 16 * 1024;
pub const COMMUNITY_SHARD_SUBMIT_BODY_LIMIT_BYTES: usize = 2 * 1024 * 1024;

pub trait ShardWorkerRequestIdentity {
    fn request_worker_id(&self) -> Hash256;
}

impl ShardWorkerRequestIdentity for ShardLeaseRequest {
    fn request_worker_id(&self) -> Hash256 {
        self.worker_id
    }
}

impl ShardWorkerRequestIdentity for ShardHeartbeatRequest {
    fn request_worker_id(&self) -> Hash256 {
        self.worker_id
    }
}

impl ShardWorkerRequestIdentity for ShardClaimStepRequest {
    fn request_worker_id(&self) -> Hash256 {
        self.worker_id
    }
}

impl ShardWorkerRequestIdentity for ShardStepSubmitRequest {
    fn request_worker_id(&self) -> Hash256 {
        self.worker_id
    }
}

impl ShardWorkerRequestIdentity for ShardReleaseRequest {
    fn request_worker_id(&self) -> Hash256 {
        self.worker_id
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SignedShardWorkerPayload<T> {
    pub worker_id: String,
    pub request: T,
}

impl<T: ShardWorkerRequestIdentity> SignedShardWorkerPayload<T> {
    pub fn new(request: T) -> Self {
        let worker_id = format!("0x{}", request.request_worker_id().to_hex());
        Self { worker_id, request }
    }
}

impl<T: ShardWorkerRequestIdentity> super::CommunityAuthenticatedPayload
    for SignedShardWorkerPayload<T>
{
    fn signer_id(&self) -> &str {
        &self.worker_id
    }

    fn validate_for_auth(&self) -> Result<(), String> {
        let inner = self.request.request_worker_id();
        if inner == Hash256::ZERO {
            return Err("shard worker request worker_id must be nonzero".to_string());
        }
        let expected = format!("0x{}", inner.to_hex());
        if self.worker_id != expected {
            return Err(
                "outer envelope worker_id does not match inner shard request worker_id".to_string(),
            );
        }
        Ok(())
    }
}

pub fn sign_shard_worker_request<T>(
    path: &str,
    request: T,
    keypair: &arc_crypto::KeyPair,
    target_coordinator: Hash256,
    transaction_domain: Option<Hash256>,
) -> Result<super::CommunitySignedRequest<SignedShardWorkerPayload<T>>, String>
where
    T: Serialize + ShardWorkerRequestIdentity,
{
    super::sign_community_request(
        path,
        SignedShardWorkerPayload::new(request),
        keypair,
        target_coordinator,
        transaction_domain,
    )
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CommunityShardStatusResponse {
    pub enabled: bool,
    pub protocol_version: u16,
    pub verified_dispatch_enabled: bool,
    pub rewards_enabled: bool,
    pub status: Option<CoordinatorStatusSnapshot>,
}

pub fn init_community_shard_coordinator(
    node: &super::NodeState,
    enabled: bool,
) -> anyhow::Result<()> {
    if !enabled {
        return Ok(());
    }
    init_community_shard_coordinator_with_limits(node, CommunityShardLimits::default())
}

pub fn init_community_shard_coordinator_with_limits(
    node: &super::NodeState,
    limits: CommunityShardLimits,
) -> anyhow::Result<()> {
    let model_id = node.model_artifact_id.ok_or_else(|| {
        anyhow::anyhow!(
            "--community-shard-coordinator requires a loaded --model with an exact artifact commitment"
        )
    })?;
    let model = node.inference_model.as_ref().ok_or_else(|| {
        anyhow::anyhow!(
            "--community-shard-coordinator requires a loaded canonical integer inference model"
        )
    })?;
    if !model.has_canonical_i8_profile()
        || model.effective_precision_label() != CANONICAL_REWARD_INFERENCE_PROFILE
    {
        anyhow::bail!(
            "--community-shard-coordinator requires the canonical per-row INT8 integer profile"
        );
    }
    let spec = TrustedModelSpec {
        model_id,
        execution_profile: CANONICAL_REWARD_INFERENCE_PROFILE.to_string(),
        n_layers: model.config.n_layers,
        d_model: model.config.d_model,
        vocab_size: model.config.vocab_size,
        max_seq: model.config.max_seq,
        eos_tokens: model.config.eos_tokens.clone(),
        bos_token: model.config.bos_token,
    };
    let boot_nonce = arc_crypto::KeyPair::generate_ed25519().address();
    let coordinator = CommunityShardCoordinator::new(boot_nonce, spec, limits)
        .map_err(|error| anyhow::anyhow!("initialize community shard coordinator: {error}"))?;
    let tile_count = coordinator.fixed_tiles().len();
    let mut guard = node.community_shard_coordinator.lock();
    if guard.is_some() {
        anyhow::bail!("community shard coordinator is already initialized");
    }
    *guard = Some(coordinator);
    drop(guard);
    tracing::info!(
        model_id = %format!("0x{}", model_id.to_hex()),
        n_layers = model.config.n_layers,
        d_model = model.config.d_model,
        tiles = tile_count,
        "community shard coordinator worker transport enabled (verified dispatch not yet active)"
    );
    Ok(())
}

pub fn routes() -> Router<super::NodeState> {
    Router::new()
        .route(
            COMMUNITY_SHARD_LEASE_PATH,
            post(community_shard_lease_signed).layer(DefaultBodyLimit::max(
                COMMUNITY_SHARD_METADATA_BODY_LIMIT_BYTES,
            )),
        )
        .route(
            COMMUNITY_SHARD_HEARTBEAT_PATH,
            post(community_shard_heartbeat_signed).layer(DefaultBodyLimit::max(
                COMMUNITY_SHARD_METADATA_BODY_LIMIT_BYTES,
            )),
        )
        .route(
            COMMUNITY_SHARD_CLAIM_STEP_PATH,
            post(community_shard_claim_step_signed).layer(DefaultBodyLimit::max(
                COMMUNITY_SHARD_METADATA_BODY_LIMIT_BYTES,
            )),
        )
        .route(
            COMMUNITY_SHARD_SUBMIT_STEP_PATH,
            post(community_shard_submit_step_signed).layer(DefaultBodyLimit::max(
                COMMUNITY_SHARD_SUBMIT_BODY_LIMIT_BYTES,
            )),
        )
        .route(
            COMMUNITY_SHARD_RELEASE_PATH,
            post(community_shard_release_signed).layer(DefaultBodyLimit::max(
                COMMUNITY_SHARD_METADATA_BODY_LIMIT_BYTES,
            )),
        )
        .route(COMMUNITY_SHARD_STATUS_PATH, get(community_shard_status))
}

struct AuthenticatedShardWorker<T> {
    worker_id: Hash256,
    worker_id_hex: String,
    registered_model_id: Hash256,
    registered_profile: String,
    request: T,
}

fn ensure_coordinator_enabled(node: &super::NodeState) -> Result<(), (StatusCode, String)> {
    if node.community_shard_coordinator.lock().is_none() {
        return Err((
            StatusCode::SERVICE_UNAVAILABLE,
            "community shard coordinator is not enabled on this node".to_string(),
        ));
    }
    Ok(())
}

fn authenticate_registered_shard_worker<T>(
    node: &super::NodeState,
    path: &str,
    signed: super::CommunitySignedRequest<SignedShardWorkerPayload<T>>,
) -> Result<AuthenticatedShardWorker<T>, (StatusCode, String)>
where
    T: Serialize + ShardWorkerRequestIdentity,
{
    ensure_coordinator_enabled(node)?;
    let envelope = super::authenticate_community_request(node, path, signed)?;
    let worker_id = envelope.request.request_worker_id();
    let worker_id_hex = envelope.worker_id;

    let (registered_worker, last_seen) = node
        .community_workers
        .get(&worker_id_hex)
        .map(|entry| (entry.value().0.clone(), entry.value().1))
        .ok_or_else(|| {
            (
                StatusCode::NOT_FOUND,
                "worker_id not registered - call /community/register first".to_string(),
            )
        })?;

    let ttl = Duration::from_secs(super::COMMUNITY_WORKER_TTL_SECS);
    if last_seen.elapsed() > ttl {
        return Err((
            StatusCode::NOT_FOUND,
            "community worker registration expired - call /community/register again".to_string(),
        ));
    }

    let has_shard_cap = registered_worker
        .capabilities
        .iter()
        .any(|cap| cap == COMMUNITY_SHARD_CAPABILITY);
    if !has_shard_cap {
        return Err((
            StatusCode::FORBIDDEN,
            format!("worker registration must include '{COMMUNITY_SHARD_CAPABILITY}' capability"),
        ));
    }
    if registered_worker
        .capabilities
        .iter()
        .any(|cap| cap == "inference")
    {
        return Err((
            StatusCode::CONFLICT,
            "shard worker must not register whole-model 'inference' capability".to_string(),
        ));
    }

    let registered_model_id = registered_worker
        .model_id
        .as_deref()
        .ok_or_else(|| {
            (
                StatusCode::CONFLICT,
                "registered shard worker is missing pinned model_id".to_string(),
            )
        })
        .and_then(|value| {
            super::parse_hash256_hex(value, "registered model_id")
                .map_err(|error| (StatusCode::CONFLICT, error))
        })?;
    let registered_profile = registered_worker.execution_profile.ok_or_else(|| {
        (
            StatusCode::CONFLICT,
            "registered shard worker is missing canonical execution_profile".to_string(),
        )
    })?;
    if registered_profile != CANONICAL_REWARD_INFERENCE_PROFILE {
        return Err((
            StatusCode::CONFLICT,
            "registered shard worker execution_profile is not the canonical INT8 profile"
                .to_string(),
        ));
    }

    Ok(AuthenticatedShardWorker {
        worker_id,
        worker_id_hex,
        registered_model_id,
        registered_profile,
        request: envelope.request,
    })
}

fn check_registered_model_matches_coordinator(
    coord: &CommunityShardCoordinator,
    registered_model_id: Hash256,
    registered_profile: &str,
) -> Result<(), (StatusCode, String)> {
    let spec = coord.model_spec();
    if registered_model_id != spec.model_id {
        return Err((
            StatusCode::CONFLICT,
            "registered worker model_id does not match coordinator model".to_string(),
        ));
    }
    if registered_profile != spec.execution_profile {
        return Err((
            StatusCode::CONFLICT,
            "registered worker execution_profile does not match coordinator profile".to_string(),
        ));
    }
    Ok(())
}

fn refresh_worker_registration_timestamp(node: &super::NodeState, worker_id_hex: &str) {
    if let Some(mut entry) = node.community_workers.get_mut(worker_id_hex) {
        entry.value_mut().1 = std::time::Instant::now();
    }
}

fn now_unix_ms() -> Result<u64, (StatusCode, String)> {
    super::community_unix_ms().map_err(|error| {
        (
            StatusCode::SERVICE_UNAVAILABLE,
            format!("cannot read coordinator clock: {error}"),
        )
    })
}

fn map_coordinator_error(error: ShardCoordinatorError) -> (StatusCode, String) {
    let status = match &error {
        ShardCoordinatorError::InvalidConfig(_) => StatusCode::INTERNAL_SERVER_ERROR,
        ShardCoordinatorError::InvalidWorkerIdentity
        | ShardCoordinatorError::WorkerIdentityMismatch
        | ShardCoordinatorError::UnsupportedProtocolVersion { .. }
        | ShardCoordinatorError::InsufficientLayerBudget { .. }
        | ShardCoordinatorError::InvalidPrompt(_)
        | ShardCoordinatorError::InvalidLayersProcessed { .. }
        | ShardCoordinatorError::TerminalModeMismatch { .. }
        | ShardCoordinatorError::InvalidHiddenLength { .. }
        | ShardCoordinatorError::HiddenValueOutOfBounds
        | ShardCoordinatorError::HiddenHashMismatch
        | ShardCoordinatorError::TokenOutOfRange { .. }
        | ShardCoordinatorError::ZeroDigest
        | ShardCoordinatorError::ReasonTooLong => StatusCode::BAD_REQUEST,
        ShardCoordinatorError::WorkerQuarantined => StatusCode::FORBIDDEN,
        ShardCoordinatorError::LeaseNotFound | ShardCoordinatorError::SessionNotFound => {
            StatusCode::NOT_FOUND
        }
        ShardCoordinatorError::ModelMismatch(_)
        | ShardCoordinatorError::BootNonceMismatch
        | ShardCoordinatorError::LeaseCapacityConflict { .. }
        | ShardCoordinatorError::LeaseIdMismatch
        | ShardCoordinatorError::LeaseEpochMismatch { .. }
        | ShardCoordinatorError::LeaseNotReady
        | ShardCoordinatorError::LeaseRangeMismatch
        | ShardCoordinatorError::ReadyStateRegression
        | ShardCoordinatorError::UnexpectedStep => StatusCode::CONFLICT,
        ShardCoordinatorError::StepDeadlineExpired => StatusCode::GONE,
        ShardCoordinatorError::WorkerCapacityReached
        | ShardCoordinatorError::SessionCapacityReached => StatusCode::TOO_MANY_REQUESTS,
        ShardCoordinatorError::InsufficientShardCoverage { .. }
        | ShardCoordinatorError::TimestampOverflow
        | ShardCoordinatorError::CounterOverflow => StatusCode::SERVICE_UNAVAILABLE,
    };
    (status, error.to_string())
}

pub(crate) async fn community_shard_lease_signed(
    AxumState(node): AxumState<super::NodeState>,
    Json(signed): Json<super::CommunitySignedRequest<SignedShardWorkerPayload<ShardLeaseRequest>>>,
) -> Result<Json<ShardLeaseResponse>, (StatusCode, String)> {
    let auth = authenticate_registered_shard_worker(&node, COMMUNITY_SHARD_LEASE_PATH, signed)?;
    let now_ms = now_unix_ms()?;
    let response = {
        let mut guard = node.community_shard_coordinator.lock();
        let coord = guard.as_mut().ok_or_else(|| {
            (
                StatusCode::SERVICE_UNAVAILABLE,
                "community shard coordinator is not enabled on this node".to_string(),
            )
        })?;
        check_registered_model_matches_coordinator(
            coord,
            auth.registered_model_id,
            &auth.registered_profile,
        )?;
        coord
            .request_lease(auth.worker_id, now_ms, auth.request)
            .map_err(map_coordinator_error)?
    };
    refresh_worker_registration_timestamp(&node, &auth.worker_id_hex);
    Ok(Json(response))
}

pub(crate) async fn community_shard_heartbeat_signed(
    AxumState(node): AxumState<super::NodeState>,
    Json(signed): Json<
        super::CommunitySignedRequest<SignedShardWorkerPayload<ShardHeartbeatRequest>>,
    >,
) -> Result<Json<ShardHeartbeatResponse>, (StatusCode, String)> {
    let auth = authenticate_registered_shard_worker(&node, COMMUNITY_SHARD_HEARTBEAT_PATH, signed)?;
    let now_ms = now_unix_ms()?;
    let response = {
        let mut guard = node.community_shard_coordinator.lock();
        let coord = guard.as_mut().ok_or_else(|| {
            (
                StatusCode::SERVICE_UNAVAILABLE,
                "community shard coordinator is not enabled on this node".to_string(),
            )
        })?;
        check_registered_model_matches_coordinator(
            coord,
            auth.registered_model_id,
            &auth.registered_profile,
        )?;
        coord
            .heartbeat(auth.worker_id, now_ms, auth.request)
            .map_err(map_coordinator_error)?
    };
    refresh_worker_registration_timestamp(&node, &auth.worker_id_hex);
    node.community_shard_notify.notify_waiters();
    Ok(Json(response))
}

pub(crate) async fn community_shard_claim_step_signed(
    AxumState(node): AxumState<super::NodeState>,
    Json(signed): Json<
        super::CommunitySignedRequest<SignedShardWorkerPayload<ShardClaimStepRequest>>,
    >,
) -> Result<Json<ShardClaimStepResponse>, (StatusCode, String)> {
    let auth =
        authenticate_registered_shard_worker(&node, COMMUNITY_SHARD_CLAIM_STEP_PATH, signed)?;
    let now_ms = now_unix_ms()?;
    let response = {
        let mut guard = node.community_shard_coordinator.lock();
        let coord = guard.as_mut().ok_or_else(|| {
            (
                StatusCode::SERVICE_UNAVAILABLE,
                "community shard coordinator is not enabled on this node".to_string(),
            )
        })?;
        check_registered_model_matches_coordinator(
            coord,
            auth.registered_model_id,
            &auth.registered_profile,
        )?;
        coord
            .claim_step(auth.worker_id, now_ms, auth.request)
            .map_err(map_coordinator_error)?
    };
    Ok(Json(response))
}

pub(crate) async fn community_shard_submit_step_signed(
    AxumState(node): AxumState<super::NodeState>,
    Json(signed): Json<
        super::CommunitySignedRequest<SignedShardWorkerPayload<ShardStepSubmitRequest>>,
    >,
) -> Result<Json<ShardStepSubmitResponse>, (StatusCode, String)> {
    let auth =
        authenticate_registered_shard_worker(&node, COMMUNITY_SHARD_SUBMIT_STEP_PATH, signed)?;
    let now_ms = now_unix_ms()?;
    let response = {
        let mut guard = node.community_shard_coordinator.lock();
        let coord = guard.as_mut().ok_or_else(|| {
            (
                StatusCode::SERVICE_UNAVAILABLE,
                "community shard coordinator is not enabled on this node".to_string(),
            )
        })?;
        check_registered_model_matches_coordinator(
            coord,
            auth.registered_model_id,
            &auth.registered_profile,
        )?;
        coord
            .submit_step(auth.worker_id, now_ms, auth.request)
            .map_err(map_coordinator_error)?
    };
    if response.accepted {
        node.community_shard_notify.notify_waiters();
    }
    Ok(Json(response))
}

pub(crate) async fn community_shard_release_signed(
    AxumState(node): AxumState<super::NodeState>,
    Json(signed): Json<
        super::CommunitySignedRequest<SignedShardWorkerPayload<ShardReleaseRequest>>,
    >,
) -> Result<Json<ShardReleaseResponse>, (StatusCode, String)> {
    let auth = authenticate_registered_shard_worker(&node, COMMUNITY_SHARD_RELEASE_PATH, signed)?;
    let now_ms = now_unix_ms()?;
    let response = {
        let mut guard = node.community_shard_coordinator.lock();
        let coord = guard.as_mut().ok_or_else(|| {
            (
                StatusCode::SERVICE_UNAVAILABLE,
                "community shard coordinator is not enabled on this node".to_string(),
            )
        })?;
        check_registered_model_matches_coordinator(
            coord,
            auth.registered_model_id,
            &auth.registered_profile,
        )?;
        coord
            .release_lease(auth.worker_id, now_ms, auth.request)
            .map_err(map_coordinator_error)?
    };
    node.community_shard_notify.notify_waiters();
    Ok(Json(response))
}

pub(crate) async fn community_shard_status(
    AxumState(node): AxumState<super::NodeState>,
) -> Result<Json<CommunityShardStatusResponse>, (StatusCode, String)> {
    let now_ms = now_unix_ms()?;
    let snapshot = {
        let guard = node.community_shard_coordinator.lock();
        guard.as_ref().map(|coord| coord.status_snapshot(now_ms))
    };
    Ok(Json(CommunityShardStatusResponse {
        enabled: snapshot.is_some(),
        protocol_version: COMMUNITY_SHARD_PROTOCOL_VERSION,
        verified_dispatch_enabled: false,
        rewards_enabled: false,
        status: snapshot,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::community_sharding::{
        HeartbeatDirective, SessionOutcome, ShardLayerRange, ShardStepInput, ShardStepOutcome,
        ShardStepOutput, hidden_state_hash,
    };
    use arc_crypto::KeyPair;
    use arc_inference::cached_integer_model::{
        ArithmeticProfile, CachedIntegerModel, CachedLayer, I8Weights, KVCache, ModelConfig,
        ShardInput, ShardOutput,
    };
    use std::sync::Arc;
    use std::sync::atomic::AtomicU32;
    use std::time::Instant;

    const Q16_ONE: i64 = 1i64 << 16;

    fn tiny_i8_weights(rng: &mut u64, n_rows: usize, n_cols: usize) -> I8Weights {
        let mut next = || -> u64 {
            *rng = rng
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            *rng
        };
        let mut data = Vec::with_capacity(n_rows * n_cols);
        for _ in 0..(n_rows * n_cols) {
            data.push((((next() >> 32) % 121) as i64 - 60) as i8);
        }
        let mut scales = Vec::with_capacity(n_rows);
        for _ in 0..n_rows {
            scales.push(((next() >> 32) % 64) as i64 + 32);
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

    fn build_tiny_model(n_layers: usize) -> CachedIntegerModel {
        let vocab_size = 32usize;
        let d_model = 16usize;
        let n_heads = 2usize;
        let d_ff = 32usize;
        let max_seq = 64usize;
        let d_head = d_model / n_heads;
        let d_kv = d_head * n_heads;
        let mut rng = 0x1234_5678_9ABC_DEF0u64;
        let embedding_i8 = tiny_i8_weights(&mut rng, vocab_size, d_model);
        let mut embedding_q16 = Vec::with_capacity(vocab_size * d_model);
        for row in 0..vocab_size {
            let scale = embedding_i8.scales[row];
            for col in 0..d_model {
                embedding_q16.push(i64::from(embedding_i8.data[row * d_model + col]) * scale);
            }
        }
        let output_weight = tiny_i8_weights(&mut rng, vocab_size, d_model);
        let mut layers = Vec::with_capacity(n_layers);
        for _ in 0..n_layers {
            layers.push(CachedLayer {
                wq: tiny_i8_weights(&mut rng, d_model, d_model),
                wk: tiny_i8_weights(&mut rng, d_kv, d_model),
                wv: tiny_i8_weights(&mut rng, d_kv, d_model),
                wo: tiny_i8_weights(&mut rng, d_model, d_model),
                w_gate: tiny_i8_weights(&mut rng, d_ff, d_model),
                w_up: tiny_i8_weights(&mut rng, d_ff, d_model),
                w_down: tiny_i8_weights(&mut rng, d_model, d_ff),
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
                eos_tokens: vec![128_001, 128_009],
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

    fn slice_partial_model(
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
            config: ModelConfig {
                n_layers: full.config.n_layers,
                d_model: full.config.d_model,
                n_heads: full.config.n_heads,
                n_kv_heads: full.config.n_kv_heads,
                d_ff: full.config.d_ff,
                d_head: full.config.d_head,
                d_kv: full.config.d_kv,
                vocab_size: full.config.vocab_size,
                attn_scale: full.config.attn_scale,
                rope_cos: full.config.rope_cos.clone(),
                rope_sin: full.config.rope_sin.clone(),
                max_seq: full.config.max_seq,
                eos_tokens: full.config.eos_tokens.clone(),
                bos_token: full.config.bos_token,
                chat_template: full.config.chat_template.clone(),
                arithmetic_profile: full.config.arithmetic_profile,
            },
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

    fn build_test_node(
        model: Option<Arc<CachedIntegerModel>>,
        model_id: Option<Hash256>,
    ) -> super::super::NodeState {
        let coord_kp = Arc::new(KeyPair::generate_ed25519());
        let coord_addr = coord_kp.address();
        super::super::build_node_state(
            Arc::new(arc_state::StateDB::new()),
            Arc::new(arc_mempool::Mempool::new(1_000)),
            coord_addr,
            Some(coord_kp),
            1_000,
            Instant::now(),
            Arc::new(AtomicU32::new(0)),
            model,
            None,
            None,
            model_id,
        )
    }

    fn make_register_req(
        keypair: &KeyPair,
        model_id: Hash256,
        capabilities: Vec<&str>,
    ) -> super::super::CommunityRegisterRequest {
        super::super::CommunityRegisterRequest {
            worker_id: format!("0x{}", keypair.address().to_hex()),
            name: "shard-test-worker".to_string(),
            capabilities: capabilities.into_iter().map(String::from).collect(),
            model: Some("arc-4L-16d-2h-32v".to_string()),
            model_id: Some(format!("0x{}", model_id.to_hex())),
            execution_profile: Some(CANONICAL_REWARD_INFERENCE_PROFILE.to_string()),
            platform: "linux-x86_64".to_string(),
        }
    }

    async fn register_worker_signed(
        node: &super::super::NodeState,
        keypair: &KeyPair,
        model_id: Hash256,
        capabilities: Vec<&str>,
    ) -> Result<(), (StatusCode, String)> {
        let req = make_register_req(keypair, model_id, capabilities);
        let signed = super::super::sign_community_request(
            super::super::COMMUNITY_REGISTER_PATH,
            req,
            keypair,
            node.validator_address,
            node.state.transaction_domain_hash(),
        )
        .expect("sign registration");
        super::super::community_register_signed(AxumState(node.clone()), Json(signed))
            .await
            .map(|_| ())
    }

    fn make_lease_request(
        worker_id: Hash256,
        model_id: Hash256,
        budget: usize,
    ) -> ShardLeaseRequest {
        ShardLeaseRequest {
            worker_id,
            protocol_version: COMMUNITY_SHARD_PROTOCOL_VERSION,
            model_id,
            execution_profile: CANONICAL_REWARD_INFERENCE_PROFILE.to_string(),
            n_layers: 4,
            d_model: 16,
            vocab_size: 32,
            max_seq: 64,
            max_resident_layers: budget,
        }
    }

    #[tokio::test]
    async fn community_shard_http_init_requires_model_artifact_canonical_profile_and_runs_once() {
        let model_id = arc_crypto::hash_bytes(b"http-init-model");
        let full = Arc::new(build_tiny_model(4));

        let no_model = build_test_node(None, Some(model_id));
        assert!(init_community_shard_coordinator(&no_model, false).is_ok());
        assert!(init_community_shard_coordinator(&no_model, true).is_err());

        let no_artifact = build_test_node(Some(full.clone()), None);
        assert!(init_community_shard_coordinator(&no_artifact, true).is_err());

        let mut non_canonical = build_tiny_model(4);
        non_canonical.q4_layers = Some(Vec::new());
        let bad_profile_node = build_test_node(Some(Arc::new(non_canonical)), Some(model_id));
        assert!(init_community_shard_coordinator(&bad_profile_node, true).is_err());

        let mut wrong_arithmetic = build_tiny_model(4);
        wrong_arithmetic.config.arithmetic_profile = ArithmeticProfile::GgufInterleavedRowsV1;
        let wrong_arithmetic_node =
            build_test_node(Some(Arc::new(wrong_arithmetic)), Some(model_id));
        assert!(init_community_shard_coordinator(&wrong_arithmetic_node, true).is_err());

        let partial = Arc::new(slice_partial_model(
            &full,
            ShardLayerRange {
                start_layer: 0,
                end_layer: 2,
            },
        ));
        assert!(!partial.has_all_transformer_layers());
        assert!(partial.has_canonical_i8_profile());
        let partial_node = build_test_node(Some(partial), Some(model_id));
        init_community_shard_coordinator(&partial_node, true)
            .expect("partial validator model with trusted shape must initialize");
        assert!(init_community_shard_coordinator(&partial_node, true).is_err());
    }

    #[tokio::test]
    async fn community_shard_http_auth_rejects_unregistered_forged_domain_outer_inner_and_replay() {
        let model_id = arc_crypto::hash_bytes(b"http-auth-model");
        let full = Arc::new(build_tiny_model(4));
        let node = build_test_node(Some(full), Some(model_id));
        init_community_shard_coordinator_with_limits(
            &node,
            CommunityShardLimits {
                target_partitions: 2,
                ..CommunityShardLimits::default()
            },
        )
        .unwrap();

        let worker_kp = KeyPair::generate_ed25519();
        let other_kp = KeyPair::generate_ed25519();
        let worker_addr = worker_kp.address();

        let unregistered_signed = sign_shard_worker_request(
            COMMUNITY_SHARD_LEASE_PATH,
            make_lease_request(worker_addr, model_id, 2),
            &worker_kp,
            node.validator_address,
            node.state.transaction_domain_hash(),
        )
        .unwrap();
        let err = community_shard_lease_signed(AxumState(node.clone()), Json(unregistered_signed))
            .await
            .unwrap_err();
        assert_eq!(err.0, StatusCode::NOT_FOUND);

        register_worker_signed(
            &node,
            &worker_kp,
            model_id,
            vec![COMMUNITY_SHARD_CAPABILITY],
        )
        .await
        .unwrap();

        let mismatched_envelope = SignedShardWorkerPayload {
            worker_id: format!("0x{}", worker_addr.to_hex()),
            request: make_lease_request(other_kp.address(), model_id, 2),
        };
        let mismatched_signed = super::super::sign_community_request(
            COMMUNITY_SHARD_LEASE_PATH,
            mismatched_envelope,
            &worker_kp,
            node.validator_address,
            node.state.transaction_domain_hash(),
        )
        .unwrap();
        let err = community_shard_lease_signed(AxumState(node.clone()), Json(mismatched_signed))
            .await
            .unwrap_err();
        assert_eq!(err.0, StatusCode::BAD_REQUEST);

        let forged_signed = sign_shard_worker_request(
            COMMUNITY_SHARD_LEASE_PATH,
            make_lease_request(worker_addr, model_id, 2),
            &other_kp,
            node.validator_address,
            node.state.transaction_domain_hash(),
        )
        .unwrap();
        let err = community_shard_lease_signed(AxumState(node.clone()), Json(forged_signed))
            .await
            .unwrap_err();
        assert_eq!(err.0, StatusCode::UNAUTHORIZED);

        let wrong_coord_signed = sign_shard_worker_request(
            COMMUNITY_SHARD_LEASE_PATH,
            make_lease_request(worker_addr, model_id, 2),
            &worker_kp,
            arc_crypto::hash_bytes(b"different-coordinator"),
            node.state.transaction_domain_hash(),
        )
        .unwrap();
        let err = community_shard_lease_signed(AxumState(node.clone()), Json(wrong_coord_signed))
            .await
            .unwrap_err();
        assert_eq!(err.0, StatusCode::FORBIDDEN);

        let wrong_domain_signed = sign_shard_worker_request(
            COMMUNITY_SHARD_LEASE_PATH,
            make_lease_request(worker_addr, model_id, 2),
            &worker_kp,
            node.validator_address,
            Some(arc_crypto::hash_bytes(b"wrong-network-domain")),
        )
        .unwrap();
        let err = community_shard_lease_signed(AxumState(node.clone()), Json(wrong_domain_signed))
            .await
            .unwrap_err();
        assert_eq!(err.0, StatusCode::FORBIDDEN);

        let valid_signed = sign_shard_worker_request(
            COMMUNITY_SHARD_LEASE_PATH,
            make_lease_request(worker_addr, model_id, 2),
            &worker_kp,
            node.validator_address,
            node.state.transaction_domain_hash(),
        )
        .unwrap();
        let first =
            community_shard_lease_signed(AxumState(node.clone()), Json(valid_signed.clone()))
                .await
                .expect("valid signed lease must succeed");
        assert_eq!(first.0.assignment.lease_epoch, 1);

        let replay_err = community_shard_lease_signed(AxumState(node.clone()), Json(valid_signed))
            .await
            .unwrap_err();
        assert_eq!(replay_err.0, StatusCode::CONFLICT);
    }

    #[tokio::test]
    async fn community_shard_http_rejects_missing_capability_whole_mix_model_mismatch_and_expired()
    {
        let model_id = arc_crypto::hash_bytes(b"http-cap-model");
        let other_model_id = arc_crypto::hash_bytes(b"http-other-model");
        let full = Arc::new(build_tiny_model(4));
        let node = build_test_node(Some(full), Some(model_id));
        init_community_shard_coordinator_with_limits(
            &node,
            CommunityShardLimits {
                target_partitions: 2,
                ..CommunityShardLimits::default()
            },
        )
        .unwrap();

        let whole_kp = KeyPair::generate_ed25519();
        register_worker_signed(&node, &whole_kp, model_id, vec!["inference"])
            .await
            .unwrap();
        let no_cap_signed = sign_shard_worker_request(
            COMMUNITY_SHARD_LEASE_PATH,
            make_lease_request(whole_kp.address(), model_id, 2),
            &whole_kp,
            node.validator_address,
            node.state.transaction_domain_hash(),
        )
        .unwrap();
        let err = community_shard_lease_signed(AxumState(node.clone()), Json(no_cap_signed))
            .await
            .unwrap_err();
        assert_eq!(err.0, StatusCode::FORBIDDEN);

        let mixed_kp = KeyPair::generate_ed25519();
        let mixed_err = register_worker_signed(
            &node,
            &mixed_kp,
            model_id,
            vec!["inference", COMMUNITY_SHARD_CAPABILITY],
        )
        .await
        .unwrap_err();
        assert_eq!(mixed_err.0, StatusCode::BAD_REQUEST);

        let wrong_model_kp = KeyPair::generate_ed25519();
        register_worker_signed(
            &node,
            &wrong_model_kp,
            other_model_id,
            vec![COMMUNITY_SHARD_CAPABILITY],
        )
        .await
        .unwrap();
        let wrong_model_signed = sign_shard_worker_request(
            COMMUNITY_SHARD_LEASE_PATH,
            make_lease_request(wrong_model_kp.address(), model_id, 2),
            &wrong_model_kp,
            node.validator_address,
            node.state.transaction_domain_hash(),
        )
        .unwrap();
        let err = community_shard_lease_signed(AxumState(node.clone()), Json(wrong_model_signed))
            .await
            .unwrap_err();
        assert_eq!(err.0, StatusCode::CONFLICT);

        let shard_kp = KeyPair::generate_ed25519();
        register_worker_signed(&node, &shard_kp, model_id, vec![COMMUNITY_SHARD_CAPABILITY])
            .await
            .unwrap();
        let whole_claim = super::super::sign_community_request(
            super::super::COMMUNITY_CLAIM_WORK_PATH,
            super::super::ClaimWorkRequest {
                worker_id: format!("0x{}", shard_kp.address().to_hex()),
                capabilities: vec!["inference".to_string()],
                model_id: format!("0x{}", model_id.to_hex()),
                execution_profile: CANONICAL_REWARD_INFERENCE_PROFILE.to_string(),
            },
            &shard_kp,
            node.validator_address,
            node.state.transaction_domain_hash(),
        )
        .unwrap();
        let whole_claim_err =
            super::super::community_claim_work_signed(AxumState(node.clone()), Json(whole_claim))
                .await
                .unwrap_err();
        assert_eq!(whole_claim_err.0, StatusCode::BAD_REQUEST);

        let worker_hex = format!("0x{}", shard_kp.address().to_hex());
        let expired_instant = Instant::now()
            .checked_sub(Duration::from_secs(
                super::super::COMMUNITY_WORKER_TTL_SECS + 5,
            ))
            .expect("host monotonic clock exceeds community worker ttl");
        node.community_workers
            .get_mut(&worker_hex)
            .unwrap()
            .value_mut()
            .1 = expired_instant;
        let expired_signed = sign_shard_worker_request(
            COMMUNITY_SHARD_LEASE_PATH,
            make_lease_request(shard_kp.address(), model_id, 2),
            &shard_kp,
            node.validator_address,
            node.state.transaction_domain_hash(),
        )
        .unwrap();
        let expired_err =
            community_shard_lease_signed(AxumState(node.clone()), Json(expired_signed))
                .await
                .unwrap_err();
        assert_eq!(expired_err.0, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn community_shard_http_disabled_coordinator_returns_503_and_honest_status() {
        let model_id = arc_crypto::hash_bytes(b"http-disabled-model");
        let full = Arc::new(build_tiny_model(4));
        let node = build_test_node(Some(full), Some(model_id));

        let status = community_shard_status(AxumState(node.clone()))
            .await
            .expect("status succeeds when disabled")
            .0;
        assert!(!status.enabled);
        assert!(!status.verified_dispatch_enabled);
        assert!(!status.rewards_enabled);
        assert!(status.status.is_none());

        let worker_kp = KeyPair::generate_ed25519();
        register_worker_signed(
            &node,
            &worker_kp,
            model_id,
            vec![COMMUNITY_SHARD_CAPABILITY],
        )
        .await
        .unwrap();
        let signed = sign_shard_worker_request(
            COMMUNITY_SHARD_LEASE_PATH,
            make_lease_request(worker_kp.address(), model_id, 2),
            &worker_kp,
            node.validator_address,
            node.state.transaction_domain_hash(),
        )
        .unwrap();
        let err = community_shard_lease_signed(AxumState(node.clone()), Json(signed))
            .await
            .unwrap_err();
        assert_eq!(err.0, StatusCode::SERVICE_UNAVAILABLE);
    }

    #[tokio::test]
    async fn community_shard_http_lifecycle_lease_ready_claim_submit_status_and_release() {
        let model_id = arc_crypto::hash_bytes(b"http-flow-model");
        let full = Arc::new(build_tiny_model(4));
        let node = build_test_node(Some(full.clone()), Some(model_id));
        init_community_shard_coordinator_with_limits(
            &node,
            CommunityShardLimits {
                target_partitions: 2,
                ..CommunityShardLimits::default()
            },
        )
        .unwrap();

        let kp1 = KeyPair::generate_ed25519();
        let kp2 = KeyPair::generate_ed25519();
        register_worker_signed(&node, &kp1, model_id, vec![COMMUNITY_SHARD_CAPABILITY])
            .await
            .unwrap();
        register_worker_signed(&node, &kp2, model_id, vec![COMMUNITY_SHARD_CAPABILITY])
            .await
            .unwrap();

        let w1_hex = format!("0x{}", kp1.address().to_hex());
        let aged = Instant::now()
            .checked_sub(Duration::from_secs(10))
            .unwrap_or_else(Instant::now);
        node.community_workers
            .get_mut(&w1_hex)
            .unwrap()
            .value_mut()
            .1 = aged;

        let lease1 = community_shard_lease_signed(
            AxumState(node.clone()),
            Json(
                sign_shard_worker_request(
                    COMMUNITY_SHARD_LEASE_PATH,
                    make_lease_request(kp1.address(), model_id, 2),
                    &kp1,
                    node.validator_address,
                    node.state.transaction_domain_hash(),
                )
                .unwrap(),
            ),
        )
        .await
        .expect("worker 1 lease")
        .0
        .assignment;
        let refreshed = node.community_workers.get(&w1_hex).unwrap().value().1;
        assert!(refreshed >= aged);

        let lease2 = community_shard_lease_signed(
            AxumState(node.clone()),
            Json(
                sign_shard_worker_request(
                    COMMUNITY_SHARD_LEASE_PATH,
                    make_lease_request(kp2.address(), model_id, 2),
                    &kp2,
                    node.validator_address,
                    node.state.transaction_domain_hash(),
                )
                .unwrap(),
            ),
        )
        .await
        .expect("worker 2 lease")
        .0
        .assignment;
        assert_ne!(lease1.range, lease2.range);

        let hb1 = community_shard_heartbeat_signed(
            AxumState(node.clone()),
            Json(
                sign_shard_worker_request(
                    COMMUNITY_SHARD_HEARTBEAT_PATH,
                    ShardHeartbeatRequest {
                        worker_id: kp1.address(),
                        lease_id: lease1.lease_id,
                        lease_epoch: lease1.lease_epoch,
                        model_id,
                        execution_profile: CANONICAL_REWARD_INFERENCE_PROFILE.to_string(),
                        loaded_range: lease1.range,
                        ready: true,
                    },
                    &kp1,
                    node.validator_address,
                    node.state.transaction_domain_hash(),
                )
                .unwrap(),
            ),
        )
        .await
        .expect("worker 1 ready")
        .0;
        assert!(matches!(hb1.directive, HeartbeatDirective::Active { .. }));

        let idle = community_shard_claim_step_signed(
            AxumState(node.clone()),
            Json(
                sign_shard_worker_request(
                    COMMUNITY_SHARD_CLAIM_STEP_PATH,
                    ShardClaimStepRequest {
                        worker_id: kp1.address(),
                        lease_id: lease1.lease_id,
                        lease_epoch: lease1.lease_epoch,
                        model_id,
                        execution_profile: CANONICAL_REWARD_INFERENCE_PROFILE.to_string(),
                    },
                    &kp1,
                    node.validator_address,
                    node.state.transaction_domain_hash(),
                )
                .unwrap(),
            ),
        )
        .await
        .expect("worker 1 idle claim")
        .0;
        assert_eq!(idle, ShardClaimStepResponse::Idle);

        let hb2 = community_shard_heartbeat_signed(
            AxumState(node.clone()),
            Json(
                sign_shard_worker_request(
                    COMMUNITY_SHARD_HEARTBEAT_PATH,
                    ShardHeartbeatRequest {
                        worker_id: kp2.address(),
                        lease_id: lease2.lease_id,
                        lease_epoch: lease2.lease_epoch,
                        model_id,
                        execution_profile: CANONICAL_REWARD_INFERENCE_PROFILE.to_string(),
                        loaded_range: lease2.range,
                        ready: true,
                    },
                    &kp2,
                    node.validator_address,
                    node.state.transaction_domain_hash(),
                )
                .unwrap(),
            ),
        )
        .await
        .expect("worker 2 ready")
        .0;
        assert!(matches!(hb2.directive, HeartbeatDirective::Active { .. }));

        let status = community_shard_status(AxumState(node.clone()))
            .await
            .unwrap()
            .0;
        assert!(status.enabled);
        assert!(!status.verified_dispatch_enabled);
        assert!(!status.rewards_enabled);
        let snap = status.status.expect("status snapshot");
        assert!(snap.fully_covered_ready);
        assert_eq!(snap.ready_workers, 2);

        let prompt = vec![1u32, 3];
        let max_tokens = 2u32;
        let session_id = {
            let now_ms = super::super::community_unix_ms().unwrap();
            let mut guard = node.community_shard_coordinator.lock();
            guard
                .as_mut()
                .unwrap()
                .open_session(now_ms, prompt.clone(), max_tokens)
                .expect("internal session open")
        };

        let p1 = slice_partial_model(&full, lease1.range);
        let p2 = slice_partial_model(&full, lease2.range);
        let mut kv1 = KVCache::new(full.config.n_layers);
        let mut kv2 = KVCache::new(full.config.n_layers);
        let mut done = false;
        for _ in 0..16 {
            for (kp, lease, partial, kv) in [
                (&kp1, &lease1, &p1, &mut kv1),
                (&kp2, &lease2, &p2, &mut kv2),
            ] {
                let claim = community_shard_claim_step_signed(
                    AxumState(node.clone()),
                    Json(
                        sign_shard_worker_request(
                            COMMUNITY_SHARD_CLAIM_STEP_PATH,
                            ShardClaimStepRequest {
                                worker_id: kp.address(),
                                lease_id: lease.lease_id,
                                lease_epoch: lease.lease_epoch,
                                model_id,
                                execution_profile: CANONICAL_REWARD_INFERENCE_PROFILE.to_string(),
                            },
                            kp,
                            node.validator_address,
                            node.state.transaction_domain_hash(),
                        )
                        .unwrap(),
                    ),
                )
                .await
                .unwrap()
                .0;
                if let ShardClaimStepResponse::Step { work } = claim {
                    let input = match &work.input {
                        ShardStepInput::Token { token } => ShardInput::Token(*token),
                        ShardStepInput::Hidden { hidden, .. } => ShardInput::Hidden(hidden.clone()),
                    };
                    let out = partial
                        .forward_shard_token_with_history(
                            input,
                            kv,
                            work.range.start_layer,
                            work.range.end_layer,
                            work.position,
                            &work.generated_tokens,
                        )
                        .unwrap();
                    let step_out = match out {
                        ShardOutput::Hidden(hidden) => ShardStepOutput::Hidden {
                            hidden_hash: hidden_state_hash(&hidden),
                            hidden,
                        },
                        ShardOutput::Token { id, logits_hash } => ShardStepOutput::Token {
                            token_id: id,
                            logits_hash,
                        },
                    };
                    let sub_resp = community_shard_submit_step_signed(
                        AxumState(node.clone()),
                        Json(
                            sign_shard_worker_request(
                                COMMUNITY_SHARD_SUBMIT_STEP_PATH,
                                ShardStepSubmitRequest {
                                    worker_id: kp.address(),
                                    lease_id: work.lease_id,
                                    lease_epoch: work.lease_epoch,
                                    session_id: work.session_id,
                                    step_id: work.step_id,
                                    model_id,
                                    execution_profile: CANONICAL_REWARD_INFERENCE_PROFILE
                                        .to_string(),
                                    range: work.range,
                                    position: work.position,
                                    layers_processed: work.range.len(),
                                    outcome: ShardStepOutcome::Completed { output: step_out },
                                },
                                kp,
                                node.validator_address,
                                node.state.transaction_domain_hash(),
                            )
                            .unwrap(),
                        ),
                    )
                    .await
                    .unwrap()
                    .0;
                    assert!(sub_resp.accepted);
                    if sub_resp.session_completed {
                        done = true;
                    }
                }
            }
            if done {
                break;
            }
        }
        assert!(done);
        let outcome = node
            .community_shard_coordinator
            .lock()
            .as_mut()
            .unwrap()
            .take_result(session_id)
            .expect("pending verification outcome");
        let pending = match outcome {
            SessionOutcome::PendingVerification(pending) => pending,
            other => panic!("expected pending verification outcome, got {other:?}"),
        };
        assert!(pending.verification_required);
        assert!(!pending.reward_eligible);
        let (expected_tokens, _) = full
            .try_generate(&prompt, max_tokens, &full.config.eos_tokens)
            .unwrap();
        assert_eq!(pending.unverified_generated_tokens, expected_tokens);

        let release = community_shard_release_signed(
            AxumState(node.clone()),
            Json(
                sign_shard_worker_request(
                    COMMUNITY_SHARD_RELEASE_PATH,
                    ShardReleaseRequest {
                        worker_id: kp1.address(),
                        lease_id: lease1.lease_id,
                        lease_epoch: lease1.lease_epoch,
                    },
                    &kp1,
                    node.validator_address,
                    node.state.transaction_domain_hash(),
                )
                .unwrap(),
            ),
        )
        .await
        .expect("worker 1 release")
        .0;
        assert!(release.released);

        let status_after = community_shard_status(AxumState(node.clone()))
            .await
            .unwrap()
            .0;
        let snap_after = status_after.status.unwrap();
        assert_eq!(snap_after.ready_workers, 1);
        assert!(!snap_after.fully_covered_ready);
    }

    #[tokio::test]
    async fn community_shard_http_router_body_limits_enforced() {
        let model_id = arc_crypto::hash_bytes(b"http-body-limits");
        let full = Arc::new(build_tiny_model(4));
        let node = build_test_node(Some(full), Some(model_id));
        init_community_shard_coordinator(&node, true).unwrap();

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind loopback test listener");
        let addr = listener.local_addr().unwrap();
        let app = routes().with_state(node);
        let server = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        let client = reqwest::Client::new();
        let base = format!("http://{addr}");

        let status_resp = client
            .get(format!("{base}{COMMUNITY_SHARD_STATUS_PATH}"))
            .send()
            .await
            .expect("GET status");
        assert_eq!(status_resp.status(), StatusCode::OK);
        let decoded: CommunityShardStatusResponse = status_resp.json().await.unwrap();
        assert!(decoded.enabled);
        assert!(!decoded.verified_dispatch_enabled);

        let meta_oversize = vec![b' '; COMMUNITY_SHARD_METADATA_BODY_LIMIT_BYTES + 1];
        let lease_resp = client
            .post(format!("{base}{COMMUNITY_SHARD_LEASE_PATH}"))
            .header("content-type", "application/json")
            .body(meta_oversize.clone())
            .send()
            .await
            .expect("oversize lease");
        assert_eq!(lease_resp.status(), StatusCode::PAYLOAD_TOO_LARGE);

        let submit_mid = client
            .post(format!("{base}{COMMUNITY_SHARD_SUBMIT_STEP_PATH}"))
            .header("content-type", "application/json")
            .body(meta_oversize)
            .send()
            .await
            .expect("mid-sized submit");
        assert_ne!(submit_mid.status(), StatusCode::PAYLOAD_TOO_LARGE);

        let submit_oversize = vec![b' '; COMMUNITY_SHARD_SUBMIT_BODY_LIMIT_BYTES + 1];
        let submit_resp = client
            .post(format!("{base}{COMMUNITY_SHARD_SUBMIT_STEP_PATH}"))
            .header("content-type", "application/json")
            .body(submit_oversize)
            .send()
            .await
            .expect("oversize submit");
        assert_eq!(submit_resp.status(), StatusCode::PAYLOAD_TOO_LARGE);

        server.abort();
    }

    #[derive(Debug, Clone, serde::Serialize)]
    struct TestShardClaimPayload {
        worker_id: Hash256,
        step: u64,
    }

    impl super::ShardWorkerRequestIdentity for TestShardClaimPayload {
        fn request_worker_id(&self) -> Hash256 {
            self.worker_id
        }
    }

    fn sign_at_timestamp<T: serde::Serialize>(
        path: &str,
        payload: T,
        keypair: &KeyPair,
        target_coordinator: Hash256,
        transaction_domain: Option<Hash256>,
        timestamp_unix_ms: u64,
    ) -> super::super::CommunitySignedRequest<T> {
        let mut signed = super::super::sign_community_request(
            path,
            payload,
            keypair,
            target_coordinator,
            transaction_domain,
        )
        .unwrap();
        signed.timestamp_unix_ms = timestamp_unix_ms;
        let commitment = super::super::community_request_commitment(
            path,
            signed.version,
            signed.target_coordinator,
            signed.transaction_domain,
            signed.payload_hash,
            signed.timestamp_unix_ms,
            signed.nonce,
        );
        signed.signature = keypair.sign(&commitment).unwrap();
        signed
    }

    #[tokio::test]
    async fn community_shard_http_replay_cache_allows_over_64_claims_enforces_30s_skew_and_isolates_paths()
     {
        for exact in [
            COMMUNITY_SHARD_LEASE_PATH,
            COMMUNITY_SHARD_HEARTBEAT_PATH,
            COMMUNITY_SHARD_CLAIM_STEP_PATH,
            COMMUNITY_SHARD_SUBMIT_STEP_PATH,
            COMMUNITY_SHARD_RELEASE_PATH,
        ] {
            assert!(super::super::is_community_shard_authenticated_path(exact));
        }
        for rejected in [
            COMMUNITY_SHARD_STATUS_PATH,
            "/community/shards/",
            "/community/shards/unknown",
            "/community/shards/lease/",
            "/community/shards/claim_step?x=1",
        ] {
            assert!(!super::super::is_community_shard_authenticated_path(
                rejected
            ));
        }

        let model_id = arc_crypto::hash_bytes(b"http-shard-replay-model");
        let node = build_test_node(Some(Arc::new(build_tiny_model(4))), Some(model_id));
        init_community_shard_coordinator_with_limits(
            &node,
            CommunityShardLimits {
                target_partitions: 2,
                ..CommunityShardLimits::default()
            },
        )
        .unwrap();

        let worker_kp = KeyPair::generate_ed25519();
        let worker_addr = worker_kp.address();
        let domain = node.state.transaction_domain_hash();
        register_worker_signed(
            &node,
            &worker_kp,
            model_id,
            vec![COMMUNITY_SHARD_CAPABILITY],
        )
        .await
        .unwrap();

        let lease_signed = sign_shard_worker_request(
            COMMUNITY_SHARD_LEASE_PATH,
            make_lease_request(worker_addr, model_id, 2),
            &worker_kp,
            node.validator_address,
            domain,
        )
        .unwrap();
        let lease = community_shard_lease_signed(AxumState(node.clone()), Json(lease_signed))
            .await
            .unwrap()
            .0
            .assignment;
        assert_eq!(lease.lease_epoch, 1);
        assert_eq!(node.community_shard_request_replays.lock().seen.len(), 1);
        assert_eq!(node.community_request_replays.lock().seen.len(), 1);

        let heartbeat = sign_shard_worker_request(
            COMMUNITY_SHARD_HEARTBEAT_PATH,
            ShardHeartbeatRequest {
                worker_id: worker_addr,
                lease_id: lease.lease_id,
                lease_epoch: lease.lease_epoch,
                model_id,
                execution_profile: lease.execution_profile.clone(),
                loaded_range: lease.range,
                ready: true,
            },
            &worker_kp,
            node.validator_address,
            domain,
        )
        .unwrap();
        let heartbeat_response =
            community_shard_heartbeat_signed(AxumState(node.clone()), Json(heartbeat))
                .await
                .unwrap();
        assert!(matches!(
            heartbeat_response.0.directive,
            HeartbeatDirective::Active { .. }
        ));
        assert!(heartbeat_response.0.cancellations.is_empty());

        let mut first_claim = None;
        let mut last_claim = None;
        for step in 0u64..80 {
            let signed = sign_shard_worker_request(
                COMMUNITY_SHARD_CLAIM_STEP_PATH,
                ShardClaimStepRequest {
                    worker_id: worker_addr,
                    lease_id: lease.lease_id,
                    lease_epoch: lease.lease_epoch,
                    model_id,
                    execution_profile: lease.execution_profile.clone(),
                },
                &worker_kp,
                node.validator_address,
                domain,
            )
            .unwrap();
            if step == 0 {
                first_claim = Some(signed.clone());
            }
            if step == 79 {
                last_claim = Some(signed.clone());
            }
            assert_eq!(
                community_shard_claim_step_signed(AxumState(node.clone()), Json(signed))
                    .await
                    .unwrap()
                    .0,
                ShardClaimStepResponse::Idle,
            );
        }
        assert_eq!(
            super::super::authenticate_community_request(
                &node,
                COMMUNITY_SHARD_CLAIM_STEP_PATH,
                first_claim.unwrap(),
            )
            .unwrap_err()
            .0,
            StatusCode::CONFLICT
        );
        assert_eq!(
            super::super::authenticate_community_request(
                &node,
                COMMUNITY_SHARD_CLAIM_STEP_PATH,
                last_claim.unwrap(),
            )
            .unwrap_err()
            .0,
            StatusCode::CONFLICT
        );

        for (idx, path) in [
            COMMUNITY_SHARD_HEARTBEAT_PATH,
            COMMUNITY_SHARD_SUBMIT_STEP_PATH,
            COMMUNITY_SHARD_RELEASE_PATH,
        ]
        .into_iter()
        .enumerate()
        {
            let signed = sign_shard_worker_request(
                path,
                TestShardClaimPayload {
                    worker_id: worker_addr,
                    step: 100 + idx as u64,
                },
                &worker_kp,
                node.validator_address,
                domain,
            )
            .unwrap();
            super::super::authenticate_community_request(&node, path, signed).unwrap();
        }
        assert_eq!(node.community_shard_request_replays.lock().seen.len(), 85);
        assert_eq!(node.community_request_replays.lock().seen.len(), 1);

        let now = super::super::community_unix_ms().unwrap();
        for bad_ts in [now.saturating_sub(31_000), now.saturating_add(31_000)] {
            let signed = sign_at_timestamp(
                COMMUNITY_SHARD_CLAIM_STEP_PATH,
                SignedShardWorkerPayload::new(TestShardClaimPayload {
                    worker_id: worker_addr,
                    step: bad_ts,
                }),
                &worker_kp,
                node.validator_address,
                domain,
                bad_ts,
            );
            assert_eq!(
                super::super::authenticate_community_request(
                    &node,
                    COMMUNITY_SHARD_CLAIM_STEP_PATH,
                    signed,
                )
                .unwrap_err()
                .0,
                StatusCode::UNAUTHORIZED
            );
        }
        assert_eq!(node.community_shard_request_replays.lock().seen.len(), 85);

        for ok_ts in [now.saturating_sub(25_000), now.saturating_add(25_000)] {
            let signed = sign_at_timestamp(
                COMMUNITY_SHARD_CLAIM_STEP_PATH,
                SignedShardWorkerPayload::new(TestShardClaimPayload {
                    worker_id: worker_addr,
                    step: ok_ts,
                }),
                &worker_kp,
                node.validator_address,
                domain,
                ok_ts,
            );
            super::super::authenticate_community_request(
                &node,
                COMMUNITY_SHARD_CLAIM_STEP_PATH,
                signed,
            )
            .unwrap();
        }
        assert_eq!(node.community_shard_request_replays.lock().seen.len(), 87);

        for unprivileged in [COMMUNITY_SHARD_STATUS_PATH, "/community/shards/unknown"] {
            let signed = sign_at_timestamp(
                unprivileged,
                SignedShardWorkerPayload::new(TestShardClaimPayload {
                    worker_id: worker_addr,
                    step: 999,
                }),
                &worker_kp,
                node.validator_address,
                domain,
                now.saturating_sub(31_000),
            );
            super::super::authenticate_community_request(&node, unprivileged, signed).unwrap();
        }
        assert_eq!(node.community_shard_request_replays.lock().seen.len(), 87);
        assert_eq!(node.community_request_replays.lock().seen.len(), 3);

        for _ in 3..super::super::COMMUNITY_AUTH_REPLAYS_PER_IDENTITY_MAX {
            let signed = super::super::sign_community_request(
                super::super::COMMUNITY_REGISTER_PATH,
                make_register_req(&worker_kp, model_id, vec![COMMUNITY_SHARD_CAPABILITY]),
                &worker_kp,
                node.validator_address,
                domain,
            )
            .unwrap();
            super::super::authenticate_community_request(
                &node,
                super::super::COMMUNITY_REGISTER_PATH,
                signed,
            )
            .unwrap();
        }
        let normal_over_cap = super::super::sign_community_request(
            super::super::COMMUNITY_REGISTER_PATH,
            make_register_req(&worker_kp, model_id, vec![COMMUNITY_SHARD_CAPABILITY]),
            &worker_kp,
            node.validator_address,
            domain,
        )
        .unwrap();
        assert_eq!(
            super::super::authenticate_community_request(
                &node,
                super::super::COMMUNITY_REGISTER_PATH,
                normal_over_cap,
            )
            .unwrap_err()
            .0,
            StatusCode::TOO_MANY_REQUESTS
        );
        let shard_after_normal_cap = sign_shard_worker_request(
            COMMUNITY_SHARD_CLAIM_STEP_PATH,
            TestShardClaimPayload {
                worker_id: worker_addr,
                step: 10_000,
            },
            &worker_kp,
            node.validator_address,
            domain,
        )
        .unwrap();
        super::super::authenticate_community_request(
            &node,
            COMMUNITY_SHARD_CLAIM_STEP_PATH,
            shard_after_normal_cap,
        )
        .unwrap();
        assert_eq!(node.community_shard_request_replays.lock().seen.len(), 88);
        node.community_shard_request_replays
            .lock()
            .assert_invariants();
        node.community_request_replays.lock().assert_invariants();
    }
}

#[cfg(test)]
mod worker_tests;
