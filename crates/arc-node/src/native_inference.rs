use arc_crypto::signature::{KeyPair, Signature};
use arc_crypto::{Hash256, hash_bytes};
use arc_mempool::Mempool;
use arc_state::inference_contract_state::{AllowedExecution, InferenceAdmissionContext};
use arc_types::inference_contract::{
    InferenceCertificate, InferenceDomain, InferenceVote, ValidatorMember, sign_vote,
    validator_set_commitment,
};
use arc_types::transaction::{
    NativeInferenceFinalizeBody, TIER1_INPUT_BLOB_MAX, TIER1_MAX_TOKENS, TIER1_OUTPUT_BLOB_MAX,
    Transaction, TxBody, TxType, gas_costs,
};
use dashmap::DashSet;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
#[cfg(not(windows))]
use std::fs::OpenOptions;
use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Weak};
use thiserror::Error;
use tokio::sync::Notify;

pub const MAX_QUEUE: usize = 32;
pub const MAX_INPUT_BYTES: usize = TIER1_INPUT_BLOB_MAX;
pub const MAX_OUTPUT_BYTES: usize = TIER1_OUTPUT_BLOB_MAX;
pub const MAX_TOKENS: usize = TIER1_MAX_TOKENS as usize;
const MAX_CERTIFICATE_CANDIDATES: usize = 1024;
const CANONICAL_I8_V2_GENERATION_SEMANTICS: &[u8] =
    b"ARC-native-inference/gguf-llama-i8-interleaved-rope/generation-v2/bos-once/le-u32/v1";

pub fn canonical_i8_profile_commitment() -> Hash256 {
    hash_bytes(
        arc_inference::cached_integer_model::GGUF_INTERLEAVED_ROPE_I8_INFERENCE_PROFILE.as_bytes(),
    )
}

