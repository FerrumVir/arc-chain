use crate::community_sharding::{
    COMMUNITY_SHARD_PROTOCOL_VERSION, CommunityShardCoordinator, LeaseId,
    MAX_GENERATED_TOKENS_HARD_CAP, MAX_SEQUENCE_POSITIONS_HARD_CAP, PendingVerificationResult,
    SessionAbortReason, SessionId, SessionOutcome, SessionParticipantRecord, ShardCoordinatorError,
    ShardLayerRange, TrustedModelSpec, unverified_tokens_digest,
};
use arc_crypto::Hash256;
use arc_inference::cached_integer_model::{CANONICAL_REWARD_INFERENCE_PROFILE, CachedIntegerModel};
use axum::{
    Json, Router,
    extract::{DefaultBodyLimit, State as AxumState},
    http::StatusCode,
    routing::post,
};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::sync::Arc;
use std::time::{Duration, Instant};

pub const COMMUNITY_SHARD_DISPATCH_PATH: &str = "/inference/run_community_sharded";
pub const COMMUNITY_SHARD_DISPATCH_BODY_LIMIT_BYTES: usize = 16 * 1024;
pub const COMMUNITY_SHARD_DISPATCH_MAX_PROMPT_BYTES: usize = 1000;
pub const COMMUNITY_SHARD_DISPATCH_MAX_TOKENS: u32 = 64;
pub const COMMUNITY_SHARD_PARTICIPANT_IDENTITY_BASIS: &str =
    "node_key_not_proof_of_independent_operator";