pub fn canonical_i8_generation_commitment() -> Hash256 {
    hash_bytes(CANONICAL_I8_V2_GENERATION_SEMANTICS)
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PendingJob {
    pub request_id: Hash256,
    pub genesis: Hash256,
    pub context: Hash256,
    pub expires_at: u64,
    pub artifact: Hash256,
    pub generation_semantics: String,
    pub model_hash: Hash256,
    pub profile_hash: Hash256,
    pub input_hash: Hash256,
    pub generation_hash: Hash256,
    pub assignment_hash: Hash256,
    pub max_tokens: usize,
    pub max_output_bytes: usize,
    pub input: Vec<u8>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ExecutionOutput {
    pub tokens: Vec<u32>,
    pub output_hash: Hash256,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DecisionMaterial {
    pub request_id: Hash256,
    pub genesis: Hash256,
    pub context: Hash256,
    pub artifact: Hash256,
    pub generation_semantics: String,
    pub model_hash: Hash256,
    pub profile_hash: Hash256,
    pub input_hash: Hash256,
    pub generation_hash: Hash256,
    pub assignment_hash: Hash256,
    pub max_tokens: usize,
    pub max_output_bytes: usize,
    pub tokens: Vec<u32>,
    pub output_hash: Hash256,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
struct StoredDecision {
    request_id: Hash256,
    genesis: Hash256,
    context: Hash256,
    artifact: Hash256,
    generation_semantics: String,
    model_hash: Hash256,
    profile_hash: Hash256,
    input_hash: Hash256,
    generation_hash: Hash256,
    assignment_hash: Hash256,
    max_tokens: usize,
    max_output_bytes: usize,
    tokens: Vec<u32>,
    output_hash: Hash256,
    validator: Hash256,
    signature: Signature,
}

#[derive(Debug, Error)]
pub enum NativeInferenceError {
    #[error("worker queue is full")]
    QueueFull,
    #[error("worker is cancelled")]
    Cancelled,
    #[error("request expired")]
    Expired,
    #[error("request is no longer pending: it was settled, or never admitted")]
    NotPending,
    #[error("input exceeds bounded worker limit")]
    InputTooLarge,
    #[error("output exceeds bounded worker limit")]
    OutputTooLarge,
    #[error("pending request context does not match worker context")]
    ContextMismatch,
    #[error("decision conflicts with durable prior output")]
    Equivocation,
    #[error("durable decision store is corrupt")]
    CorruptStore,
    #[error("durable decision store is poisoned after an uncertain publication")]
    StorePoisoned,
    #[error("source: {0}")]
    Source(String),
    #[error("executor: {0}")]
    Executor(String),
    #[error("signer: {0}")]
    Signer(String),
    #[error("vote sink: {0}")]
    Sink(String),
    #[error("io: {0}")]
    Io(#[from] io::Error),
}

impl NativeInferenceError {
    pub fn is_request_refusal(&self) -> bool {
        matches!(
            self,
            Self::Expired
                | Self::NotPending
                | Self::InputTooLarge
                | Self::OutputTooLarge
                | Self::Executor(_)
        )
    }
}

pub trait PendingSource: Send + Sync {
    fn load_pending(&self, request_id: Hash256) -> Result<PendingJob, NativeInferenceError>;

    fn ensure_live(&self, job: &PendingJob, now: u64) -> Result<(), NativeInferenceError> {
        if now >= job.expires_at || self.load_pending(job.request_id)? != *job {
            return Err(NativeInferenceError::Expired);
        }
        Ok(())
    }
}

pub struct StatePendingSource {
    state: Arc<arc_state::StateDB>,
    context_commitment: Hash256,
}

impl StatePendingSource {
    pub fn new(state: Arc<arc_state::StateDB>, context_commitment: Hash256) -> Self {
        Self {
            state,
            context_commitment,
        }
    }

    pub fn from_active(state: Arc<arc_state::StateDB>) -> Result<Self, NativeInferenceError> {
        let context = state
            .try_native_inference_context()
            .map_err(|error| NativeInferenceError::Source(error.to_string()))?
            .ok_or_else(|| {
                NativeInferenceError::Source("native inference is not activated".into())
            })?;
        let context_commitment = context
            .commitment()
            .map_err(|error| NativeInferenceError::Source(error.to_string()))?;
        Ok(Self::new(state, context_commitment))
    }
}

impl StatePendingSource {
    fn live_commitment(&self) -> Hash256 {
        self.state
            .try_native_inference_context()
            .ok()
            .flatten()
            .and_then(|context| context.commitment().ok())
            .unwrap_or(self.context_commitment)
    }
}

impl PendingSource for StatePendingSource {
    fn load_pending(&self, request_id: Hash256) -> Result<PendingJob, NativeInferenceError> {
        let pending = self
            .state
            .native_inference_pending_requests(self.live_commitment())
            .map_err(|error| NativeInferenceError::Source(error.to_string()))?
            .into_iter()
            .find(|snapshot| snapshot.request_id == request_id)
            .ok_or(NativeInferenceError::NotPending)?;
        let job = pending.request.job;
        Ok(PendingJob {
            request_id,
            genesis: job.domain.chain_genesis,
            context: pending.context_commitment,
            expires_at: job.expires_at,
            artifact: job.model_hash,
            generation_semantics: format!(
                "profile:{:?}:generation:{:?}",
                job.profile_hash.0, job.generation_hash.0
            ),
            model_hash: job.model_hash,
            profile_hash: job.profile_hash,
            input_hash: job.input_hash,
            generation_hash: job.generation_hash,
            assignment_hash: job.assignment_hash,
            max_tokens: job.max_tokens as usize,
            max_output_bytes: job.max_output_bytes as usize,
            input: pending.input_blob,
        })
    }

    fn ensure_live(&self, job: &PendingJob, _now: u64) -> Result<(), NativeInferenceError> {
        let context = self
            .state
            .try_native_inference_context()
            .map_err(|error| NativeInferenceError::Source(error.to_string()))?
            .ok_or(NativeInferenceError::ContextMismatch)?;
        let commitment = context
            .commitment()
            .map_err(|error| NativeInferenceError::Source(error.to_string()))?;
        if job.context != commitment {
            return Err(NativeInferenceError::ContextMismatch);
        }
        let next_height = self.state.height().saturating_add(1);
        if next_height >= job.expires_at || self.load_pending(job.request_id)? != *job {
            return Err(NativeInferenceError::Expired);
        }
        Ok(())
    }
}

pub trait NativeExecutor: Send + Sync {
    fn ready_for_requests(&self, _context: &InferenceAdmissionContext, _now: u64) -> bool {
        false
    }

    fn policy_allows_requests(&self, context: &InferenceAdmissionContext, now: u64) -> bool {
        self.ready_for_requests(context, now)
    }

    fn validate_job(&self, _job: &PendingJob) -> Result<(), NativeInferenceError> {
        Ok(())
    }

    fn execute(&self, job: &PendingJob) -> Result<ExecutionOutput, NativeInferenceError>;

    fn execute_at(
        &self,
        job: &PendingJob,
        _now: u64,
    ) -> Result<ExecutionOutput, NativeInferenceError> {
        self.execute(job)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CanonicalI8Qualification {
    pub artifact_hash: Hash256,
    pub profile_hash: Hash256,
    pub generation_hash: Hash256,
    pub reference_generation_qualified: bool,
}

pub fn local_canonical_assignment_hash() -> Hash256 {
    hash_bytes(b"ARC-native-local-canonical-i8-assignment-v1")
}

pub fn runtime_assignment_hash(
    cohort: Option<&crate::row_cohort::RowCohortConfig>,
    low_residency: bool,
) -> Result<Hash256, NativeInferenceError> {
    match cohort {
        Some(config) if config.partial_rows.is_some() && !low_residency => {
            Err(NativeInferenceError::Executor(
                "partial-row assignment requires low residency without full-model fallback".into(),
            ))
        }
        Some(config) => Ok(config.assignment_hash(low_residency)),
        None if !low_residency => Ok(local_canonical_assignment_hash()),
        None => Err(NativeInferenceError::Executor(
            "low-residency assignment requires a private row cohort".into(),
        )),
    }
}

pub const DEFAULT_NATIVE_KV_BUDGET_BYTES: u64 = 4 << 30;

pub fn native_kv_bytes(positions: u64, n_layers: u64, d_kv: u64) -> Option<u64> {
    positions
        .checked_mul(n_layers)?
        .checked_mul(d_kv)?
        .checked_mul(2 * std::mem::size_of::<i64>() as u64)
}

pub const MIN_NATIVE_KV_POSITIONS: u64 = 4;

pub fn allocatable_kv_positions(budget: u64, per_position: u64, max_seq: u64) -> u64 {
    (budget / per_position.max(1))
        .checked_ilog2()
        .map_or(0, |bits| 1u64 << bits)
        .min(max_seq)
}

pub fn check_native_kv_budget(
    prompt_tokens: usize,
    max_tokens: usize,
    n_layers: usize,
    d_kv: usize,
    budget: u64,
) -> Result<(), NativeInferenceError> {
    let positions = (prompt_tokens as u64)
        .checked_add(max_tokens as u64)
        .and_then(|p| p.checked_add(1));
    let allocated = positions.and_then(u64::checked_next_power_of_two);
    let needed = allocated.and_then(|p| native_kv_bytes(p, n_layers as u64, d_kv as u64));
    match (positions, allocated, needed) {
        (_, _, Some(bytes)) if bytes <= budget => Ok(()),
        (Some(positions), Some(allocated), Some(bytes)) => {
            Err(NativeInferenceError::Executor(format!(
                "native job needs {bytes} bytes of KV cache ({positions} positions, allocated \
                 as {allocated}); this node's budget is {budget}. Raise \
                 --native-kv-budget-bytes if the host can hold it and the model's context \
                 allows it"
            )))
        }
        _ => Err(NativeInferenceError::Executor(
            "native job KV-cache size overflows".into(),
        )),
    }
}

pub struct CanonicalI8NativeExecutor {
    model: crate::row_cohort::CoordinatorModel,
    qualification: CanonicalI8Qualification,
    kv_budget_bytes: u64,
    row_cohort: Option<Arc<crate::row_cohort::RowCohort>>,
}

impl CanonicalI8NativeExecutor {
    fn assignment_hash(&self) -> Result<Hash256, NativeInferenceError> {
        match &self.row_cohort {
            Some(cohort) => Ok(cohort.assignment_hash()),
            None => runtime_assignment_hash(None, self.model.is_low_residency()),
        }
    }

    pub fn load_qualified(
        path: impl AsRef<Path>,
        qualification: CanonicalI8Qualification,
    ) -> Result<Self, NativeInferenceError> {
        let path = path.as_ref();
        let actual = artifact_hash(path)?;
        if actual != qualification.artifact_hash {
            return Err(NativeInferenceError::Executor(
                "canonical I8 artifact bytes do not match the pinned model hash".into(),
            ));
        }
        let path = path.to_str().ok_or_else(|| {
            NativeInferenceError::Executor("canonical I8 artifact path is not valid UTF-8".into())
        })?;
        let model = Arc::new(
            arc_inference::cached_integer_model::load_cached_model_canonical_i8_interleaved_rope(
                path,
            )
            .map_err(|error| NativeInferenceError::Executor(error.to_string()))?,
        );
        Self::from_model(model, qualification)
    }

    fn from_model(
        model: Arc<arc_inference::cached_integer_model::CachedIntegerModel>,
        qualification: CanonicalI8Qualification,
    ) -> Result<Self, NativeInferenceError> {
        use arc_inference::cached_integer_model::GGUF_INTERLEAVED_ROPE_I8_INFERENCE_PROFILE;
        Self::validate_qualification(&qualification)?;
        if model.canonical_execution_profile() != Some(GGUF_INTERLEAVED_ROPE_I8_INFERENCE_PROFILE)
            || !model.has_all_transformer_layers()
        {
            return Err(NativeInferenceError::Executor(
                "model is not the complete corrected GGUF interleaved-RoPE canonical I8 profile"
                    .into(),
            ));
        }
        Ok(Self {
            model: crate::row_cohort::CoordinatorModel::Resident(model),
            qualification,
            kv_budget_bytes: DEFAULT_NATIVE_KV_BUDGET_BYTES,
            row_cohort: None,
        })
    }

    fn validate_qualification(
        qualification: &CanonicalI8Qualification,
    ) -> Result<(), NativeInferenceError> {
        if !qualification.reference_generation_qualified {
            return Err(NativeInferenceError::Executor(
                "canonical I8 generation has not passed reference qualification".into(),
            ));
        }
        if qualification.profile_hash != canonical_i8_profile_commitment()
            || qualification.generation_hash != canonical_i8_generation_commitment()
        {
            return Err(NativeInferenceError::Executor(
                "canonical I8 activation does not use the exact versioned profile/generation commitments".into(),
            ));
        }
        Ok(())
    }

    pub fn load_qualified_low_residency(
        path: impl AsRef<Path>,
        qualification: CanonicalI8Qualification,
    ) -> Result<Self, NativeInferenceError> {
        Self::validate_qualification(&qualification)?;
        let model = arc_inference::low_residency::load_low_residency_model(
            path.as_ref(),
            qualification.artifact_hash,
        )
        .map_err(|e| NativeInferenceError::Executor(e.to_string()))?;
        tracing::info!(
            resident_state_bytes = model.resident_state_bytes(),
            "native low-residency coordinator loaded; projection and embedding matrices are not resident; complete verified remote coverage is required"
        );
        Ok(Self {
            model: crate::row_cohort::CoordinatorModel::LowResidency(Arc::new(model)),
            qualification,
            kv_budget_bytes: DEFAULT_NATIVE_KV_BUDGET_BYTES,
            row_cohort: None,
        })
    }

    pub fn with_kv_budget(mut self, bytes: u64) -> Self {
        self.kv_budget_bytes = bytes;
        self
    }

    pub fn connect_row_cohort(
        mut self,
        config: crate::row_cohort::RowCohortConfig,
        validator: Hash256,
        height: u64,
    ) -> Result<Self, NativeInferenceError> {
        let cohort = crate::row_cohort::RowCohort::connect_model(
            config,
            validator,
            self.model.clone(),
            self.qualification.artifact_hash,
            height,
        )
        .map_err(NativeInferenceError::Executor)?;
        self.row_cohort = Some(cohort);
        Ok(self)
    }

    pub fn row_cohort(&self) -> Option<Arc<crate::row_cohort::RowCohort>> {
        self.row_cohort.clone()
    }

    pub fn max_positions(&self) -> u64 {
        let config = self.model.config();
        let per_position =
            native_kv_bytes(1, config.n_layers as u64, config.d_kv as u64).unwrap_or(u64::MAX);
        allocatable_kv_positions(self.kv_budget_bytes, per_position, config.max_seq as u64)
    }

    pub fn verify_package(
        &self,
        artifact: &Path,
        tokenizer: &arc_inference::llama_spm_tokenizer::LlamaGgufSpmTokenizer,
        manifest: &Path,
        package_manifest_hash: Hash256,
    ) -> Result<(), NativeInferenceError> {
        let loaded = package_facts_from_config(
            self.model.config(),
            self.qualification.artifact_hash,
            artifact,
            tokenizer,
        )?;
        verify_package_manifest(manifest, package_manifest_hash, &loaded)
    }

    fn prequalified_prompt(
        job: &PendingJob,
        bos_token: u32,
        vocab_size: usize,
    ) -> Result<Vec<u32>, NativeInferenceError> {
        if job.input.is_empty() || !job.input.len().is_multiple_of(std::mem::size_of::<u32>()) {
            return Err(NativeInferenceError::Executor(
                "native canonical I8 input must be non-empty little-endian u32 token IDs".into(),
            ));
        }
        let tokens: Vec<u32> = job
            .input
            .chunks_exact(4)
            .map(|chunk| u32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]))
            .collect();
        if tokens.len() > MAX_INPUT_BYTES / 4 || tokens.first() == Some(&bos_token) {
            return Err(NativeInferenceError::Executor(
                "native canonical I8 prompt is oversized or includes BOS owned by generation v2"
                    .into(),
            ));
        }
        if tokens.iter().any(|&token| token as usize >= vocab_size) {
            return Err(NativeInferenceError::Executor(
                "native canonical I8 prompt contains a token id outside the model vocabulary"
                    .into(),
            ));
        }
        Ok(tokens)
    }
}

pub fn package_facts(
    model: &arc_inference::cached_integer_model::CachedIntegerModel,
    artifact_hash: Hash256,
    artifact: &Path,
    tokenizer: &arc_inference::llama_spm_tokenizer::LlamaGgufSpmTokenizer,
) -> Result<arc_inference::model_package::LoadedPackage, NativeInferenceError> {
    package_facts_from_config(&model.config, artifact_hash, artifact, tokenizer)
}

fn package_facts_from_config(
    config: &arc_inference::cached_integer_model::ModelConfig,
    artifact_hash: Hash256,
    artifact: &Path,
    tokenizer: &arc_inference::llama_spm_tokenizer::LlamaGgufSpmTokenizer,
) -> Result<arc_inference::model_package::LoadedPackage, NativeInferenceError> {
    let refuse = |message: String| NativeInferenceError::Executor(message);
    let (_, tensors) = arc_inference::gguf_meta::read_header_from_path(artifact)
        .map_err(|error| refuse(format!("reading the artifact's tensor inventory: {error}")))?;
    if tokenizer.bos_token() != config.bos_token
        || tokenizer.eos_tokens() != config.eos_tokens.as_slice()
    {
        return Err(refuse(
            "the artifact's tokenizer and model disagree on BOS/EOS ids".into(),
        ));
    }
    let widen = |value: usize| value as u64;
    Ok(arc_inference::model_package::LoadedPackage {
        artifact_blake3: artifact_hash.to_hex(),
        artifact_bytes: std::fs::metadata(artifact)?.len(),
        profile_commitment: canonical_i8_profile_commitment().to_hex(),
        generation_commitment: canonical_i8_generation_commitment().to_hex(),
        n_layers: widen(config.n_layers),
        d_model: widen(config.d_model),
        n_heads: widen(config.n_heads),
        n_kv_heads: widen(config.n_kv_heads),
        d_head: widen(config.d_head),
        d_kv: widen(config.d_kv),
        d_ff: widen(config.d_ff),
        vocab_size: widen(config.vocab_size),
        max_seq: widen(config.max_seq),
        tokenizer_tokens: widen(tokenizer.vocab_len()),
        vocab_blake3: hex::encode(tokenizer.vocabulary_digest()),
        bos: u64::from(config.bos_token),
        eos: config.eos_tokens.iter().map(|id| u64::from(*id)).collect(),
        tensor_count: widen(tensors.len()),
        inventory_blake3: hex::encode(arc_inference::gguf_meta::inventory_digest(&tensors)),
        kv_bytes_per_position: native_kv_bytes(1, widen(config.n_layers), widen(config.d_kv))
            .ok_or_else(|| refuse("KV bytes per position overflow".into()))?,
    })
}

pub fn verify_package_manifest(
    manifest: &Path,
    package_manifest_hash: Hash256,
    loaded: &arc_inference::model_package::LoadedPackage,
) -> Result<(), NativeInferenceError> {
    use arc_inference::model_package::MAX_MANIFEST_BYTES;
    let refuse = |message: String| NativeInferenceError::Executor(message);
    if std::fs::metadata(manifest)?.len() > MAX_MANIFEST_BYTES as u64 {
        return Err(refuse(format!(
            "package manifest {} is larger than {MAX_MANIFEST_BYTES} bytes",
            manifest.display()
        )));
    }
    let bytes = std::fs::read(manifest)?;
    arc_inference::model_package::verify_loaded_package(
        &bytes,
        &package_manifest_hash.to_hex(),
        loaded,
    )
    .map_err(|error| refuse(format!("package manifest {}: {error}", manifest.display())))
}

fn artifact_hash(path: &Path) -> Result<Hash256, NativeInferenceError> {
    let metadata = std::fs::metadata(path)?;
    if !metadata.is_file() {
        return Err(NativeInferenceError::Executor(
            "canonical I8 artifact path is not a regular file".into(),
        ));
    }
    let mut file = File::open(path)?;
    let mut hasher = blake3::Hasher::new();
    let mut buffer = [0u8; 1024 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(Hash256(*hasher.finalize().as_bytes()))
}

impl CanonicalI8NativeExecutor {
    fn run(
        &self,
        job: &PendingJob,
        now: Option<u64>,
    ) -> Result<ExecutionOutput, NativeInferenceError> {
        self.validate_job(job)?;
        if self.row_cohort.is_some() && now.is_none() {
            return Err(NativeInferenceError::Executor(
                "a row-cohort assignment requires a chain height; local execution is not a substitute"
                    .into(),
            ));
        }
        let prompt = Self::prequalified_prompt(
            job,
            self.model.config().bos_token,
            self.model.config().vocab_size,
        )?;
        let max_tokens =
            u32::try_from(job.max_tokens).map_err(|_| NativeInferenceError::OutputTooLarge)?;
        check_native_kv_budget(
            prompt.len(),
            job.max_tokens,
            self.model.config().n_layers,
            self.model.config().d_kv,
            self.kv_budget_bytes,
        )?;
        let (tokens, output_hash) = match (&self.row_cohort, now) {
            (Some(cohort), Some(now)) => cohort.generate(
                job.request_id,
                job.assignment_hash,
                job.expires_at,
                now,
                &prompt,
                max_tokens,
                &self.model.config().eos_tokens,
            )?,
            _ => self
                .model
                .local_generate(&prompt, max_tokens, &self.model.config().eos_tokens)?,
        };
        if tokens.is_empty()
            || tokens.len() > job.max_tokens
            || tokens.len() * std::mem::size_of::<u32>() > job.max_output_bytes
        {
            return Err(NativeInferenceError::OutputTooLarge);
        }
        if token_hash(&tokens) != output_hash {
            return Err(NativeInferenceError::Executor(
                "canonical I8 generation returned a non-canonical token hash".into(),
            ));
        }
        Ok(ExecutionOutput {
            tokens,
            output_hash,
        })
    }
}

impl NativeExecutor for CanonicalI8NativeExecutor {
    fn validate_job(&self, job: &PendingJob) -> Result<(), NativeInferenceError> {
        if job.model_hash != self.qualification.artifact_hash
            || job.artifact != self.qualification.artifact_hash
            || job.profile_hash != self.qualification.profile_hash
            || job.generation_hash != self.qualification.generation_hash
            || job.assignment_hash != self.assignment_hash()?
        {
            return Err(NativeInferenceError::Executor(
                "this node's executor does not run this request's model/profile/generation/assignment policy"
                    .into(),
            ));
        }
        Ok(())
    }

    fn ready_for_requests(&self, context: &InferenceAdmissionContext, now: u64) -> bool {
        self.policy_allows_requests(context, now)
            && match &self.row_cohort {
                Some(cohort) => cohort.ready_for_requests(now),
                None => !self.model.is_low_residency(),
            }
    }

    fn policy_allows_requests(&self, context: &InferenceAdmissionContext, _now: u64) -> bool {
        let Ok(assignment) = self.assignment_hash() else {
            return false;
        };
        context.allowed_executions.iter().any(|allowed| {
            allowed.model_hash == self.qualification.artifact_hash
                && allowed.profile_hash == self.qualification.profile_hash
                && allowed.generation_hash == self.qualification.generation_hash
                && allowed.assignment_hash == assignment
        })
    }

    fn execute(&self, job: &PendingJob) -> Result<ExecutionOutput, NativeInferenceError> {
        self.run(job, None)
    }

    fn execute_at(
        &self,
        job: &PendingJob,
        now: u64,
    ) -> Result<ExecutionOutput, NativeInferenceError> {
        self.run(job, Some(now))
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NativeExecutorKind {
    CanonicalI8,
    DeterministicTest,
}

impl NativeExecutorKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::CanonicalI8 => "canonical_i8",
            Self::DeterministicTest => "deterministic_test",
        }
    }

    pub fn input_format(self) -> &'static str {
        match self {
            Self::CanonicalI8 => "le_u32_token_ids_without_bos",
            Self::DeterministicTest => "opaque_bytes",
        }
    }
}

pub struct NativeServing {
    pub executor: NativeExecutorKind,
    pub tokenizer: Option<arc_inference::llama_spm_tokenizer::LlamaGgufSpmTokenizer>,
    pub max_positions: Option<u64>,
    pub row_cohort: Option<Arc<crate::row_cohort::RowCohort>>,
}

impl NativeServing {
    pub fn deterministic_test() -> Self {
        Self {
            executor: NativeExecutorKind::DeterministicTest,
            tokenizer: None,
            max_positions: None,
            row_cohort: None,
        }
    }

    pub fn canonical(
        tokenizer: Option<arc_inference::llama_spm_tokenizer::LlamaGgufSpmTokenizer>,
        max_positions: Option<u64>,
    ) -> Self {
        Self {
            executor: NativeExecutorKind::CanonicalI8,
            tokenizer,
            max_positions,
            row_cohort: None,
        }
    }

    pub fn with_row_cohort(mut self, cohort: Option<Arc<crate::row_cohort::RowCohort>>) -> Self {
        self.row_cohort = cohort;
        self
    }

    pub fn tokenize_prompt(&self, text: &str) -> Result<Vec<u32>, NativeInferenceError> {
        let tokenizer = self.tokenizer.as_ref().ok_or_else(|| {
            NativeInferenceError::Executor("this node holds no canonical tokenizer".into())
        })?;
        let encoded = tokenizer
            .encode_prompt(text)
            .map_err(|error| NativeInferenceError::Executor(error.to_string()))?;
        let bos = tokenizer.bos_token();
        let tokens = match encoded.split_first() {
            Some((first, rest)) if *first == bos => rest.to_vec(),
            _ => {
                return Err(NativeInferenceError::Executor(
                    "tokenizer did not lead with its BOS".into(),
                ));
            }
        };
        if tokens.is_empty() {
            return Err(NativeInferenceError::Executor("the prompt is empty".into()));
        }
        if tokens.first() == Some(&bos) {
            return Err(NativeInferenceError::Executor(
                "the prompt begins with a literal BOS marker, which generation v2 owns".into(),
            ));
        }
        if tokens.len() > MAX_INPUT_BYTES / 4 {
            return Err(NativeInferenceError::Executor(format!(
                "the prompt is {} tokens; the input limit is {}",
                tokens.len(),
                MAX_INPUT_BYTES / 4
            )));
        }
        if tokens
            .iter()
            .any(|&token| token as usize >= tokenizer.vocab_len())
        {
            return Err(NativeInferenceError::Executor(
                "tokenizer produced an id outside its vocabulary".into(),
            ));
        }
        Ok(tokens)
    }

    pub fn decode_output(&self, output: &[u8]) -> Option<String> {
        let tokenizer = self.tokenizer.as_ref()?;
        if output.is_empty() || !output.len().is_multiple_of(4) {
            return None;
        }
        let tokens: Vec<u32> = output
            .chunks_exact(4)
            .map(|chunk| u32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]))
            .collect();
        Some(tokenizer.decode_generated_content(&tokens))
    }
}

pub trait VoteSigner: Send + Sync {
    fn sign(&self, decision: &DecisionMaterial) -> Result<InferenceVote, NativeInferenceError>;
}

pub struct KeyPairVoteSigner {
    keypair: KeyPair,
}

impl KeyPairVoteSigner {
    pub fn new(keypair: KeyPair) -> Self {
        Self { keypair }
    }
}

impl VoteSigner for KeyPairVoteSigner {
    fn sign(&self, decision: &DecisionMaterial) -> Result<InferenceVote, NativeInferenceError> {
        sign_vote(
            decision.request_id,
            &token_bytes(&decision.tokens),
            &self.keypair,
        )
        .map_err(|error| NativeInferenceError::Signer(error.to_string()))
    }
}

pub trait VoteSink: Send + Sync {
    fn emit(&self, decision: &StoredVote) -> Result<(), NativeInferenceError>;
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StoredVote {
    pub request_id: Hash256,
    pub output_hash: Hash256,
    pub tokens: Vec<u32>,
    pub vote: InferenceVote,
}

pub struct NativeFinalizeSink {
    state: Arc<arc_state::StateDB>,
    mempool: Arc<Mempool>,
    submitter: Arc<KeyPair>,
    candidates: Mutex<BTreeMap<[u8; 32], CandidateVotes>>,
    in_flight: Mutex<Option<InFlightFinalize>>,
    emission_gate: Mutex<()>,
    outbound: Mutex<std::collections::VecDeque<StoredVote>>,
}

const MAX_OUTBOUND_NATIVE_VOTES: usize = 1_024;

type ValidatorVotes = BTreeMap<[u8; 32], InferenceVote>;
type OutputVotes = BTreeMap<[u8; 32], (Vec<u8>, ValidatorVotes)>;

#[derive(Default)]
struct CandidateVotes {
    outputs: OutputVotes,
    voted: BTreeMap<[u8; 32], [u8; 32]>,
    submitted: Option<InFlightFinalize>,
}

#[derive(Clone, Copy, Debug)]
struct InFlightFinalize {
    request_id: [u8; 32],
    tx_hash: Hash256,
    nonce: u64,
    at: std::time::Instant,
}

const FINALIZE_IN_FLIGHT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);

impl NativeFinalizeSink {
    pub fn new(
        state: Arc<arc_state::StateDB>,
        mempool: Arc<Mempool>,
        submitter: Arc<KeyPair>,
    ) -> Self {
        Self {
            state,
            mempool,
            submitter,
            candidates: Mutex::new(BTreeMap::new()),
            in_flight: Mutex::new(None),
            emission_gate: Mutex::new(()),
            outbound: Mutex::new(std::collections::VecDeque::new()),
        }
    }

    pub fn take_outbound_votes(&self, max: usize) -> Vec<StoredVote> {
        let mut queue = self.outbound.lock();
        let n = max.min(queue.len());
        queue.drain(..n).collect()
    }

    pub fn accept_peer_vote(&self, decision: &StoredVote) -> Result<(), NativeInferenceError> {
        self.accept(decision)
    }

    fn pending_and_context(
        &self,
        request_id: Hash256,
    ) -> Result<
        (
            arc_state::InferenceAdmissionContext,
            arc_state::NativeInferencePendingSnapshot,
        ),
        NativeInferenceError,
    > {
        let context = self
            .state
            .try_native_inference_context()
            .map_err(|error| NativeInferenceError::Source(error.to_string()))?
            .ok_or_else(|| {
                NativeInferenceError::Source("native inference is not activated".into())
            })?;
        let commitment = context
            .commitment()
            .map_err(|error| NativeInferenceError::Source(error.to_string()))?;
        let pending = self
            .state
            .native_inference_pending_request(request_id, commitment)
            .map_err(|error| NativeInferenceError::Source(error.to_string()))?
            .ok_or_else(|| {
                NativeInferenceError::Source("native request is no longer pending".into())
            })?;
        if self.state.height().saturating_add(1) >= pending.request.job.expires_at {
            return Err(NativeInferenceError::Expired);
        }
        Ok((context, pending))
    }

    fn certificate_has_threshold(
        context: &arc_state::InferenceAdmissionContext,
        certificate: &InferenceCertificate,
    ) -> Result<bool, NativeInferenceError> {
        let mut total = 0u64;
        let mut signed = 0u64;
        for member in &context.members {
            total = total.checked_add(member.stake).ok_or_else(|| {
                NativeInferenceError::Source("native member stake overflow".into())
            })?;
            if certificate
                .votes
                .iter()
                .any(|vote| vote.validator == member.address)
            {
                signed = signed.checked_add(member.stake).ok_or_else(|| {
                    NativeInferenceError::Source("native signed stake overflow".into())
                })?;
            }
        }
        Ok(signed >= arc_types::strict_supermajority_threshold(total))
    }
}

impl VoteSink for NativeFinalizeSink {
    fn emit(&self, decision: &StoredVote) -> Result<(), NativeInferenceError> {
        {
            let mut queue = self.outbound.lock();
            if queue.len() >= MAX_OUTBOUND_NATIVE_VOTES {
                queue.pop_front();
            }
            queue.push_back(decision.clone());
        }
        match self.accept(decision) {
            Err(NativeInferenceError::Expired) => Ok(()),
            Err(NativeInferenceError::Source(message)) if message.contains("no longer pending") => {
                Ok(())
            }
            other => other,
        }
    }
}

impl NativeFinalizeSink {
    fn still_in_flight(&self, flight: &InFlightFinalize) -> bool {
        if self.state.receipts.contains_key(&flight.tx_hash.0) {
            return false;
        }
        let account_nonce = self
            .state
            .get_account(&self.submitter.address())
            .map(|account| account.nonce)
            .unwrap_or(0);
        if account_nonce > flight.nonce {
            return false;
        }
        let pending = self
            .state
            .try_native_inference_context()
            .ok()
            .flatten()
            .and_then(|context| context.commitment().ok())
            .and_then(|commitment| {
                self.state
                    .native_inference_pending_request(Hash256(flight.request_id), commitment)
                    .ok()
                    .flatten()
            })
            .is_some();
        pending && flight.at.elapsed() < FINALIZE_IN_FLIGHT_TIMEOUT
    }

    fn accept(&self, decision: &StoredVote) -> Result<(), NativeInferenceError> {
        let _emission = self.emission_gate.lock();
        let already_counted = self
            .candidates
            .lock()
            .get(&decision.request_id.0)
            .and_then(|candidate| candidate.outputs.get(&decision.output_hash.0))
            .and_then(|(_, votes)| votes.get(&decision.vote.validator.0))
            .is_some_and(|existing| existing == &decision.vote);
        let (context, pending) = self.pending_and_context(decision.request_id)?;
        if !already_counted {
            let output = token_bytes(&decision.tokens);
            if output.is_empty()
                || output.len() > pending.request.job.max_output_bytes as usize
                || hash_bytes(&output) != decision.output_hash
                || !verify_vote(&decision.vote, decision.request_id, &decision.tokens)
            {
                return Err(NativeInferenceError::Signer(
                    "invalid native vote/output binding".into(),
                ));
            }
            if !context
                .members
                .iter()
                .any(|member| member.address == decision.vote.validator)
            {
                return Err(NativeInferenceError::Signer(
                    "native vote is not from a frozen member".into(),
                ));
            }
            let mut candidates = self.candidates.lock();
            if !candidates.contains_key(&decision.request_id.0)
                && candidates.len() >= MAX_CERTIFICATE_CANDIDATES
            {
                let commitment = context
                    .commitment()
                    .map_err(|error| NativeInferenceError::Source(error.to_string()))?;
                candidates.retain(|request_id, _| {
                    self.state
                        .native_inference_pending_request(Hash256(*request_id), commitment)
                        .ok()
                        .flatten()
                        .is_some()
                });
                if candidates.len() >= MAX_CERTIFICATE_CANDIDATES {
                    return Err(NativeInferenceError::QueueFull);
                }
            }
            let candidate = candidates.entry(decision.request_id.0).or_default();
            match candidate.voted.get(&decision.vote.validator.0) {
                Some(voted) if *voted != decision.output_hash.0 => {
                    return Err(NativeInferenceError::Equivocation);
                }
                Some(_) => {}
                None => {
                    candidate
                        .voted
                        .insert(decision.vote.validator.0, decision.output_hash.0);
                }
            }
            let entry = candidate
                .outputs
                .entry(decision.output_hash.0)
                .or_insert_with(|| (output, BTreeMap::new()));
            match entry.1.get(&decision.vote.validator.0) {
                Some(existing) if existing != &decision.vote => {
                    return Err(NativeInferenceError::Equivocation);
                }
                Some(_) => {}
                None => {
                    entry
                        .1
                        .insert(decision.vote.validator.0, decision.vote.clone());
                }
            }
        }

        let certificate = {
            let mut candidates = self.candidates.lock();
            let Some(candidate) = candidates.get_mut(&decision.request_id.0) else {
                return Ok(());
            };
            if let Some(flight) = candidate.submitted {
                if self.still_in_flight(&flight)
                    || self.state.receipts.contains_key(&flight.tx_hash.0)
                {
                    return Ok(());
                }
                candidate.submitted = None;
            }
            let mut found = None;
            for (output, votes) in candidate.outputs.values() {
                let certificate = InferenceCertificate {
                    output: output.clone(),
                    votes: votes.values().cloned().collect(),
                };
                if Self::certificate_has_threshold(&context, &certificate)? {
                    found = Some(certificate);
                    break;
                }
            }
            match found {
                Some(certificate) => certificate,
                None => return Ok(()),
            }
        };

        {
            let mut flight = self.in_flight.lock();
            if let Some(current) = *flight {
                if current.request_id != decision.request_id.0 && self.still_in_flight(&current) {
                    return Ok(());
                }
                *flight = None;
            }
        }

        let (current_context, current_pending) = self.pending_and_context(decision.request_id)?;
        if current_context != context || current_pending != pending {
            return Err(NativeInferenceError::ContextMismatch);
        }
        let nonce = self
            .state
            .get_account(&self.submitter.address())
            .ok_or_else(|| {
                NativeInferenceError::Source("native finalizer has no canonical account".into())
            })?
            .nonce;
        let mut tx = Transaction {
            tx_type: TxType::NativeInferenceFinalize,
            from: self.submitter.address(),
            nonce,
            body: TxBody::NativeInferenceFinalize(NativeInferenceFinalizeBody {
                request_id: decision.request_id.0,
                certificate,
            }),
            fee: 0,
            gas_limit: gas_costs::NATIVE_INFERENCE_FINALIZE,
            hash: Hash256::ZERO,
            signature: Signature::null(),
            sig_verified: false,
        };
        self.state
            .sign_transaction(&mut tx, &self.submitter)
            .map_err(|error| NativeInferenceError::Signer(error.to_string()))?;
        self.state
            .validate_native_inference_transaction_admission_next(&tx, &context)
            .map_err(|error| NativeInferenceError::Source(error.to_string()))?;
        if !self.mempool.contains(&tx.hash) {
            self.mempool
                .insert(tx.clone())
                .map_err(|error| NativeInferenceError::Sink(error.to_string()))?;
        }
        let flight = InFlightFinalize {
            request_id: decision.request_id.0,
            tx_hash: tx.hash,
            nonce,
            at: std::time::Instant::now(),
        };
        *self.in_flight.lock() = Some(flight);
        if let Some(candidate) = self.candidates.lock().get_mut(&decision.request_id.0) {
            candidate.submitted = Some(flight);
        }
        Ok(())
    }
}

#[derive(Clone, Debug)]
pub struct DecisionStore {
    root: PathBuf,
    validator: Hash256,
    genesis: Hash256,
    poisoned: Arc<AtomicBool>,
}

impl DecisionStore {
    pub fn open(
        root: impl AsRef<Path>,
        validator: Hash256,
        genesis: Hash256,
        _context: Hash256,
    ) -> Result<Self, NativeInferenceError> {
        fs::create_dir_all(root.as_ref())?;
        Ok(Self {
            root: root.as_ref().to_path_buf(),
            validator,
            genesis,
            poisoned: Arc::new(AtomicBool::new(false)),
        })
    }

    fn path(&self, context: Hash256, request_id: Hash256) -> PathBuf {
        let mut bytes = Vec::with_capacity(128);
        bytes.extend_from_slice(&self.validator.0);
        bytes.extend_from_slice(&self.genesis.0);
        bytes.extend_from_slice(&context.0);
        bytes.extend_from_slice(&request_id.0);
        self.root.join(format!(
            "{}.decision",
            hex::encode(blake3::hash(&bytes).as_bytes())
        ))
    }

    fn load(
        &self,
        context: Hash256,
        request_id: Hash256,
    ) -> Result<Option<StoredDecision>, NativeInferenceError> {
        if self.poisoned.load(Ordering::Acquire) {
            return Err(NativeInferenceError::StorePoisoned);
        }
        let path = self.path(context, request_id);
        let file = match File::open(path) {
            Ok(file) => file,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e.into()),
        };
        const MAX_DECISION_BYTES: usize = 256 * 1024;
        if file.metadata()?.len() > MAX_DECISION_BYTES as u64 {
            return Err(NativeInferenceError::CorruptStore);
        }
        let mut bytes = Vec::with_capacity(MAX_DECISION_BYTES.min(8192));
        file.take(MAX_DECISION_BYTES as u64 + 1)
            .read_to_end(&mut bytes)?;
        if bytes.len() > MAX_DECISION_BYTES {
            return Err(NativeInferenceError::CorruptStore);
        }
        bincode::deserialize_limited_exact::<StoredDecision, { 256 * 1024 }>(&bytes)
            .map(Some)
            .map_err(|_| NativeInferenceError::CorruptStore)
    }

    fn sync_published(
        &self,
        context: Hash256,
        request_id: Hash256,
        expected: &StoredDecision,
    ) -> Result<(), NativeInferenceError> {
        let path = self.path(context, request_id);
        #[cfg(windows)]
        {
            const MAX_DECISION_BYTES: u64 = 256 * 1024;
            let file = File::open(&path)?;
            if file.metadata()?.len() > MAX_DECISION_BYTES {
                return Err(NativeInferenceError::CorruptStore);
            }
            let mut bytes = Vec::with_capacity(8192);
            file.take(MAX_DECISION_BYTES + 1).read_to_end(&mut bytes)?;
            if bytes.len() as u64 > MAX_DECISION_BYTES {
                return Err(NativeInferenceError::CorruptStore);
            }
            let actual =
                bincode::deserialize_limited_exact::<StoredDecision, { 256 * 1024 }>(&bytes)
                    .map_err(|_| NativeInferenceError::CorruptStore)?;
            if &actual != expected {
                return Err(NativeInferenceError::Equivocation);
            }
            arc_crypto::secret_file::durably_replace_private(&path, &bytes)?;
        }
        #[cfg(not(windows))]
        {
            let file = File::open(&path)?;
            file.sync_all()?;
            arc_crypto::secret_file::sync_parent_directory(&path)?;
            let _ = expected;
        }
        Ok(())
    }

    fn load_existing_valid(
        &self,
        job: &PendingJob,
    ) -> Result<Option<StoredVote>, NativeInferenceError> {
        let Some(stored) = self.load(job.context, job.request_id)? else {
            return Ok(None);
        };
        if stored.request_id != job.request_id
            || stored.validator != self.validator
            || stored.genesis != job.genesis
            || stored.context != job.context
            || stored.artifact != job.artifact
            || stored.generation_semantics != job.generation_semantics
            || stored.model_hash != job.model_hash
            || stored.profile_hash != job.profile_hash
            || stored.input_hash != job.input_hash
            || stored.generation_hash != job.generation_hash
            || stored.assignment_hash != job.assignment_hash
            || stored.max_tokens != job.max_tokens
            || stored.max_output_bytes != job.max_output_bytes
            || stored.tokens.is_empty()
            || stored.tokens.len() > job.max_tokens
            || stored.tokens.len() > MAX_TOKENS
            || stored.tokens.len() * std::mem::size_of::<u32>() > job.max_output_bytes
            || stored.tokens.len() * std::mem::size_of::<u32>() > MAX_OUTPUT_BYTES
            || stored.output_hash != token_hash(&stored.tokens)
        {
            return Err(NativeInferenceError::Equivocation);
        }
        let vote = InferenceVote {
            validator: stored.validator,
            output_hash: stored.output_hash,
            signature: stored.signature.clone(),
        };
        if !verify_vote(&vote, job.request_id, &stored.tokens) {
            return Err(NativeInferenceError::CorruptStore);
        }
        #[cfg(windows)]
        self.sync_published(job.context, job.request_id, &stored)?;
        Ok(Some(StoredVote {
            request_id: job.request_id,
            output_hash: stored.output_hash,
            tokens: stored.tokens,
            vote,
        }))
    }

    pub fn prune_terminal(
        &self,
        is_terminal: impl Fn(Hash256) -> bool,
    ) -> Result<usize, NativeInferenceError> {
        if self.poisoned.load(Ordering::Acquire) {
            return Err(NativeInferenceError::StorePoisoned);
        }
        let mut removed = 0usize;
        for entry in fs::read_dir(&self.root)? {
            let path = entry?.path();
            if path.extension().and_then(|e| e.to_str()) != Some("decision") {
                continue;
            }
            let Ok(bytes) = fs::read(&path) else {
                continue;
            };
            let Ok(stored) =
                bincode::deserialize_limited_exact::<StoredDecision, { 256 * 1024 }>(&bytes)
            else {
                continue;
            };
            if stored.validator != self.validator
                || stored.genesis != self.genesis
                || path != self.path(stored.context, stored.request_id)
            {
                continue;
            }
            if is_terminal(stored.request_id) {
                fs::remove_file(&path)?;
                removed += 1;
            }
        }
        if removed > 0 {
            #[cfg(unix)]
            File::open(&self.root)?.sync_all()?;
            #[cfg(windows)]
            {}
        }
        Ok(removed)
    }

    pub fn persist_signed(
        &self,
        decision: StoredVote,
        job: &PendingJob,
    ) -> Result<StoredVote, NativeInferenceError> {
        if self.poisoned.load(Ordering::Acquire) {
            return Err(NativeInferenceError::StorePoisoned);
        }
        if job.genesis != self.genesis {
            return Err(NativeInferenceError::ContextMismatch);
        }
        if decision.request_id != job.request_id {
            return Err(NativeInferenceError::ContextMismatch);
        }
        if job.max_tokens == 0
            || job.max_tokens > MAX_TOKENS
            || job.max_output_bytes == 0
            || job.max_output_bytes > MAX_OUTPUT_BYTES
        {
            return Err(NativeInferenceError::OutputTooLarge);
        }
        if decision.tokens.is_empty()
            || decision.tokens.len() > job.max_tokens
            || decision.tokens.len() > MAX_TOKENS
        {
            return Err(NativeInferenceError::OutputTooLarge);
        }
        if decision.tokens.len() * std::mem::size_of::<u32>() > job.max_output_bytes
            || decision.tokens.len() * std::mem::size_of::<u32>() > MAX_OUTPUT_BYTES
            || token_hash(&decision.tokens) != decision.output_hash
        {
            return Err(NativeInferenceError::Executor(
                "output hash does not match canonical token bytes".into(),
            ));
        }
        if decision.vote.validator != self.validator
            || decision.vote.output_hash != decision.output_hash
            || !verify_vote(&decision.vote, job.request_id, &decision.tokens)
        {
            return Err(NativeInferenceError::Signer(
                "invalid typed inference vote".into(),
            ));
        }
        let stored = StoredDecision {
            request_id: job.request_id,
            genesis: job.genesis,
            context: job.context,
            artifact: job.artifact,
            generation_semantics: job.generation_semantics.clone(),
            model_hash: job.model_hash,
            profile_hash: job.profile_hash,
            input_hash: job.input_hash,
            generation_hash: job.generation_hash,
            assignment_hash: job.assignment_hash,
            max_tokens: job.max_tokens,
            max_output_bytes: job.max_output_bytes,
            tokens: decision.tokens.clone(),
            output_hash: decision.output_hash,
            validator: decision.vote.validator,
            signature: decision.vote.signature.clone(),
        };
        if let Some(existing) = self.load(job.context, job.request_id)? {
            self.sync_published(job.context, job.request_id, &existing)?;
            if existing == stored {
                return Ok(decision);
            }
            return Err(NativeInferenceError::Equivocation);
        }
        let bytes = bincode::serialize(&stored).map_err(|_| NativeInferenceError::CorruptStore)?;
        let tmp = self.root.join(format!(".{}.tmp", uuid::Uuid::new_v4()));
        let target = self.path(job.context, job.request_id);
        #[cfg(windows)]
        {
            let mut file = arc_crypto::secret_file::create_new_private(&tmp)?;
            if let Err(error) = file.write_all(&bytes).and_then(|()| file.sync_all()) {
                drop(file);
                let _ = fs::remove_file(&tmp);
                return Err(error.into());
            }
        }
        #[cfg(not(windows))]
        {
            let mut file = OpenOptions::new().write(true).create_new(true).open(&tmp)?;
            file.write_all(&bytes)?;
            file.sync_all()?;
        }
        let publication = {
            #[cfg(windows)]
            {
                arc_crypto::secret_file::durably_publish_existing_private_no_replace(&tmp, &target)
            }
            #[cfg(not(windows))]
            {
                match fs::hard_link(&tmp, &target) {
                    Ok(()) => fs::remove_file(&tmp),
                    Err(error) => Err(error),
                }
            }
        };
        match publication {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
                let _ = fs::remove_file(&tmp);
                let existing = self
                    .load(job.context, job.request_id)?
                    .ok_or(NativeInferenceError::Equivocation)?;
                self.sync_published(job.context, job.request_id, &existing)?;
                return match self.load(job.context, job.request_id)? {
                    Some(existing) if existing == stored => Ok(decision),
                    _ => Err(NativeInferenceError::Equivocation),
                };
            }
            Err(e) => {
                let _ = fs::remove_file(&tmp);
                self.poisoned.store(true, Ordering::Release);
                return Err(e.into());
            }
        }
        #[cfg(not(windows))]
        if let Err(error) = self.sync_published(job.context, job.request_id, &stored) {
            self.poisoned.store(true, Ordering::Release);
            return Err(error);
        }
        Ok(decision)
    }
}

pub struct NativeWorker<S, E, G, V> {
    source: Arc<S>,
    executor: Arc<E>,
    signer: Arc<G>,
    sink: Arc<V>,
    store: DecisionStore,
    queue: Mutex<arc_assign::queue::FairQueue>,
    queued: DashSet<[u8; 32]>,
    cancelled: Arc<AtomicBool>,
    compute_permit: Mutex<()>,
}

impl<S: PendingSource, E: NativeExecutor, G: VoteSigner, V: VoteSink> NativeWorker<S, E, G, V> {
    pub fn new(
        source: Arc<S>,
        executor: Arc<E>,
        signer: Arc<G>,
        sink: Arc<V>,
        store: DecisionStore,
    ) -> Self {
        Self {
            source,
            executor,
            signer,
            sink,
            store,
            queue: Mutex::new(arc_assign::queue::FairQueue::new(MAX_QUEUE)),
            queued: DashSet::new(),
            cancelled: Arc::new(AtomicBool::new(false)),
            compute_permit: Mutex::new(()),
        }
    }

    pub fn submit(&self, request_id: Hash256) -> Result<(), NativeInferenceError> {
        self.submit_for(request_id, Hash256::ZERO, u64::MAX)
    }

    pub fn submit_for(
        &self,
        request_id: Hash256,
        requester: Hash256,
        expires_at: u64,
    ) -> Result<(), NativeInferenceError> {
        if self.cancelled.load(Ordering::Acquire) {
            return Err(NativeInferenceError::Cancelled);
        }
        if !self.queued.insert(request_id.0) {
            return Ok(());
        }
        let call = arc_assign::queue::Call {
            request: requester,
            call_id: request_id,
            deadline: expires_at,
        };
        self.queue.lock().push(call).map_err(|_| {
            self.queued.remove(&request_id.0);
            NativeInferenceError::QueueFull
        })
    }

    pub fn is_idle(&self) -> bool {
        self.queue.lock().is_empty()
    }

    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
    }

    pub fn run_one(&self, now: u64) -> Result<StoredVote, NativeInferenceError> {
        if self.cancelled.load(Ordering::Acquire) {
            return Err(NativeInferenceError::Cancelled);
        }
        let request_id = self
            .next_request(now)
            .ok_or(NativeInferenceError::QueueFull)?;
        self.run_request(request_id, now)
    }

    pub fn next_request(&self, now: u64) -> Option<Hash256> {
        loop {
            let next = self.queue.lock().next(now);
            match next {
                arc_assign::queue::Next::Run(call) => {
                    self.queued.remove(&call.call_id.0);
                    return Some(call.call_id);
                }
                arc_assign::queue::Next::Expired(calls) => {
                    for call in calls {
                        self.queued.remove(&call.call_id.0);
                    }
                }
                arc_assign::queue::Next::Idle => return None,
            }
        }
    }

    pub fn run_request(
        &self,
        request_id: Hash256,
        now: u64,
    ) -> Result<StoredVote, NativeInferenceError> {
        let _permit = self.compute_permit.lock();
        if self.cancelled.load(Ordering::Acquire) {
            return Err(NativeInferenceError::Cancelled);
        }
        let job = self.source.load_pending(request_id)?;
        if now >= job.expires_at {
            return Err(NativeInferenceError::Expired);
        }
        if job.input.len() > MAX_INPUT_BYTES {
            return Err(NativeInferenceError::InputTooLarge);
        }
        if job.max_tokens == 0 || job.max_tokens > MAX_TOKENS {
            return Err(NativeInferenceError::OutputTooLarge);
        }
        if job.max_output_bytes == 0 || job.max_output_bytes > MAX_OUTPUT_BYTES {
            return Err(NativeInferenceError::OutputTooLarge);
        }
        if arc_crypto::hash_bytes(&job.input) != job.input_hash {
            return Err(NativeInferenceError::ContextMismatch);
        }
        self.executor.validate_job(&job)?;
        if let Some(vote) = self.store.load_existing_valid(&job)? {
            self.source.ensure_live(&job, now)?;
            self.sink.emit(&vote)?;
            return Ok(vote);
        }
        if self.cancelled.load(Ordering::Acquire) {
            return Err(NativeInferenceError::Cancelled);
        }
        let output = self.executor.execute_at(&job, now)?;
        if output.tokens.is_empty()
            || output.tokens.len() > job.max_tokens
            || output.tokens.len() > MAX_TOKENS
        {
            return Err(NativeInferenceError::OutputTooLarge);
        }
        if output.tokens.len() * std::mem::size_of::<u32>() > job.max_output_bytes
            || output.tokens.len() * std::mem::size_of::<u32>() > MAX_OUTPUT_BYTES
            || token_hash(&output.tokens) != output.output_hash
        {
            return Err(NativeInferenceError::Executor(
                "output hash does not match canonical token bytes".into(),
            ));
        }
        let material = DecisionMaterial {
            request_id: job.request_id,
            genesis: job.genesis,
            context: job.context,
            artifact: job.artifact,
            generation_semantics: job.generation_semantics.clone(),
            model_hash: job.model_hash,
            profile_hash: job.profile_hash,
            input_hash: job.input_hash,
            generation_hash: job.generation_hash,
            assignment_hash: job.assignment_hash,
            max_tokens: job.max_tokens,
            max_output_bytes: job.max_output_bytes,
            tokens: output.tokens.clone(),
            output_hash: output.output_hash,
        };
        self.source.ensure_live(&job, now)?;
        let signature = self.signer.sign(&material)?;
        if self.cancelled.load(Ordering::Acquire) {
            return Err(NativeInferenceError::Cancelled);
        }
        let vote = StoredVote {
            request_id,
            output_hash: output.output_hash,
            tokens: output.tokens,
            vote: signature,
        };
        let vote = self.store.persist_signed(vote, &job)?;
        self.source.ensure_live(&job, now)?;
        if self.cancelled.load(Ordering::Acquire) {
            return Err(NativeInferenceError::Cancelled);
        }
        self.sink.emit(&vote)?;
        Ok(vote)
    }
}

pub struct NativeWorkerRuntime<E, G, V> {
    state: Arc<arc_state::StateDB>,
    source: Arc<StatePendingSource>,
    worker: NativeWorker<StatePendingSource, E, G, V>,
    last_emitted: Mutex<BTreeMap<[u8; 32], std::time::Instant>>,
    refused: Mutex<BTreeMap<[u8; 32], String>>,
    pruned_at: Mutex<Option<std::time::Instant>>,
}

pub const NATIVE_DECISION_PRUNE_INTERVAL: std::time::Duration = std::time::Duration::from_secs(60);

pub const NATIVE_VOTE_REEMIT_INTERVAL: std::time::Duration = std::time::Duration::from_secs(3);

impl<E: NativeExecutor, G: VoteSigner, V: VoteSink> NativeWorkerRuntime<E, G, V> {
    pub fn from_active(
        state: Arc<arc_state::StateDB>,
        executor: Arc<E>,
        signer: Arc<G>,
        sink: Arc<V>,
        store: DecisionStore,
    ) -> Result<Self, NativeInferenceError> {
        let source = Arc::new(StatePendingSource::from_active(state.clone())?);
        let worker = NativeWorker::new(source.clone(), executor, signer, sink, store);
        Ok(Self {
            state,
            source,
            worker,
            last_emitted: Mutex::new(BTreeMap::new()),
            refused: Mutex::new(BTreeMap::new()),
            pruned_at: Mutex::new(None),
        })
    }

    fn ready_for_requests(&self) -> bool {
        self.state
            .try_native_inference_context()
            .ok()
            .flatten()
            .is_some_and(|context| {
                self.worker
                    .executor
                    .ready_for_requests(&context, self.state.height())
            })
    }

    pub fn poll_once(&self) -> Result<Option<StoredVote>, NativeInferenceError> {
        let context = self
            .state
            .try_native_inference_context()
            .map_err(|error| NativeInferenceError::Source(error.to_string()))?
            .ok_or_else(|| {
                NativeInferenceError::Source("native inference is not activated".into())
            })?;
        let commitment = context
            .commitment()
            .map_err(|error| NativeInferenceError::Source(error.to_string()))?;
        let pending: Vec<_> = self
            .state
            .native_inference_pending_requests(commitment)
            .map_err(|error| NativeInferenceError::Source(error.to_string()))?;
        {
            let mut last = self.last_emitted.lock();
            let mut refused = self.refused.lock();
            let live: BTreeSet<[u8; 32]> = pending.iter().map(|p| p.request_id.0).collect();
            last.retain(|id, _| live.contains(id));
            refused.retain(|id, _| live.contains(id));
            let mut by_requester: BTreeMap<[u8; 32], std::collections::VecDeque<_>> =
                BTreeMap::new();
            let mut due: Vec<_> = pending
                .iter()
                .filter(|item| !refused.contains_key(&item.request_id.0))
                .filter(|item| {
                    last.get(&item.request_id.0)
                        .is_none_or(|at| at.elapsed() >= NATIVE_VOTE_REEMIT_INTERVAL)
                })
                .collect();
            due.sort_by_key(|item| (item.admission_height, item.request_id.0));
            for item in due {
                by_requester
                    .entry(item.request.job.requester.0)
                    .or_default()
                    .push_back(item);
            }
            'offer: loop {
                let mut offered = false;
                for queue in by_requester.values_mut() {
                    let Some(item) = queue.pop_front() else {
                        continue;
                    };
                    offered = true;
                    let job = &item.request.job;
                    match self
                        .worker
                        .submit_for(item.request_id, job.requester, job.expires_at)
                    {
                        Ok(()) => {}
                        Err(NativeInferenceError::QueueFull) => break 'offer,
                        Err(error) => return Err(error),
                    }
                }
                if !offered {
                    break;
                }
            }
        }
        self.prune_settled_decisions(commitment);
        let now = self.state.height().saturating_add(1);
        for _ in 0..MAX_QUEUE {
            let Some(request_id) = self.worker.next_request(now) else {
                return Ok(None);
            };
            match self.worker.run_request(request_id, now) {
                Ok(vote) => {
                    self.last_emitted
                        .lock()
                        .insert(vote.request_id.0, std::time::Instant::now());
                    return Ok(Some(vote));
                }
                Err(NativeInferenceError::NotPending) => {
                    tracing::debug!(
                        request = %request_id.to_hex(),
                        "native request settled before this member ran it"
                    );
                    self.refused
                        .lock()
                        .insert(request_id.0, NativeInferenceError::NotPending.to_string());
                }
                Err(error) if error.is_request_refusal() => {
                    tracing::warn!(
                        request = %request_id.to_hex(),
                        %error,
                        "native worker refused this request; it will not vote on it (it expires \
                         and refunds unless the rest of the committee certifies it)"
                    );
                    self.refused.lock().insert(request_id.0, error.to_string());
                }
                Err(error) => return Err(error),
            }
        }
        Ok(None)
    }

    fn prune_settled_decisions(&self, commitment: Hash256) {
        {
            let mut at = self.pruned_at.lock();
            if at.is_some_and(|t| t.elapsed() < NATIVE_DECISION_PRUNE_INTERVAL) {
                return;
            }
            *at = Some(std::time::Instant::now());
        }
        let state = self.state.clone();
        let settled = move |request_id: Hash256| {
            matches!(
                state.native_inference_receipt(request_id, commitment),
                Ok(Some(receipt)) if receipt.metadata.status
                    != arc_state::wal::InferenceTransitionStatus::Pending
            )
        };
        match self.worker.store.prune_terminal(settled) {
            Ok(0) => {}
            Ok(removed) => tracing::debug!(removed, "Pruned decision files of settled requests"),
            Err(error) => tracing::warn!(%error, "Could not prune settled decision files"),
        }
    }

    pub fn cancel(&self) {
        self.worker.cancel();
    }

    pub fn source(&self) -> &Arc<StatePendingSource> {
        &self.source
    }
}

fn token_hash(tokens: &[u32]) -> Hash256 {
    hash_bytes(&token_bytes(tokens))
}

fn token_bytes(tokens: &[u32]) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(std::mem::size_of_val(tokens));
    for token in tokens {
        bytes.extend_from_slice(&token.to_le_bytes());
    }
    bytes
}

fn verify_vote(vote: &InferenceVote, request_id: Hash256, tokens: &[u32]) -> bool {
    let output = token_bytes(tokens);
    let output_hash = hash_bytes(&output);
    if vote.output_hash != output_hash {
        return false;
    }
    let mut hasher = blake3::Hasher::new_derive_key("ARC-native-inference-vote-signature-v1");
    hasher.update(request_id.as_ref());
    hasher.update(output_hash.as_ref());
    hasher.update(&(output.len() as u64).to_le_bytes());
    vote.signature
        .verify(&Hash256(*hasher.finalize().as_bytes()), &vote.validator)
        .is_ok()
}

#[cfg(feature = "native-test-executor")]
pub struct DeterministicTestExecutor {
    qualification: CanonicalI8Qualification,
}

#[cfg(feature = "native-test-executor")]
impl DeterministicTestExecutor {
    pub fn new(qualification: CanonicalI8Qualification) -> Self {
        Self { qualification }
    }
}

#[cfg(feature = "native-test-executor")]
impl NativeExecutor for DeterministicTestExecutor {
    fn ready_for_requests(&self, context: &InferenceAdmissionContext, _now: u64) -> bool {
        context.allowed_executions.iter().any(|allowed| {
            allowed.model_hash == self.qualification.artifact_hash
                && allowed.profile_hash == self.qualification.profile_hash
                && allowed.generation_hash == self.qualification.generation_hash
        })
    }

    fn execute(&self, job: &PendingJob) -> Result<ExecutionOutput, NativeInferenceError> {
        let mut seed = Vec::new();
        seed.extend_from_slice(b"ARC-DETERMINISTIC-TEST-EXECUTOR-v1");
        seed.extend_from_slice(&self.qualification.artifact_hash.0);
        seed.extend_from_slice(&job.request_id.0);
        let digest = hash_bytes(&seed);
        let tokens: Vec<u32> = digest.0[..8]
            .chunks(4)
            .map(|c| u32::from_le_bytes([c[0], c[1], c[2], c[3]]))
            .collect();
        let bytes: Vec<u8> = tokens.iter().flat_map(|t| t.to_le_bytes()).collect();
        Ok(ExecutionOutput {
            tokens,
            output_hash: hash_bytes(&bytes),
        })
    }
}

#[derive(Default)]
pub struct NativeRequestAdmission {
    operator_enabled: bool,
    running: AtomicBool,
    health: Mutex<NativeRuntimeHealth>,
    policy: Mutex<Option<NativeAdmissionPolicy>>,
    worker_execution: Arc<WorkerExecutionGate>,
}

#[derive(Default)]
struct NativeRuntimeHealth {
    healthy_at: Option<std::time::Instant>,
    busy_since: Option<std::time::Instant>,
    unavailable_since: Option<std::time::Instant>,
}

struct NativeAdmissionPolicy {
    state: Weak<arc_state::StateDB>,
    executor: Weak<dyn NativeExecutor>,
}

#[derive(Default)]
pub struct WorkerExecutionGate {
    state: Mutex<WorkerExecutionState>,
    changed: Notify,
}

#[derive(Default)]
struct WorkerExecutionState {
    closed: bool,
    active_jobs: usize,
    request_id: Option<[u8; 32]>,
    lease_generation: u64,
    shutdown_committed: bool,
}

#[derive(Clone)]
pub struct WorkerExecutionPermit {
    _lease: Arc<WorkerExecutionLease>,
}

struct WorkerExecutionLease(Arc<WorkerExecutionGate>);