const COMMUNITY_SHARD_WAIT_RECHECK_INTERVAL: Duration = Duration::from_millis(100);
const COMMUNITY_SHARD_WAIT_MIN_SLICE_MS: u64 = 10;
const COMMUNITY_SHARD_DEADLINE_GRACE_MS: u64 = 250;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CommunityShardedInferenceRequest {
    pub input: String,
    pub max_tokens: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CommunityShardParticipantSummary {
    pub worker_id: String,
    pub lease_id: LeaseId,
    pub lease_epoch: u64,
    pub range: ShardLayerRange,
}

#[derive(Debug, Serialize)]
pub struct CommunityShardedInferenceResponse {
    pub protocol_version: u16,
    pub session_id: SessionId,
    pub model_name: String,
    pub model_id: String,
    pub execution_profile: String,
    pub generated_tokens: Vec<u32>,
    pub tokens_generated: usize,
    pub output: String,
    pub output_hash: String,
    pub stopped_by_eos: bool,
    pub total_ms: u64,
    pub participants: Vec<CommunityShardParticipantSummary>,
    pub participant_identity_basis: &'static str,
    verification: super::CommunityVerificationSummary,
    pub rewards_enabled: bool,
    pub reward_eligible: bool,
}

struct VerifiedDispatchContext {
    model: Arc<CachedIntegerModel>,
    model_name: String,
    model_id: Hash256,
}

struct OwnedShardSessionGuard {
    node: super::NodeState,
    session_id: SessionId,
    opened_at_ms: u64,
    active: bool,
}

impl OwnedShardSessionGuard {
    fn new(node: super::NodeState, session_id: SessionId, opened_at_ms: u64) -> Self {
        Self {
            node,
            session_id,
            opened_at_ms,
            active: true,
        }
    }

    fn disarm(&mut self) {
        self.active = false;
    }
}

impl Drop for OwnedShardSessionGuard {
    fn drop(&mut self) {
        if !self.active {
            return;
        }
        let now_ms = super::community_unix_ms().unwrap_or(self.opened_at_ms);
        {
            let mut guard = self.node.community_shard_coordinator.lock();
            if let Some(coordinator) = guard.as_mut() {
                let _ = coordinator.cancel_session(
                    now_ms,
                    self.session_id,
                    SessionAbortReason::CallerCancelled,
                );
                let _ = coordinator.take_result(self.session_id);
            }
        }
        self.node.community_shard_notify.notify_waiters();
    }
}

pub fn routes() -> Router<super::NodeState> {
    Router::new().route(
        COMMUNITY_SHARD_DISPATCH_PATH,
        post(run_community_sharded).layer(DefaultBodyLimit::max(
            COMMUNITY_SHARD_DISPATCH_BODY_LIMIT_BYTES,
        )),
    )
}

pub(crate) fn is_verified_dispatch_available(node: &super::NodeState) -> bool {
    let Ok(ctx) = load_canonical_dispatch_model(node) else {
        return false;
    };
    ensure_verification_availability(node, ctx.model.config.n_layers, false).is_ok()
}

pub(crate) async fn run_community_sharded(
    AxumState(node): AxumState<super::NodeState>,
    Json(req): Json<CommunityShardedInferenceRequest>,
) -> Result<Json<CommunityShardedInferenceResponse>, (StatusCode, String)> {
    let started_at = Instant::now();
    ensure_coordinator_enabled(&node)?;
    validate_request_bounds(&req)?;

    let _inference_permit = node
        .public_inference_permits
        .clone()
        .try_acquire_owned()
        .map_err(|_| {
            (
                StatusCode::TOO_MANY_REQUESTS,
                "public inference capacity is saturated; retry after 2 seconds".to_string(),
            )
        })?;

    let ctx = load_canonical_dispatch_model(&node)?;
    let prompt_tokens = encode_and_preflight_prompt(&ctx.model, &req.input, req.max_tokens)?;
    ensure_verification_availability(&node, ctx.model.config.n_layers, true)?;

    let open_now_ms = now_unix_ms()?;
    let (session_id, session_deadline_ms) = {
        let mut guard = node.community_shard_coordinator.lock();
        let coordinator = guard.as_mut().ok_or_else(|| {
            (
                StatusCode::SERVICE_UNAVAILABLE,
                "community shard coordinator is disabled".to_string(),
            )
        })?;
        validate_coordinator_invariants(coordinator, ctx.model_id, &ctx.model)?;
        let session_id = coordinator
            .open_session(open_now_ms, prompt_tokens.clone(), req.max_tokens)
            .map_err(map_open_session_error)?;
        let session_deadline_ms = coordinator
            .session_status(session_id)
            .map(|status| status.deadline_unix_ms)
            .unwrap_or(open_now_ms);
        (session_id, session_deadline_ms)
    };
    let mut session_guard = OwnedShardSessionGuard::new(node.clone(), session_id, open_now_ms);

    let job_budget_ms = session_deadline_ms.saturating_sub(open_now_ms);
    let monotonic_deadline = tokio::time::Instant::now()
        + Duration::from_millis(job_budget_ms.saturating_add(COMMUNITY_SHARD_DEADLINE_GRACE_MS));

    let outcome = wait_for_session_outcome(&node, session_id, monotonic_deadline).await?;
    session_guard.disarm();

    let pending = match outcome {
        SessionOutcome::PendingVerification(pending) => pending,
        SessionOutcome::Aborted(aborted) => {
            return Err(map_abort_reason(&aborted.reason));
        }
    };

    validate_pending_result(
        &pending,
        session_id,
        ctx.model_id,
        &prompt_tokens,
        req.max_tokens,
        &ctx.model,
    )?;

    let candidate_output = ctx.model.decode(&pending.unverified_generated_tokens);
    let candidate_output_hash = raw_token_output_hash(&pending.unverified_generated_tokens);
    let verification_id = format!(
        "community-shard-{}-{}",
        session_id.boot_nonce.to_hex(),
        session_id.seq
    );
    let candidate = super::CommunityVerificationCandidate {
        verification_id,
        input: &req.input,
        max_tokens: req.max_tokens,
        model_id: ctx.model_id,
        execution_profile: CANONICAL_REWARD_INFERENCE_PROFILE,
        submitted_at_unix_ms: i64::try_from(open_now_ms).unwrap_or(i64::MAX),
        expires_at_unix_ms: session_deadline_ms,
        candidate_tokens: &pending.unverified_generated_tokens,
        candidate_output: &candidate_output,
        candidate_output_hash,
    };

    let verified = super::verify_community_candidate_with_quorum(&node, &candidate)
        .await
        .map_err(|error| match error {
            super::CommunityResultVerificationError::Invalid(reason) => (
                StatusCode::BAD_GATEWAY,
                format!("community shard output failed validator quorum verification: {reason}"),
            ),
            super::CommunityResultVerificationError::Unavailable(reason) => (
                StatusCode::SERVICE_UNAVAILABLE,
                format!("validator quorum verification unavailable: {reason}"),
            ),
        })?;

    if verified.execution_profile != CANONICAL_REWARD_INFERENCE_PROFILE {
        return Err((
            StatusCode::BAD_GATEWAY,
            format!(
                "validator verification profile {:?} differs from required {:?}",
                verified.execution_profile, CANONICAL_REWARD_INFERENCE_PROFILE
            ),
        ));
    }
    if verified.accepted_tokens != pending.unverified_generated_tokens
        || verified.tokens_generated != pending.unverified_generated_tokens.len()
        || verified.output_hash != candidate_output_hash
    {
        return Err((
            StatusCode::BAD_GATEWAY,
            "validator-verified tokens do not match community shard session output".to_string(),
        ));
    }
    let verified_output = ctx.model.decode(&verified.accepted_tokens);
    if verified_output != candidate_output {
        return Err((
            StatusCode::BAD_GATEWAY,
            "validator-verified decoded text does not match candidate output".to_string(),
        ));
    }

    let participants = pending
        .participants
        .into_iter()
        .map(|participant| CommunityShardParticipantSummary {
            worker_id: format!("0x{}", participant.worker_id.to_hex()),
            lease_id: participant.lease_id,
            lease_epoch: participant.lease_epoch,
            range: participant.range,
        })
        .collect();
    let verification = super::CommunityVerificationSummary::from(&verified);
    let total_ms = u64::try_from(started_at.elapsed().as_millis()).unwrap_or(u64::MAX);

    Ok(Json(CommunityShardedInferenceResponse {
        protocol_version: COMMUNITY_SHARD_PROTOCOL_VERSION,
        session_id,
        model_name: ctx.model_name,
        model_id: format!("0x{}", ctx.model_id.to_hex()),
        execution_profile: CANONICAL_REWARD_INFERENCE_PROFILE.to_string(),
        generated_tokens: verified.accepted_tokens,
        tokens_generated: verified.tokens_generated,
        output: verified_output,
        output_hash: format!("0x{}", verified.output_hash.to_hex()),
        stopped_by_eos: pending.stopped_by_eos,
        total_ms,
        participants,
        participant_identity_basis: COMMUNITY_SHARD_PARTICIPANT_IDENTITY_BASIS,
        verification,
        rewards_enabled: false,
        reward_eligible: false,
    }))
}

fn ensure_coordinator_enabled(node: &super::NodeState) -> Result<(), (StatusCode, String)> {
    if node.community_shard_coordinator.lock().is_none() {
        return Err((
            StatusCode::SERVICE_UNAVAILABLE,
            "community shard coordinator is disabled".to_string(),
        ));
    }
    Ok(())
}

fn effective_max_tokens_cap() -> u32 {
    COMMUNITY_SHARD_DISPATCH_MAX_TOKENS
        .min(super::INFERENCE_RUN_MAX_TOKENS)
        .min(MAX_GENERATED_TOKENS_HARD_CAP)
}

fn validate_request_bounds(
    req: &CommunityShardedInferenceRequest,
) -> Result<(), (StatusCode, String)> {
    if req.input.is_empty() {
        return Err((
            StatusCode::BAD_REQUEST,
            "input must not be empty".to_string(),
        ));
    }
    if req.input.len() > COMMUNITY_SHARD_DISPATCH_MAX_PROMPT_BYTES {
        return Err((
            StatusCode::BAD_REQUEST,
            format!(
                "input exceeds maximum {COMMUNITY_SHARD_DISPATCH_MAX_PROMPT_BYTES} bytes, got {}",
                req.input.len()
            ),
        ));
    }
    let max_tokens_cap = effective_max_tokens_cap();
    if req.max_tokens == 0 || req.max_tokens > max_tokens_cap {
        return Err((
            StatusCode::BAD_REQUEST,
            format!(
                "max_tokens must be in 1..={max_tokens_cap}, got {}",
                req.max_tokens
            ),
        ));
    }
    Ok(())
}

fn load_canonical_dispatch_model(
    node: &super::NodeState,
) -> Result<VerifiedDispatchContext, (StatusCode, String)> {
    let (core_spec, tile_count, max_hidden_len) = {
        let guard = node.community_shard_coordinator.lock();
        let coordinator = guard.as_ref().ok_or_else(|| {
            (
                StatusCode::SERVICE_UNAVAILABLE,
                "community shard coordinator is disabled".to_string(),
            )
        })?;
        (
            coordinator.model_spec().clone(),
            coordinator.fixed_tiles().len(),
            coordinator.limits().max_hidden_len,
        )
    };
    let (model_name, model_id) = super::exact_model_identity(node)
        .map_err(|error| (StatusCode::SERVICE_UNAVAILABLE, error))?;
    if node.model_artifact_id != Some(model_id) {
        return Err((
            StatusCode::SERVICE_UNAVAILABLE,
            "coordinator model_artifact_id does not match exact model identity".to_string(),
        ));
    }
    let model = node
        .inference_model
        .as_ref()
        .ok_or_else(|| {
            (
                StatusCode::SERVICE_UNAVAILABLE,
                "coordinator has no loaded canonical integer inference model".to_string(),
            )
        })?
        .clone();
    validate_canonical_model(&model)?;
    validate_model_spec(&core_spec, tile_count, max_hidden_len, model_id, &model)?;
    if node
        .community_verify_profiles
        .verified(CANONICAL_REWARD_INFERENCE_PROFILE)
        != Some(CANONICAL_REWARD_INFERENCE_PROFILE)
    {
        return Err((
            StatusCode::SERVICE_UNAVAILABLE,
            format!(
                "coordinator verification profiles do not allow canonical profile {:?}",
                CANONICAL_REWARD_INFERENCE_PROFILE
            ),
        ));
    }
    Ok(VerifiedDispatchContext {
        model,
        model_name,
        model_id,
    })
}

fn validate_canonical_model(model: &CachedIntegerModel) -> Result<(), (StatusCode, String)> {
    if !model.has_canonical_i8_profile()
        || model.effective_precision_label() != CANONICAL_REWARD_INFERENCE_PROFILE
    {
        return Err((
            StatusCode::SERVICE_UNAVAILABLE,
            format!(
                "community shard dispatch requires canonical profile {:?}",
                CANONICAL_REWARD_INFERENCE_PROFILE
            ),
        ));
    }
    if model.config.n_layers < 2 {
        return Err((
            StatusCode::SERVICE_UNAVAILABLE,
            format!(
                "community shard dispatch requires at least 2 layers, model has {}",
                model.config.n_layers
            ),
        ));
    }
    Ok(())
}

fn validate_coordinator_invariants(
    coordinator: &CommunityShardCoordinator,
    model_id: Hash256,
    model: &CachedIntegerModel,
) -> Result<(), (StatusCode, String)> {
    validate_model_spec(
        coordinator.model_spec(),
        coordinator.fixed_tiles().len(),
        coordinator.limits().max_hidden_len,
        model_id,
        model,
    )
}

fn validate_model_spec(
    spec: &TrustedModelSpec,
    tile_count: usize,
    max_hidden_len: usize,
    model_id: Hash256,
    model: &CachedIntegerModel,
) -> Result<(), (StatusCode, String)> {
    if tile_count < 2 {
        return Err((
            StatusCode::SERVICE_UNAVAILABLE,
            format!(
                "community shard coordinator requires at least 2 partial tiles, got {tile_count}"
            ),
        ));
    }
    if model.config.d_model > max_hidden_len {
        return Err((
            StatusCode::SERVICE_UNAVAILABLE,
            format!(
                "model width {} exceeds coordinator max_hidden_len {max_hidden_len}",
                model.config.d_model
            ),
        ));
    }
    if spec.model_id != model_id
        || spec.execution_profile != CANONICAL_REWARD_INFERENCE_PROFILE
        || spec.n_layers != model.config.n_layers
        || spec.d_model != model.config.d_model
        || spec.vocab_size != model.config.vocab_size
        || spec.max_seq != model.config.max_seq
        || spec.bos_token != model.config.bos_token
        || spec.eos_tokens != model.config.eos_tokens
    {
        return Err((
            StatusCode::SERVICE_UNAVAILABLE,
            "community shard coordinator model_spec does not match loaded canonical model and tokenizer"
                .to_string(),
        ));
    }
    Ok(())
}

fn encode_and_preflight_prompt(
    model: &CachedIntegerModel,
    input: &str,
    max_tokens: u32,
) -> Result<Vec<u32>, (StatusCode, String)> {
    let prompt_tokens = model.encode(input);
    if prompt_tokens.is_empty() {
        return Err((
            StatusCode::BAD_REQUEST,
            "prompt encoded to zero tokens".to_string(),
        ));
    }
    for &token in &prompt_tokens {
        if (token as usize) >= model.config.vocab_size {
            return Err((
                StatusCode::BAD_REQUEST,
                format!(
                    "encoded prompt token {token} out of range for vocab_size {}",
                    model.config.vocab_size
                ),
            ));
        }
    }
    let preflight = model
        .preflight_generation(prompt_tokens.len(), max_tokens)
        .map_err(|error| (StatusCode::BAD_REQUEST, error.to_string()))?;
    if preflight.required_positions > MAX_SEQUENCE_POSITIONS_HARD_CAP {
        return Err((
            StatusCode::BAD_REQUEST,
            format!(
                "BOS + prompt_tokens ({}) + max_tokens ({max_tokens}) exceeds sequence cap {}",
                prompt_tokens.len(),
                model.config.max_seq.min(MAX_SEQUENCE_POSITIONS_HARD_CAP)
            ),
        ));
    }
    Ok(prompt_tokens)
}

fn active_validator_count(node: &super::NodeState) -> usize {
    let validators = node.dag_validators.read();
    validators
        .iter()
        .filter_map(|(address, stake)| (*stake > 0).then_some(*address))
        .collect::<HashSet<_>>()
        .len()
}

fn ensure_verification_availability(
    node: &super::NodeState,
    n_layers: usize,
    prune_expired_shards: bool,
) -> Result<(), (StatusCode, String)> {
    let active_validators = active_validator_count(node);
    if active_validators < super::COMMUNITY_VERIFICATION_SIGNATURES_REQUIRED {
        return Err((
            StatusCode::SERVICE_UNAVAILABLE,
            format!(
                "validator quorum verification requires at least {} distinct active validators, found {active_validators}",
                super::COMMUNITY_VERIFICATION_SIGNATURES_REQUIRED
            ),
        ));
    }
    let plan = super::plan_community_verification(
        node,
        CANONICAL_REWARD_INFERENCE_PROFILE,
        prune_expired_shards,
    )
    .map_err(|(_, detail)| (StatusCode::SERVICE_UNAVAILABLE, detail))?;
    validate_verification_plan_coverage(&plan, n_layers)
        .map_err(|detail| (StatusCode::SERVICE_UNAVAILABLE, detail))
}

fn validate_verification_plan_coverage(
    plan: &super::CommunityVerificationPlan,
    n_layers: usize,
) -> Result<(), String> {
    if plan.pipeline.is_empty() || plan.pipeline.len() != plan.replicas_per_range.len() {
        return Err("verification pipeline plan is empty or malformed".to_string());
    }
    let mut cursor = 0usize;
    for (((start, end), _), &replicas) in plan.pipeline.iter().zip(plan.replicas_per_range.iter()) {
        if *start != cursor || *end <= *start || *end > n_layers {
            return Err(format!(
                "verification pipeline range [{start}, {end}) does not contiguously cover 0..{n_layers}"
            ));
        }
        if replicas < super::COMMUNITY_VERIFICATION_SIGNATURES_REQUIRED {
            return Err(format!(
                "verification pipeline range [{start}, {end}) has {replicas} replica(s), requires at least {}",
                super::COMMUNITY_VERIFICATION_SIGNATURES_REQUIRED
            ));
        }
        cursor = *end;
    }
    if cursor != n_layers {
        return Err(format!(
            "verification pipeline ends at layer {cursor}, expected {n_layers}"
        ));
    }
    Ok(())
}

async fn wait_for_session_outcome(
    node: &super::NodeState,
    session_id: SessionId,
    monotonic_deadline: tokio::time::Instant,
) -> Result<SessionOutcome, (StatusCode, String)> {
    loop {
        let notified = node.community_shard_notify.notified();
        tokio::pin!(notified);
        let _ = notified.as_mut().enable();

        let now_ms = now_unix_ms()?;
        let next_wall_deadline_ms = {
            let mut guard = node.community_shard_coordinator.lock();
            let coordinator = guard.as_mut().ok_or_else(|| {
                (
                    StatusCode::SERVICE_UNAVAILABLE,
                    "community shard coordinator is disabled".to_string(),
                )
            })?;
            coordinator.expire_stale(now_ms);
            if let Some(outcome) = coordinator.take_result(session_id) {
                return Ok(outcome);
            }
            let Some(status) = coordinator.session_status(session_id) else {
                return Err((
                    StatusCode::SERVICE_UNAVAILABLE,
                    "community shard session closed without a retained outcome".to_string(),
                ));
            };
            let lease_deadline = coordinator
                .worker_lease_snapshot(status.expected_worker)
                .map(|lease| lease.deadline_unix_ms)
                .unwrap_or(status.deadline_unix_ms);
            status.deadline_unix_ms.min(lease_deadline)
        };

        let remaining_monotonic =
            monotonic_deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining_monotonic.is_zero() {
            let final_now_ms = now_unix_ms()?;
            let mut guard = node.community_shard_coordinator.lock();
            if let Some(coordinator) = guard.as_mut() {
                coordinator.expire_stale(final_now_ms);
                if let Some(outcome) = coordinator.take_result(session_id) {
                    return Ok(outcome);
                }
            }
            return Err((
                StatusCode::GATEWAY_TIMEOUT,
                "community shard inference session timed out".to_string(),
            ));
        }

        let wall_hint = Duration::from_millis(
            next_wall_deadline_ms
                .saturating_sub(now_ms)
                .max(COMMUNITY_SHARD_WAIT_MIN_SLICE_MS),
        );
        let wait_slice = remaining_monotonic
            .min(wall_hint)
            .min(COMMUNITY_SHARD_WAIT_RECHECK_INTERVAL);
        let _ = tokio::time::timeout(wait_slice, notified).await;
    }
}

fn validate_pending_result(
    pending: &PendingVerificationResult,
    expected_session_id: SessionId,
    expected_model_id: Hash256,
    expected_prompt_tokens: &[u32],
    max_tokens: u32,
    model: &CachedIntegerModel,
) -> Result<(), (StatusCode, String)> {
    if pending.session_id != expected_session_id {
        return Err((
            StatusCode::BAD_GATEWAY,
            "community shard result session_id does not match owned session".to_string(),
        ));
    }
    if pending.model_id != expected_model_id {
        return Err((
            StatusCode::BAD_GATEWAY,
            "community shard result model_id does not match coordinator model".to_string(),
        ));
    }
    if pending.execution_profile != CANONICAL_REWARD_INFERENCE_PROFILE {
        return Err((
            StatusCode::BAD_GATEWAY,
            format!(
                "community shard result execution_profile {:?} differs from required {:?}",
                pending.execution_profile, CANONICAL_REWARD_INFERENCE_PROFILE
            ),
        ));
    }
    if pending.prompt_tokens != expected_prompt_tokens {
        return Err((
            StatusCode::BAD_GATEWAY,
            "community shard result prompt_tokens do not match encoded request prompt".to_string(),
        ));
    }
    if !pending.verification_required {
        return Err((
            StatusCode::BAD_GATEWAY,
            "community shard result must require independent validator verification".to_string(),
        ));
    }
    if pending.reward_eligible {
        return Err((
            StatusCode::BAD_GATEWAY,
            "community shard result unexpectedly marked reward_eligible".to_string(),
        ));
    }
    let expected_unverified_digest = unverified_tokens_digest(
        pending.model_id,
        &pending.execution_profile,
        &pending.prompt_tokens,
        &pending.unverified_generated_tokens,
    );
    if pending.unverified_output_digest == Hash256::ZERO
        || pending.unverified_output_digest != expected_unverified_digest
    {
        return Err((
            StatusCode::BAD_GATEWAY,
            "community shard result unverified_output_digest does not match unverified_tokens_digest"
                .to_string(),
        ));
    }
    for &token in &pending.unverified_generated_tokens {
        if (token as usize) >= model.config.vocab_size {
            return Err((
                StatusCode::BAD_GATEWAY,
                format!(
                    "community shard result token {token} out of range for vocab_size {}",
                    model.config.vocab_size
                ),
            ));
        }
    }
    validate_pending_eos_and_logits(pending, max_tokens, &model.config.eos_tokens)?;
    if pending.terminal_logits_hashes.contains(&Hash256::ZERO) {
        return Err((
            StatusCode::BAD_GATEWAY,
            "community shard result contains a zero terminal logits hash".to_string(),
        ));
    }
    validate_pending_participants(
        &pending.participants,
        expected_session_id,
        model.config.n_layers,
    )
}

fn validate_pending_eos_and_logits(
    pending: &PendingVerificationResult,
    max_tokens: u32,
    eos_tokens: &[u32],
) -> Result<(), (StatusCode, String)> {
    let max_tokens_cap = effective_max_tokens_cap();
    if max_tokens == 0 || max_tokens > max_tokens_cap {
        return Err((
            StatusCode::BAD_REQUEST,
            format!("max_tokens must be in 1..={max_tokens_cap}, got {max_tokens}"),
        ));
    }
    let generated = &pending.unverified_generated_tokens;
    if generated.is_empty()
        || generated.len() > max_tokens as usize
        || pending.terminal_logits_hashes.len() != generated.len()
    {
        return Err((StatusCode::BAD_GATEWAY,
            "community shard result must contain one logits hash per generated token within the request limit".into()));
    }
    let first_eos = generated
        .iter()
        .position(|token| eos_tokens.contains(token));
    let valid_stop = if pending.stopped_by_eos {
        first_eos == Some(generated.len() - 1)
    } else {
        first_eos.is_none() && generated.len() == max_tokens as usize
    };
    if !valid_stop {
        return Err((StatusCode::BAD_GATEWAY,
            "community shard result must retain its terminal EOS token or fill max_tokens without EOS".into()));
    }
    Ok(())
}

fn validate_pending_participants(
    participants: &[SessionParticipantRecord],
    expected_session_id: SessionId,
    n_layers: usize,
) -> Result<(), (StatusCode, String)> {
    if n_layers < 2 {
        return Err((
            StatusCode::SERVICE_UNAVAILABLE,
            format!("community shard dispatch requires at least 2 layers, model has {n_layers}"),
        ));
    }
    if participants.len() < 2 {
        return Err((
            StatusCode::BAD_GATEWAY,
            format!(
                "community shard session requires at least 2 partial participants, got {}",
                participants.len()
            ),
        ));
    }
    let mut seen_workers = HashSet::with_capacity(participants.len());
    let mut seen_leases: Vec<LeaseId> = Vec::with_capacity(participants.len());
    let mut cursor = 0usize;
    for participant in participants {
        if participant.worker_id == Hash256::ZERO {
            return Err((
                StatusCode::BAD_GATEWAY,
                "community shard participant has zero worker_id".to_string(),
            ));
        }
        if !seen_workers.insert(participant.worker_id) {
            return Err((
                StatusCode::BAD_GATEWAY,
                format!(
                    "community shard session duplicated participant worker 0x{}",
                    participant.worker_id.to_hex()
                ),
            ));
        }
        if participant.lease_id.boot_nonce != expected_session_id.boot_nonce
            || participant.lease_id.seq == 0
        {
            return Err((
                StatusCode::BAD_GATEWAY,
                "community shard participant lease_id is invalid for session".to_string(),
            ));
        }
        if participant.lease_epoch == 0 {
            return Err((
                StatusCode::BAD_GATEWAY,
                "community shard participant has zero lease_epoch".to_string(),
            ));
        }
        if seen_leases.contains(&participant.lease_id) {
            return Err((
                StatusCode::BAD_GATEWAY,
                "community shard session duplicated participant lease_id".to_string(),
            ));
        }
        seen_leases.push(participant.lease_id);
        let range = participant.range;
        if !range.is_valid_for(n_layers) || range.start_layer != cursor {
            return Err((
                StatusCode::BAD_GATEWAY,
                format!(
                    "community shard participant range [{}, {}) breaks contiguous coverage of 0..{n_layers} at layer {cursor}",
                    range.start_layer, range.end_layer
                ),
            ));
        }
        if range.len() >= n_layers {
            return Err((
                StatusCode::BAD_GATEWAY,
                format!(
                    "community shard participant range [{}, {}) is not a partial range of 0..{n_layers}",
                    range.start_layer, range.end_layer
                ),
            ));
        }
        cursor = range.end_layer;
    }
    if cursor != n_layers {
        return Err((
            StatusCode::BAD_GATEWAY,
            format!(
                "community shard participants cover 0..{cursor}, expected full coverage 0..{n_layers}"
            ),
        ));
    }
    Ok(())
}

fn raw_token_output_hash(tokens: &[u32]) -> Hash256 {
    let output_bytes: Vec<u8> = tokens
        .iter()
        .flat_map(|token| token.to_le_bytes())
        .collect();
    arc_crypto::hash_bytes(&output_bytes)
}

fn now_unix_ms() -> Result<u64, (StatusCode, String)> {
    super::community_unix_ms().map_err(|error| {
        (
            StatusCode::SERVICE_UNAVAILABLE,
            format!("cannot read coordinator clock: {error}"),
        )
    })
}

fn map_abort_reason(reason: &SessionAbortReason) -> (StatusCode, String) {
    match reason {
        SessionAbortReason::JobDeadlineExceeded => (
            StatusCode::GATEWAY_TIMEOUT,
            "community shard session aborted: job deadline exceeded".to_string(),
        ),
        SessionAbortReason::StepDeadlineExceeded { worker_id } => (
            StatusCode::GATEWAY_TIMEOUT,
            format!(
                "community shard session aborted: step deadline exceeded for worker 0x{}",
                worker_id.to_hex()
            ),
        ),
        SessionAbortReason::CallerCancelled => (
            StatusCode::SERVICE_UNAVAILABLE,
            "community shard session aborted: caller cancelled".to_string(),
        ),
        SessionAbortReason::CompletedEarlyEos => (
            StatusCode::SERVICE_UNAVAILABLE,
            "community shard session aborted unexpectedly".to_string(),
        ),
        SessionAbortReason::WorkerReleased { worker_id } => (
            StatusCode::SERVICE_UNAVAILABLE,
            format!(
                "community shard session aborted: worker 0x{} released lease",
                worker_id.to_hex()
            ),
        ),
        SessionAbortReason::WorkerLeaseExpired { worker_id } => (
            StatusCode::SERVICE_UNAVAILABLE,
            format!(
                "community shard session aborted: worker 0x{} lease expired",
                worker_id.to_hex()
            ),
        ),
        SessionAbortReason::WorkerQuarantined { worker_id } => (
            StatusCode::SERVICE_UNAVAILABLE,
            format!(
                "community shard session aborted: worker 0x{} quarantined",
                worker_id.to_hex()
            ),
        ),
        SessionAbortReason::WorkerDeclined { worker_id, detail } => (
            StatusCode::SERVICE_UNAVAILABLE,
            format!(
                "community shard session aborted: worker 0x{} declined step ({detail})",
                worker_id.to_hex()
            ),
        ),
        SessionAbortReason::WorkerFailed { worker_id, detail } => (
            StatusCode::SERVICE_UNAVAILABLE,
            format!(
                "community shard session aborted: worker 0x{} failed step ({detail})",
                worker_id.to_hex()
            ),
        ),
    }
}

fn map_open_session_error(error: ShardCoordinatorError) -> (StatusCode, String) {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::community_sharding::{
        CommunityShardLimits, HeartbeatDirective, ShardClaimStepRequest, ShardClaimStepResponse,
        ShardHeartbeatRequest, ShardLeaseAssignment, ShardLeaseRequest, ShardStepInput,
        ShardStepOutcome, ShardStepOutput, ShardStepSubmitRequest, hidden_state_hash,
    };
    use crate::rpc::community_shard_http::tests::{
        build_tiny_model, register_worker_signed, slice_partial_model,
    };
    use crate::rpc::community_shard_http::{
        COMMUNITY_SHARD_CAPABILITY, COMMUNITY_SHARD_CLAIM_STEP_PATH,
        COMMUNITY_SHARD_HEARTBEAT_PATH, COMMUNITY_SHARD_LEASE_PATH, COMMUNITY_SHARD_STATUS_PATH,
        COMMUNITY_SHARD_SUBMIT_STEP_PATH, CommunityShardStatusResponse,
        community_shard_claim_step_signed, community_shard_heartbeat_signed,
        community_shard_lease_signed, community_shard_status, community_shard_submit_step_signed,
        init_community_shard_coordinator_with_limits, sign_shard_worker_request,
    };
    use crate::rpc::tests::{
        canonical_profile, fake_node_with_workers, recovered_six_validator_state,
    };
    use crate::rpc::{
        COMMUNITY_VERIFICATION_SIGNATURES_REQUIRED, NodeState, ShardInfo,
        build_community_verification_work_item, inference_cleanup_shard, inference_forward_shard,
        model_display_name, network_info,
    };
    use arc_crypto::KeyPair;
    use arc_inference::cached_integer_model::{KVCache, ShardInput, ShardOutput};
    use axum::routing::get;
    use serde_json::Value;
    use std::collections::BTreeMap;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicBool, Ordering};

    struct SignedPartialWorker {
        keypair: Arc<KeyPair>,
        worker_id: Hash256,
        assignment: ShardLeaseAssignment,
        partial_model: Arc<CachedIntegerModel>,
    }

    struct VerifiedDispatchFixture {
        coordinator: NodeState,
        model: Arc<CachedIntegerModel>,
        model_id: Hash256,
        workers: Vec<SignedPartialWorker>,
        validator_handles: Vec<tokio::task::JoinHandle<()>>,
        temporary: PathBuf,
    }

    impl Drop for VerifiedDispatchFixture {
        fn drop(&mut self) {
            for handle in &self.validator_handles {
                handle.abort();
            }
            let _ = std::fs::remove_dir_all(&self.temporary);
        }
    }

    fn build_dispatch_test_model(eos_tokens: Option<Vec<u32>>) -> CachedIntegerModel {
        let mut model = build_tiny_model(4);
        model.vocab[3] = "▁ARC".to_string();
        model.vocab[4] = "ARC".to_string();
        model.vocab[5] = "▁ok".to_string();
        model.vocab[6] = "ok".to_string();
        if let Some(eos) = eos_tokens {
            model.config.eos_tokens = eos;
        }
        assert!(!model.encode("ARC").is_empty());
        model
    }

    async fn build_verified_fixture(
        replicas_range_0: usize,
        replicas_range_1: usize,
        limits: CommunityShardLimits,
        eos_tokens: Option<Vec<u32>>,
        attach_two_workers: bool,
    ) -> VerifiedDispatchFixture {
        let model = Arc::new(build_dispatch_test_model(eos_tokens));
        assert_eq!(model.config.n_layers, 4);
        assert!(model.has_canonical_i8_profile());
        let model_id = arc_crypto::hash_bytes(b"community-shard-dispatch-4layer-model");
        let (validator_keys, recovered, temporary) = recovered_six_validator_state();
        let active = recovered.active_validators();

        let mut validator_handles = Vec::new();
        let mut range0_replicas = Vec::new();
        let mut range1_replicas = Vec::new();

        for (index, key) in validator_keys.iter().take(replicas_range_0).enumerate() {
            let mut shard_node = fake_node_with_workers(Vec::new());
            shard_node.state = recovered.clone();
            shard_node.validator_address = key.address();
            shard_node.validator_keypair = Some(Arc::new(key.clone()));
            shard_node.inference_model = Some(model.clone());
            shard_node.model_artifact_id = Some(model_id);
            *shard_node.dag_validators.write() = active.clone();
            let info = ShardInfo {
                start_layer: 0,
                end_layer: 2,
                total_layers: 4,
                model_id: format!("0x{}", model_id.to_hex()),
                model_name: model_display_name(&model),
                execution_profile: canonical_profile(),
                memory_mb: 1,
                full_model_mb: 1,
                socket_addr: String::new(),
                node_name: format!("validator-range0-{index}"),
            };
            shard_node.shard_infos = vec![info.clone()];
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let origin = format!("http://{}", listener.local_addr().unwrap());
            let app = Router::new()
                .route("/network/info", get(network_info))
                .route("/inference/forward_shard", post(inference_forward_shard))
                .route("/inference/cleanup_shard", post(inference_cleanup_shard))
                .with_state(shard_node);
            validator_handles.push(tokio::spawn(async move {
                axum::serve(listener, app).await.unwrap();
            }));
            range0_replicas.push(ShardInfo {
                socket_addr: origin,
                ..info
            });
        }

        for (index, key) in validator_keys
            .iter()
            .skip(2)
            .take(replicas_range_1)
            .enumerate()
        {
            let mut shard_node = fake_node_with_workers(Vec::new());
            shard_node.state = recovered.clone();
            shard_node.validator_address = key.address();
            shard_node.validator_keypair = Some(Arc::new(key.clone()));
            shard_node.inference_model = Some(model.clone());
            shard_node.model_artifact_id = Some(model_id);
            *shard_node.dag_validators.write() = active.clone();
            let info = ShardInfo {
                start_layer: 2,
                end_layer: 4,
                total_layers: 4,
                model_id: format!("0x{}", model_id.to_hex()),
                model_name: model_display_name(&model),
                execution_profile: canonical_profile(),
                memory_mb: 1,
                full_model_mb: 1,
                socket_addr: String::new(),
                node_name: format!("validator-range1-{index}"),
            };
            shard_node.shard_infos = vec![info.clone()];
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let origin = format!("http://{}", listener.local_addr().unwrap());
            let app = Router::new()
                .route("/network/info", get(network_info))
                .route("/inference/forward_shard", post(inference_forward_shard))
                .route("/inference/cleanup_shard", post(inference_cleanup_shard))
                .with_state(shard_node);
            validator_handles.push(tokio::spawn(async move {
                axum::serve(listener, app).await.unwrap();
            }));
            range1_replicas.push(ShardInfo {
                socket_addr: origin,
                ..info
            });
        }

        let mut coordinator = fake_node_with_workers(Vec::new());
        coordinator.state = recovered.clone();
        coordinator.validator_address = validator_keys[4].address();
        coordinator.validator_keypair = Some(Arc::new(validator_keys[4].clone()));
        coordinator.inference_model = Some(model.clone());
        coordinator.model_artifact_id = Some(model_id);
        *coordinator.dag_validators.write() = active;
        if !range0_replicas.is_empty() || !range1_replicas.is_empty() {
            let mut pipeline = Vec::new();
            if !range0_replicas.is_empty() {
                pipeline.push(((0, 2), range0_replicas));
            }
            if !range1_replicas.is_empty() {
                pipeline.push(((2, 4), range1_replicas));
            }
            coordinator.community_verification_pipeline_override = Some(pipeline);
        }

        let coordinator_limits = CommunityShardLimits {
            target_partitions: 2,
            ..limits
        };
        init_community_shard_coordinator_with_limits(&coordinator, coordinator_limits).unwrap();

        let mut workers = Vec::new();
        if attach_two_workers {
            let domain = coordinator.state.transaction_domain_hash();
            for _ in 0..2 {
                let keypair = Arc::new(KeyPair::generate_ed25519());
                let worker_id = keypair.address();
                register_worker_signed(
                    &coordinator,
                    &keypair,
                    model_id,
                    vec![COMMUNITY_SHARD_CAPABILITY],
                )
                .await
                .unwrap();
                let lease_signed = sign_shard_worker_request(
                    COMMUNITY_SHARD_LEASE_PATH,
                    ShardLeaseRequest {
                        worker_id,
                        protocol_version: COMMUNITY_SHARD_PROTOCOL_VERSION,
                        model_id,
                        execution_profile: CANONICAL_REWARD_INFERENCE_PROFILE.to_string(),
                        n_layers: model.config.n_layers,
                        d_model: model.config.d_model,
                        vocab_size: model.config.vocab_size,
                        max_seq: model.config.max_seq,
                        max_resident_layers: 2,
                    },
                    &keypair,
                    coordinator.validator_address,
                    domain,
                )
                .unwrap();
                let assignment = community_shard_lease_signed(
                    AxumState(coordinator.clone()),
                    Json(lease_signed),
                )
                .await
                .unwrap()
                .0
                .assignment;
                let partial_model = Arc::new(slice_partial_model(&model, assignment.range));
                assert!(!partial_model.has_all_transformer_layers());
                let hb_signed = sign_shard_worker_request(
                    COMMUNITY_SHARD_HEARTBEAT_PATH,
                    ShardHeartbeatRequest {
                        worker_id,
                        lease_id: assignment.lease_id,
                        lease_epoch: assignment.lease_epoch,
                        model_id,
                        execution_profile: CANONICAL_REWARD_INFERENCE_PROFILE.to_string(),
                        loaded_range: assignment.range,
                        ready: true,
                    },
                    &keypair,
                    coordinator.validator_address,
                    domain,
                )
                .unwrap();
                let hb = community_shard_heartbeat_signed(
                    AxumState(coordinator.clone()),
                    Json(hb_signed),
                )
                .await
                .unwrap()
                .0;
                assert!(matches!(hb.directive, HeartbeatDirective::Active { .. }));
                workers.push(SignedPartialWorker {
                    keypair,
                    worker_id,
                    assignment,
                    partial_model,
                });
            }
            assert_ne!(workers[0].assignment.range, workers[1].assignment.range);
            let now_ms = crate::rpc::community_unix_ms().unwrap();
            assert!(
                coordinator
                    .community_shard_coordinator
                    .lock()
                    .as_ref()
                    .unwrap()
                    .status_snapshot(now_ms)
                    .fully_covered_ready
            );
        }

        VerifiedDispatchFixture {
            coordinator,
            model,
            model_id,
            workers,
            validator_handles,
            temporary,
        }
    }

    #[derive(Clone, Copy)]
    enum WorkerBehavior {
        Honest,
        TamperTerminalToken,
        FailSecondHop,
    }

    fn spawn_partial_worker_driver(
        coordinator: NodeState,
        workers: Vec<SignedPartialWorker>,
        behavior: WorkerBehavior,
        via_signed_http: bool,
        stop: Arc<AtomicBool>,
    ) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            let domain = coordinator.state.transaction_domain_hash();
            let mut caches: BTreeMap<(u64, [u8; 32]), KVCache> = BTreeMap::new();
            while !stop.load(Ordering::Acquire) {
                let mut progressed = false;
                for worker in &workers {
                    let claim = if via_signed_http {
                        let signed = sign_shard_worker_request(
                            COMMUNITY_SHARD_CLAIM_STEP_PATH,
                            ShardClaimStepRequest {
                                worker_id: worker.worker_id,
                                lease_id: worker.assignment.lease_id,
                                lease_epoch: worker.assignment.lease_epoch,
                                model_id: worker.assignment.model_id,
                                execution_profile: worker.assignment.execution_profile.clone(),
                            },
                            &worker.keypair,
                            coordinator.validator_address,
                            domain,
                        )
                        .unwrap();
                        community_shard_claim_step_signed(
                            AxumState(coordinator.clone()),
                            Json(signed),
                        )
                        .await
                        .unwrap()
                        .0
                    } else {
                        let now_ms = crate::rpc::community_unix_ms().unwrap();
                        let mut guard = coordinator.community_shard_coordinator.lock();
                        let Some(core) = guard.as_mut() else {
                            return;
                        };
                        core.claim_step(
                            worker.worker_id,
                            now_ms,
                            ShardClaimStepRequest {
                                worker_id: worker.worker_id,
                                lease_id: worker.assignment.lease_id,
                                lease_epoch: worker.assignment.lease_epoch,
                                model_id: worker.assignment.model_id,
                                execution_profile: worker.assignment.execution_profile.clone(),
                            },
                        )
                        .unwrap()
                    };

                    if let ShardClaimStepResponse::Step { work } = claim {
                        progressed = true;
                        let outcome = match behavior {
                            WorkerBehavior::FailSecondHop if work.range.start_layer > 0 => {
                                ShardStepOutcome::Failed {
                                    reason: "simulated partial worker failure".to_string(),
                                }
                            }
                            _ => {
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
                                    .entry((work.session_id.seq, worker.worker_id.0))
                                    .or_insert_with(|| {
                                        KVCache::new(worker.partial_model.config.n_layers)
                                    });
                                let shard_out = worker
                                    .partial_model
                                    .forward_shard_token_with_history(
                                        input,
                                        cache,
                                        work.range.start_layer,
                                        work.range.end_layer,
                                        work.position,
                                        &work.generated_tokens,
                                    )
                                    .unwrap();
                                if work.last_token {
                                    caches.remove(&(work.session_id.seq, worker.worker_id.0));
                                }
                                let mut output = match shard_out {
                                    ShardOutput::Hidden(hidden) => ShardStepOutput::Hidden {
                                        hidden_hash: hidden_state_hash(&hidden),
                                        hidden,
                                    },
                                    ShardOutput::Token { id, logits_hash } => {
                                        ShardStepOutput::Token {
                                            token_id: id,
                                            logits_hash,
                                        }
                                    }
                                };
                                if matches!(behavior, WorkerBehavior::TamperTerminalToken)
                                    && work.expect_terminal
                                    && let ShardStepOutput::Token {
                                        ref mut token_id, ..
                                    } = output
                                {
                                    let original = *token_id;
                                    *token_id = (0..worker.partial_model.config.vocab_size as u32)
                                        .find(|&candidate| {
                                            candidate != original
                                                && !worker
                                                    .partial_model
                                                    .config
                                                    .eos_tokens
                                                    .contains(&candidate)
                                        })
                                        .expect("alternative non-EOS token");
                                }
                                ShardStepOutcome::Completed { output }
                            }
                        };

                        let submit_req = ShardStepSubmitRequest {
                            worker_id: worker.worker_id,
                            lease_id: work.lease_id,
                            lease_epoch: work.lease_epoch,
                            session_id: work.session_id,
                            step_id: work.step_id,
                            model_id: work.model_id,
                            execution_profile: work.execution_profile.clone(),
                            range: work.range,
                            position: work.position,
                            layers_processed: work.range.len(),
                            outcome,
                        };
                        if via_signed_http {
                            let signed = sign_shard_worker_request(
                                COMMUNITY_SHARD_SUBMIT_STEP_PATH,
                                submit_req,
                                &worker.keypair,
                                coordinator.validator_address,
                                domain,
                            )
                            .unwrap();
                            let resp = community_shard_submit_step_signed(
                                AxumState(coordinator.clone()),
                                Json(signed),
                            )
                            .await
                            .unwrap()
                            .0;
                            assert!(resp.accepted);
                        } else {
                            let submit_now_ms = crate::rpc::community_unix_ms().unwrap();
                            let mut guard = coordinator.community_shard_coordinator.lock();
                            let core = guard.as_mut().unwrap();
                            let resp = core
                                .submit_step(worker.worker_id, submit_now_ms, submit_req)
                                .unwrap();
                            assert!(resp.accepted);
                        }
                    }
                }
                if !progressed {
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
            }
        })
    }

    fn clone_workers(workers: &[SignedPartialWorker]) -> Vec<SignedPartialWorker> {
        workers
            .iter()
            .map(|worker| SignedPartialWorker {
                keypair: worker.keypair.clone(),
                worker_id: worker.worker_id,
                assignment: worker.assignment.clone(),
                partial_model: worker.partial_model.clone(),
            })
            .collect()
    }

    #[tokio::test]
    async fn valid_two_partial_worker_session_returns_verified_answer() {
        let fixture =
            build_verified_fixture(2, 2, CommunityShardLimits::default(), None, true).await;
        assert!(
            fixture
                .coordinator
                .state
                .transaction_domain_hash()
                .is_some()
        );

        let Json(status): Json<CommunityShardStatusResponse> =
            community_shard_status(AxumState(fixture.coordinator.clone()))
                .await
                .unwrap();
        assert!(status.enabled);
        assert!(status.verified_dispatch_enabled);
        assert!(!status.rewards_enabled);

        let prompt = "ARC";
        let max_tokens = 3u32;
        let prompt_tokens = fixture.model.encode(prompt);
        assert!(!prompt_tokens.is_empty());
        let (expected_tokens, expected_hash) = fixture
            .model
            .try_generate(&prompt_tokens, max_tokens, &fixture.model.config.eos_tokens)
            .unwrap();
        assert_eq!(expected_tokens.len(), max_tokens as usize);

        let stop = Arc::new(AtomicBool::new(false));
        let driver = spawn_partial_worker_driver(
            fixture.coordinator.clone(),
            clone_workers(&fixture.workers),
            WorkerBehavior::Honest,
            true,
            stop.clone(),
        );

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let app = routes()
            .route(COMMUNITY_SHARD_STATUS_PATH, get(community_shard_status))
            .with_state(fixture.coordinator.clone());
        let server = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        let client = reqwest::Client::new();
        let resp = client
            .post(format!("http://{addr}{COMMUNITY_SHARD_DISPATCH_PATH}"))
            .json(&CommunityShardedInferenceRequest {
                input: prompt.to_string(),
                max_tokens,
            })
            .send()
            .await
            .unwrap();
        let http_status = resp.status();
        let body = resp.text().await.unwrap();
        stop.store(true, Ordering::Release);
        driver.await.unwrap();
        server.abort();

        assert_eq!(http_status, StatusCode::OK, "unexpected error body: {body}");
        let json: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(json["protocol_version"], COMMUNITY_SHARD_PROTOCOL_VERSION);
        assert_eq!(json["model_id"], format!("0x{}", fixture.model_id.to_hex()));
        assert_eq!(
            json["execution_profile"],
            CANONICAL_REWARD_INFERENCE_PROFILE
        );
        assert_eq!(
            json["generated_tokens"],
            serde_json::to_value(&expected_tokens).unwrap()
        );
        assert_eq!(json["tokens_generated"], expected_tokens.len());
        assert_eq!(json["output"], fixture.model.decode(&expected_tokens));
        assert_eq!(json["output_hash"], format!("0x{}", expected_hash.to_hex()));
        assert_eq!(json["stopped_by_eos"], false);
        assert_eq!(json["rewards_enabled"], false);
        assert_eq!(json["reward_eligible"], false);
        assert_eq!(
            json["participant_identity_basis"],
            COMMUNITY_SHARD_PARTICIPANT_IDENTITY_BASIS
        );
        assert_eq!(json["participants"].as_array().unwrap().len(), 2);
        assert_eq!(json["verification"]["quorum_verified"], true);
        assert_eq!(json["verification"]["profile_bound"], true);
        assert_eq!(json["verification"]["ranges"], 2);
        assert_eq!(
            json["verification"]["execution_profile"],
            CANONICAL_REWARD_INFERENCE_PROFILE
        );
        assert_eq!(
            json["verification"]["signatures_required_per_quorum"],
            COMMUNITY_VERIFICATION_SIGNATURES_REQUIRED
        );
        assert_eq!(
            json["verification"]["output_hash"],
            format!("0x{}", expected_hash.to_hex())
        );

        let now_ms = crate::rpc::community_unix_ms().unwrap();
        let snap = fixture
            .coordinator
            .community_shard_coordinator
            .lock()
            .as_ref()
            .unwrap()
            .status_snapshot(now_ms);
        assert_eq!(snap.active_sessions, 0);
        assert_eq!(snap.retained_results, 0);
    }

    #[tokio::test]
    async fn outbound_workers_and_public_dispatch_return_independently_verified_answer() {
        use crate::community_shard_worker::{
            CommunityShardWorkerConfig, PartialModelLoader, run_community_shard_worker,
        };
        let fixture =
            build_verified_fixture(2, 2, CommunityShardLimits::default(), None, false).await;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        let app = routes()
            .merge(crate::rpc::community_shard_http::routes())
            .route(
                crate::rpc::COMMUNITY_REGISTER_PATH,
                post(crate::rpc::community_register_signed),
            )
            .with_state(fixture.coordinator.clone());
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let spec = fixture
            .coordinator
            .community_shard_coordinator
            .lock()
            .as_ref()
            .unwrap()
            .model_spec()
            .clone();
        let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
        let mut workers = tokio::task::JoinSet::new();
        for index in 0..2 {
            let full = Arc::clone(&fixture.model);
            let loader: PartialModelLoader = Arc::new(move |assignment| {
                Ok(Arc::new(slice_partial_model(&full, assignment.range)))
            });
            let mut config = CommunityShardWorkerConfig::new(
                origin.clone(),
                fixture.coordinator.validator_address,
                fixture.coordinator.state.transaction_domain_hash(),
                2,
                format!("end-to-end-{index}"),
                model_display_name(&fixture.model),
                "test-loopback",
            );
            config.allow_insecure_for_loopback = true;
            config.idle_poll_interval_ms = 20;
            workers.spawn(run_community_shard_worker(
                config,
                Arc::new(KeyPair::generate_ed25519()),
                spec.clone(),
                loader,
                shutdown_rx.clone(),
            ));
        }
        let ready_deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        loop {
            let notified = fixture.coordinator.community_shard_notify.notified();
            let ready = fixture
                .coordinator
                .community_shard_coordinator
                .lock()
                .as_ref()
                .unwrap()
                .status_snapshot(now_unix_ms().unwrap())
                .fully_covered_ready;
            if ready {
                break;
            }
            assert!(
                tokio::time::Instant::now() < ready_deadline,
                "outbound workers must become ready"
            );
            let _ = tokio::time::timeout(Duration::from_millis(50), notified).await;
        }
        let request = CommunityShardedInferenceRequest {
            input: "ARC".into(),
            max_tokens: 3,
        };
        let (canonical, canonical_hash) = fixture
            .model
            .try_generate(
                &fixture.model.encode(&request.input),
                request.max_tokens,
                &fixture.model.config.eos_tokens,
            )
            .unwrap();
        let reply: Result<(StatusCode, Value), reqwest::Error> = async {
            let response = reqwest::Client::new()
                .post(format!("{origin}{COMMUNITY_SHARD_DISPATCH_PATH}"))
                .timeout(Duration::from_secs(30))
                .json(&request)
                .send()
                .await?;
            let status = response.status();
            Ok((status, response.json().await?))
        }
        .await;
        shutdown_tx.send(true).unwrap();
        while let Some(worker) = workers.join_next().await {
            let summary = worker.unwrap().unwrap();
            assert!(summary.steps_executed > 0);
            assert!(summary.clean_release_sent);
        }
        server.abort();
        let (status, body) = reply.unwrap();
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["generated_tokens"], serde_json::json!(canonical));
        assert_eq!(
            body["output_hash"],
            format!("0x{}", canonical_hash.to_hex())
        );
        assert_eq!(body["verification"]["quorum_verified"], true);
        assert_eq!(body["participants"].as_array().unwrap().len(), 2);
        assert_eq!(body["rewards_enabled"], false);
        assert_eq!(body["reward_eligible"], false);
    }

    #[tokio::test]
    async fn early_eos_two_partial_worker_session_returns_verified_answer() {
        let probe_model = build_dispatch_test_model(None);
        let prompt = "ARC";
        let max_tokens = 3u32;
        let prompt_tokens = probe_model.encode(prompt);
        let (full_tokens, _) = probe_model
            .try_generate(&prompt_tokens, max_tokens, &[])
            .unwrap();
        assert_eq!(full_tokens.len(), 3);
        let eos_token = if full_tokens[1] != full_tokens[0] {
            full_tokens[1]
        } else {
            full_tokens[0]
        };

        let fixture = build_verified_fixture(
            2,
            2,
            CommunityShardLimits::default(),
            Some(vec![eos_token]),
            true,
        )
        .await;
        let (expected_tokens, expected_hash) = fixture
            .model
            .try_generate(&prompt_tokens, max_tokens, &fixture.model.config.eos_tokens)
            .unwrap();
        assert!(expected_tokens.len() < max_tokens as usize);

        let stop = Arc::new(AtomicBool::new(false));
        let driver = spawn_partial_worker_driver(
            fixture.coordinator.clone(),
            clone_workers(&fixture.workers),
            WorkerBehavior::Honest,
            true,
            stop.clone(),
        );

        let Json(response) = run_community_sharded(
            AxumState(fixture.coordinator.clone()),
            Json(CommunityShardedInferenceRequest {
                input: prompt.to_string(),
                max_tokens,
            }),
        )
        .await
        .expect("early-EOS two-partial-worker session must verify and succeed");
        stop.store(true, Ordering::Release);
        driver.await.unwrap();

        assert!(response.stopped_by_eos);
        assert_eq!(response.generated_tokens, expected_tokens);
        assert_eq!(response.tokens_generated, expected_tokens.len());
        assert_eq!(
            response.output_hash,
            format!("0x{}", expected_hash.to_hex())
        );
        assert!(response.verification.quorum_verified);
        assert!(!response.rewards_enabled);
        assert!(!response.reward_eligible);
    }

    #[tokio::test]
    async fn tampered_generated_tokens_fail_verification_with_no_answer() {
        let fixture =
            build_verified_fixture(2, 2, CommunityShardLimits::default(), None, true).await;
        let stop = Arc::new(AtomicBool::new(false));
        let driver = spawn_partial_worker_driver(
            fixture.coordinator.clone(),
            clone_workers(&fixture.workers),
            WorkerBehavior::TamperTerminalToken,
            true,
            stop.clone(),
        );

        let result = run_community_sharded(
            AxumState(fixture.coordinator.clone()),
            Json(CommunityShardedInferenceRequest {
                input: "ARC".to_string(),
                max_tokens: 2,
            }),
        )
        .await;
        stop.store(true, Ordering::Release);
        driver.await.unwrap();

        let Err((status, message)) = result else {
            panic!("tampered community shard tokens must fail verification");
        };
        assert_eq!(status, StatusCode::BAD_GATEWAY);
        assert!(
            message.contains("validator quorum verification"),
            "unexpected error: {message}"
        );

        let now_ms = crate::rpc::community_unix_ms().unwrap();
        let snap = fixture
            .coordinator
            .community_shard_coordinator
            .lock()
            .as_ref()
            .unwrap()
            .status_snapshot(now_ms);
        assert_eq!(snap.active_sessions, 0);
        assert_eq!(snap.retained_results, 0);
    }

    #[tokio::test]
    async fn aborted_session_returns_no_answer() {
        let fixture =
            build_verified_fixture(2, 2, CommunityShardLimits::default(), None, true).await;
        let stop = Arc::new(AtomicBool::new(false));
        let driver = spawn_partial_worker_driver(
            fixture.coordinator.clone(),
            clone_workers(&fixture.workers),
            WorkerBehavior::FailSecondHop,
            true,
            stop.clone(),
        );

        let result = run_community_sharded(
            AxumState(fixture.coordinator.clone()),
            Json(CommunityShardedInferenceRequest {
                input: "ARC".to_string(),
                max_tokens: 2,
            }),
        )
        .await;
        stop.store(true, Ordering::Release);
        driver.await.unwrap();

        let Err((status, message)) = result else {
            panic!("aborted session must not return an answer");
        };
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert!(
            message.contains("aborted"),
            "unexpected abort error: {message}"
        );

        let now_ms = crate::rpc::community_unix_ms().unwrap();
        let snap = fixture
            .coordinator
            .community_shard_coordinator
            .lock()
            .as_ref()
            .unwrap()
            .status_snapshot(now_ms);
        assert_eq!(snap.active_sessions, 0);
        assert_eq!(snap.retained_results, 0);
    }

    #[tokio::test]
    async fn missing_verification_replicas_fails_before_opening_session() {
        for (r0, r1) in [(0usize, 0usize), (1usize, 1usize), (2usize, 1usize)] {
            let fixture =
                build_verified_fixture(r0, r1, CommunityShardLimits::default(), None, true).await;
            let Json(status): Json<CommunityShardStatusResponse> =
                community_shard_status(AxumState(fixture.coordinator.clone()))
                    .await
                    .unwrap();
            assert!(status.enabled);
            assert!(!status.verified_dispatch_enabled);
            assert!(!status.rewards_enabled);

            let result = run_community_sharded(
                AxumState(fixture.coordinator.clone()),
                Json(CommunityShardedInferenceRequest {
                    input: "ARC".to_string(),
                    max_tokens: 2,
                }),
            )
            .await;
            let Err((code, _)) = result else {
                panic!("dispatch must fail with ({r0}, {r1}) verification replicas");
            };
            assert_eq!(code, StatusCode::SERVICE_UNAVAILABLE);

            let now_ms = crate::rpc::community_unix_ms().unwrap();
            let mut guard = fixture.coordinator.community_shard_coordinator.lock();
            let core = guard.as_mut().unwrap();
            let snap = core.status_snapshot(now_ms);
            assert_eq!(snap.active_sessions, 0);
            assert_eq!(snap.retained_results, 0);
            for worker in &fixture.workers {
                let claim = core
                    .claim_step(
                        worker.worker_id,
                        now_ms,
                        ShardClaimStepRequest {
                            worker_id: worker.worker_id,
                            lease_id: worker.assignment.lease_id,
                            lease_epoch: worker.assignment.lease_epoch,
                            model_id: worker.assignment.model_id,
                            execution_profile: worker.assignment.execution_profile.clone(),
                        },
                    )
                    .unwrap();
                assert_eq!(claim, ShardClaimStepResponse::Idle);
            }
        }

        let fixture =
            build_verified_fixture(2, 2, CommunityShardLimits::default(), None, true).await;
        {
            let mut validators = fixture.coordinator.dag_validators.write();
            validators.truncate(1);
        }
        let Json(status): Json<CommunityShardStatusResponse> =
            community_shard_status(AxumState(fixture.coordinator.clone()))
                .await
                .unwrap();
        assert!(!status.verified_dispatch_enabled);
        let Err((code, _)) = run_community_sharded(
            AxumState(fixture.coordinator.clone()),
            Json(CommunityShardedInferenceRequest {
                input: "ARC".to_string(),
                max_tokens: 2,
            }),
        )
        .await
        else {
            panic!("dispatch must fail when fewer than 2 active validators exist");
        };
        assert_eq!(code, StatusCode::SERVICE_UNAVAILABLE);
        let now_ms = crate::rpc::community_unix_ms().unwrap();
        let snap = fixture
            .coordinator
            .community_shard_coordinator
            .lock()
            .as_ref()
            .unwrap()
            .status_snapshot(now_ms);
        assert_eq!(snap.active_sessions, 0);
        assert_eq!(snap.retained_results, 0);
    }

    #[tokio::test]
    async fn body_input_capacity_and_disabled_failures() {
        let disabled_node = fake_node_with_workers(Vec::new());
        let Json(disabled_status): Json<CommunityShardStatusResponse> =
            community_shard_status(AxumState(disabled_node.clone()))
                .await
                .unwrap();
        assert!(!disabled_status.enabled);
        assert!(!disabled_status.verified_dispatch_enabled);
        assert!(!disabled_status.rewards_enabled);
        assert!(disabled_status.status.is_none());

        let Err((code, _)) = run_community_sharded(
            AxumState(disabled_node),
            Json(CommunityShardedInferenceRequest {
                input: "ARC".to_string(),
                max_tokens: 2,
            }),
        )
        .await
        else {
            panic!("disabled coordinator must return 503");
        };
        assert_eq!(code, StatusCode::SERVICE_UNAVAILABLE);

        let fixture =
            build_verified_fixture(2, 2, CommunityShardLimits::default(), None, true).await;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let app = routes().with_state(fixture.coordinator.clone());
        let server = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        let client = reqwest::Client::new();
        let url = format!("http://{addr}{COMMUNITY_SHARD_DISPATCH_PATH}");

        let oversized_body = vec![b' '; COMMUNITY_SHARD_DISPATCH_BODY_LIMIT_BYTES + 1];
        let oversized_resp = client
            .post(&url)
            .header("content-type", "application/json")
            .body(oversized_body)
            .send()
            .await
            .unwrap();
        assert_eq!(oversized_resp.status(), StatusCode::PAYLOAD_TOO_LARGE);

        let unknown_resp = client
            .post(&url)
            .header("content-type", "application/json")
            .body("{\"input\":\"ARC\",\"max_tokens\":1,\"unknown\":true}")
            .send()
            .await
            .unwrap();
        assert_eq!(unknown_resp.status(), StatusCode::UNPROCESSABLE_ENTITY);

        let empty_resp = client
            .post(&url)
            .json(&CommunityShardedInferenceRequest {
                input: String::new(),
                max_tokens: 1,
            })
            .send()
            .await
            .unwrap();
        assert_eq!(empty_resp.status(), StatusCode::BAD_REQUEST);

        let over_prompt_resp = client
            .post(&url)
            .json(&CommunityShardedInferenceRequest {
                input: "a".repeat(COMMUNITY_SHARD_DISPATCH_MAX_PROMPT_BYTES + 1),
                max_tokens: 1,
            })
            .send()
            .await
            .unwrap();
        assert_eq!(over_prompt_resp.status(), StatusCode::BAD_REQUEST);

        let zero_tokens_resp = client
            .post(&url)
            .json(&CommunityShardedInferenceRequest {
                input: "ARC".to_string(),
                max_tokens: 0,
            })
            .send()
            .await
            .unwrap();
        assert_eq!(zero_tokens_resp.status(), StatusCode::BAD_REQUEST);

        let excess_tokens_resp = client
            .post(&url)
            .json(&CommunityShardedInferenceRequest {
                input: "ARC".to_string(),
                max_tokens: COMMUNITY_SHARD_DISPATCH_MAX_TOKENS + 1,
            })
            .send()
            .await
            .unwrap();
        assert_eq!(excess_tokens_resp.status(), StatusCode::BAD_REQUEST);

        let zero_encoded_prompt = "zzzz";
        if fixture.model.encode(zero_encoded_prompt).is_empty() {
            let zero_encoded_resp = client
                .post(&url)
                .json(&CommunityShardedInferenceRequest {
                    input: zero_encoded_prompt.to_string(),
                    max_tokens: 1,
                })
                .send()
                .await
                .unwrap();
            assert_eq!(zero_encoded_resp.status(), StatusCode::BAD_REQUEST);
        }

        let context_overflow_prompt = "ARC ".repeat(20);
        let context_overflow_resp = client
            .post(&url)
            .json(&CommunityShardedInferenceRequest {
                input: context_overflow_prompt,
                max_tokens: 55,
            })
            .send()
            .await
            .unwrap();
        assert_eq!(context_overflow_resp.status(), StatusCode::BAD_REQUEST);

        let mut held_permits = Vec::new();
        while let Ok(permit) = fixture
            .coordinator
            .public_inference_permits
            .clone()
            .try_acquire_owned()
        {
            held_permits.push(permit);
        }
        assert!(!held_permits.is_empty());

        let saturated_resp = client
            .post(&url)
            .json(&CommunityShardedInferenceRequest {
                input: "ARC".to_string(),
                max_tokens: 1,
            })
            .send()
            .await
            .unwrap();
        assert_eq!(saturated_resp.status(), StatusCode::TOO_MANY_REQUESTS);

        drop(held_permits);
        server.abort();
    }

    #[tokio::test]
    async fn dropped_caller_future_synchronously_cancels_session_and_clears_retained_result() {
        let fixture =
            build_verified_fixture(2, 2, CommunityShardLimits::default(), None, true).await;
        let permits_before = fixture
            .coordinator
            .public_inference_permits
            .available_permits();

        let mut fut = Box::pin(run_community_sharded(
            AxumState(fixture.coordinator.clone()),
            Json(CommunityShardedInferenceRequest {
                input: "ARC".to_string(),
                max_tokens: 2,
            }),
        ));
        let timed_out = tokio::time::timeout(Duration::from_millis(80), &mut fut).await;
        assert!(timed_out.is_err());

        let now_ms = crate::rpc::community_unix_ms().unwrap();
        {
            let guard = fixture.coordinator.community_shard_coordinator.lock();
            let snap = guard.as_ref().unwrap().status_snapshot(now_ms);
            assert_eq!(snap.active_sessions, 1);
            assert_eq!(snap.retained_results, 0);
        }

        drop(fut);

        assert_eq!(
            fixture
                .coordinator
                .public_inference_permits
                .available_permits(),
            permits_before
        );
        let cancel_now_ms = crate::rpc::community_unix_ms().unwrap();
        {
            let guard = fixture.coordinator.community_shard_coordinator.lock();
            let snap = guard.as_ref().unwrap().status_snapshot(cancel_now_ms);
            assert_eq!(snap.active_sessions, 0);
            assert_eq!(snap.retained_results, 0);
            assert!(snap.queued_cancellations >= 2);
        }

        let signed_claim = sign_shard_worker_request(
            COMMUNITY_SHARD_CLAIM_STEP_PATH,
            ShardClaimStepRequest {
                worker_id: fixture.workers[0].worker_id,
                lease_id: fixture.workers[0].assignment.lease_id,
                lease_epoch: fixture.workers[0].assignment.lease_epoch,
                model_id: fixture.workers[0].assignment.model_id,
                execution_profile: fixture.workers[0].assignment.execution_profile.clone(),
            },
            &fixture.workers[0].keypair,
            fixture.coordinator.validator_address,
            fixture.coordinator.state.transaction_domain_hash(),
        )
        .unwrap();
        let claim = community_shard_claim_step_signed(
            AxumState(fixture.coordinator.clone()),
            Json(signed_claim),
        )
        .await
        .unwrap()
        .0;
        match claim {
            ShardClaimStepResponse::CancelSessions { cancellations } => {
                assert!(
                    cancellations
                        .iter()
                        .any(|notice| notice.reason == SessionAbortReason::CallerCancelled)
                );
            }
            other => panic!("expected CancelSessions notice after caller drop, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn lost_notify_does_not_hang() {
        let limits = CommunityShardLimits {
            job_deadline_ms: 30_000,
            step_timeout_ms: 20_000,
            ..CommunityShardLimits::default()
        };
        let fixture = build_verified_fixture(2, 2, limits, None, true).await;

        let stop = Arc::new(AtomicBool::new(false));
        let driver = spawn_partial_worker_driver(
            fixture.coordinator.clone(),
            clone_workers(&fixture.workers),
            WorkerBehavior::Honest,
            false,
            stop.clone(),
        );

        let result = tokio::time::timeout(
            Duration::from_secs(5),
            run_community_sharded(
                AxumState(fixture.coordinator.clone()),
                Json(CommunityShardedInferenceRequest {
                    input: "ARC".to_string(),
                    max_tokens: 2,
                }),
            ),
        )
        .await;
        stop.store(true, Ordering::Release);
        driver.await.unwrap();

        let inner = result.expect("dispatch must complete without hanging when Notify is lost");
        let Json(response) = inner.expect("lost-notify session must still verify and succeed");
        assert!(response.verification.quorum_verified);
        assert!(!response.rewards_enabled);
        assert!(!response.reward_eligible);
    }

    #[test]
    fn verification_candidate_binds_node_transaction_domain() {
        let (_keys, recovered, temporary) = recovered_six_validator_state();
        let expected_domain = recovered
            .transaction_domain_hash()
            .expect("recovered state has transaction domain");

        let mut recovered_node = fake_node_with_workers(Vec::new());
        recovered_node.state = recovered;
        let model_id = arc_crypto::hash_bytes(b"domain-bind-model");
        let tokens = vec![3u32, 4u32];
        let candidate = super::super::CommunityVerificationCandidate {
            verification_id: "community-shard-domain-test".to_string(),
            input: "ARC",
            max_tokens: 2,
            model_id,
            execution_profile: CANONICAL_REWARD_INFERENCE_PROFILE,
            submitted_at_unix_ms: 1_000,
            expires_at_unix_ms: 2_000,
            candidate_tokens: &tokens,
            candidate_output: "ARC",
            candidate_output_hash: raw_token_output_hash(&tokens),
        };
        let recovered_item = build_community_verification_work_item(&recovered_node, &candidate);
        assert_eq!(
            recovered_item.transaction_domain.as_deref(),
            Some(format!("0x{}", expected_domain.to_hex()).as_str())
        );

        let fresh_node = fake_node_with_workers(Vec::new());
        let fresh_item = build_community_verification_work_item(&fresh_node, &candidate);
        assert_eq!(fresh_item.transaction_domain, None);

        let _ = std::fs::remove_dir_all(&temporary);
    }

    #[test]
    fn validate_pending_result_rejects_altered_digest_and_eos_or_token_anomalies() {
        let model = build_dispatch_test_model(Some(vec![2]));
        let boot_nonce = arc_crypto::hash_bytes(b"boot-nonce");
        let session_id = SessionId { boot_nonce, seq: 7 };
        let model_id = arc_crypto::hash_bytes(b"model-id");
        let prompt = model.encode("ARC");
        let generated = vec![3u32, 4u32];
        let expected_digest = unverified_tokens_digest(
            model_id,
            CANONICAL_REWARD_INFERENCE_PROFILE,
            &prompt,
            &generated,
        );
        let raw_hash = raw_token_output_hash(&generated);
        assert_ne!(expected_digest, raw_hash);

        let lease_0 = LeaseId { boot_nonce, seq: 1 };
        let lease_1 = LeaseId { boot_nonce, seq: 2 };
        let worker_0 = arc_crypto::hash_bytes(b"worker-0");
        let worker_1 = arc_crypto::hash_bytes(b"worker-1");

        let valid = PendingVerificationResult {
            session_id,
            model_id,
            execution_profile: CANONICAL_REWARD_INFERENCE_PROFILE.to_string(),
            prompt_tokens: prompt.clone(),
            unverified_generated_tokens: generated.clone(),
            unverified_output_digest: expected_digest,
            terminal_logits_hashes: vec![
                arc_crypto::hash_bytes(b"logits-0"),
                arc_crypto::hash_bytes(b"logits-1"),
            ],
            stopped_by_eos: false,
            participants: vec![
                SessionParticipantRecord {
                    worker_id: worker_0,
                    lease_id: lease_0,
                    lease_epoch: 1,
                    range: ShardLayerRange {
                        start_layer: 0,
                        end_layer: 2,
                    },
                },
                SessionParticipantRecord {
                    worker_id: worker_1,
                    lease_id: lease_1,
                    lease_epoch: 1,
                    range: ShardLayerRange {
                        start_layer: 2,
                        end_layer: 4,
                    },
                },
            ],
            verification_required: true,
            reward_eligible: false,
        };
        assert!(validate_pending_result(&valid, session_id, model_id, &prompt, 2, &model).is_ok());

        let mut raw_hash_as_digest = valid.clone();
        raw_hash_as_digest.unverified_output_digest = raw_hash;
        assert_eq!(
            validate_pending_result(
                &raw_hash_as_digest,
                session_id,
                model_id,
                &prompt,
                2,
                &model
            )
            .unwrap_err()
            .0,
            StatusCode::BAD_GATEWAY
        );

        let mut altered_nonzero_digest = valid.clone();
        altered_nonzero_digest.unverified_output_digest =
            arc_crypto::hash_bytes(b"tampered-nonzero-digest");
        assert_eq!(
            validate_pending_result(
                &altered_nonzero_digest,
                session_id,
                model_id,
                &prompt,
                2,
                &model
            )
            .unwrap_err()
            .0,
            StatusCode::BAD_GATEWAY
        );

        let mut zero_digest = valid.clone();
        zero_digest.unverified_output_digest = Hash256::ZERO;
        assert_eq!(
            validate_pending_result(&zero_digest, session_id, model_id, &prompt, 2, &model)
                .unwrap_err()
                .0,
            StatusCode::BAD_GATEWAY
        );

        let mut eos_without_flag = valid.clone();
        eos_without_flag.unverified_generated_tokens = vec![3, 2];
        eos_without_flag.unverified_output_digest = unverified_tokens_digest(
            eos_without_flag.model_id,
            &eos_without_flag.execution_profile,
            &eos_without_flag.prompt_tokens,
            &eos_without_flag.unverified_generated_tokens,
        );
        assert_eq!(
            validate_pending_result(&eos_without_flag, session_id, model_id, &prompt, 2, &model)
                .unwrap_err()
                .0,
            StatusCode::BAD_GATEWAY
        );

        let mut eos_followed_by_extra = valid.clone();
        eos_followed_by_extra.stopped_by_eos = true;
        eos_followed_by_extra.unverified_generated_tokens = vec![2, 3];
        eos_followed_by_extra.unverified_output_digest = unverified_tokens_digest(
            eos_followed_by_extra.model_id,
            &eos_followed_by_extra.execution_profile,
            &eos_followed_by_extra.prompt_tokens,
            &eos_followed_by_extra.unverified_generated_tokens,
        );
        assert_eq!(
            validate_pending_result(
                &eos_followed_by_extra,
                session_id,
                model_id,
                &prompt,
                3,
                &model
            )
            .unwrap_err()
            .0,
            StatusCode::BAD_GATEWAY
        );

        let mut duplicate_eos = valid.clone();
        duplicate_eos.stopped_by_eos = true;
        duplicate_eos.unverified_generated_tokens = vec![2, 2];
        duplicate_eos.unverified_output_digest = unverified_tokens_digest(
            duplicate_eos.model_id,
            &duplicate_eos.execution_profile,
            &duplicate_eos.prompt_tokens,
            &duplicate_eos.unverified_generated_tokens,
        );
        assert_eq!(
            validate_pending_result(&duplicate_eos, session_id, model_id, &prompt, 3, &model)
                .unwrap_err()
                .0,
            StatusCode::BAD_GATEWAY
        );

        let mut eos_after_full_non_eos = valid.clone();
        eos_after_full_non_eos.stopped_by_eos = true;
        assert_eq!(
            validate_pending_result(
                &eos_after_full_non_eos,
                session_id,
                model_id,
                &prompt,
                2,
                &model
            )
            .unwrap_err()
            .0,
            StatusCode::BAD_GATEWAY
        );

        let mut extra_token_over_max = valid.clone();
        extra_token_over_max.unverified_generated_tokens = vec![3, 4, 5];
        extra_token_over_max.unverified_output_digest = unverified_tokens_digest(
            extra_token_over_max.model_id,
            &extra_token_over_max.execution_profile,
            &extra_token_over_max.prompt_tokens,
            &extra_token_over_max.unverified_generated_tokens,
        );
        extra_token_over_max
            .terminal_logits_hashes
            .push(arc_crypto::hash_bytes(b"logits-2"));
        assert_eq!(
            validate_pending_result(
                &extra_token_over_max,
                session_id,
                model_id,
                &prompt,
                2,
                &model
            )
            .unwrap_err()
            .0,
            StatusCode::BAD_GATEWAY
        );

        let mut extra_logits_hash = valid.clone();
        extra_logits_hash
            .terminal_logits_hashes
            .push(arc_crypto::hash_bytes(b"logits-extra"));
        assert_eq!(
            validate_pending_result(&extra_logits_hash, session_id, model_id, &prompt, 2, &model)
                .unwrap_err()
                .0,
            StatusCode::BAD_GATEWAY
        );

        let mut early_eos_extra_logits = valid.clone();
        early_eos_extra_logits.stopped_by_eos = true;
        early_eos_extra_logits.unverified_generated_tokens = vec![3];
        early_eos_extra_logits.unverified_output_digest = unverified_tokens_digest(
            early_eos_extra_logits.model_id,
            &early_eos_extra_logits.execution_profile,
            &early_eos_extra_logits.prompt_tokens,
            &early_eos_extra_logits.unverified_generated_tokens,
        );
        early_eos_extra_logits.terminal_logits_hashes = vec![
            arc_crypto::hash_bytes(b"logits-0"),
            arc_crypto::hash_bytes(b"logits-1"),
        ];
        assert_eq!(
            validate_pending_result(
                &early_eos_extra_logits,
                session_id,
                model_id,
                &prompt,
                3,
                &model
            )
            .unwrap_err()
            .0,
            StatusCode::BAD_GATEWAY
        );

        let mut no_eos_model = build_dispatch_test_model(Some(Vec::new()));
        no_eos_model.config.eos_tokens.clear();
        let mut fake_eos_on_no_eos_model = valid.clone();
        fake_eos_on_no_eos_model.stopped_by_eos = true;
        fake_eos_on_no_eos_model.unverified_generated_tokens = vec![3];
        fake_eos_on_no_eos_model.unverified_output_digest = unverified_tokens_digest(
            fake_eos_on_no_eos_model.model_id,
            &fake_eos_on_no_eos_model.execution_profile,
            &fake_eos_on_no_eos_model.prompt_tokens,
            &fake_eos_on_no_eos_model.unverified_generated_tokens,
        );
        assert_eq!(
            validate_pending_result(
                &fake_eos_on_no_eos_model,
                session_id,
                model_id,
                &prompt,
                2,
                &no_eos_model
            )
            .unwrap_err()
            .0,
            StatusCode::BAD_GATEWAY
        );

        let mut single_full_range = valid.clone();
        single_full_range.participants = vec![SessionParticipantRecord {
            worker_id: worker_0,
            lease_id: lease_0,
            lease_epoch: 1,
            range: ShardLayerRange {
                start_layer: 0,
                end_layer: 4,
            },
        }];
        assert_eq!(
            validate_pending_result(&single_full_range, session_id, model_id, &prompt, 2, &model)
                .unwrap_err()
                .0,
            StatusCode::BAD_GATEWAY
        );

        let mut duplicate_worker = valid.clone();
        duplicate_worker.participants[1].worker_id = worker_0;
        assert_eq!(
            validate_pending_result(&duplicate_worker, session_id, model_id, &prompt, 2, &model)
                .unwrap_err()
                .0,
            StatusCode::BAD_GATEWAY
        );

        let mut zero_lease_epoch = valid.clone();
        zero_lease_epoch.participants[0].lease_epoch = 0;
        assert_eq!(
            validate_pending_result(&zero_lease_epoch, session_id, model_id, &prompt, 2, &model)
                .unwrap_err()
                .0,
            StatusCode::BAD_GATEWAY
        );

        let mut reward_marked = valid.clone();
        reward_marked.reward_eligible = true;
        assert_eq!(
            validate_pending_result(&reward_marked, session_id, model_id, &prompt, 2, &model)
                .unwrap_err()
                .0,
            StatusCode::BAD_GATEWAY
        );
    }
}