impl Drop for WorkerExecutionLease {
    fn drop(&mut self) {
        let mut state = self.0.state.lock();
        state.active_jobs = state.active_jobs.saturating_sub(1);
        drop(state);
        self.0.changed.notify_waiters();
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WorkerQuiescenceResult {
    Quiesced { active_jobs: usize },
    Busy { active_jobs: usize },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WorkerResumeError {
    NoMatchingDrain,
    ShutdownCommitted,
}

impl WorkerExecutionGate {
    pub fn try_enter(self: &Arc<Self>) -> Option<WorkerExecutionPermit> {
        let mut state = self.state.lock();
        if state.closed {
            return None;
        }
        state.active_jobs = state.active_jobs.checked_add(1)?;
        Some(WorkerExecutionPermit {
            _lease: Arc::new(WorkerExecutionLease(self.clone())),
        })
    }

    pub async fn quiesce(
        self: &Arc<Self>,
        request_id: [u8; 32],
        timeout: std::time::Duration,
        lease_deadline: tokio::time::Instant,
    ) -> WorkerQuiescenceResult {
        if lease_deadline <= tokio::time::Instant::now() {
            return WorkerQuiescenceResult::Busy {
                active_jobs: self.active_jobs(),
            };
        }
        let (already_drained, newly_closed, generation) = {
            let mut state = self.state.lock();
            if state.shutdown_committed {
                return WorkerQuiescenceResult::Busy {
                    active_jobs: state.active_jobs,
                };
            }
            if state.closed {
                if state.request_id != Some(request_id) {
                    return WorkerQuiescenceResult::Busy {
                        active_jobs: state.active_jobs,
                    };
                }
                return if state.active_jobs == 0 {
                    WorkerQuiescenceResult::Quiesced { active_jobs: 0 }
                } else {
                    WorkerQuiescenceResult::Busy {
                        active_jobs: state.active_jobs,
                    }
                };
            } else {
                let Some(generation) = state.lease_generation.checked_add(1) else {
                    return WorkerQuiescenceResult::Busy {
                        active_jobs: state.active_jobs,
                    };
                };
                state.closed = true;
                state.request_id = Some(request_id);
                state.lease_generation = generation;
                state.shutdown_committed = false;
                (state.active_jobs == 0, true, generation)
            }
        };
        if newly_closed {
            let gate = self.clone();
            tokio::spawn(async move {
                tokio::time::sleep_until(lease_deadline).await;
                gate.expire_lease(request_id, generation);
            });
        }
        if already_drained {
            return WorkerQuiescenceResult::Quiesced { active_jobs: 0 };
        }

        let deadline = (tokio::time::Instant::now() + timeout).min(lease_deadline);
        loop {
            let notified = self.changed.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            {
                let state = self.state.lock();
                if state.shutdown_committed {
                    return WorkerQuiescenceResult::Busy {
                        active_jobs: state.active_jobs,
                    };
                }
                if state.request_id != Some(request_id) || !state.closed {
                    return WorkerQuiescenceResult::Busy {
                        active_jobs: state.active_jobs,
                    };
                }
                if state.active_jobs == 0 {
                    drop(state);
                    return WorkerQuiescenceResult::Quiesced { active_jobs: 0 };
                }
            }
            if tokio::time::timeout_at(deadline, notified).await.is_err() {
                return self.timeout_quiescence(request_id);
            }
        }
    }

    fn timeout_quiescence(&self, request_id: [u8; 32]) -> WorkerQuiescenceResult {
        let mut state = self.state.lock();
        if state.request_id == Some(request_id) {
            let active_jobs = state.active_jobs;
            if !state.shutdown_committed {
                state.closed = false;
                state.request_id = None;
                drop(state);
                self.changed.notify_waiters();
            }
            return WorkerQuiescenceResult::Busy { active_jobs };
        }
        WorkerQuiescenceResult::Busy {
            active_jobs: state.active_jobs,
        }
    }

    pub fn resume(&self, request_id: [u8; 32]) -> Result<usize, WorkerResumeError> {
        let mut state = self.state.lock();
        if state.shutdown_committed {
            return Err(WorkerResumeError::ShutdownCommitted);
        }
        if !state.closed {
            return Ok(state.active_jobs);
        }
        if state.request_id != Some(request_id) {
            return Err(WorkerResumeError::NoMatchingDrain);
        }
        state.closed = false;
        state.request_id = None;
        state.shutdown_committed = false;
        let active_jobs = state.active_jobs;
        drop(state);
        self.changed.notify_waiters();
        Ok(active_jobs)
    }

    pub fn commit_shutdown(&self) {
        let mut state = self.state.lock();
        state.closed = true;
        state.shutdown_committed = true;
        drop(state);
        self.changed.notify_waiters();
    }

    fn expire_lease(&self, request_id: [u8; 32], generation: u64) {
        let mut state = self.state.lock();
        if !state.closed
            || state.shutdown_committed
            || state.request_id != Some(request_id)
            || state.lease_generation != generation
        {
            return;
        }
        state.closed = false;
        state.request_id = None;
        drop(state);
        self.changed.notify_waiters();
    }

    pub fn active_jobs(&self) -> usize {
        self.state.lock().active_jobs
    }

    pub fn is_closed(&self) -> bool {
        self.state.lock().closed
    }
}

const NATIVE_READINESS_MAX_AGE: std::time::Duration = std::time::Duration::from_secs(5);
const NATIVE_BUSY_MAX_AGE: std::time::Duration = std::time::Duration::from_secs(60 * 60);
const NATIVE_COHORT_READINESS_GRACE: std::time::Duration = std::time::Duration::from_secs(30);

impl NativeRequestAdmission {
    pub fn new(operator_enabled: bool) -> Self {
        Self {
            operator_enabled,
            ..Self::default()
        }
    }

    pub fn operator_enabled(&self) -> bool {
        self.operator_enabled
    }

    pub fn runtime_ready(&self) -> bool {
        if !self.running.load(Ordering::Acquire) || !self.policy_allows_requests() {
            return false;
        }
        let health = self.health.lock();
        self.running.load(Ordering::Acquire)
            && health.healthy_at.is_some_and(|at| {
                health.busy_since.map_or_else(
                    || at.elapsed() < NATIVE_READINESS_MAX_AGE,
                    |since| since.elapsed() < NATIVE_BUSY_MAX_AGE,
                )
            })
            && health
                .unavailable_since
                .is_none_or(|since| since.elapsed() < NATIVE_COHORT_READINESS_GRACE)
    }

    pub fn accepting_requests(&self) -> bool {
        self.operator_enabled && self.runtime_ready()
    }

    pub fn allows(&self, tx: &arc_types::Transaction) -> bool {
        !matches!(tx.body, arc_types::TxBody::NativeInferenceRequest(_))
            || self.accepting_requests()
    }

    pub fn worker_execution_gate(&self) -> Arc<WorkerExecutionGate> {
        self.worker_execution.clone()
    }

    pub(crate) fn retain_for_proposal(&self, transactions: &mut Vec<arc_types::Transaction>) {
        transactions.retain(|tx| self.allows(tx));
    }

    pub(crate) fn insert(
        &self,
        mempool: &arc_mempool::Mempool,
        tx: arc_types::Transaction,
    ) -> bool {
        self.allows(&tx) && mempool.insert(tx).is_ok()
    }

    fn record_health(&self, ready: bool) {
        *self.health.lock() = NativeRuntimeHealth {
            healthy_at: ready.then(std::time::Instant::now),
            ..NativeRuntimeHealth::default()
        };
    }

    fn policy_allows_requests(&self) -> bool {
        self.policy.lock().as_ref().is_none_or(|policy| {
            let (Some(state), Some(executor)) = (policy.state.upgrade(), policy.executor.upgrade())
            else {
                return false;
            };
            state
                .try_native_inference_context()
                .ok()
                .flatten()
                .is_some_and(|context| executor.policy_allows_requests(&context, state.height()))
        })
    }

    fn record_readiness(&self, ready: bool) {
        if !self.policy_allows_requests() {
            self.record_health(false);
            return;
        }
        let mut health = self.health.lock();
        health.busy_since = None;
        let now = std::time::Instant::now();
        if ready {
            health.healthy_at = Some(now);
            health.unavailable_since = None;
        } else {
            let since = *health.unavailable_since.get_or_insert(now);
            if health.healthy_at.is_some() && since.elapsed() < NATIVE_COHORT_READINESS_GRACE {
                health.healthy_at = Some(now);
            } else {
                health.healthy_at = None;
            }
        }
    }

    fn begin_poll(&self) {
        self.health.lock().busy_since = Some(std::time::Instant::now());
    }

    fn stopped(&self) {
        self.running.store(false, Ordering::Release);
        self.record_health(false);
    }

    #[cfg(test)]
    pub(crate) fn ready_for_test() -> Self {
        let gate = Self::new(true);
        gate.running.store(true, Ordering::Release);
        gate.record_health(true);
        gate
    }
}

struct NativeReadinessGuard(Arc<NativeRequestAdmission>);
impl Drop for NativeReadinessGuard {
    fn drop(&mut self) {
        self.0.stopped();
    }
}

pub struct NativeRuntimeHandle {
    cancel: Arc<AtomicBool>,
    join: Option<std::thread::JoinHandle<()>>,
    admission: Arc<NativeRequestAdmission>,
}

impl NativeRuntimeHandle {
    pub fn shutdown(mut self) {
        self.admission.stopped();
        self.cancel.store(true, Ordering::Release);
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }

    pub fn is_running(&self) -> bool {
        !self.cancel.load(Ordering::Acquire)
            && self.join.as_ref().is_some_and(|join| !join.is_finished())
    }
}

impl Drop for NativeRuntimeHandle {
    fn drop(&mut self) {
        self.admission.stopped();
        self.cancel.store(true, Ordering::Release);
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct NativeRuntimeBounds {
    pub idle_interval: std::time::Duration,
    pub error_backoff: std::time::Duration,
    pub max_consecutive_errors: Option<u32>,
}

impl Default for NativeRuntimeBounds {
    fn default() -> Self {
        Self {
            idle_interval: std::time::Duration::from_millis(500),
            error_backoff: std::time::Duration::from_secs(2),
            max_consecutive_errors: Some(60),
        }
    }
}

pub fn spawn_native_runtime<E, G, V>(
    runtime: NativeWorkerRuntime<E, G, V>,
    bounds: NativeRuntimeBounds,
    cancel: Arc<AtomicBool>,
) -> NativeRuntimeHandle
where
    E: NativeExecutor + Send + Sync + 'static,
    G: VoteSigner + Send + Sync + 'static,
    V: VoteSink + Send + Sync + 'static,
{
    spawn_native_runtime_with_admission(
        runtime,
        bounds,
        cancel,
        Arc::new(NativeRequestAdmission::default()),
    )
}

pub fn spawn_native_runtime_with_admission<E, G, V>(
    runtime: NativeWorkerRuntime<E, G, V>,
    bounds: NativeRuntimeBounds,
    cancel: Arc<AtomicBool>,
    admission: Arc<NativeRequestAdmission>,
) -> NativeRuntimeHandle
where
    E: NativeExecutor + Send + Sync + 'static,
    G: VoteSigner + Send + Sync + 'static,
    V: VoteSink + Send + Sync + 'static,
{
    let flag = cancel.clone();
    let worker_admission = admission.clone();
    let executor: Arc<dyn NativeExecutor> = runtime.worker.executor.clone();
    *admission.policy.lock() = Some(NativeAdmissionPolicy {
        state: Arc::downgrade(&runtime.state),
        executor: Arc::downgrade(&executor),
    });
    let join = std::thread::Builder::new()
        .name("arc-native-worker".into())
        .spawn(move || {
            let _readiness = NativeReadinessGuard(worker_admission.clone());
            worker_admission.running.store(true, Ordering::Release);
            let mut consecutive_errors: u32 = 0;
            while !flag.load(Ordering::Acquire) {
                let Some(_execution_permit) = worker_admission.worker_execution.try_enter() else {
                    std::thread::sleep(bounds.idle_interval);
                    continue;
                };
                if consecutive_errors == 0 {
                    worker_admission.record_readiness(runtime.ready_for_requests());
                }
                worker_admission.begin_poll();
                match runtime.poll_once() {
                    Ok(Some(vote)) => {
                        worker_admission.record_readiness(runtime.ready_for_requests());
                        consecutive_errors = 0;
                        tracing::info!(
                            request = %vote.request_id.to_hex(),
                            "native worker produced a signed decision"
                        );
                    }
                    Ok(None) => {
                        worker_admission.record_readiness(runtime.ready_for_requests());
                        consecutive_errors = 0;
                        std::thread::sleep(bounds.idle_interval);
                    }
                    Err(error) => {
                        worker_admission.record_health(false);
                        consecutive_errors = consecutive_errors.saturating_add(1);
                        tracing::warn!(
                            %error,
                            consecutive_errors,
                            "native worker poll failed"
                        );
                        if let Some(limit) = bounds.max_consecutive_errors
                            && consecutive_errors >= limit
                        {
                            tracing::error!(
                                limit,
                                "native worker stopping after repeated failures; \
                                 the contract stays activated but no further work is done"
                            );
                            break;
                        }
                        std::thread::sleep(bounds.error_backoff);
                    }
                }
            }
            tracing::info!("native worker loop stopped");
        })
        .expect("spawning the native worker thread must succeed");
    NativeRuntimeHandle {
        cancel,
        join: Some(join),
        admission,
    }
}

#[derive(Debug)]
pub enum QualificationError {
    Missing,
    Unreadable(String),
    Malformed(String),
    NotQualified,
    IdentityMismatch {
        field: String,
        expected: String,
        found: String,
    },
    Incomplete(String),
}

impl std::fmt::Display for QualificationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Missing => write!(
                f,
                "real-model execution requires an explicit reference-qualification record \
                 (--native-inference-qualification). Matching model/profile/generation hashes \
                 establish execution identity, not qualification, and startup will not assume it. \
                 Use --native-inference-test-executor for protocol testing instead."
            ),
            Self::Unreadable(e) => write!(f, "cannot read qualification record: {e}"),
            Self::Malformed(e) => write!(f, "qualification record is not valid JSON: {e}"),
            Self::NotQualified => write!(
                f,
                "the qualification record states reference_generation_qualified = false; \
                 real-model execution stays unavailable"
            ),
            Self::IdentityMismatch {
                field,
                expected,
                found,
            } => write!(
                f,
                "the qualification record is bound to a different execution identity: {field} is \
                 {found} in the record but {expected} in the activated allowlist. A qualification \
                 decision is valid only for the identity it was made against."
            ),
            Self::Incomplete(what) => write!(
                f,
                "the qualification record is missing {what}; an explicit decision needs a named \
                 decider, a date, and the evidence it rests on"
            ),
        }
    }
}

impl std::error::Error for QualificationError {}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NativeQualificationRecord {
    pub model_hash: Hash256,
    pub profile_hash: Hash256,
    pub generation_hash: Hash256,
    pub reference_generation_qualified: bool,
    #[serde(default)]
    pub decided_by: String,
    #[serde(default)]
    pub decided_at: String,
    #[serde(default)]
    pub evidence: String,
    #[serde(default)]
    pub package_manifest_hash: Option<Hash256>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RealExecutionDecision {
    pub qualification: CanonicalI8Qualification,
    pub package_manifest_hash: Hash256,
}

pub fn resolve_real_execution_qualification(
    path: Option<&std::path::Path>,
    allowed: &AllowedExecution,
) -> Result<CanonicalI8Qualification, QualificationError> {
    resolve_real_execution_decision(path, allowed).map(|decision| decision.qualification)
}

pub fn resolve_real_execution_decision(
    path: Option<&std::path::Path>,
    allowed: &AllowedExecution,
) -> Result<RealExecutionDecision, QualificationError> {
    let path = path.ok_or(QualificationError::Missing)?;
    let raw = fs::read_to_string(path)
        .map_err(|e| QualificationError::Unreadable(format!("{}: {e}", path.display())))?;
    let record: NativeQualificationRecord =
        serde_json::from_str(&raw).map_err(|e| QualificationError::Malformed(e.to_string()))?;

    if !record.reference_generation_qualified {
        return Err(QualificationError::NotQualified);
    }
    for (field, expected, found) in [
        ("model_hash", allowed.model_hash, record.model_hash),
        ("profile_hash", allowed.profile_hash, record.profile_hash),
        (
            "generation_hash",
            allowed.generation_hash,
            record.generation_hash,
        ),
    ] {
        if expected != found {
            return Err(QualificationError::IdentityMismatch {
                field: field.to_string(),
                expected: expected.to_hex(),
                found: found.to_hex(),
            });
        }
    }
    let mut missing = Vec::new();
    if record.decided_by.trim().is_empty() {
        missing.push("decided_by");
    }
    if record.decided_at.trim().is_empty() {
        missing.push("decided_at");
    }
    if record.evidence.trim().is_empty() {
        missing.push("evidence");
    }
    if record.package_manifest_hash.is_none() {
        missing.push("package_manifest_hash");
    }
    let Some(package_manifest_hash) = record.package_manifest_hash.filter(|_| missing.is_empty())
    else {
        return Err(QualificationError::Incomplete(missing.join(", ")));
    };

    Ok(RealExecutionDecision {
        qualification: CanonicalI8Qualification {
            artifact_hash: record.model_hash,
            profile_hash: record.profile_hash,
            generation_hash: record.generation_hash,
            reference_generation_qualified: true,
        },
        package_manifest_hash,
    })
}

#[derive(Debug)]
pub enum ActivationConfigError {
    Unreadable(String),
    Malformed(String),
    InvalidContext(String),
    ChainNotEmpty(u64),
    StateUnavailable(String),
    ExpectationMismatch {
        field: String,
        expected: String,
        actual: String,
    },
    ReconfigurationRejected {
        field: String,
        persisted: String,
        requested: String,
    },
    Refused(String),
    MigrationPending {
        at: u64,
        height: u64,
    },
    BindingUpdatePending {
        at: u64,
        height: u64,
    },
}

impl std::fmt::Display for ActivationConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unreadable(e) => write!(f, "cannot read activation config: {e}"),
            Self::Malformed(e) => write!(f, "activation config is not valid JSON: {e}"),
            Self::InvalidContext(e) => {
                write!(f, "assembled activation context failed validation: {e}")
            }
            Self::ChainNotEmpty(h) => write!(
                f,
                "native inference activation requires an unused private genesis, but this chain \
                 is at height {h}. It cannot be enabled on an existing chain."
            ),
            Self::StateUnavailable(e) => {
                write!(f, "node state cannot supply the activation domain: {e}")
            }
            Self::ExpectationMismatch {
                field,
                expected,
                actual,
            } => write!(
                f,
                "activation config pinned {field}={expected} but this node has {actual}. \
                 Refusing rather than activating a context the operator did not approve."
            ),
            Self::ReconfigurationRejected {
                field,
                persisted,
                requested,
            } => write!(
                f,
                "native inference is already activated and cannot be reconfigured: {field} is \
                 {persisted} in the persisted binding but {requested} in this config. Restart \
                 with the original activation file, or start a new private genesis."
            ),
            Self::Refused(e) => write!(f, "activation refused: {e}"),
            Self::MigrationPending { at, height } => write!(
                f,
                "migration authorised for height {at}; this chain is at {height} and will \
                 activate when it gets there"
            ),
            Self::BindingUpdatePending { at, height } => write!(
                f,
                "binding update authorised for height {at}; this chain is at {height} and the \
                 binding in force is unchanged until it gets there"
            ),
        }
    }
}

impl std::error::Error for ActivationConfigError {}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActivationExpectations {
    #[serde(default)]
    pub chain_genesis: Option<Hash256>,
    #[serde(default)]
    pub validator_set_hash: Option<Hash256>,
    #[serde(default)]
    pub members: Option<Vec<ValidatorMember>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NativeActivationRequest {
    pub allowed_executions: Vec<AllowedExecution>,
    #[serde(default)]
    pub recovery_epoch: u64,
    #[serde(default)]
    pub expect: Option<ActivationExpectations>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selection_rule: Option<arc_state::NativeSelectionRule>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub migration: Option<MigrationAuthorization>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub update_from: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub update_at_height: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MigrationAuthorization {
    pub chain_genesis: String,
    pub recovery_epoch: u64,
    pub validator_set_id: u64,
    pub activation_height: u64,
    pub context_commitment: String,
}

fn hash_from_hex(field: &str, value: &str) -> Result<Hash256, ActivationConfigError> {
    let bytes = hex::decode(value).map_err(|error| {
        ActivationConfigError::Malformed(format!("migration.{field} is not hex: {error}"))
    })?;
    let bytes: [u8; 32] = bytes.as_slice().try_into().map_err(|_| {
        ActivationConfigError::Malformed(format!(
            "migration.{field} must be 32 bytes, got {}",
            bytes.len()
        ))
    })?;
    Ok(Hash256(bytes))
}

pub fn load_activation_request(
    path: &std::path::Path,
) -> Result<NativeActivationRequest, ActivationConfigError> {
    let raw = fs::read_to_string(path)
        .map_err(|e| ActivationConfigError::Unreadable(format!("{}: {e}", path.display())))?;
    serde_json::from_str(&raw).map_err(|e| ActivationConfigError::Malformed(e.to_string()))
}

pub fn assemble_activation_context(
    state: &arc_state::StateDB,
    request: &NativeActivationRequest,
) -> Result<InferenceAdmissionContext, ActivationConfigError> {
    let dir = state
        .persistence_dir()
        .ok_or_else(|| ActivationConfigError::StateUnavailable("state is not persistent".into()))?;
    let binding = dir.join("genesis.network-hash");
    let chain_genesis = fs::read_to_string(&binding)
        .map_err(|e| ActivationConfigError::StateUnavailable(format!("{}: {e}", binding.display())))
        .and_then(|v| {
            Hash256::from_hex(v.trim()).map_err(|e| {
                ActivationConfigError::StateUnavailable(format!("invalid genesis binding: {e}"))
            })
        })?;

    let mut members: Vec<ValidatorMember> = state
        .active_validators()
        .into_iter()
        .map(|(address, stake)| ValidatorMember { address, stake })
        .collect();
    members.sort_by_key(|m| m.address.0);
    if members.is_empty() {
        return Err(ActivationConfigError::StateUnavailable(
            "this node has no active validators, so there is no committee to bind. A stake-0 \
             observer cannot activate the contract."
                .into(),
        ));
    }
    let validator_set_hash = validator_set_commitment(&members)
        .map_err(|e| ActivationConfigError::InvalidContext(format!("{e:?}")))?;

    if let Some(expect) = &request.expect {
        if let Some(pin) = expect.chain_genesis
            && pin != chain_genesis
        {
            return Err(ActivationConfigError::ExpectationMismatch {
                field: "chain_genesis".into(),
                expected: pin.to_hex(),
                actual: chain_genesis.to_hex(),
            });
        }
        if let Some(pin) = expect.validator_set_hash
            && pin != validator_set_hash
        {
            return Err(ActivationConfigError::ExpectationMismatch {
                field: "validator_set_hash".into(),
                expected: pin.to_hex(),
                actual: validator_set_hash.to_hex(),
            });
        }
        if let Some(pin) = &expect.members
            && pin != &members
        {
            return Err(ActivationConfigError::ExpectationMismatch {
                field: "members".into(),
                expected: format!("{} member(s)", pin.len()),
                actual: format!("{} member(s)", members.len()),
            });
        }
    }

    let context = InferenceAdmissionContext {
        domain: InferenceDomain {
            chain_genesis,
            recovery_epoch: request.recovery_epoch,
            validator_set_hash,
        },
        members,
        allowed_executions: request.allowed_executions.clone(),
        selection_rule: request
            .selection_rule
            .unwrap_or(arc_state::NativeSelectionRule::SkipUsedNoncesV2),
    };
    context
        .commitment()
        .map_err(|e| ActivationConfigError::InvalidContext(e.to_string()))?;
    Ok(context)
}

pub fn activate_native_inference_from_config(
    state: &arc_state::StateDB,
    path: &std::path::Path,
) -> Result<Hash256, ActivationConfigError> {
    let request = load_activation_request(path)?;

    if let Some(persisted) = state.native_inference_context() {
        return resume_persisted_activation(state, &request, &persisted);
    }

    let height = state.height();
    let context = assemble_activation_context(state, &request)?;

    if let Some(migration) = &request.migration {
        let record = arc_state::NativeMigrationRecord {
            chain_genesis: hash_from_hex("chain_genesis", &migration.chain_genesis)?,
            recovery_epoch: migration.recovery_epoch,
            validator_set_id: migration.validator_set_id,
            activation_height: migration.activation_height,
            context_commitment: hash_from_hex("context_commitment", &migration.context_commitment)?,
        };
        state
            .authorize_native_migration(record, context.clone())
            .map_err(|e| ActivationConfigError::Refused(e.to_string()))?;
        if height != migration.activation_height {
            return Err(ActivationConfigError::MigrationPending {
                at: migration.activation_height,
                height,
            });
        }
        return state
            .activate_native_inference(context)
            .map_err(|e| ActivationConfigError::Refused(e.to_string()));
    }

    if height != 0 {
        return Err(ActivationConfigError::ChainNotEmpty(height));
    }
    state
        .activate_native_inference(context)
        .map_err(|e| ActivationConfigError::Refused(e.to_string()))
}

fn resume_persisted_activation(
    state: &arc_state::StateDB,
    request: &NativeActivationRequest,
    persisted: &InferenceAdmissionContext,
) -> Result<Hash256, ActivationConfigError> {
    let mismatch = |field: &str, persisted_value: String, requested: String| {
        ActivationConfigError::ReconfigurationRejected {
            field: field.to_string(),
            persisted: persisted_value,
            requested,
        }
    };

    if request.recovery_epoch != persisted.domain.recovery_epoch {
        return Err(mismatch(
            "recovery_epoch",
            persisted.domain.recovery_epoch.to_string(),
            request.recovery_epoch.to_string(),
        ));
    }
    if let Some(rule) = request.selection_rule
        && rule != persisted.selection_rule
    {
        return Err(mismatch(
            "selection_rule",
            persisted.selection_rule.as_str().to_string(),
            rule.as_str().to_string(),
        ));
    }
    if let Some(replacing) = &request.update_from {
        let replacing = hash_from_hex("update_from", replacing)?;
        let current = persisted
            .commitment()
            .map_err(|e| ActivationConfigError::InvalidContext(e.to_string()))?;
        if replacing != current {
            return Err(mismatch(
                "update_from",
                current.to_hex(),
                replacing.to_hex(),
            ));
        }
        let Some(at) = request.update_at_height else {
            return Err(ActivationConfigError::Malformed(
                "update_from requires update_at_height: the coordinated block at which every \
                 validator publishes the new binding"
                    .into(),
            ));
        };
        let next = assemble_activation_context(state, request)?;
        let published = next
            .commitment()
            .map_err(|e| ActivationConfigError::InvalidContext(e.to_string()))?;
        state
            .authorize_binding_update(at, replacing, next)
            .map_err(|e| ActivationConfigError::Refused(e.to_string()))?;
        state
            .apply_due_binding_update()
            .map_err(|e| ActivationConfigError::Refused(e.to_string()))?;
        return match state.native_inference_context() {
            Some(active) if active.commitment().ok() == Some(published) => Ok(published),
            _ => Err(ActivationConfigError::BindingUpdatePending {
                at,
                height: state.height(),
            }),
        };
    }
    if request.allowed_executions != persisted.allowed_executions {
        return Err(mismatch(
            "allowed_executions",
            format!("{} entry/entries", persisted.allowed_executions.len()),
            format!("{} entry/entries", request.allowed_executions.len()),
        ));
    }

    if let Some(expect) = &request.expect {
        if let Some(pin) = expect.chain_genesis
            && pin != persisted.domain.chain_genesis
        {
            return Err(mismatch(
                "expect.chain_genesis",
                persisted.domain.chain_genesis.to_hex(),
                pin.to_hex(),
            ));
        }
        if let Some(pin) = expect.validator_set_hash
            && pin != persisted.domain.validator_set_hash
        {
            return Err(mismatch(
                "expect.validator_set_hash",
                persisted.domain.validator_set_hash.to_hex(),
                pin.to_hex(),
            ));
        }
        if let Some(pin) = &expect.members
            && pin != &persisted.members
        {
            return Err(mismatch(
                "expect.members",
                format!("{} member(s)", persisted.members.len()),
                format!("{} member(s)", pin.len()),
            ));
        }
    }

    state
        .activate_native_inference(persisted.clone())
        .map_err(|e| ActivationConfigError::Refused(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use arc_state::{AllowedExecution, InferenceAdmissionContext, StateDB};
    use arc_types::inference_contract::{
        INFERENCE_CONTRACT_VERSION, InferenceDomain, InferenceJob, InferenceRequest,
        ValidatorMember, validator_set_commitment,
    };
    use arc_types::transaction::NativeInferenceRequestBody;
    use tempfile::tempdir;
    struct Source(PendingJob);
    impl PendingSource for Source {
        fn load_pending(&self, id: Hash256) -> Result<PendingJob, NativeInferenceError> {
            if id == self.0.request_id {
                Ok(self.0.clone())
            } else {
                Err(NativeInferenceError::Source("missing".into()))
            }
        }
    }
    struct Exec;
    impl NativeExecutor for Exec {
        fn execute(&self, _: &PendingJob) -> Result<ExecutionOutput, NativeInferenceError> {
            Ok(ExecutionOutput {
                tokens: vec![1, 2],
                output_hash: token_hash(&[1, 2]),
            })
        }
    }
    fn test_signer() -> &'static Arc<KeyPair> {
        static SIGNER: std::sync::OnceLock<Arc<KeyPair>> = std::sync::OnceLock::new();
        SIGNER.get_or_init(|| Arc::new(KeyPair::generate_ed25519()))
    }

    struct Sign;
    impl VoteSigner for Sign {
        fn sign(&self, decision: &DecisionMaterial) -> Result<InferenceVote, NativeInferenceError> {
            sign_vote(
                decision.request_id,
                &token_bytes(&decision.tokens),
                test_signer(),
            )
            .map_err(|error| NativeInferenceError::Signer(error.to_string()))
        }
    }
    struct Sink;
    impl VoteSink for Sink {
        fn emit(&self, _: &StoredVote) -> Result<(), NativeInferenceError> {
            Ok(())
        }
    }
    fn job() -> PendingJob {
        PendingJob {
            request_id: Hash256([1; 32]),
            genesis: Hash256([2; 32]),
            context: Hash256([3; 32]),
            expires_at: 100,
            artifact: Hash256([4; 32]),
            generation_semantics: "test.synthetic.v1".into(),
            model_hash: Hash256([5; 32]),
            profile_hash: Hash256([6; 32]),
            input_hash: arc_crypto::hash_bytes(b"input"),
            generation_hash: Hash256([7; 32]),
            assignment_hash: Hash256([8; 32]),
            max_tokens: 2,
            max_output_bytes: 128,
            input: b"input".to_vec(),
        }
    }

    fn policy_executor_fixture() -> CanonicalI8NativeExecutor {
        use arc_inference::cached_integer_model::{
            ArithmeticProfile, CachedIntegerModel, I8Weights, ModelConfig,
        };
        let empty = || I8Weights {
            data: vec![],
            scales: vec![],
            n_rows: 0,
            n_cols: 0,
        };
        let model = Arc::new(CachedIntegerModel {
            config: ModelConfig {
                n_layers: 0,
                d_model: 2,
                n_heads: 1,
                n_kv_heads: 1,
                d_ff: 2,
                d_head: 2,
                d_kv: 2,
                vocab_size: 2,
                attn_scale: 1,
                rope_cos: vec![],
                rope_sin: vec![],
                max_seq: 8,
                eos_tokens: vec![],
                bos_token: 1,
                chat_template: String::new(),
                arithmetic_profile: ArithmeticProfile::GgufInterleavedRowsV1,
            },
            embedding_q16: vec![],
            embedding_i8: empty(),
            layers: vec![],
            final_norm: vec![],
            output_weight: empty(),
            vocab: vec![],
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
        });
        CanonicalI8NativeExecutor {
            model: crate::row_cohort::CoordinatorModel::Resident(model),
            qualification: CanonicalI8Qualification {
                artifact_hash: job().artifact,
                profile_hash: job().profile_hash,
                generation_hash: job().generation_hash,
                reference_generation_qualified: false,
            },
            kv_budget_bytes: DEFAULT_NATIVE_KV_BUDGET_BYTES,
            row_cohort: None,
        }
    }

    #[test]
    fn canonical_assignment_policy_refuses_mismatched_jobs_and_closes_readiness() {
        let mut executor = policy_executor_fixture();
        let mut j = job();
        j.model_hash = j.artifact;
        let mut context = InferenceAdmissionContext {
            domain: InferenceDomain {
                chain_genesis: j.genesis,
                recovery_epoch: 0,
                validator_set_hash: Hash256::ZERO,
            },
            members: vec![],
            allowed_executions: vec![AllowedExecution {
                model_hash: j.model_hash,
                profile_hash: j.profile_hash,
                generation_hash: j.generation_hash,
                assignment_hash: j.assignment_hash,
            }],
            selection_rule: arc_state::NativeSelectionRule::SkipUsedNoncesV2,
        };
        assert!(!executor.ready_for_requests(&context, 1));
        assert!(
            matches!(executor.execute_at(&j, 1), Err(NativeInferenceError::Executor(e)) if e.contains("assignment policy"))
        );
        j.assignment_hash = local_canonical_assignment_hash();
        context.allowed_executions[0].assignment_hash = j.assignment_hash;
        executor.validate_job(&j).unwrap();
        assert!(executor.ready_for_requests(&context, 1));
        let config = crate::row_cohort::RowCohortConfig {
            workers: vec![],
            partial_rows: None,
            max_workers: 4,
            duplicate_per_mille: 50,
            spot_rows_per_stage: 2,
        };
        let crate::row_cohort::CoordinatorModel::Resident(model) = &executor.model else {
            unreachable!()
        };
        let cohort = crate::row_cohort::RowCohort::disconnected_policy_fixture(
            config.clone(),
            model.clone(),
            j.artifact,
        );
        let cohort_policy = cohort.assignment_hash();
        assert_ne!(cohort_policy, j.assignment_hash);
        assert_ne!(cohort_policy, config.assignment_hash(true));
        executor.row_cohort = Some(cohort.clone());
        assert!(!executor.ready_for_requests(&context, 1));
        assert!(
            matches!(executor.execute_at(&j, 1), Err(NativeInferenceError::Executor(e)) if e.contains("assignment policy"))
        );
        assert!(
            cohort
                .generate(j.request_id, j.assignment_hash, 100, 1, &[1], 1, &[])
                .is_err()
        );
        j.assignment_hash = cohort_policy;
        context.allowed_executions[0].assignment_hash = cohort_policy;
        executor.validate_job(&j).unwrap();
        assert!(executor.ready_for_requests(&context, 1));
        assert!(
            matches!(executor.execute(&j), Err(NativeInferenceError::Executor(e)) if e.contains("chain height"))
        );
        for change in [0, 1, 2] {
            let mut changed = config.clone();
            match change {
                0 => changed.max_workers += 1,
                1 => changed.duplicate_per_mille += 1,
                _ => changed.spot_rows_per_stage += 1,
            }
            assert_ne!(changed.assignment_hash(false), cohort_policy);
        }
    }

    #[test]
    fn mismatched_assignment_cannot_reemit_or_replace_a_durable_vote() {
        struct CountSink(std::sync::atomic::AtomicUsize);
        impl VoteSink for CountSink {
            fn emit(&self, _: &StoredVote) -> Result<(), NativeInferenceError> {
                self.0.fetch_add(1, Ordering::SeqCst);
                Ok(())
            }
        }
        let dir = tempdir().unwrap();
        let mut j = job();
        j.model_hash = j.artifact;
        let store =
            DecisionStore::open(dir.path(), test_signer().address(), j.genesis, j.context).unwrap();
        let saved = StoredVote {
            request_id: j.request_id,
            output_hash: token_hash(&[1, 2]),
            tokens: vec![1, 2],
            vote: sign_vote(j.request_id, &token_bytes(&[1, 2]), test_signer()).unwrap(),
        };
        store.persist_signed(saved.clone(), &j).unwrap();
        let path = store.path(j.context, j.request_id);
        let bytes = fs::read(&path).unwrap();
        let sink = Arc::new(CountSink(std::sync::atomic::AtomicUsize::new(0)));
        let worker = NativeWorker::new(
            Arc::new(Source(j.clone())),
            Arc::new(policy_executor_fixture()),
            Arc::new(Sign),
            sink.clone(),
            store,
        );
        assert!(
            matches!(worker.run_request(j.request_id, 1), Err(NativeInferenceError::Executor(e)) if e.contains("assignment policy"))
        );
        assert_eq!(sink.0.load(Ordering::SeqCst), 0);
        assert_eq!(fs::read(&path).unwrap(), bytes);
        assert_eq!(
            worker.store.load_existing_valid(&j).unwrap().unwrap(),
            saved
        );

        let dir = tempdir().unwrap();
        j.assignment_hash = local_canonical_assignment_hash();
        j.request_id = hash_bytes(b"matching-policy-replay-fixture");
        let saved = StoredVote {
            request_id: j.request_id,
            output_hash: token_hash(&[1, 2]),
            tokens: vec![1, 2],
            vote: sign_vote(j.request_id, &token_bytes(&[1, 2]), test_signer()).unwrap(),
        };
        let store =
            DecisionStore::open(dir.path(), test_signer().address(), j.genesis, j.context).unwrap();
        store.persist_signed(saved.clone(), &j).unwrap();
        let path = store.path(j.context, j.request_id);
        let before = fs::read(&path).unwrap();
        let worker = NativeWorker::new(
            Arc::new(Source(j.clone())),
            Arc::new(policy_executor_fixture()),
            Arc::new(Sign),
            sink.clone(),
            store,
        );
        assert_eq!(worker.run_request(j.request_id, 1).unwrap(), saved);
        assert_eq!(sink.0.load(Ordering::SeqCst), 1);
        assert_eq!(fs::read(path).unwrap(), before);
    }

    #[test]
    fn a_job_whose_kv_cache_exceeds_the_budget_is_refused_before_execution() {
        assert_eq!(native_kv_bytes(1, 32, 4096), Some(2 << 20));
        let budget = DEFAULT_NATIVE_KV_BUDGET_BYTES;
        assert!(check_native_kv_budget(1023, 1024, 32, 4096, budget).is_ok());
        assert!(matches!(
            check_native_kv_budget(1024, 1024, 32, 4096, budget),
            Err(NativeInferenceError::Executor(message)) if message.contains("KV cache")
        ));
        assert!(check_native_kv_budget(2047, 2048, 32, 4096, budget).is_err());
        assert!(check_native_kv_budget(2047, 2048, 32, 4096, 8 << 30).is_ok());
        assert!(check_native_kv_budget(usize::MAX, 1, 32, 4096, u64::MAX).is_err());
        assert!(check_native_kv_budget(1499, 1500, 32, 4096, 6 << 30).is_err());
        assert!(check_native_kv_budget(1023, 1024, 32, 4096, 6 << 30).is_ok());
        let per_position = 2 << 20;
        assert_eq!(allocatable_kv_positions(budget, per_position, 4096), 2048);
        assert_eq!(allocatable_kv_positions(6 << 30, per_position, 4096), 2048);
        assert_eq!(allocatable_kv_positions(8 << 30, per_position, 4096), 4096);
        assert_eq!(allocatable_kv_positions(64 << 30, per_position, 4096), 4096);
        assert_eq!(allocatable_kv_positions(7 << 20, per_position, 4096), 2);
        assert_eq!(allocatable_kv_positions(1 << 20, per_position, 4096), 0);
        assert!(allocatable_kv_positions(7 << 20, per_position, 4096) < MIN_NATIVE_KV_POSITIONS);
    }

    #[test]
    fn a_real_execution_decision_pins_the_approved_package_manifest() {
        let dir = tempdir().unwrap();
        let tuple = hash_bytes(b"decision-tuple");
        let allowed = AllowedExecution {
            model_hash: tuple,
            profile_hash: tuple,
            generation_hash: tuple,
            assignment_hash: tuple,
        };
        let manifest = hash_bytes(b"approved package manifest");
        let record = |pin: Option<Hash256>| {
            let mut value = serde_json::json!({
                "model_hash": tuple.to_hex(),
                "profile_hash": tuple.to_hex(),
                "generation_hash": tuple.to_hex(),
                "reference_generation_qualified": true,
                "decided_by": "release engineering",
                "decided_at": "2026-09-22",
                "evidence": "reference comparison run 7",
            });
            if let Some(pin) = pin {
                value["package_manifest_hash"] = serde_json::json!(pin.to_hex());
            }
            let path = dir.path().join(format!("record-{}.json", pin.is_some()));
            std::fs::write(&path, value.to_string()).unwrap();
            path
        };
        let pinned = record(Some(manifest));
        let decision = resolve_real_execution_decision(Some(&pinned), &allowed).unwrap();
        assert_eq!(decision.package_manifest_hash, manifest);
        assert_eq!(decision.qualification.artifact_hash, tuple);
        assert!(decision.qualification.reference_generation_qualified);
        let unpinned = record(None);
        match resolve_real_execution_decision(Some(&unpinned), &allowed) {
            Err(QualificationError::Incomplete(missing)) => {
                assert!(missing.contains("package_manifest_hash"), "{missing}")
            }
            other => panic!("expected an incomplete decision, got {other:?}"),
        }
        assert!(resolve_real_execution_qualification(Some(&unpinned), &allowed).is_err());
    }

    #[test]
    fn a_node_without_the_canonical_tokenizer_never_tokenizes_or_decodes() {
        let serving = NativeServing::deterministic_test();
        assert_eq!(serving.executor.as_str(), "deterministic_test");
        assert_eq!(serving.executor.input_format(), "opaque_bytes");
        assert!(serving.tokenize_prompt("hello").is_err());
        assert_eq!(serving.decode_output(&[1, 0, 0, 0]), None);
        let canonical = NativeServing::canonical(None, Some(2_048));
        assert_eq!(canonical.max_positions, Some(2_048));
        assert_eq!(
            canonical.executor.input_format(),
            "le_u32_token_ids_without_bos"
        );
        assert!(canonical.tokenize_prompt("hello").is_err());
    }

    #[test]
    fn a_prompt_token_outside_the_vocabulary_is_refused_before_execution() {
        let prompt_bytes =
            |ids: &[u32]| -> Vec<u8> { ids.iter().flat_map(|id| id.to_le_bytes()).collect() };
        let mut j = job();
        j.input = prompt_bytes(&[5, 31_999]);
        assert_eq!(
            CanonicalI8NativeExecutor::prequalified_prompt(&j, 1, 32_000).unwrap(),
            vec![5, 31_999]
        );
        j.input = prompt_bytes(&[5, 32_000]);
        assert!(matches!(
            CanonicalI8NativeExecutor::prequalified_prompt(&j, 1, 32_000),
            Err(NativeInferenceError::Executor(message))
                if message.contains("outside the model vocabulary")
        ));
    }

    #[test]
    fn restart_and_conflict_are_idempotent() {
        let dir = tempdir().unwrap();
        let j = job();
        let v = StoredVote {
            request_id: j.request_id,
            output_hash: token_hash(&[1, 2]),
            tokens: vec![1, 2],
            vote: sign_vote(
                j.request_id,
                &token_bytes(&[1, 2]),
                &KeyPair::generate_ed25519(),
            )
            .unwrap(),
        };
        let s = DecisionStore::open(dir.path(), v.vote.validator, j.genesis, j.context).unwrap();
        assert_eq!(s.persist_signed(v.clone(), &j).unwrap(), v);
        drop(s);
        let s = DecisionStore::open(dir.path(), v.vote.validator, j.genesis, j.context).unwrap();
        assert_eq!(s.persist_signed(v.clone(), &j).unwrap(), v);
        let bad = StoredVote {
            vote: sign_vote(
                j.request_id,
                &token_bytes(&[1, 2]),
                &KeyPair::generate_ed25519(),
            )
            .unwrap(),
            ..v
        };
        assert!(s.persist_signed(bad, &j).is_err());
    }

    #[test]
    fn concurrent_conflicting_decisions_leave_one_durable_winner() {
        let dir = tempdir().unwrap();
        let key = KeyPair::generate_ed25519();
        let j = job();
        let make_vote = |token: u32| StoredVote {
            request_id: j.request_id,
            output_hash: token_hash(&[token]),
            tokens: vec![token],
            vote: sign_vote(j.request_id, &token_bytes(&[token]), &key).unwrap(),
        };
        let votes = [make_vote(11), make_vote(22)];
        let store = DecisionStore::open(dir.path(), key.address(), j.genesis, j.context).unwrap();
        let barrier = Arc::new(std::sync::Barrier::new(votes.len() + 1));
        let workers = votes
            .iter()
            .cloned()
            .map(|vote| {
                let store = store.clone();
                let j = j.clone();
                let barrier = Arc::clone(&barrier);
                std::thread::spawn(move || {
                    barrier.wait();
                    store.persist_signed(vote, &j)
                })
            })
            .collect::<Vec<_>>();
        barrier.wait();
        let results = workers
            .into_iter()
            .map(|worker| worker.join().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
        assert_eq!(
            results
                .iter()
                .filter(|result| matches!(result, Err(NativeInferenceError::Equivocation)))
                .count(),
            1
        );

        let winner = store.load(j.context, j.request_id).unwrap().unwrap();
        assert!(votes.iter().any(|vote| {
            vote.output_hash == winner.output_hash && vote.tokens == winner.tokens
        }));
        assert_eq!(
            fs::read(store.path(j.context, j.request_id)).unwrap(),
            bincode::serialize(&winner).unwrap(),
            "the durable record must be exactly one of the two conflicting winners"
        );
    }

    #[cfg(windows)]
    #[test]
    fn windows_rebarrier_refuses_mismatched_winner_without_mutating_it() {
        let dir = tempdir().unwrap();
        let key = KeyPair::generate_ed25519();
        let j = job();
        let vote = StoredVote {
            request_id: j.request_id,
            output_hash: token_hash(&[31]),
            tokens: vec![31],
            vote: sign_vote(j.request_id, &token_bytes(&[31]), &key).unwrap(),
        };
        let store = DecisionStore::open(dir.path(), key.address(), j.genesis, j.context).unwrap();
        store.persist_signed(vote, &j).unwrap();

        let path = store.path(j.context, j.request_id);
        let before = fs::read(&path).unwrap();
        let mut mismatched = store.load(j.context, j.request_id).unwrap().unwrap();
        mismatched.output_hash = token_hash(&[99]);
        assert!(matches!(
            store.sync_published(j.context, j.request_id, &mismatched),
            Err(NativeInferenceError::Equivocation)
        ));
        assert_eq!(fs::read(path).unwrap(), before);
    }

    #[test]
    fn a_decision_store_signs_for_the_binding_the_job_names() {
        let dir = tempdir().unwrap();
        let key = KeyPair::generate_ed25519();
        let mut opened_under = job();
        opened_under.context = Hash256([0xA1; 32]);
        let store = DecisionStore::open(
            dir.path(),
            key.address(),
            opened_under.genesis,
            opened_under.context,
        )
        .unwrap();

        let mut later = job();
        later.context = Hash256([0xB2; 32]);
        later.request_id = Hash256([0x33; 32]);
        let vote = StoredVote {
            request_id: later.request_id,
            output_hash: token_hash(&[7]),
            tokens: vec![7],
            vote: sign_vote(later.request_id, &token_bytes(&[7]), &key).unwrap(),
        };
        store
            .persist_signed(vote.clone(), &later)
            .expect("a worker must sign for the binding the job was admitted under");

        assert!(store.path(later.context, later.request_id).exists());
        assert!(!store.path(opened_under.context, later.request_id).exists());

        let mut elsewhere = later.clone();
        elsewhere.genesis = Hash256([0xCC; 32]);
        assert!(matches!(
            store.persist_signed(vote, &elsewhere),
            Err(NativeInferenceError::ContextMismatch)
        ));
    }

    #[test]
    fn only_settled_requests_decisions_are_pruned() {
        let dir = tempdir().unwrap();
        let key = KeyPair::generate_ed25519();
        let decide = |id: u8| {
            let mut j = job();
            j.request_id = Hash256([id; 32]);
            let v = StoredVote {
                request_id: j.request_id,
                output_hash: token_hash(&[id as u32]),
                tokens: vec![id as u32],
                vote: sign_vote(j.request_id, &token_bytes(&[id as u32]), &key).unwrap(),
            };
            (j, v)
        };
        let (j0, v0) = decide(10);
        let s = DecisionStore::open(dir.path(), v0.vote.validator, j0.genesis, j0.context).unwrap();
        let (settled, still_pending) = (decide(10), decide(11));
        s.persist_signed(settled.1.clone(), &settled.0).unwrap();
        s.persist_signed(still_pending.1.clone(), &still_pending.0)
            .unwrap();
        fs::write(dir.path().join("unreadable.decision"), b"bad").unwrap();

        let removed = s.prune_terminal(|id| id == settled.0.request_id).unwrap();
        assert_eq!(removed, 1);
        assert!(
            !s.path(settled.0.context, settled.0.request_id).exists(),
            "the settled request's file is gone"
        );
        assert!(
            s.path(still_pending.0.context, still_pending.0.request_id)
                .exists(),
            "a pending request keeps its record"
        );
        assert!(dir.path().join("unreadable.decision").exists());
        let conflicting = StoredVote {
            output_hash: token_hash(&[99]),
            tokens: vec![99],
            vote: sign_vote(still_pending.0.request_id, &token_bytes(&[99]), &key).unwrap(),
            ..still_pending.1.clone()
        };
        assert!(matches!(
            s.persist_signed(conflicting, &still_pending.0),
            Err(NativeInferenceError::Equivocation)
        ));
        let other =
            DecisionStore::open(dir.path(), Hash256([0xEE; 32]), j0.genesis, j0.context).unwrap();
        assert_eq!(other.prune_terminal(|_| true).unwrap(), 0);
        assert!(
            s.path(still_pending.0.context, still_pending.0.request_id)
                .exists()
        );
    }
    #[test]
    fn corrupt_store_is_rejected() {
        let dir = tempdir().unwrap();
        let j = job();
        let v = StoredVote {
            request_id: j.request_id,
            output_hash: token_hash(&[1]),
            tokens: vec![1],
            vote: sign_vote(
                j.request_id,
                &token_bytes(&[1]),
                &KeyPair::generate_ed25519(),
            )
            .unwrap(),
        };
        let s = DecisionStore::open(dir.path(), v.vote.validator, j.genesis, j.context).unwrap();
        fs::write(s.path(j.context, j.request_id), b"bad").unwrap();
        assert!(matches!(
            s.persist_signed(v, &j),
            Err(NativeInferenceError::CorruptStore)
        ));
    }
    #[test]
    fn cancellation_and_expiry_prevent_execution() {
        let dir = tempdir().unwrap();
        let j = job();
        let w = NativeWorker::new(
            Arc::new(Source(j.clone())),
            Arc::new(Exec),
            Arc::new(Sign),
            Arc::new(Sink),
            DecisionStore::open(dir.path(), test_signer().address(), j.genesis, j.context).unwrap(),
        );
        w.cancel();
        assert!(matches!(
            w.submit(j.request_id),
            Err(NativeInferenceError::Cancelled)
        ));

        let dir = tempdir().unwrap();
        let executor = Arc::new(CountingExec(std::sync::atomic::AtomicUsize::new(0)));
        let w = NativeWorker::new(
            Arc::new(Source(j.clone())),
            executor.clone(),
            Arc::new(Sign),
            Arc::new(Sink),
            DecisionStore::open(dir.path(), test_signer().address(), j.genesis, j.context).unwrap(),
        );
        for now in [j.expires_at, j.expires_at + 5] {
            w.submit(j.request_id).unwrap();
            assert!(matches!(w.run_one(now), Err(NativeInferenceError::Expired)));
        }
        assert_eq!(
            executor.0.load(Ordering::SeqCst),
            0,
            "an expired job never executes"
        );
    }

    struct Jobs(std::collections::HashMap<[u8; 32], PendingJob>);
    impl PendingSource for Jobs {
        fn load_pending(&self, id: Hash256) -> Result<PendingJob, NativeInferenceError> {
            self.0
                .get(&id.0)
                .cloned()
                .ok_or_else(|| NativeInferenceError::Source("missing".into()))
        }
    }
    struct RecordingExec(Mutex<Vec<Hash256>>);
    impl NativeExecutor for RecordingExec {
        fn execute(&self, job: &PendingJob) -> Result<ExecutionOutput, NativeInferenceError> {
            self.0.lock().push(job.request_id);
            Ok(ExecutionOutput {
                tokens: vec![1, 2],
                output_hash: token_hash(&[1, 2]),
            })
        }
    }
    fn numbered_worker(
        dir: &std::path::Path,
        count: u8,
    ) -> (
        NativeWorker<Jobs, RecordingExec, Sign, Sink>,
        Arc<RecordingExec>,
    ) {
        let base = job();
        let jobs = (1..=count)
            .map(|n| {
                let numbered = PendingJob {
                    request_id: Hash256([n; 32]),
                    ..base.clone()
                };
                (numbered.request_id.0, numbered)
            })
            .collect();
        let executor = Arc::new(RecordingExec(Mutex::new(Vec::new())));
        let worker = NativeWorker::new(
            Arc::new(Jobs(jobs)),
            executor.clone(),
            Arc::new(Sign),
            Arc::new(Sink),
            DecisionStore::open(dir, test_signer().address(), base.genesis, base.context).unwrap(),
        );
        (worker, executor)
    }

    #[test]
    fn requesters_take_turns_and_an_expired_request_never_runs() {
        let dir = tempdir().unwrap();
        let (w, executor) = numbered_worker(dir.path(), 5);
        let alice = Hash256([0xa1; 32]);
        let bob = Hash256([0xb0; 32]);
        for n in 1..=3 {
            w.submit_for(Hash256([n; 32]), alice, 100).unwrap();
        }
        w.submit_for(Hash256([4; 32]), bob, 100).unwrap();
        for _ in 0..4 {
            w.run_one(10).unwrap();
        }
        assert_eq!(
            *executor.0.lock(),
            vec![
                Hash256([1; 32]),
                Hash256([4; 32]),
                Hash256([2; 32]),
                Hash256([3; 32])
            ]
        );
        w.submit_for(Hash256([5; 32]), bob, 20).unwrap();
        assert!(matches!(
            w.run_one(20),
            Err(NativeInferenceError::QueueFull)
        ));
        assert!(w.is_idle());
        assert_eq!(
            executor.0.lock().len(),
            4,
            "the expired request never executed"
        );
        w.submit_for(Hash256([5; 32]), bob, 30).unwrap();
        w.run_one(21).unwrap();
        assert_eq!(executor.0.lock().last(), Some(&Hash256([5; 32])));
    }

    #[test]
    fn a_full_queue_is_backpressure_not_a_failure() {
        let dir = tempdir().unwrap();
        let count = (MAX_QUEUE + 1) as u8;
        let (w, executor) = numbered_worker(dir.path(), count);
        let requester = Hash256([0xc0; 32]);
        for n in 1..count {
            w.submit_for(Hash256([n; 32]), requester, 100).unwrap();
        }
        assert!(matches!(
            w.submit_for(Hash256([count; 32]), requester, 100),
            Err(NativeInferenceError::QueueFull)
        ));
        w.run_one(10).unwrap();
        w.submit_for(Hash256([count; 32]), requester, 100).unwrap();
        while !w.is_idle() {
            w.run_one(10).unwrap();
        }
        assert_eq!(executor.0.lock().len(), MAX_QUEUE + 1);
    }

    struct CountingExec(std::sync::atomic::AtomicUsize);
    impl NativeExecutor for CountingExec {
        fn execute(&self, _: &PendingJob) -> Result<ExecutionOutput, NativeInferenceError> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok(ExecutionOutput {
                tokens: vec![1, 2],
                output_hash: token_hash(&[1, 2]),
            })
        }
    }

    #[test]
    fn durable_vote_is_verified_and_reemitted_without_recompute() {
        let dir = tempdir().unwrap();
        let j = job();
        let key = KeyPair::generate_ed25519();
        let validator = key.address();
        let executor = Arc::new(CountingExec(std::sync::atomic::AtomicUsize::new(0)));
        let worker = NativeWorker::new(
            Arc::new(Source(j.clone())),
            executor.clone(),
            Arc::new(KeyPairVoteSigner::new(key)),
            Arc::new(Sink),
            DecisionStore::open(dir.path(), validator, j.genesis, j.context).unwrap(),
        );
        worker.submit(j.request_id).unwrap();
        worker.run_one(1).unwrap();
        worker.submit(j.request_id).unwrap();
        worker.run_one(1).unwrap();
        assert_eq!(executor.0.load(Ordering::SeqCst), 1);
    }

    struct FixtureExecutor;
    impl NativeExecutor for FixtureExecutor {
        fn ready_for_requests(&self, _: &InferenceAdmissionContext, _: u64) -> bool {
            true
        }
        fn execute(&self, _: &PendingJob) -> Result<ExecutionOutput, NativeInferenceError> {
            Ok(ExecutionOutput {
                tokens: vec![71, 72],
                output_hash: token_hash(&[71, 72]),
            })
        }
    }

    struct NativeFixture {
        directory: tempfile::TempDir,
        state_dir: PathBuf,
        genesis: Hash256,
        state: Arc<StateDB>,
        requester: KeyPair,
        finalizer: Arc<KeyPair>,
        validators: Vec<KeyPair>,
        context: InferenceAdmissionContext,
        prefunded: Vec<(Hash256, u64)>,
    }

    fn native_fixture() -> NativeFixture {
        let directory = tempdir().unwrap();
        let state_dir = directory.path().join("state");
        let genesis = hash_bytes(b"native-worker-synthetic-fixture-genesis");
        let requester = KeyPair::generate_ed25519();
        let finalizer = Arc::new(KeyPair::generate_ed25519());
        let validators: Vec<_> = (0..6).map(|_| KeyPair::generate_ed25519()).collect();
        let mut members: Vec<_> = validators
            .iter()
            .map(|key| ValidatorMember::new(key.address(), StateDB::MIN_VALIDATOR_STAKE))
            .collect();
        members.sort_by_key(|member| member.address.0);
        let marker = hash_bytes(b"native-worker-synthetic-fixture-tuple");
        let context = InferenceAdmissionContext {
            domain: InferenceDomain {
                chain_genesis: genesis,
                recovery_epoch: 0,
                validator_set_hash: validator_set_commitment(&members).unwrap(),
            },
            members: members.clone(),
            allowed_executions: vec![AllowedExecution {
                model_hash: marker,
                profile_hash: marker,
                generation_hash: marker,
                assignment_hash: marker,
            }],
            selection_rule: arc_state::NativeSelectionRule::SkipUsedNoncesV2,
        };
        let mut prefunded = vec![(requester.address(), 1_000), (finalizer.address(), 0)];
        prefunded.extend(members.iter().map(|member| (member.address, 0)));
        let state =
            Arc::new(StateDB::with_genesis_persistent(&prefunded, &state_dir, genesis).unwrap());
        state.seed_genesis_validators(
            &members
                .iter()
                .map(|member| (member.address, member.stake))
                .collect::<Vec<_>>(),
        );
        state.activate_native_inference(context.clone()).unwrap();
        NativeFixture {
            directory,
            state_dir,
            genesis,
            state,
            requester,
            finalizer,
            validators,
            context,
            prefunded,
        }
    }

    fn request_for(fixture: &NativeFixture, nonce: u64, expires_at: u64) -> InferenceRequest {
        let tuple = fixture.context.allowed_executions[0];
        let input = token_bytes(&[11, 12]);
        InferenceRequest::sign(
            InferenceJob {
                version: INFERENCE_CONTRACT_VERSION,
                domain: fixture.context.domain,
                requester: fixture.requester.address(),
                nonce,
                model_hash: tuple.model_hash,
                profile_hash: tuple.profile_hash,
                input_hash: hash_bytes(&input),
                generation_hash: tuple.generation_hash,
                assignment_hash: tuple.assignment_hash,
                max_tokens: 8,
                max_output_bytes: 128,
                execution_price: 10,
                reserved_max_payment: 100,
                expires_at,
            },
            &fixture.requester,
        )
        .unwrap()
    }

    fn signed_native(
        state: &StateDB,
        signer: &KeyPair,
        nonce: u64,
        body: TxBody,
        gas_limit: u64,
    ) -> Transaction {
        let mut transaction = Transaction {
            tx_type: body.tx_type(),
            from: signer.address(),
            nonce,
            body,
            fee: 0,
            gas_limit,
            hash: Hash256::ZERO,
            signature: Signature::null(),
            sig_verified: false,
        };
        state.sign_transaction(&mut transaction, signer).unwrap();
        transaction
    }

    fn marker() -> AllowedExecution {
        let m = hash_bytes(b"activation-config-test-marker");
        AllowedExecution {
            model_hash: m,
            profile_hash: m,
            generation_hash: m,
            assignment_hash: m,
        }
    }

    fn activatable_state(dir: &std::path::Path) -> (arc_state::StateDB, Vec<ValidatorMember>) {
        let genesis = hash_bytes(b"activation-config-test-genesis");
        let mut members: Vec<ValidatorMember> = (0u8..6)
            .map(|i| ValidatorMember {
                address: hash_bytes(&[i; 4]),
                stake: 1_000_000,
            })
            .collect();
        members.sort_by_key(|m| m.address.0);
        let prefunded: Vec<(Hash256, u64)> = members.iter().map(|m| (m.address, 0)).collect();
        let state = arc_state::StateDB::with_genesis_persistent(&prefunded, dir, genesis).unwrap();
        state.seed_genesis_validators(
            &members
                .iter()
                .map(|m| (m.address, m.stake))
                .collect::<Vec<_>>(),
        );
        (state, members)
    }

    fn write_request(path: &std::path::Path, req: &NativeActivationRequest) {
        fs::write(path, serde_json::to_string_pretty(req).unwrap()).unwrap();
    }

    fn running_state(
        dir: &std::path::Path,
        to_height: u64,
    ) -> (arc_state::StateDB, Vec<ValidatorMember>) {
        let (state, members) = activatable_state(dir);
        while state.height() < to_height {
            state
                .execute_block_adaptive_at(&[], members[0].address, 1_700_000 + state.height())
                .unwrap();
        }
        (state, members)
    }

    fn migration_request(
        genesis: Hash256,
        activation_height: u64,
        commitment: Hash256,
    ) -> NativeActivationRequest {
        NativeActivationRequest {
            allowed_executions: vec![marker()],
            recovery_epoch: 1,
            expect: None,
            selection_rule: None,
            migration: Some(MigrationAuthorization {
                chain_genesis: genesis.to_hex(),
                recovery_epoch: 1,
                validator_set_id: 0,
                activation_height,
                context_commitment: commitment.to_hex(),
            }),
            update_from: None,
            update_at_height: None,
        }
    }

    #[test]
    fn a_migration_config_is_parsed_and_decided_by_the_state() {
        let dir = tempfile::tempdir().unwrap();
        let (state, _) = running_state(&dir.path().join("state"), 4);
        let path = dir.path().join("activation.json");
        let genesis = hash_bytes(b"activation-config-test-genesis");
        write_request(&path, &migration_request(genesis, 5, Hash256::ZERO));
        match activate_native_inference_from_config(&state, &path) {
            Err(ActivationConfigError::Refused(reason)) => {
                assert!(reason.contains("recovery context"), "{reason}");
            }
            other => panic!("expected the state to refuse, got {other:?}"),
        }
        assert!(
            state.native_inference_context().is_none(),
            "a refused migration activates nothing"
        );
    }

    #[test]
    fn a_malformed_migration_identity_is_refused_by_field() {
        let dir = tempfile::tempdir().unwrap();
        let (state, _) = running_state(&dir.path().join("state"), 4);
        let path = dir.path().join("activation.json");
        let genesis = hash_bytes(b"activation-config-test-genesis");

        let mut bad_hex = migration_request(genesis, 5, Hash256::ZERO);
        bad_hex.migration.as_mut().unwrap().chain_genesis = "not hex".into();
        write_request(&path, &bad_hex);
        match activate_native_inference_from_config(&state, &path) {
            Err(ActivationConfigError::Malformed(reason)) => {
                assert!(reason.contains("chain_genesis"), "{reason}");
            }
            other => panic!("expected a malformed config, got {other:?}"),
        }

        let mut short = migration_request(genesis, 5, Hash256::ZERO);
        short.migration.as_mut().unwrap().context_commitment = "abcd".into();
        write_request(&path, &short);
        match activate_native_inference_from_config(&state, &path) {
            Err(ActivationConfigError::Malformed(reason)) => {
                assert!(reason.contains("context_commitment"), "{reason}");
                assert!(reason.contains("32 bytes"), "{reason}");
            }
            other => panic!("expected a malformed config, got {other:?}"),
        }
        assert!(state.native_inference_context().is_none());
    }

    #[test]
    fn without_a_migration_a_running_chain_is_still_refused() {
        let dir = tempfile::tempdir().unwrap();
        let (state, _) = running_state(&dir.path().join("state"), 4);
        let path = dir.path().join("activation.json");
        write_request(
            &path,
            &NativeActivationRequest {
                allowed_executions: vec![marker()],
                recovery_epoch: 1,
                expect: None,
                selection_rule: None,
                migration: None,
                update_from: None,
                update_at_height: None,
            },
        );
        assert!(matches!(
            activate_native_inference_from_config(&state, &path),
            Err(ActivationConfigError::ChainNotEmpty(4))
        ));
    }

    #[test]
    fn a_migration_section_round_trips_and_refuses_unknown_fields() {
        let genesis = hash_bytes(b"round-trip");
        let request = migration_request(genesis, 1_570_000, Hash256::ZERO);
        let json = serde_json::to_string_pretty(&request).unwrap();
        let back: NativeActivationRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(back.migration, request.migration);
        assert!(json.contains("\"activation_height\": 1570000"), "{json}");

        let mut value: serde_json::Value = serde_json::from_str(&json).unwrap();
        value["migration"]["activation_heigth"] = serde_json::json!(5);
        assert!(
            serde_json::from_value::<NativeActivationRequest>(value).is_err(),
            "a misspelled field must not be ignored"
        );
    }

    fn upgraded() -> AllowedExecution {
        let m = hash_bytes(b"activation-config-test-upgraded-model");
        AllowedExecution {
            model_hash: m,
            profile_hash: m,
            generation_hash: m,
            assignment_hash: m,
        }
    }

    #[test]
    fn a_pinned_update_publishes_a_new_binding() {
        let dir = tempfile::tempdir().unwrap();
        let (state, _) = activatable_state(&dir.path().join("state"));
        let path = dir.path().join("activation.json");
        let base = NativeActivationRequest {
            allowed_executions: vec![marker()],
            recovery_epoch: 0,
            expect: None,
            selection_rule: None,
            migration: None,
            update_from: None,
            update_at_height: None,
        };
        write_request(&path, &base);
        let v1 = activate_native_inference_from_config(&state, &path).expect("fresh activation");

        let mut changed = base.clone();
        changed.allowed_executions.push(upgraded());
        write_request(&path, &changed);
        match activate_native_inference_from_config(&state, &path) {
            Err(ActivationConfigError::ReconfigurationRejected { field, .. }) => {
                assert_eq!(field, "allowed_executions");
            }
            other => panic!("expected a reconfiguration refusal, got {other:?}"),
        }
        assert_eq!(
            state.native_inference_context().unwrap().allowed_executions,
            vec![marker()],
            "a refusal changes nothing"
        );

        let mut wrong = changed.clone();
        wrong.update_from = Some(Hash256::ZERO.to_hex());
        write_request(&path, &wrong);
        match activate_native_inference_from_config(&state, &path) {
            Err(ActivationConfigError::ReconfigurationRejected { field, .. }) => {
                assert_eq!(field, "update_from");
            }
            other => panic!("expected a pin refusal, got {other:?}"),
        }
        assert_eq!(
            state.native_inference_context().unwrap().allowed_executions,
            vec![marker()]
        );

        let mut unscheduled = changed.clone();
        unscheduled.update_from = Some(v1.to_hex());
        write_request(&path, &unscheduled);
        match activate_native_inference_from_config(&state, &path) {
            Err(ActivationConfigError::Malformed(reason)) => {
                assert!(reason.contains("update_at_height"), "{reason}");
            }
            other => panic!("expected a missing-height refusal, got {other:?}"),
        }

        let mut pinned = changed;
        pinned.update_from = Some(v1.to_hex());
        pinned.update_at_height = Some(2);
        write_request(&path, &pinned);
        match activate_native_inference_from_config(&state, &path) {
            Err(ActivationConfigError::BindingUpdatePending { at, height }) => {
                assert_eq!((at, height), (2, 0));
            }
            other => panic!("expected a pending update, got {other:?}"),
        }
        assert_eq!(
            state.native_inference_context().unwrap().allowed_executions,
            vec![marker()],
            "the binding in force is unchanged until the coordinated height"
        );

        let producer = state.native_inference_context().unwrap().members[0].address;
        while state.height() < 2 {
            state
                .execute_block_adaptive_at(&[], producer, 1_700_000 + state.height())
                .unwrap();
        }
        let active = state.native_inference_context().unwrap();
        assert_eq!(
            active.allowed_executions,
            vec![marker(), upgraded()],
            "the new configuration governs new work from its coordinated height"
        );
        assert_ne!(active.commitment().unwrap(), v1);

        match activate_native_inference_from_config(&state, &path) {
            Err(ActivationConfigError::ReconfigurationRejected { field, .. }) => {
                assert_eq!(field, "update_from");
            }
            other => panic!("expected a pin refusal on replay, got {other:?}"),
        }
    }

    #[test]
    fn activation_request_round_trips_and_is_hex_readable() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("activation.json");
        let req = NativeActivationRequest {
            allowed_executions: vec![marker()],
            recovery_epoch: 0,
            expect: Some(ActivationExpectations {
                chain_genesis: Some(hash_bytes(b"pin")),
                ..Default::default()
            }),
            selection_rule: None,
            migration: None,
            update_from: None,
            update_at_height: None,
        };
        write_request(&path, &req);
        let raw = fs::read_to_string(&path).unwrap();
        assert!(
            raw.contains(&hash_bytes(b"pin").to_hex()),
            "hashes must serialise as hex so the file is reviewable"
        );
        assert_eq!(load_activation_request(&path).unwrap(), req);
    }

    #[test]
    fn activation_request_rejects_unreadable_and_malformed_files() {
        let dir = tempfile::tempdir().unwrap();
        assert!(matches!(
            load_activation_request(&dir.path().join("nope.json")),
            Err(ActivationConfigError::Unreadable(_))
        ));
        let bad = dir.path().join("bad.json");
        fs::write(&bad, "{ not json").unwrap();
        assert!(matches!(
            load_activation_request(&bad),
            Err(ActivationConfigError::Malformed(_))
        ));
    }

    #[test]
    fn activation_assembles_the_domain_from_state_and_activates() {
        let dir = tempfile::tempdir().unwrap();
        let (state, members) = activatable_state(&dir.path().join("state"));
        let path = dir.path().join("activation.json");
        write_request(
            &path,
            &NativeActivationRequest {
                allowed_executions: vec![marker()],
                recovery_epoch: 0,
                expect: None,
                selection_rule: None,
                migration: None,
                update_from: None,
                update_at_height: None,
            },
        );

        assert!(
            state.native_inference_context().is_none(),
            "inert before activation"
        );
        let commitment =
            activate_native_inference_from_config(&state, &path).expect("activation must succeed");

        let active = state
            .native_inference_context()
            .expect("context must be readable back");
        assert_eq!(
            active.members, members,
            "members must come from the node's own registry"
        );
        assert_eq!(
            active.allowed_executions,
            vec![marker()],
            "allowlist is the operator's input"
        );
        assert_eq!(
            active.domain.validator_set_hash,
            validator_set_commitment(&members).unwrap(),
            "validator-set hash must be computed, not transcribed"
        );
        assert_eq!(commitment, active.commitment().unwrap());
    }

    #[test]
    fn activation_refuses_when_an_operator_pin_does_not_match() {
        let dir = tempfile::tempdir().unwrap();
        let (state, _) = activatable_state(&dir.path().join("state"));
        let path = dir.path().join("activation.json");
        write_request(
            &path,
            &NativeActivationRequest {
                allowed_executions: vec![marker()],
                recovery_epoch: 0,
                expect: Some(ActivationExpectations {
                    chain_genesis: Some(hash_bytes(b"a genesis this node does not have")),
                    ..Default::default()
                }),
                selection_rule: None,
                migration: None,
                update_from: None,
                update_at_height: None,
            },
        );
        match activate_native_inference_from_config(&state, &path) {
            Err(ActivationConfigError::ExpectationMismatch { field, .. }) => {
                assert_eq!(field, "chain_genesis");
            }
            other => panic!("expected ExpectationMismatch, got {other:?}"),
        }
        assert!(
            state.native_inference_context().is_none(),
            "a refused activation must leave the contract inert"
        );
    }

    #[test]
    fn activation_refuses_an_empty_allowlist_and_a_committee_less_node() {
        let dir = tempfile::tempdir().unwrap();
        let (state, _) = activatable_state(&dir.path().join("state"));
        let path = dir.path().join("empty.json");
        write_request(
            &path,
            &NativeActivationRequest {
                allowed_executions: vec![],
                recovery_epoch: 0,
                expect: None,
                selection_rule: None,
                migration: None,
                update_from: None,
                update_at_height: None,
            },
        );
        assert!(
            matches!(
                activate_native_inference_from_config(&state, &path),
                Err(ActivationConfigError::InvalidContext(_))
            ),
            "the existing allowlist bound must still apply"
        );

        let obs_dir = dir.path().join("observer");
        let observer = arc_state::StateDB::with_genesis_persistent(
            &[(hash_bytes(b"x"), 0)],
            &obs_dir,
            hash_bytes(b"observer-genesis"),
        )
        .unwrap();
        write_request(
            &path,
            &NativeActivationRequest {
                allowed_executions: vec![marker()],
                recovery_epoch: 0,
                expect: None,
                selection_rule: None,
                migration: None,
                update_from: None,
                update_at_height: None,
            },
        );
        assert!(matches!(
            activate_native_inference_from_config(&observer, &path),
            Err(ActivationConfigError::StateUnavailable(_))
        ));
    }

    #[test]
    fn activation_resumes_after_a_committed_block_with_the_same_config() {
        let dir = tempfile::tempdir().unwrap();
        let (state, members) = activatable_state(&dir.path().join("state"));
        let path = dir.path().join("activation.json");
        write_request(
            &path,
            &NativeActivationRequest {
                allowed_executions: vec![marker()],
                recovery_epoch: 0,
                expect: None,
                selection_rule: None,
                migration: None,
                update_from: None,
                update_at_height: None,
            },
        );

        let first = activate_native_inference_from_config(&state, &path).expect("fresh activation");
        state
            .execute_block_verified_at(&[], Hash256::ZERO, 1)
            .expect("empty block must apply");
        assert_eq!(state.height(), 1, "the chain must have moved past genesis");

        let resumed = activate_native_inference_from_config(&state, &path)
            .expect("a restart with the SAME config must resume, not abort");
        assert_eq!(
            first, resumed,
            "the commitment must be unchanged across restart"
        );
        assert_eq!(
            state.native_inference_context().unwrap().members,
            members,
            "the frozen binding must survive the restart intact"
        );
    }

    #[test]
    fn activation_restart_refuses_a_changed_allowlist_or_pin() {
        let dir = tempfile::tempdir().unwrap();
        let (state, _) = activatable_state(&dir.path().join("state"));
        let path = dir.path().join("activation.json");
        let original = NativeActivationRequest {
            allowed_executions: vec![marker()],
            recovery_epoch: 0,
            expect: None,
            selection_rule: None,
            migration: None,
            update_from: None,
            update_at_height: None,
        };
        write_request(&path, &original);
        activate_native_inference_from_config(&state, &path).expect("fresh activation");
        state
            .execute_block_verified_at(&[], Hash256::ZERO, 1)
            .unwrap();

        let other = hash_bytes(b"a different allowed execution");
        write_request(
            &path,
            &NativeActivationRequest {
                allowed_executions: vec![AllowedExecution {
                    model_hash: other,
                    profile_hash: other,
                    generation_hash: other,
                    assignment_hash: other,
                }],
                recovery_epoch: 0,
                expect: None,
                selection_rule: None,
                migration: None,
                update_from: None,
                update_at_height: None,
            },
        );
        match activate_native_inference_from_config(&state, &path) {
            Err(ActivationConfigError::ReconfigurationRejected { field, .. }) => {
                assert_eq!(field, "allowed_executions");
            }
            other => panic!("expected ReconfigurationRejected, got {other:?}"),
        }

        write_request(
            &path,
            &NativeActivationRequest {
                allowed_executions: vec![marker()],
                recovery_epoch: 0,
                expect: Some(ActivationExpectations {
                    chain_genesis: Some(hash_bytes(b"not this chain")),
                    ..Default::default()
                }),
                selection_rule: None,
                migration: None,
                update_from: None,
                update_at_height: None,
            },
        );
        match activate_native_inference_from_config(&state, &path) {
            Err(ActivationConfigError::ReconfigurationRejected { field, .. }) => {
                assert_eq!(field, "expect.chain_genesis");
            }
            other => panic!("expected ReconfigurationRejected, got {other:?}"),
        }
    }

    #[test]
    fn the_selection_rule_is_fixed_at_activation_and_resumed_unchanged() {
        let dir = tempfile::tempdir().unwrap();
        let (state, _) = activatable_state(&dir.path().join("state"));
        let path = dir.path().join("activation.json");
        let v1 = NativeActivationRequest {
            allowed_executions: vec![marker()],
            recovery_epoch: 0,
            expect: None,
            selection_rule: Some(arc_state::NativeSelectionRule::CountEveryCandidateV1),
            migration: None,
            update_from: None,
            update_at_height: None,
        };
        write_request(&path, &v1);
        activate_native_inference_from_config(&state, &path).expect("fresh activation");
        assert_eq!(
            state.native_inference_context().unwrap().selection_rule,
            arc_state::NativeSelectionRule::CountEveryCandidateV1
        );
        write_request(
            &path,
            &NativeActivationRequest {
                selection_rule: None,
                ..v1.clone()
            },
        );
        activate_native_inference_from_config(&state, &path).expect("resume keeps rule v1");
        write_request(
            &path,
            &NativeActivationRequest {
                selection_rule: Some(arc_state::NativeSelectionRule::SkipUsedNoncesV2),
                ..v1
            },
        );
        match activate_native_inference_from_config(&state, &path) {
            Err(ActivationConfigError::ReconfigurationRejected { field, .. }) => {
                assert_eq!(field, "selection_rule");
            }
            other => panic!("expected ReconfigurationRejected, got {other:?}"),
        }

        let fresh_dir = tempfile::tempdir().unwrap();
        let (fresh, _) = activatable_state(&fresh_dir.path().join("state"));
        let fresh_path = fresh_dir.path().join("activation.json");
        write_request(
            &fresh_path,
            &NativeActivationRequest {
                allowed_executions: vec![marker()],
                recovery_epoch: 0,
                expect: None,
                selection_rule: None,
                migration: None,
                update_from: None,
                update_at_height: None,
            },
        );
        activate_native_inference_from_config(&fresh, &fresh_path).expect("fresh activation");
        assert_eq!(
            fresh.native_inference_context().unwrap().selection_rule,
            arc_state::NativeSelectionRule::SkipUsedNoncesV2
        );
    }

    #[test]
    fn activation_restart_never_absorbs_a_drifted_committee() {
        let dir = tempfile::tempdir().unwrap();
        let (state, original_members) = activatable_state(&dir.path().join("state"));
        let path = dir.path().join("activation.json");
        write_request(
            &path,
            &NativeActivationRequest {
                allowed_executions: vec![marker()],
                recovery_epoch: 0,
                expect: None,
                selection_rule: None,
                migration: None,
                update_from: None,
                update_at_height: None,
            },
        );
        activate_native_inference_from_config(&state, &path).expect("fresh activation");

        let intruder = hash_bytes(b"validator that joined after activation");
        let mut drifted: Vec<(Hash256, u64)> = original_members
            .iter()
            .map(|m| (m.address, m.stake))
            .collect();
        drifted.push((intruder, 1_000_000));
        drifted.sort_by_key(|(a, _)| a.0);
        state.seed_genesis_validators(&drifted);

        let block = state.execute_block_verified_at(&[], Hash256::ZERO, 1);
        assert!(
            block.is_err(),
            "block execution must refuse to proceed under a drifted committee, got {block:?}"
        );

        let outcome = activate_native_inference_from_config(&state, &path);
        assert!(
            outcome.is_err(),
            "a drifted committee must refuse the restart, not absorb the new set"
        );

        let active = state.native_inference_context().unwrap();
        assert_eq!(
            active.members, original_members,
            "the persisted binding must NOT absorb the drifted committee"
        );
        assert!(
            !active.members.iter().any(|m| m.address == intruder),
            "a validator that joined after activation must never enter the frozen binding"
        );
    }

    #[test]
    fn a_restart_with_a_drifted_genesis_committee_is_refused_with_the_difference_and_a_remedy() {
        let dir = tempfile::tempdir().unwrap();
        let (state, original_members) = activatable_state(&dir.path().join("state"));
        let path = dir.path().join("activation.json");
        write_request(
            &path,
            &NativeActivationRequest {
                allowed_executions: vec![marker()],
                recovery_epoch: 0,
                expect: None,
                selection_rule: None,
                migration: None,
                update_from: None,
                update_at_height: None,
            },
        );
        activate_native_inference_from_config(&state, &path).expect("fresh activation");
        let bound = state.native_inference_context().unwrap().members;
        let as_genesis = |members: &[ValidatorMember]| -> Vec<(Hash256, u64)> {
            members.iter().map(|m| (m.address, m.stake)).collect()
        };

        let mut shuffled = as_genesis(&original_members);
        shuffled.reverse();
        arc_state::inference_contract_state::genesis_committee_matches_binding(&shuffled, &bound)
            .expect("the same committee must be accepted");

        let intruder = hash_bytes(b"validator that joined after activation");
        let mut drifted = as_genesis(&original_members);
        let removed = drifted.remove(0).0;
        drifted[0].1 += 5;
        let changed = drifted[0].0;
        drifted.push((intruder, 1_000_000));
        let message = arc_state::inference_contract_state::genesis_committee_matches_binding(
            &drifted, &bound,
        )
        .expect_err("a drifted committee must be refused");
        assert!(
            message.contains(&format!("added {}", intruder.to_hex())),
            "{message}"
        );
        assert!(
            message.contains(&format!("removed {}", removed.to_hex())),
            "{message}"
        );
        assert!(message.contains(&changed.to_hex()), "{message}");
        assert!(
            message.contains("Restart with a genesis file that matches the current binding"),
            "{message}"
        );
        assert!(
            message.contains("A committee change IS supported"),
            "{message}"
        );

        assert_eq!(
            state.native_inference_context().unwrap().members,
            original_members
        );
        activate_native_inference_from_config(&state, &path)
            .expect("the unchanged state still resumes its binding");
    }

    #[test]
    fn a_registry_change_is_refused_by_the_executor_while_a_binding_is_active() {
        let dir = tempfile::tempdir().unwrap();
        let (state, _) = activatable_state(&dir.path().join("state"));
        let path = dir.path().join("activation.json");
        write_request(
            &path,
            &NativeActivationRequest {
                allowed_executions: vec![marker()],
                recovery_epoch: 0,
                expect: None,
                selection_rule: None,
                migration: None,
                update_from: None,
                update_at_height: None,
            },
        );
        assert!(state.refuse_registry_change_under_native_binding().is_ok());
        activate_native_inference_from_config(&state, &path).expect("fresh activation");
        assert!(state.refuse_registry_change_under_native_binding().is_err());
    }

    #[test]
    fn staking_is_unaffected_on_a_chain_without_a_native_binding() {
        let dir = tempfile::tempdir().unwrap();
        let (state, _) = activatable_state(&dir.path().join("state"));
        assert!(state.native_inference_context().is_none());
        assert!(state.refuse_registry_change_under_native_binding().is_ok());
    }

    #[test]
    fn activation_refuses_a_chain_that_is_not_a_fresh_genesis() {
        let dir = tempfile::tempdir().unwrap();
        let (state, _) = activatable_state(&dir.path().join("state"));
        let path = dir.path().join("activation.json");
        write_request(
            &path,
            &NativeActivationRequest {
                allowed_executions: vec![marker()],
                recovery_epoch: 0,
                expect: None,
                selection_rule: None,
                migration: None,
                update_from: None,
                update_at_height: None,
            },
        );
        state
            .execute_block_verified_at(&[], Hash256::ZERO, 1)
            .expect("empty block must apply");
        assert_eq!(state.height(), 1);

        match activate_native_inference_from_config(&state, &path) {
            Err(ActivationConfigError::ChainNotEmpty(h)) => assert_eq!(h, 1),
            other => panic!("expected ChainNotEmpty(1), got {other:?}"),
        }
        assert!(
            state.native_inference_context().is_none(),
            "must stay inert"
        );
    }

    #[test]
    fn synthetic_private_signed_mempool_worker_finalize_and_reopen() {
        let mut fixture = native_fixture();
        let request = request_for(&fixture, 0, 10);
        let request_id = request.job.request_id();
        let mempool = Arc::new(Mempool::new(16));
        let request_tx = signed_native(
            &fixture.state,
            &fixture.requester,
            0,
            TxBody::NativeInferenceRequest(NativeInferenceRequestBody {
                request,
                input_blob: token_bytes(&[11, 12]),
            }),
            gas_costs::NATIVE_INFERENCE_REQUEST,
        );
        mempool.insert(request_tx).unwrap();
        fixture
            .state
            .execute_block_verified(&mempool.drain(1), fixture.finalizer.address())
            .unwrap();

        let sink = Arc::new(NativeFinalizeSink::new(
            fixture.state.clone(),
            mempool.clone(),
            fixture.finalizer.clone(),
        ));
        let commitment = fixture.context.commitment().unwrap();
        for index in 0..5 {
            let validator = fixture.validators.remove(0);
            let address = validator.address();
            let worker = NativeWorker::new(
                Arc::new(StatePendingSource::new(fixture.state.clone(), commitment)),
                Arc::new(FixtureExecutor),
                Arc::new(KeyPairVoteSigner::new(validator)),
                sink.clone(),
                DecisionStore::open(
                    fixture.directory.path().join(format!("vote-{index}")),
                    address,
                    fixture.genesis,
                    commitment,
                )
                .unwrap(),
            );
            worker.submit(request_id).unwrap();
            worker
                .run_one(fixture.state.height().saturating_add(1))
                .unwrap();
        }
        assert_eq!(
            mempool.len(),
            1,
            "5/6 synthetic validator votes produced one finalizer tx"
        );
        fixture
            .state
            .execute_block_verified(&mempool.drain(1), fixture.requester.address())
            .unwrap();
        let receipt = fixture
            .state
            .native_inference_receipt(request_id, commitment)
            .unwrap()
            .unwrap();
        assert_eq!(format!("{:?}", receipt.metadata.status), "Finalized");
        assert!(receipt.admission_transaction.is_some());
        assert!(receipt.terminal_transaction.is_some());

        let state_dir = fixture.state_dir.clone();
        let prefunded = fixture.prefunded.clone();
        let genesis = fixture.genesis;
        drop(sink);
        drop(fixture.state);
        let reopened = StateDB::with_genesis_persistent(&prefunded, state_dir, genesis).unwrap();
        let reopened_context = reopened.try_native_inference_context().unwrap().unwrap();
        assert!(
            reopened
                .native_inference_pending_requests(reopened_context.commitment().unwrap())
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            format!(
                "{:?}",
                reopened
                    .native_inference_receipt(request_id, reopened_context.commitment().unwrap())
                    .unwrap()
                    .unwrap()
                    .metadata
                    .status
            ),
            "Finalized"
        );
    }

    fn wait_until(check: impl Fn() -> bool) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
        while !check() {
            assert!(
                std::time::Instant::now() < deadline,
                "native runtime did not reach expected state"
            );
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    }

    #[test]
    fn request_gate_requires_opt_in_live_polls_and_closes_on_failure_panic_stop_and_restart() {
        struct Availability(std::sync::atomic::AtomicU8);
        impl NativeExecutor for Availability {
            fn ready_for_requests(&self, _: &InferenceAdmissionContext, _: u64) -> bool {
                match self.0.load(Ordering::Acquire) {
                    0 => true,
                    1 => false,
                    _ => panic!("synthetic dependency panic"),
                }
            }
            fn execute(&self, job: &PendingJob) -> Result<ExecutionOutput, NativeInferenceError> {
                FixtureExecutor.execute(job)
            }
        }
        let fixture = native_fixture();
        let availability = Arc::new(Availability(std::sync::atomic::AtomicU8::new(0)));
        let bounds = NativeRuntimeBounds {
            idle_interval: std::time::Duration::from_millis(5),
            error_backoff: std::time::Duration::from_millis(5),
            max_consecutive_errors: Some(1),
        };
        let start = |gate: Arc<NativeRequestAdmission>| {
            let key = fixture.validators[0].clone();
            let store = DecisionStore::open(
                fixture.directory.path().join("readiness-decisions"),
                key.address(),
                fixture.genesis,
                fixture.context.commitment().unwrap(),
            )
            .unwrap();
            let runtime = NativeWorkerRuntime::from_active(
                fixture.state.clone(),
                availability.clone(),
                Arc::new(KeyPairVoteSigner::new(key)),
                Arc::new(Sink),
                store,
            )
            .unwrap();
            spawn_native_runtime_with_admission(
                runtime,
                bounds,
                Arc::new(AtomicBool::new(false)),
                gate,
            )
        };
        let disabled = Arc::new(NativeRequestAdmission::default());
        let handle = start(disabled.clone());
        wait_until(|| disabled.runtime_ready());
        assert!(
            !disabled.accepting_requests(),
            "runtime health cannot open operator policy"
        );
        handle.shutdown();
        assert!(!disabled.runtime_ready());
        let enabled = Arc::new(NativeRequestAdmission::new(true));
        assert!(
            !enabled.accepting_requests(),
            "configured does not mean running"
        );
        let handle = start(enabled.clone());
        wait_until(|| enabled.accepting_requests());
        availability.0.store(1, Ordering::Release);
        wait_until(|| !enabled.runtime_ready());
        availability.0.store(0, Ordering::Release);
        wait_until(|| enabled.accepting_requests());
        availability.0.store(2, Ordering::Release);
        wait_until(|| !handle.is_running());
        assert!(!enabled.accepting_requests(), "unwinding clears readiness");
        drop(handle);
        let restarted = Arc::new(NativeRequestAdmission::new(true));
        assert!(
            !restarted.accepting_requests(),
            "restart never inherits health"
        );
        availability.0.store(0, Ordering::Release);
        let handle = start(restarted.clone());
        wait_until(|| restarted.accepting_requests());
        handle.shutdown();
        assert!(!restarted.accepting_requests());
        let stale = NativeRequestAdmission::ready_for_test();
        stale.health.lock().healthy_at = Some(std::time::Instant::now() - NATIVE_READINESS_MAX_AGE);
        assert!(
            !stale.accepting_requests(),
            "an idle worker's old poll expires"
        );
    }

    #[test]
    fn request_gate_keeps_busy_poll_ready_until_error_panic_or_execution_bound() {
        struct BlockingExecutor {
            started: std::sync::mpsc::Sender<()>,
            finish: Mutex<std::sync::mpsc::Receiver<bool>>,
        }
        impl NativeExecutor for BlockingExecutor {
            fn ready_for_requests(&self, _: &InferenceAdmissionContext, _: u64) -> bool {
                true
            }
            fn execute(&self, job: &PendingJob) -> Result<ExecutionOutput, NativeInferenceError> {
                self.started.send(()).unwrap();
                let panic = self
                    .finish
                    .lock()
                    .recv_timeout(std::time::Duration::from_secs(3))
                    .unwrap();
                assert!(!panic, "synthetic execution panic");
                FixtureExecutor.execute(job)
            }
        }
        struct FailedSink(std::sync::atomic::AtomicUsize);
        impl VoteSink for FailedSink {
            fn emit(&self, _: &StoredVote) -> Result<(), NativeInferenceError> {
                self.0.fetch_add(1, Ordering::Release);
                Err(NativeInferenceError::Sink("synthetic sink failure".into()))
            }
        }
        for panic in [false, true] {
            let fixture = native_fixture();
            admit_requests(&fixture, &[0]);
            let (started, executing) = std::sync::mpsc::channel();
            let (finish, released) = std::sync::mpsc::channel();
            let key = fixture.validators[0].clone();
            let store = DecisionStore::open(
                fixture.directory.path().join("busy-decisions"),
                key.address(),
                fixture.genesis,
                fixture.context.commitment().unwrap(),
            )
            .unwrap();
            let sink = Arc::new(FailedSink(std::sync::atomic::AtomicUsize::new(0)));
            let runtime = NativeWorkerRuntime::from_active(
                fixture.state.clone(),
                Arc::new(BlockingExecutor {
                    started,
                    finish: Mutex::new(released),
                }),
                Arc::new(KeyPairVoteSigner::new(key)),
                sink.clone(),
                store,
            )
            .unwrap();
            let gate = Arc::new(NativeRequestAdmission::new(true));
            let handle = spawn_native_runtime_with_admission(
                runtime,
                NativeRuntimeBounds {
                    error_backoff: std::time::Duration::from_millis(5),
                    max_consecutive_errors: None,
                    ..NativeRuntimeBounds::default()
                },
                Arc::new(AtomicBool::new(false)),
                gate.clone(),
            );
            executing
                .recv_timeout(std::time::Duration::from_secs(3))
                .unwrap();
            {
                let mut health = gate.health.lock();
                assert!(health.busy_since.is_some(), "the worker marks active polls");
                let old = std::time::Instant::now()
                    - NATIVE_READINESS_MAX_AGE
                    - std::time::Duration::from_secs(1);
                health.healthy_at = Some(old);
                health.busy_since = Some(old);
            }
            assert!(
                gate.accepting_requests(),
                "execution can outlast an idle poll"
            );
            if let Some(expired) = std::time::Instant::now().checked_sub(NATIVE_BUSY_MAX_AGE) {
                gate.health.lock().busy_since = Some(expired);
                assert!(!gate.accepting_requests(), "a hung poll eventually expires");
            }
            gate.health.lock().busy_since = Some(std::time::Instant::now());
            assert!(gate.accepting_requests());
            finish.send(panic).unwrap();
            if panic {
                wait_until(|| !handle.is_running());
                assert!(gate.health.lock().busy_since.is_none());
            } else {
                wait_until(|| sink.0.load(Ordering::Acquire) >= 2);
                assert!(
                    handle.is_running(),
                    "poll errors close admission before exit"
                );
            }
            assert!(!gate.accepting_requests());
            assert!(gate.health.lock().healthy_at.is_none());
            handle.shutdown();
        }
    }

    #[test]
    fn request_gate_cohort_grace_requires_health_and_continuous_failure() {
        let gate = NativeRequestAdmission::new(true);
        gate.running.store(true, Ordering::Release);
        gate.record_readiness(false);
        assert!(!gate.accepting_requests(), "a cold cohort has no grace");
        gate.record_readiness(true);
        gate.record_readiness(false);
        assert!(
            gate.accepting_requests(),
            "a transient failure retains health"
        );
        let old = std::time::Instant::now() - NATIVE_COHORT_READINESS_GRACE
            + std::time::Duration::from_secs(1);
        gate.health.lock().unavailable_since = Some(old);
        gate.record_readiness(false);
        assert!(gate.accepting_requests());
        assert_eq!(gate.health.lock().unavailable_since, Some(old));
        gate.record_readiness(true);
        assert!(gate.health.lock().unavailable_since.is_none());
        gate.record_readiness(false);
        assert!(
            gate.accepting_requests(),
            "recovery resets the grace window"
        );
        gate.begin_poll();
        gate.health.lock().unavailable_since =
            Some(std::time::Instant::now() - NATIVE_COHORT_READINESS_GRACE);
        assert!(
            !gate.accepting_requests(),
            "busy polls cannot extend cohort grace"
        );
        gate.record_readiness(false);
        gate.record_readiness(false);
        assert!(
            !gate.accepting_requests(),
            "continued failures never renew grace"
        );
        gate.record_readiness(true);
        assert!(gate.accepting_requests());
        gate.record_health(false);
        gate.record_readiness(false);
        assert!(
            !gate.accepting_requests(),
            "poll errors discard prior health"
        );
        gate.record_readiness(true);
        gate.begin_poll();
        gate.stopped();
        assert!(!gate.accepting_requests(), "stop overrides a busy poll");
        assert!(gate.health.lock().busy_since.is_none());
    }

    #[test]
    fn request_gate_policy_mismatch_overrides_busy_poll_and_cohort_grace() {
        let fixture = native_fixture();
        let executor = Arc::new(policy_executor_fixture());
        let mut context = fixture.context.clone();
        context.allowed_executions = vec![AllowedExecution {
            model_hash: executor.qualification.artifact_hash,
            profile_hash: executor.qualification.profile_hash,
            generation_hash: executor.qualification.generation_hash,
            assignment_hash: executor.assignment_hash().unwrap(),
        }];
        fixture
            .state
            .update_native_inference_binding(context.clone())
            .unwrap();
        let gate = NativeRequestAdmission::ready_for_test();
        let executor: Arc<dyn NativeExecutor> = executor;
        *gate.policy.lock() = Some(NativeAdmissionPolicy {
            state: Arc::downgrade(&fixture.state),
            executor: Arc::downgrade(&executor),
        });
        gate.record_readiness(false);
        gate.begin_poll();
        assert!(gate.accepting_requests());
        for field in 0..4 {
            let mut mismatch = context.clone();
            let allowed = &mut mismatch.allowed_executions[0];
            match field {
                0 => allowed.model_hash = Hash256::ZERO,
                1 => allowed.profile_hash = Hash256::ZERO,
                2 => allowed.generation_hash = Hash256::ZERO,
                _ => allowed.assignment_hash = Hash256::ZERO,
            }
            fixture
                .state
                .update_native_inference_binding(mismatch)
                .unwrap();
            assert!(
                !gate.accepting_requests(),
                "policy changes need no worker poll"
            );
            fixture
                .state
                .update_native_inference_binding(context.clone())
                .unwrap();
            assert!(gate.accepting_requests());
        }
        gate.record_health(false);
        gate.record_readiness(false);
        assert!(
            !gate.accepting_requests(),
            "policy alone cannot establish health"
        );
    }

    #[tokio::test]
    async fn worker_quiescence_closes_admission_drains_leases_and_resumes_only_matching_request() {
        let gate = Arc::new(WorkerExecutionGate::default());
        let request_id = [0x71; 32];
        let other_request = [0x72; 32];
        let permit = gate.try_enter().expect("open gate admits work");
        let gate_for_drain = gate.clone();
        let drain = tokio::spawn(async move {
            gate_for_drain
                .quiesce(
                    request_id,
                    std::time::Duration::from_secs(1),
                    tokio::time::Instant::now() + std::time::Duration::from_secs(5),
                )
                .await
        });
        tokio::task::yield_now().await;
        assert!(gate.is_closed());
        assert_eq!(gate.active_jobs(), 1);
        assert!(gate.try_enter().is_none(), "drain rejects new work");
        drop(permit);
        assert_eq!(
            drain.await.unwrap(),
            WorkerQuiescenceResult::Quiesced { active_jobs: 0 }
        );
        assert!(gate.try_enter().is_none(), "successful drain stays closed");
        assert_eq!(
            gate.resume(other_request),
            Err(WorkerResumeError::NoMatchingDrain)
        );
        assert_eq!(gate.resume(request_id), Ok(0));
        assert_eq!(
            gate.resume(request_id),
            Ok(0),
            "resume after expiry is idempotent"
        );
        assert!(gate.try_enter().is_some(), "matching resume reopens gate");
    }

    #[test]
    fn worker_execution_lease_survives_caller_cancellation_clone() {
        let gate = Arc::new(WorkerExecutionGate::default());
        let caller_permit = gate.try_enter().unwrap();
        let detached_compute_permit = caller_permit.clone();
        drop(caller_permit);
        assert_eq!(gate.active_jobs(), 1);
        drop(detached_compute_permit);
        assert_eq!(gate.active_jobs(), 0);
    }

    #[tokio::test]
    async fn worker_quiescence_timeout_reopens_admission() {
        let gate = Arc::new(WorkerExecutionGate::default());
        let _permit = gate.try_enter().unwrap();
        let result = gate
            .quiesce(
                [0x73; 32],
                std::time::Duration::from_millis(1),
                tokio::time::Instant::now() + std::time::Duration::from_secs(5),
            )
            .await;
        assert_eq!(result, WorkerQuiescenceResult::Busy { active_jobs: 1 });
        assert!(!gate.is_closed());
        assert_eq!(gate.active_jobs(), 1);
        assert!(gate.try_enter().is_some());
    }

    #[tokio::test]
    async fn worker_quiescence_lease_reopens_when_shutdown_never_begins() {
        let gate = Arc::new(WorkerExecutionGate::default());
        let result = gate
            .quiesce(
                [0x74; 32],
                std::time::Duration::from_secs(1),
                tokio::time::Instant::now() + std::time::Duration::from_millis(15),
            )
            .await;
        assert_eq!(result, WorkerQuiescenceResult::Quiesced { active_jobs: 0 });
        assert!(gate.is_closed());
        tokio::time::sleep(std::time::Duration::from_millis(30)).await;
        assert!(!gate.is_closed());
        assert!(gate.try_enter().is_some());
    }

    #[tokio::test]
    async fn canceled_pending_quiescence_reopens_at_lease_deadline() {
        let gate = Arc::new(WorkerExecutionGate::default());
        let _active = gate.try_enter().unwrap();
        let gate_for_drain = gate.clone();
        let drain = tokio::spawn(async move {
            gate_for_drain
                .quiesce(
                    [0x75; 32],
                    std::time::Duration::from_secs(1),
                    tokio::time::Instant::now() + std::time::Duration::from_millis(20),
                )
                .await
        });
        tokio::task::yield_now().await;
        assert!(gate.is_closed());
        // A second timer completing does not mean the spawned expiry task has
        // run (both timers can become ready during a scheduler stall). Register
        // before cancellation so even an immediate expiry cannot be missed.
        let reopened = gate.changed.notified();
        tokio::pin!(reopened);
        reopened.as_mut().enable();
        drain.abort();
        assert!(drain.await.unwrap_err().is_cancelled());
        tokio::time::timeout(std::time::Duration::from_secs(5), reopened)
            .await
            .expect("lease expiry must notify after canceled drain");
        assert!(!gate.is_closed());
        assert_eq!(
            gate.active_jobs(),
            1,
            "cancellation does not drop active work"
        );
        assert!(gate.try_enter().is_some(), "expired lease admits new work");
    }

    #[tokio::test]
    async fn authenticated_shutdown_commit_keeps_quiescence_closed_past_lease() {
        let gate = Arc::new(WorkerExecutionGate::default());
        let result = gate
            .quiesce(
                [0x76; 32],
                std::time::Duration::from_secs(1),
                tokio::time::Instant::now() + std::time::Duration::from_millis(20),
            )
            .await;
        assert_eq!(result, WorkerQuiescenceResult::Quiesced { active_jobs: 0 });
        gate.commit_shutdown();
        tokio::time::sleep(std::time::Duration::from_millis(35)).await;
        assert!(gate.is_closed());
        assert!(gate.try_enter().is_none());
        assert_eq!(
            gate.resume([0x76; 32]),
            Err(WorkerResumeError::ShutdownCommitted)
        );
    }

    #[tokio::test]
    async fn shutdown_during_active_drain_prevents_timeout_or_resume_reopening() {
        let gate = Arc::new(WorkerExecutionGate::default());
        let _active = gate.try_enter().unwrap();
        let request_id = [0x77; 32];
        let gate_for_drain = gate.clone();
        let drain = tokio::spawn(async move {
            gate_for_drain
                .quiesce(
                    request_id,
                    std::time::Duration::from_millis(40),
                    tokio::time::Instant::now() + std::time::Duration::from_secs(2),
                )
                .await
        });
        tokio::task::yield_now().await;
        assert!(gate.is_closed());

        gate.commit_shutdown();
        assert_eq!(
            drain.await.unwrap(),
            WorkerQuiescenceResult::Busy { active_jobs: 1 }
        );
        assert!(gate.is_closed());
        assert_eq!(
            gate.resume(request_id),
            Err(WorkerResumeError::ShutdownCommitted)
        );
        assert!(gate.try_enter().is_none());
    }

    #[test]
    fn committed_shutdown_at_active_drain_timeout_does_not_reopen_gate() {
        let gate = Arc::new(WorkerExecutionGate::default());
        let _active = gate.try_enter().unwrap();
        let request_id = [0x79; 32];
        {
            let mut state = gate.state.lock();
            state.closed = true;
            state.request_id = Some(request_id);
        }
        gate.commit_shutdown();
        assert_eq!(
            gate.timeout_quiescence(request_id),
            WorkerQuiescenceResult::Busy { active_jobs: 1 }
        );
        assert!(gate.is_closed());
        assert_eq!(
            gate.resume(request_id),
            Err(WorkerResumeError::ShutdownCommitted)
        );
        assert!(gate.try_enter().is_none());
    }

    #[tokio::test]
    async fn shutdown_commit_on_open_gate_is_terminal_and_rejects_quiesce() {
        let gate = Arc::new(WorkerExecutionGate::default());
        gate.commit_shutdown();
        assert!(gate.is_closed());
        assert!(gate.try_enter().is_none());
        assert_eq!(
            gate.quiesce(
                [0x78; 32],
                std::time::Duration::from_secs(1),
                tokio::time::Instant::now() + std::time::Duration::from_secs(2),
            )
            .await,
            WorkerQuiescenceResult::Busy { active_jobs: 0 }
        );
        assert_eq!(
            gate.resume([0x78; 32]),
            Err(WorkerResumeError::ShutdownCommitted)
        );
        assert!(gate.is_closed());
    }

    #[test]
    fn runtime_error_limit_closes_admission_without_discarding_canonical_pending_job() {
        struct FailedSink;
        impl VoteSink for FailedSink {
            fn emit(&self, _: &StoredVote) -> Result<(), NativeInferenceError> {
                Err(NativeInferenceError::Sink(
                    "synthetic unavailable sink".into(),
                ))
            }
        }
        let fixture = native_fixture();
        let ids = admit_requests(&fixture, &[0]);
        let key = fixture.validators[0].clone();
        let store = DecisionStore::open(
            fixture.directory.path().join("failed-sink-decisions"),
            key.address(),
            fixture.genesis,
            fixture.context.commitment().unwrap(),
        )
        .unwrap();
        let runtime = NativeWorkerRuntime::from_active(
            fixture.state.clone(),
            Arc::new(FixtureExecutor),
            Arc::new(KeyPairVoteSigner::new(key)),
            Arc::new(FailedSink),
            store,
        )
        .unwrap();
        let gate = Arc::new(NativeRequestAdmission::new(true));
        let handle = spawn_native_runtime_with_admission(
            runtime,
            NativeRuntimeBounds {
                max_consecutive_errors: Some(1),
                ..NativeRuntimeBounds::default()
            },
            Arc::new(AtomicBool::new(false)),
            gate.clone(),
        );
        wait_until(|| !handle.is_running());
        assert!(!gate.accepting_requests());
        assert_eq!(
            fixture
                .state
                .native_inference_pending_requests(fixture.context.commitment().unwrap())
                .unwrap()[0]
                .request_id,
            ids[0]
        );
        handle.shutdown();
    }

    fn admit_requests(fixture: &NativeFixture, nonces: &[u64]) -> Vec<Hash256> {
        nonces
            .iter()
            .map(|nonce| {
                let request = request_for(fixture, *nonce, 50);
                let id = request.job.request_id();
                let tx = signed_native(
                    &fixture.state,
                    &fixture.requester,
                    *nonce,
                    TxBody::NativeInferenceRequest(NativeInferenceRequestBody {
                        request,
                        input_blob: token_bytes(&[11, 12]),
                    }),
                    gas_costs::NATIVE_INFERENCE_REQUEST,
                );
                fixture
                    .state
                    .execute_block_verified(&[tx], fixture.finalizer.address())
                    .unwrap();
                id
            })
            .collect()
    }

    fn vote(request_id: Hash256, tokens: &[u32], key: &KeyPair) -> StoredVote {
        StoredVote {
            request_id,
            output_hash: token_hash(tokens),
            tokens: tokens.to_vec(),
            vote: sign_vote(request_id, &token_bytes(tokens), key).unwrap(),
        }
    }

    #[test]
    fn a_byzantine_vote_for_an_invented_output_cannot_block_the_honest_certificate() {
        let fixture = native_fixture();
        let [request_id] = admit_requests(&fixture, &[0])[..] else {
            unreachable!()
        };
        let mempool = Arc::new(Mempool::new(16));
        let sink = NativeFinalizeSink::new(
            fixture.state.clone(),
            mempool.clone(),
            fixture.finalizer.clone(),
        );
        let byzantine = &fixture.validators[5];
        sink.accept_peer_vote(&vote(request_id, &[99, 98], byzantine))
            .expect("a well-formed vote for another output is counted, not an error");
        for honest in &fixture.validators[..5] {
            sink.accept_peer_vote(&vote(request_id, &[71, 72], honest))
                .expect("an honest vote is never refused because of someone else's");
        }
        assert_eq!(
            mempool.len(),
            1,
            "the honest certificate produced one finalize"
        );
        assert!(matches!(
            sink.accept_peer_vote(&vote(request_id, &[71, 72], byzantine)),
            Err(NativeInferenceError::Equivocation)
        ));
        fixture
            .state
            .execute_block_verified(&mempool.drain(1), fixture.requester.address())
            .unwrap();
        let receipt = fixture
            .state
            .native_inference_receipt(request_id, fixture.context.commitment().unwrap())
            .unwrap()
            .unwrap();
        assert_eq!(format!("{:?}", receipt.metadata.status), "Finalized");
        assert_eq!(receipt.metadata.output, token_bytes(&[71, 72]));
    }

    #[test]
    fn a_peer_vote_without_its_signed_output_or_from_a_non_member_is_refused() {
        let fixture = native_fixture();
        let [request_id] = admit_requests(&fixture, &[0])[..] else {
            unreachable!()
        };
        let mempool = Arc::new(Mempool::new(16));
        let sink = NativeFinalizeSink::new(
            fixture.state.clone(),
            mempool.clone(),
            fixture.finalizer.clone(),
        );
        let member = &fixture.validators[0];
        let mut mismatched = vote(request_id, &[71, 72], member);
        mismatched.output_hash = token_hash(&[71, 73]);
        let mut unsigned_tokens = vote(request_id, &[71, 72], member);
        unsigned_tokens.tokens = vec![71, 73];
        unsigned_tokens.output_hash = token_hash(&[71, 73]);
        let outsider = KeyPair::generate_ed25519();
        for (label, bad) in [
            ("empty output", vote(request_id, &[], member)),
            (
                "output over max_output_bytes",
                vote(request_id, &[7; 33], member),
            ),
            ("output hash mismatch", mismatched),
            ("tokens the vote did not sign", unsigned_tokens),
            ("non-member", vote(request_id, &[71, 72], &outsider)),
        ] {
            assert!(
                matches!(
                    sink.accept_peer_vote(&bad),
                    Err(NativeInferenceError::Signer(_))
                ),
                "{label} must be refused"
            );
        }
        assert_eq!(mempool.len(), 0);
        sink.accept_peer_vote(&vote(request_id, &[71, 72], member))
            .expect("the member's well-formed vote still counts after refused ones");
    }

    struct RefuseOne {
        refused: Hash256,
        calls: Mutex<Vec<Hash256>>,
    }
    impl NativeExecutor for RefuseOne {
        fn execute(&self, job: &PendingJob) -> Result<ExecutionOutput, NativeInferenceError> {
            self.calls.lock().push(job.request_id);
            if job.request_id == self.refused {
                return Err(NativeInferenceError::Executor(
                    "native job needs more KV cache than this node's budget".into(),
                ));
            }
            Ok(ExecutionOutput {
                tokens: vec![1, 2],
                output_hash: token_hash(&[1, 2]),
            })
        }
    }

    #[test]
    fn a_refused_request_is_tried_once_and_never_stops_the_worker() {
        let fixture = native_fixture();
        let mut ids = Vec::new();
        for nonce in 0..2 {
            let request = request_for(&fixture, nonce, 1_000);
            ids.push(request.job.request_id());
            let tx = signed_native(
                &fixture.state,
                &fixture.requester,
                nonce,
                TxBody::NativeInferenceRequest(NativeInferenceRequestBody {
                    request,
                    input_blob: token_bytes(&[11, 12]),
                }),
                gas_costs::NATIVE_INFERENCE_REQUEST,
            );
            fixture
                .state
                .execute_block_verified(&[tx], fixture.finalizer.address())
                .unwrap();
        }
        let executor = Arc::new(RefuseOne {
            refused: ids[0],
            calls: Mutex::new(Vec::new()),
        });
        let dir = tempdir().unwrap();
        let store = DecisionStore::open(
            dir.path(),
            test_signer().address(),
            fixture.genesis,
            fixture.context.commitment().unwrap(),
        )
        .unwrap();
        let runtime = NativeWorkerRuntime::from_active(
            fixture.state.clone(),
            executor.clone(),
            Arc::new(Sign),
            Arc::new(Sink),
            store,
        )
        .unwrap();
        let mut votes = 0;
        for poll in 0..80 {
            match runtime.poll_once() {
                Ok(Some(_)) => votes += 1,
                Ok(None) => {}
                Err(error) => panic!("poll {poll} failed: {error}"),
            }
        }
        let calls = executor.calls.lock().clone();
        assert_eq!(
            calls.iter().filter(|id| **id == ids[0]).count(),
            1,
            "the refused request ran once"
        );
        assert!(calls.contains(&ids[1]), "the other request still ran");
        assert!(votes >= 1);
    }

    #[test]
    fn a_pending_source_follows_the_binding_in_force_not_the_one_it_started_with() {
        let fixture = native_fixture();
        let source =
            StatePendingSource::new(fixture.state.clone(), fixture.context.commitment().unwrap());

        let upgraded = hash_bytes(b"a newer model for the pending source");
        let mut next = fixture.context.clone();
        next.allowed_executions.push(AllowedExecution {
            model_hash: upgraded,
            profile_hash: upgraded,
            generation_hash: upgraded,
            assignment_hash: upgraded,
        });
        fixture
            .state
            .update_native_inference_binding(next)
            .expect("a running binding can be updated");

        let request = request_for(&fixture, 0, 3);
        let request_id = request.job.request_id();
        let request_tx = signed_native(
            &fixture.state,
            &fixture.requester,
            0,
            TxBody::NativeInferenceRequest(NativeInferenceRequestBody {
                request,
                input_blob: token_bytes(&[11, 12]),
            }),
            gas_costs::NATIVE_INFERENCE_REQUEST,
        );
        fixture
            .state
            .execute_block_verified(&[request_tx], fixture.finalizer.address())
            .unwrap();

        let job = source
            .load_pending(request_id)
            .expect("the worker must still be able to load work after an update");
        source
            .ensure_live(&job, 0)
            .expect("and must consider that work live");
    }

    #[test]
    fn a_request_settled_before_this_member_runs_it_is_a_refusal_not_a_failure() {
        let fixture = native_fixture();
        let request = request_for(&fixture, 0, 3);
        let request_id = request.job.request_id();
        let request_tx = signed_native(
            &fixture.state,
            &fixture.requester,
            0,
            TxBody::NativeInferenceRequest(NativeInferenceRequestBody {
                request,
                input_blob: token_bytes(&[11, 12]),
            }),
            gas_costs::NATIVE_INFERENCE_REQUEST,
        );
        fixture
            .state
            .execute_block_verified(&[request_tx], fixture.finalizer.address())
            .unwrap();
        let source =
            StatePendingSource::new(fixture.state.clone(), fixture.context.commitment().unwrap());
        assert!(
            source.load_pending(request_id).is_ok(),
            "admitted and pending"
        );
        fixture
            .state
            .execute_block_verified(&[], fixture.finalizer.address())
            .unwrap();
        let refund = signed_native(
            &fixture.state,
            &fixture.requester,
            1,
            TxBody::NativeInferenceRefund(arc_types::transaction::NativeInferenceRefundBody {
                request_id: request_id.0,
            }),
            gas_costs::NATIVE_INFERENCE_REFUND,
        );
        fixture
            .state
            .execute_block_verified(std::slice::from_ref(&refund), fixture.finalizer.address())
            .unwrap();
        let error = source.load_pending(request_id).unwrap_err();
        assert!(matches!(error, NativeInferenceError::NotPending), "{error}");
        assert!(
            error.is_request_refusal(),
            "not counted as a worker failure"
        );
    }

    #[test]
    fn a_committee_short_of_quorum_never_finalizes_and_the_request_refunds() {
        let fixture = native_fixture();
        let request = request_for(&fixture, 0, 3);
        let request_id = request.job.request_id();
        let request_tx = signed_native(
            &fixture.state,
            &fixture.requester,
            0,
            TxBody::NativeInferenceRequest(NativeInferenceRequestBody {
                request,
                input_blob: token_bytes(&[11, 12]),
            }),
            gas_costs::NATIVE_INFERENCE_REQUEST,
        );
        fixture
            .state
            .execute_block_verified(&[request_tx], fixture.finalizer.address())
            .unwrap();
        let mempool = Arc::new(Mempool::new(16));
        let sink = NativeFinalizeSink::new(
            fixture.state.clone(),
            mempool.clone(),
            fixture.finalizer.clone(),
        );
        for member in &fixture.validators[..4] {
            sink.accept_peer_vote(&vote(request_id, &[71, 72], member))
                .expect("a member's well-formed vote is counted");
        }
        assert_eq!(mempool.len(), 0, "four of six is not a strict two-thirds");
        fixture
            .state
            .execute_block_verified(&[], fixture.finalizer.address())
            .unwrap();
        let refund = signed_native(
            &fixture.state,
            &fixture.requester,
            1,
            TxBody::NativeInferenceRefund(arc_types::transaction::NativeInferenceRefundBody {
                request_id: request_id.0,
            }),
            gas_costs::NATIVE_INFERENCE_REFUND,
        );
        fixture
            .state
            .execute_block_verified(std::slice::from_ref(&refund), fixture.finalizer.address())
            .unwrap();
        assert!(
            sink.accept_peer_vote(&vote(request_id, &[71, 72], &fixture.validators[4]))
                .is_err(),
            "a vote for a refunded request is not counted"
        );
        assert_eq!(mempool.len(), 0, "no finalize is ever produced for it");
        let commitment = fixture.context.commitment().unwrap();
        assert_eq!(
            format!(
                "{:?}",
                fixture
                    .state
                    .native_inference_receipt(request_id, commitment)
                    .unwrap()
                    .unwrap()
                    .metadata
                    .status
            ),
            "Refunded"
        );
    }

    #[test]
    fn two_requests_certified_together_both_finalize_from_one_finalizer() {
        let fixture = native_fixture();
        let ids = admit_requests(&fixture, &[0, 1]);
        let mempool = Arc::new(Mempool::new(16));
        let sink = NativeFinalizeSink::new(
            fixture.state.clone(),
            mempool.clone(),
            fixture.finalizer.clone(),
        );
        for id in &ids {
            for key in &fixture.validators[..5] {
                sink.accept_peer_vote(&vote(*id, &[71, 72], key)).unwrap();
            }
        }
        assert_eq!(mempool.len(), 1, "one finalize in flight at a time");
        fixture
            .state
            .execute_block_verified(&mempool.drain(1), fixture.requester.address())
            .unwrap();
        for id in &ids {
            sink.accept_peer_vote(&vote(*id, &[71, 72], &fixture.validators[0]))
                .or_else(|error| match error {
                    NativeInferenceError::Source(m) if m.contains("no longer pending") => Ok(()),
                    other => Err(other),
                })
                .unwrap();
        }
        assert_eq!(
            mempool.len(),
            1,
            "the waiting certificate's finalize was sent"
        );
        fixture
            .state
            .execute_block_verified(&mempool.drain(1), fixture.requester.address())
            .unwrap();
        let commitment = fixture.context.commitment().unwrap();
        for id in &ids {
            let receipt = fixture
                .state
                .native_inference_receipt(*id, commitment)
                .unwrap()
                .unwrap();
            assert_eq!(
                format!("{:?}", receipt.metadata.status),
                "Finalized",
                "{id}"
            );
        }
    }

    #[test]
    fn synthetic_private_signed_refund_is_terminal_and_replay_rejects() {
        let fixture = native_fixture();
        let request = request_for(&fixture, 0, 2);
        let request_id = request.job.request_id();
        let request_tx = signed_native(
            &fixture.state,
            &fixture.requester,
            0,
            TxBody::NativeInferenceRequest(NativeInferenceRequestBody {
                request,
                input_blob: token_bytes(&[11, 12]),
            }),
            gas_costs::NATIVE_INFERENCE_REQUEST,
        );
        fixture
            .state
            .execute_block_verified(&[request_tx], fixture.finalizer.address())
            .unwrap();
        let refund = signed_native(
            &fixture.state,
            &fixture.requester,
            1,
            TxBody::NativeInferenceRefund(arc_types::transaction::NativeInferenceRefundBody {
                request_id: request_id.0,
            }),
            gas_costs::NATIVE_INFERENCE_REFUND,
        );
        fixture
            .state
            .execute_block_verified(std::slice::from_ref(&refund), fixture.finalizer.address())
            .unwrap();
        let commitment = fixture.context.commitment().unwrap();
        assert_eq!(
            format!(
                "{:?}",
                fixture
                    .state
                    .native_inference_receipt(request_id, commitment)
                    .unwrap()
                    .unwrap()
                    .metadata
                    .status
            ),
            "Refunded"
        );
        assert!(
            fixture
                .state
                .execute_block_verified(&[refund], fixture.finalizer.address())
                .is_err()
        );
    }
}
