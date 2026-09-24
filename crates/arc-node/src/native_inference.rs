//! Private milestone-2 native inference worker seam.
//!
//! The worker accepts request IDs only. It fetches the canonical pending job
//! through `PendingSource`, computes once through an injected executor, and
//! persists a signed output decision before a vote sink can observe it.

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
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use thiserror::Error;

pub const MAX_QUEUE: usize = 32;
pub const MAX_INPUT_BYTES: usize = TIER1_INPUT_BLOB_MAX;
pub const MAX_OUTPUT_BYTES: usize = TIER1_OUTPUT_BLOB_MAX;
pub const MAX_TOKENS: usize = TIER1_MAX_TOKENS as usize;
const MAX_CERTIFICATE_CANDIDATES: usize = 1024;
const CANONICAL_I8_V2_GENERATION_SEMANTICS: &[u8] =
    b"ARC-native-inference/gguf-llama-i8-interleaved-rope/generation-v2/bos-once/le-u32/v1";

/// Commitments required in the activated execution tuple for the corrected
/// per-row-I8 worker.  They are deliberately derived from versioned shared
/// strings rather than accepted as caller-selected labels.
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

/// The complete domain-bound payload presented to the signer.  Signing only
/// the output would allow the same signature to be replayed for another
/// request, artifact, or execution context.
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
    /// A refusal of one request that running it again would repeat: it is
    /// expired, no longer pending (the rest of the committee settled it
    /// first), outside the worker's bounds, or refused by the executor (for
    /// example over this node's KV budget, or for a model this node does not
    /// run). Not a failure of the worker, so it never counts toward the
    /// runtime's consecutive-error limit. `ContextMismatch` is not one: it
    /// means the node's own context or store disagrees with the chain.
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

    /// Re-read the canonical source immediately before a durable decision is
    /// exposed. The default preserves the labelled test-source seam.
    fn ensure_live(&self, job: &PendingJob, now: u64) -> Result<(), NativeInferenceError> {
        if now >= job.expires_at || self.load_pending(job.request_id)? != *job {
            return Err(NativeInferenceError::Expired);
        }
        Ok(())
    }
}

/// Canonical pending source backed by the bounded StateDB index. It never
/// accepts a caller-supplied request body and rejects a context mismatch.
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

    /// Bind to the only context StateDB currently considers live. A caller
    /// cannot manufacture a context commitment for a different frozen set.
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

impl PendingSource for StatePendingSource {
    fn load_pending(&self, request_id: Hash256) -> Result<PendingJob, NativeInferenceError> {
        let pending = self
            .state
            .native_inference_pending_requests(self.context_commitment)
            .map_err(|error| NativeInferenceError::Source(error.to_string()))?
            .into_iter()
            .find(|snapshot| snapshot.request_id == request_id)
            // Settled meanwhile (finalized or refunded without this member),
            // or never admitted: over for this member too, not a failure.
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
        if commitment != self.context_commitment || job.context != commitment {
            return Err(NativeInferenceError::ContextMismatch);
        }
        // Expiry is evaluated at the next canonical block, not wall clock.
        // A terminal receipt has no entry in this bounded pending view.
        let next_height = self.state.height().saturating_add(1);
        if next_height >= job.expires_at || self.load_pending(job.request_id)? != *job {
            return Err(NativeInferenceError::Expired);
        }
        Ok(())
    }
}

pub trait NativeExecutor: Send + Sync {
    fn execute(&self, job: &PendingJob) -> Result<ExecutionOutput, NativeInferenceError>;

    /// [`Self::execute`] at chain height `now`, which is what the worker
    /// calls. An executor that places work on other machines uses the height
    /// to age link measurements and sweep reservations; the rest ignore it.
    fn execute_at(
        &self,
        job: &PendingJob,
        _now: u64,
    ) -> Result<ExecutionOutput, NativeInferenceError> {
        self.execute(job)
    }
}

/// A reviewed binding for the private canonical-I8 worker.  The three hashes
/// are the activation's immutable model/profile/generation tuple; the
/// generation commitment must include the independently qualified tokenizer
/// and prompt-template contract.  Setting `reference_generation_qualified`
/// is an explicit release decision made only after reference-output evidence,
/// never an optimistic default at node startup.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CanonicalI8Qualification {
    pub artifact_hash: Hash256,
    pub profile_hash: Hash256,
    pub generation_hash: Hash256,
    pub reference_generation_qualified: bool,
}

/// Default KV-cache budget for one native job: 4 GiB, which is 2,048
/// positions of the canonical 7B package (2 MiB each) beside its 7.8 GB of
/// prepared weights. The protocol admits requests up to the 4,096-position
/// context (8 GiB of KV, 16.4 GB resident: more than a 16 GB host), so a node
/// refuses what it cannot hold instead of swapping or being killed mid-job.
/// Refusing means not voting: the request expires and refunds. Operators
/// with more memory raise it with `--native-kv-budget-bytes`.
pub const DEFAULT_NATIVE_KV_BUDGET_BYTES: u64 = 4 << 30;

/// KV-cache bytes for `positions` positions: every position holds one K and
/// one V row of `d_kv` i64 values per layer. `None` on overflow.
pub fn native_kv_bytes(positions: u64, n_layers: u64, d_kv: u64) -> Option<u64> {
    positions
        .checked_mul(n_layers)?
        .checked_mul(d_kv)?
        .checked_mul(2 * std::mem::size_of::<i64>() as u64)
}

/// The fewest positions a node's KV budget must hold: the smallest native
/// job (1 BOS + 1 prompt token + 1 generated token) allocated as the next
/// power of two. A smaller budget refuses every job, so startup refuses it.
pub const MIN_NATIVE_KV_POSITIONS: u64 = 4;

/// The most positions one job may use under `budget`. The KV cache grows by
/// doubling (`KVCache` extends each layer's buffer), so a job of `p`
/// positions holds `p.next_power_of_two()` of them: the most that fit is the
/// largest power of two at or below what the budget holds, capped at the
/// model's context.
pub fn allocatable_kv_positions(budget: u64, per_position: u64, max_seq: u64) -> u64 {
    (budget / per_position.max(1))
        .checked_ilog2()
        .map_or(0, |bits| 1u64 << bits)
        .min(max_seq)
}

/// Refuse a job whose KV cache would exceed `budget` bytes: 1 BOS + prompt +
/// `max_tokens` positions, allocated as the next power of two.
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

/// Concrete adapter for the corrected GGUF Llama per-row-I8 execution path.
/// It intentionally accepts pre-qualified LE-u32 prompt IDs only.  The legacy
/// greedy `CachedIntegerModel::encode` tokenizer is never called here, so raw
/// UTF-8 cannot become a paid output before tokenizer/template qualification.
pub struct CanonicalI8NativeExecutor {
    model: Arc<arc_inference::cached_integer_model::CachedIntegerModel>,
    qualification: CanonicalI8Qualification,
    kv_budget_bytes: u64,
    /// This operator's own row machines, when configured (option (a) of the
    /// assignment trust model; `crate::row_cohort`).
    row_cohort: Option<Arc<crate::row_cohort::RowCohort>>,
}

impl CanonicalI8NativeExecutor {
    /// Load the artifact and prove its byte commitment before model loading.
    /// This is the production-facing constructor; it cannot be pointed at an
    /// already-resident arbitrary model with a claimed source hash.
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

    // Kept private for module-level fixtures. Production construction must go
    // through `load_qualified`, which hashes the actual source artifact.
    fn from_model(
        model: Arc<arc_inference::cached_integer_model::CachedIntegerModel>,
        qualification: CanonicalI8Qualification,
    ) -> Result<Self, NativeInferenceError> {
        use arc_inference::cached_integer_model::GGUF_INTERLEAVED_ROPE_I8_INFERENCE_PROFILE;
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
        if model.canonical_execution_profile() != Some(GGUF_INTERLEAVED_ROPE_I8_INFERENCE_PROFILE)
            || !model.has_all_transformer_layers()
        {
            return Err(NativeInferenceError::Executor(
                "model is not the complete corrected GGUF interleaved-RoPE canonical I8 profile"
                    .into(),
            ));
        }
        Ok(Self {
            model,
            qualification,
            kv_budget_bytes: DEFAULT_NATIVE_KV_BUDGET_BYTES,
            row_cohort: None,
        })
    }

    /// Replace the default KV-cache budget ([`DEFAULT_NATIVE_KV_BUDGET_BYTES`]).
    pub fn with_kv_budget(mut self, bytes: u64) -> Self {
        self.kv_budget_bytes = bytes;
        self
    }

    /// Connect this operator's own row machines (`crate::row_cohort`). Paid
    /// requests are then placed on them whenever placement predicts that is
    /// faster; the tokens are local execution's either way.
    pub fn connect_row_cohort(
        mut self,
        config: crate::row_cohort::RowCohortConfig,
        validator: Hash256,
        height: u64,
    ) -> Result<Self, NativeInferenceError> {
        let cohort = crate::row_cohort::RowCohort::connect(
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

    /// The connected row cohort, for the read-only view.
    pub fn row_cohort(&self) -> Option<Arc<crate::row_cohort::RowCohort>> {
        self.row_cohort.clone()
    }

    /// The most positions (1 BOS + prompt + generated tokens) one job may
    /// use on this node: its KV budget, capped at the model's context. A
    /// client checks a request against it before signing; the node refuses
    /// (never votes on) a job that needs more.
    pub fn max_positions(&self) -> u64 {
        let config = &self.model.config;
        let per_position =
            native_kv_bytes(1, config.n_layers as u64, config.d_kv as u64).unwrap_or(u64::MAX);
        allocatable_kv_positions(self.kv_budget_bytes, per_position, config.max_seq as u64)
    }

    /// Check that the package manifest pinned by `package_manifest_hash`
    /// describes exactly what this executor loaded from `artifact`: the
    /// artifact bytes, profile and generation commitments, graph, tokenizer
    /// vocabulary, tensor inventory and KV cost. Startup refuses on any
    /// difference, naming the field (model package contract v1).
    pub fn verify_package(
        &self,
        artifact: &Path,
        tokenizer: &arc_inference::llama_spm_tokenizer::LlamaGgufSpmTokenizer,
        manifest: &Path,
        package_manifest_hash: Hash256,
    ) -> Result<(), NativeInferenceError> {
        let loaded = package_facts(
            &self.model,
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
        // The signed input hash was checked by StatePendingSource and again by
        // NativeWorker. This wire form prevents a hidden text tokenizer from
        // changing a request after its generation commitment was signed.
        if job.input.is_empty() || job.input.len() % std::mem::size_of::<u32>() != 0 {
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
        // The engine returns empty logits for an id outside the vocabulary and
        // silently skips that position, so such a prompt would be paid for a
        // corrupted sequence. Refuse it; the request then expires and refunds
        // (integer profile contract v1, deviation D2).
        if tokens.iter().any(|&token| token as usize >= vocab_size) {
            return Err(NativeInferenceError::Executor(
                "native canonical I8 prompt contains a token id outside the model vocabulary"
                    .into(),
            ));
        }
        Ok(tokens)
    }
}

/// What a node measured of the package it loaded: the facts a package
/// manifest must match (model package contract v1, section 7).
/// `artifact_hash` is the BLAKE3 the loader verified for `artifact`.
pub fn package_facts(
    model: &arc_inference::cached_integer_model::CachedIntegerModel,
    artifact_hash: Hash256,
    artifact: &Path,
    tokenizer: &arc_inference::llama_spm_tokenizer::LlamaGgufSpmTokenizer,
) -> Result<arc_inference::model_package::LoadedPackage, NativeInferenceError> {
    let refuse = |message: String| NativeInferenceError::Executor(message);
    let (_, tensors) = arc_inference::gguf_meta::read_header_from_path(artifact)
        .map_err(|error| refuse(format!("reading the artifact's tensor inventory: {error}")))?;
    let config = &model.config;
    // The tokenizer serving /native-inference/tokenize must agree with the
    // loaded model before either is compared with the manifest.
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

/// Read the manifest at `manifest` (bounded) and require it to be the one
/// `package_manifest_hash` pins and to describe `loaded` exactly.
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
    /// One job. With a row cohort and the chain height, the generation may
    /// be placed on this operator's machines; without either, it runs here.
    fn run(
        &self,
        job: &PendingJob,
        now: Option<u64>,
    ) -> Result<ExecutionOutput, NativeInferenceError> {
        if job.model_hash != self.qualification.artifact_hash
            || job.artifact != self.qualification.artifact_hash
            || job.profile_hash != self.qualification.profile_hash
            || job.generation_hash != self.qualification.generation_hash
        {
            // A property of this request (another allowlisted tuple), so a
            // refusal of it, not a failure of this node.
            return Err(NativeInferenceError::Executor(
                "this node's executor does not run this request's model/profile/generation".into(),
            ));
        }
        let prompt = Self::prequalified_prompt(
            job,
            self.model.config.bos_token,
            self.model.config.vocab_size,
        )?;
        let max_tokens =
            u32::try_from(job.max_tokens).map_err(|_| NativeInferenceError::OutputTooLarge)?;
        check_native_kv_budget(
            prompt.len(),
            job.max_tokens,
            self.model.config.n_layers,
            self.model.config.d_kv,
            self.kv_budget_bytes,
        )?;
        let (tokens, output_hash) = match (&self.row_cohort, now) {
            (Some(cohort), Some(now)) => cohort.generate(
                job.request_id,
                job.expires_at,
                now,
                &prompt,
                max_tokens,
                &self.model.config.eos_tokens,
            )?,
            _ => self
                .model
                .try_generate_v2(&prompt, max_tokens, &self.model.config.eos_tokens)
                .map_err(|error| NativeInferenceError::Executor(error.to_string()))?,
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

/// Which executor this node's native worker runs. Published on
/// `/native-inference/context` so a client can build input the executor
/// accepts. Client convenience only: admission never consults it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NativeExecutorKind {
    /// The qualified canonical-I8 artifact. It reads little-endian u32 token
    /// ids from the generation contract's tokenizer, without BOS.
    CanonicalI8,
    /// The integration-only deterministic executor. It loads no model and
    /// ignores its input; its output is protocol coverage, not an answer.
    DeterministicTest,
}

impl NativeExecutorKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::CanonicalI8 => "canonical_i8",
            Self::DeterministicTest => "deterministic_test",
        }
    }

    /// The input encoding this executor reads.
    pub fn input_format(self) -> &'static str {
        match self {
            Self::CanonicalI8 => "le_u32_token_ids_without_bos",
            Self::DeterministicTest => "opaque_bytes",
        }
    }
}

/// What this node serves for native requests: its executor, and for the
/// canonical executor the tokenizer read from the same artifact.
pub struct NativeServing {
    pub executor: NativeExecutorKind,
    pub tokenizer: Option<arc_inference::llama_spm_tokenizer::LlamaGgufSpmTokenizer>,
    /// The most positions one job may use here (the executor's KV budget,
    /// capped at the model's context); `None` when the executor has no such
    /// limit.
    pub max_positions: Option<u64>,
    /// The executor's row cohort, for the read-only `/assignment/cohort`.
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

    /// Attach the executor's row cohort for the read-only view.
    pub fn with_row_cohort(mut self, cohort: Option<Arc<crate::row_cohort::RowCohort>>) -> Self {
        self.row_cohort = cohort;
        self
    }

    /// Tokenize a prompt into exactly the input the canonical executor
    /// accepts: the tokenizer's ids without the BOS that generation v2 adds
    /// itself. The same refusals as the executor apply, so a client learns
    /// before signing, not after paying for an expiry.
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

    /// Display text for a certified output, when this node holds the
    /// tokenizer and the output is whole little-endian u32 ids. Display
    /// only: the certificate commits to the bytes, not to this text.
    pub fn decode_output(&self, output: &[u8]) -> Option<String> {
        let tokenizer = self.tokenizer.as_ref()?;
        if output.is_empty() || output.len() % 4 != 0 {
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

/// Collects independently signed native votes and submits exactly one ordinary
/// signed `NativeInferenceFinalize` transaction once the frozen membership has
/// a strict >2/3 stake certificate. This is intentionally a local relay, not
/// a consensus shortcut: the StateDB preflight and later block execution both
/// verify every vote and the canonical pending request again.
pub struct NativeFinalizeSink {
    state: Arc<arc_state::StateDB>,
    mempool: Arc<Mempool>,
    submitter: Arc<KeyPair>,
    candidates: Mutex<BTreeMap<[u8; 32], CandidateVotes>>,
    /// This validator's one finalize transaction in flight, if any. See
    /// `accept`.
    in_flight: Mutex<Option<InFlightFinalize>>,
    emission_gate: Mutex<()>,
    /// This validator's OWN votes, waiting for the consensus loop to gossip
    /// them. Bounded: a vote that cannot be sent is not a correctness problem
    /// (other members' votes can still reach threshold), only a delay.
    outbound: Mutex<std::collections::VecDeque<StoredVote>>,
}

/// Votes queued for gossip at once.
const MAX_OUTBOUND_NATIVE_VOTES: usize = 1_024;

/// Votes for one request, kept per output. A single "the output" per request
/// let the first vote to arrive decide what every later vote was compared
/// against: one byzantine member voting first for invented tokens made every
/// honest vote an "equivocation", no certificate could ever form, and each
/// refused local vote counted as a worker failure until the worker stopped.
#[derive(Default)]
struct CandidateVotes {
    /// output hash -> (output bytes, validator -> vote)
    outputs: BTreeMap<[u8; 32], (Vec<u8>, BTreeMap<[u8; 32], InferenceVote>)>,
    /// validator -> the output hash it voted for (a second, different output
    /// from the same validator is its equivocation, never counted).
    voted: BTreeMap<[u8; 32], [u8; 32]>,
    submitted: Option<InFlightFinalize>,
}

/// A finalize this validator signed and handed to its mempool.
#[derive(Clone, Copy, Debug)]
struct InFlightFinalize {
    request_id: [u8; 32],
    tx_hash: Hash256,
    nonce: u64,
    at: std::time::Instant,
}

/// How long a finalize may sit neither executed nor visibly dead before this
/// validator stops waiting on it (it was omitted from a block, or dropped).
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

    /// This validator's own votes, for the consensus loop to gossip.
    pub fn take_outbound_votes(&self, max: usize) -> Vec<StoredVote> {
        let mut queue = self.outbound.lock();
        let n = max.min(queue.len());
        queue.drain(..n).collect()
    }

    /// A vote another validator gossiped. Verified exactly as a local one -
    /// signature, output binding, frozen membership, pending request - by the
    /// same `accept`, and never re-broadcast: gossip carries each vote from its
    /// signer, not in an echo loop.
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
    /// This validator's own vote: queue it for gossip, then count it.
    fn emit(&self, decision: &StoredVote) -> Result<(), NativeInferenceError> {
        {
            let mut queue = self.outbound.lock();
            if queue.len() >= MAX_OUTBOUND_NATIVE_VOTES {
                queue.pop_front();
            }
            queue.push_back(decision.clone());
        }
        match self.accept(decision) {
            // The request settled or expired while this vote was being made -
            // another validator's finalize won, which is the system working.
            // Counting it as a worker failure stopped the worker for good
            // after enough of them in a row.
            Err(NativeInferenceError::Expired) => Ok(()),
            Err(NativeInferenceError::Source(message)) if message.contains("no longer pending") => {
                Ok(())
            }
            other => other,
        }
    }
}

impl NativeFinalizeSink {
    /// Whether `flight` can still execute: not yet executed, its nonce not yet
    /// consumed by another of this validator's transactions, its request
    /// still pending, and not older than `FINALIZE_IN_FLIGHT_TIMEOUT`.
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

    /// Count a verified vote toward its request's certificate and, at a strict
    /// supermajority for one output, submit the finalize transaction.
    ///
    /// Votes are counted per output: a vote for a different output is not an
    /// error for anyone else's vote, only a different candidate certificate.
    /// A validator voting for two outputs is refused on its second.
    ///
    /// This validator keeps ONE finalize in flight. Each is signed at the
    /// account's current nonce, so two signed together collided on it: one
    /// executed, the other was dropped, and its request - marked submitted -
    /// was never re-signed. Later certificates wait for the one in flight to
    /// execute or die, and a submitted finalize that died is re-signed.
    fn accept(&self, decision: &StoredVote) -> Result<(), NativeInferenceError> {
        let _emission = self.emission_gate.lock();
        // A vote already counted costs nothing more: no state read, no
        // signature check. (Each validator re-sends its vote every few
        // seconds while a request is pending.)
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
                // Only now pay for a sweep: drop requests that are no longer
                // pending. Bounded by the state's own bounded pending index.
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
                    // This member already voted for a different output.
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

        // Is there a certificate, and is there a finalize to send for it?
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
                // It died without executing (omitted, dropped, or its nonce
                // was taken): sign again.
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

        // One finalize in flight per validator; a later one waits (the vote
        // re-emission calls back here every few seconds).
        {
            let mut flight = self.in_flight.lock();
            if let Some(current) = *flight {
                if current.request_id != decision.request_id.0 && self.still_in_flight(&current) {
                    return Ok(());
                }
                *flight = None;
            }
        }

        // Re-read immediately before signing/admitting. The state preflight
        // supplies the same check under its canonical native execution lock.
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
    context: Hash256,
    poisoned: Arc<AtomicBool>,
}

impl DecisionStore {
    pub fn open(
        root: impl AsRef<Path>,
        validator: Hash256,
        genesis: Hash256,
        context: Hash256,
    ) -> Result<Self, NativeInferenceError> {
        fs::create_dir_all(root.as_ref())?;
        Ok(Self {
            root: root.as_ref().to_path_buf(),
            validator,
            genesis,
            context,
            poisoned: Arc::new(AtomicBool::new(false)),
        })
    }

    fn path(&self, request_id: Hash256) -> PathBuf {
        let mut bytes = Vec::with_capacity(128);
        bytes.extend_from_slice(&self.validator.0);
        bytes.extend_from_slice(&self.genesis.0);
        bytes.extend_from_slice(&self.context.0);
        bytes.extend_from_slice(&request_id.0);
        self.root.join(format!(
            "{}.decision",
            hex::encode(blake3::hash(&bytes).as_bytes())
        ))
    }

    fn load(&self, request_id: Hash256) -> Result<Option<StoredDecision>, NativeInferenceError> {
        if self.poisoned.load(Ordering::Acquire) {
            return Err(NativeInferenceError::StorePoisoned);
        }
        let path = self.path(request_id);
        let file = match File::open(path) {
            Ok(file) => file,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e.into()),
        };
        const MAX_DECISION_BYTES: usize = 256 * 1024;
        if file.metadata()?.len() > MAX_DECISION_BYTES as u64 {
            return Err(NativeInferenceError::CorruptStore);
        }
        // A file can grow after metadata is checked. Read through Take so the
        // store never turns a corrupted decision file into an unbounded heap
        // allocation.
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

    fn sync_published(&self, request_id: Hash256) -> Result<(), NativeInferenceError> {
        let file = File::open(self.path(request_id))?;
        file.sync_all()?;
        File::open(&self.root)?.sync_all()?;
        Ok(())
    }

    fn load_existing_valid(
        &self,
        job: &PendingJob,
    ) -> Result<Option<StoredVote>, NativeInferenceError> {
        let Some(stored) = self.load(job.request_id)? else {
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
            signature: stored.signature,
        };
        if !verify_vote(&vote, job.request_id, &stored.tokens) {
            return Err(NativeInferenceError::CorruptStore);
        }
        Ok(Some(StoredVote {
            request_id: job.request_id,
            output_hash: stored.output_hash,
            tokens: stored.tokens,
            vote,
        }))
    }

    /// Delete this validator's decisions for requests that are terminal.
    ///
    /// A decision file is the validator's anti-equivocation record for one
    /// request: it must never sign a second output for it, even across a
    /// restart. That matters only while the request can still be finalized.
    /// Once the chain has settled it - finalized or refunded - no vote for it
    /// is admissible ever again, so the file protects nothing, and it was the
    /// one per-request artefact that grew for the life of the node (one file,
    /// about 4 KB of disk, per request).
    ///
    /// `is_terminal` must answer from canonical state. A request that is
    /// merely absent from the pending index is NOT terminal - it may have been
    /// admitted a moment ago - so the caller asks for an explicit settled
    /// receipt. Files that cannot be read, or that belong to another
    /// validator, chain or activation, are kept: an unreadable record is not
    /// evidence that it is safe to forget.
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
                || stored.context != self.context
                || path != self.path(stored.request_id)
            {
                continue;
            }
            if is_terminal(stored.request_id) {
                fs::remove_file(&path)?;
                removed += 1;
            }
        }
        if removed > 0 {
            File::open(&self.root)?.sync_all()?;
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
        if job.genesis != self.genesis || job.context != self.context {
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
        if let Some(existing) = self.load(job.request_id)? {
            self.sync_published(job.request_id)?;
            if existing == stored {
                return Ok(decision);
            }
            return Err(NativeInferenceError::Equivocation);
        }
        let bytes = bincode::serialize(&stored).map_err(|_| NativeInferenceError::CorruptStore)?;
        let tmp = self.root.join(format!(".{}.tmp", uuid::Uuid::new_v4()));
        {
            let mut file = OpenOptions::new().write(true).create_new(true).open(&tmp)?;
            file.write_all(&bytes)?;
            file.sync_all()?;
        }
        match fs::hard_link(&tmp, self.path(job.request_id)) {
            Ok(()) => {
                fs::remove_file(&tmp)?;
            }
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
                let _ = fs::remove_file(&tmp);
                self.sync_published(job.request_id)?;
                return match self.load(job.request_id)? {
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
        if let Err(error) = self.sync_published(job.request_id) {
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
    /// Round-robin across requesters, each request's expiry as its deadline
    /// (S6): one requester with many admitted requests cannot hold every
    /// execution slot, and an expired request is dropped before it runs.
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

    /// Queue a request with no requester or deadline known: it takes its
    /// turn in one shared group and never expires in the queue (the source
    /// still refuses an expired job when it runs).
    pub fn submit(&self, request_id: Hash256) -> Result<(), NativeInferenceError> {
        self.submit_for(request_id, Hash256::ZERO, u64::MAX)
    }

    /// Queue a request in its requester's turn, dropped unrun once the next
    /// block height reaches `expires_at`.
    pub fn submit_for(
        &self,
        request_id: Hash256,
        requester: Hash256,
        expires_at: u64,
    ) -> Result<(), NativeInferenceError> {
        if self.cancelled.load(Ordering::Acquire) {
            return Err(NativeInferenceError::Cancelled);
        }
        // Pollers may observe the same persisted request many times while a
        // model run is in flight. Keep one queue slot per request so a slow
        // job cannot fill the bounded queue with duplicates.
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

    /// Nothing is queued.
    pub fn is_idle(&self) -> bool {
        self.queue.lock().is_empty()
    }

    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
    }

    /// Run the next queued request. See [`Self::next_request`] and
    /// [`Self::run_request`], which the runtime uses separately so it knows
    /// which request a refusal belongs to.
    pub fn run_one(&self, now: u64) -> Result<StoredVote, NativeInferenceError> {
        if self.cancelled.load(Ordering::Acquire) {
            return Err(NativeInferenceError::Cancelled);
        }
        let request_id = self
            .next_request(now)
            .ok_or(NativeInferenceError::QueueFull)?;
        self.run_request(request_id, now)
    }

    /// The next queued request whose expiry is above `now` (the next block
    /// height), taking one requester's turn. Expired calls are dropped on the
    /// way, and `None` means nothing live is queued - idle, not an error.
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

    /// Execute (or re-emit the durable vote for) one request taken from the
    /// queue by [`Self::next_request`].
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
        // Persist before emission, then re-read canonical terminal state so a
        // concurrent finalize/refund never receives a newly emitted vote.
        self.source.ensure_live(&job, now)?;
        if self.cancelled.load(Ordering::Acquire) {
            return Err(NativeInferenceError::Cancelled);
        }
        self.sink.emit(&vote)?;
        Ok(vote)
    }
}

/// Polling bridge for the real StateDB bounded pending index.  It is suitable
/// for the node's background task and deliberately does not accept a request
/// body or arbitrary trait-source job.  A restart simply re-enqueues the
/// still-pending canonical entries; DecisionStore makes the second pass reuse
/// its exact durable signed vote instead of re-signing it.
pub struct NativeWorkerRuntime<E, G, V> {
    state: Arc<arc_state::StateDB>,
    source: Arc<StatePendingSource>,
    worker: NativeWorker<StatePendingSource, E, G, V>,
    /// When this validator last emitted its vote for each still-pending request.
    last_emitted: Mutex<BTreeMap<[u8; 32], std::time::Instant>>,
    /// Still-pending requests this node refused for good (expired, out of
    /// bounds, refused by the executor), with the reason. Never offered to
    /// the worker again while pending: re-running them would only repeat the
    /// refusal, and counting it as a worker failure used to stop the worker.
    refused: Mutex<BTreeMap<[u8; 32], String>>,
    /// When settled requests' decision files were last pruned.
    pruned_at: Mutex<Option<std::time::Instant>>,
}

/// How often the runtime deletes decision files of settled requests.
pub const NATIVE_DECISION_PRUNE_INTERVAL: std::time::Duration = std::time::Duration::from_secs(60);

/// How often an already-cast vote for a still-pending request is emitted again.
///
/// The runtime was built around a single validator, whose own vote finalizes a
/// request at once. On a committee the request stays pending until a
/// supermajority of votes arrives, and `poll_once` - which re-submits every
/// pending request - re-emitted the stored vote in a tight loop: the four-node
/// paid-flow test logged 59,000-80,000 identical decisions per validator for a
/// single request. Re-emitting occasionally is still useful (a peer that missed
/// the gossip gets it again); continuously is not.
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

    /// Enqueue every bounded canonical pending request and execute at most one.
    /// The caller supplies no height: StateDB determines the canonical next
    /// block height at each validation point.
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
            // Terminal requests leave both maps, so they stay bounded by the
            // state's own bounded pending index.
            let live: BTreeSet<[u8; 32]> = pending.iter().map(|p| p.request_id.0).collect();
            last.retain(|id, _| live.contains(id));
            refused.retain(|id, _| live.contains(id));
            // Offer due requests round-robin by requester, oldest admission
            // first within each, so the bounded queue is fair across
            // requesters as well as inside it.
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
                        // A full queue is backpressure, not a failure: the
                        // rest are offered again next poll.
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
        // A refusal runs nothing, so the next queued request is tried at once
        // rather than after the idle interval; the queue bounds the loop.
        for _ in 0..MAX_QUEUE {
            // Nothing live queued (or only expired calls, now dropped): idle.
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

    /// At most once per `NATIVE_DECISION_PRUNE_INTERVAL`, delete decision
    /// files whose request has a settled canonical receipt. A failure is
    /// logged and retried next interval: keeping a file is always safe.
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
    let mut bytes = Vec::with_capacity(tokens.len() * std::mem::size_of::<u32>());
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

// ── Operator activation config (private protocol 4) ─────────────────────────
//
// The startup constructor was previously absent entirely: every caller of
// `StateDB::activate_native_inference` lived in `#[cfg(test)]`, and nothing in
// the node binary mentioned it. `docs/arc-chain-v2/native-runtime-candidate.md`
// holds production EXECUTION behind model-quality qualification, which is a
// different thing from having no way to configure or test activation at all.
//
// This adds the missing operator-facing seam. It introduces no new commitment
// scheme, no new admission rule and no protocol surface: it deserialises the
// EXISTING `InferenceAdmissionContext` and then defers entirely to the existing
// `commitment()` validation and `activate_native_inference()` gate, which still
// require an unused private genesis at height 0, a healthy persistent WAL and
// the canonical account-root backend.

// ── Bounded native worker runtime ───────────────────────────────────────────
//
// Activating the contract is not the same as running it: a request could be
// admitted with no node process executing it and no finalization produced, so
// settlement could never happen. This is the missing lifecycle.
//
// Default disabled, bounded, and cancellable. It runs only when the operator
// asked for it AND the contract is actually activated, it sleeps between empty
// polls rather than spinning, it backs off on error instead of hot-looping a
// failing dependency, and it stops promptly on shutdown. Durability is the
// `DecisionStore`'s: a vote persisted before a crash is re-emitted on restart
// without recomputing, which `durable_vote_is_verified_and_reemitted_without_recompute`
// already covers.

/// A deterministic executor for integration testing ONLY.
///
/// It loads no model and produces output derived from the job's own committed
/// fields, so the request → vote → finalize → receipt protocol can be exercised
/// end to end without loading several multi-gigabyte models.
///
/// It is behind the `native-test-executor` cargo feature precisely so it cannot
/// exist in a default build. **Evidence produced with it is integration
/// coverage of the protocol. It qualifies no model and says nothing about
/// production inference quality.**
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
    fn execute(&self, job: &PendingJob) -> Result<ExecutionOutput, NativeInferenceError> {
        // Domain-separated so this output can never collide with a real one.
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

/// Handle to a running native worker loop.
pub struct NativeRuntimeHandle {
    cancel: Arc<AtomicBool>,
    join: Option<std::thread::JoinHandle<()>>,
}

impl NativeRuntimeHandle {
    /// Signal the loop to stop and wait for it. Safe to call once; dropping the
    /// handle without calling this also cancels, so a panicking caller cannot
    /// leave the loop running.
    pub fn shutdown(mut self) {
        self.cancel.store(true, Ordering::Release);
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }

    pub fn is_running(&self) -> bool {
        !self.cancel.load(Ordering::Acquire)
    }
}

impl Drop for NativeRuntimeHandle {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Release);
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

/// Bounds for the polling loop.
#[derive(Debug, Clone, Copy)]
pub struct NativeRuntimeBounds {
    /// Sleep between polls that found nothing.
    pub idle_interval: std::time::Duration,
    /// Sleep after a poll error, so a failing dependency is not hot-looped.
    pub error_backoff: std::time::Duration,
    /// Stop the loop after this many consecutive errors. `None` = never stop.
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

/// Spawn the bounded worker loop. The caller owns the cancellation flag, so a
/// node shutdown path can stop it without owning the handle.
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
    let flag = cancel.clone();
    let join = std::thread::Builder::new()
        .name("arc-native-worker".into())
        .spawn(move || {
            let mut consecutive_errors: u32 = 0;
            while !flag.load(Ordering::Acquire) {
                match runtime.poll_once() {
                    Ok(Some(vote)) => {
                        consecutive_errors = 0;
                        tracing::info!(
                            request = %vote.request_id.to_hex(),
                            "native worker produced a signed decision"
                        );
                    }
                    Ok(None) => {
                        consecutive_errors = 0;
                        std::thread::sleep(bounds.idle_interval);
                    }
                    Err(error) => {
                        consecutive_errors = consecutive_errors.saturating_add(1);
                        tracing::warn!(
                            %error,
                            consecutive_errors,
                            "native worker poll failed"
                        );
                        if let Some(limit) = bounds.max_consecutive_errors {
                            if consecutive_errors >= limit {
                                tracing::error!(
                                    limit,
                                    "native worker stopping after repeated failures; \
                                     the contract stays activated but no further work is done"
                                );
                                break;
                            }
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
    }
}

// ── Reference qualification (real-model execution only) ─────────────────────
//
// `CanonicalI8Qualification::reference_generation_qualified` is defined above as
// a release decision made only after independent reference-output evidence.
// Startup previously constructed it with `true` unconditionally, next to a
// comment claiming it could not make that decision. That is a fabricated
// attestation: matching artifact/profile/generation hashes establish execution
// IDENTITY, not qualification, and a constructor's boolean check cannot protect
// anything when its caller always passes true.
//
// It is now supplied as an explicit record, bound to one execution identity, or
// real-model execution does not start. The record is an operator decision, not
// a cryptographic proof and not evidence of model quality; it exists so the
// decision is made deliberately, by a named party, with a stated basis, rather
// than inferred from a flag.

/// Why a reference-qualification record was rejected.
#[derive(Debug)]
pub enum QualificationError {
    /// No record was supplied.
    Missing,
    /// The file could not be read.
    Unreadable(String),
    /// The file is not valid JSON for a [`NativeQualificationRecord`].
    Malformed(String),
    /// The record declares the execution NOT qualified.
    NotQualified,
    /// The record is bound to a different execution identity.
    IdentityMismatch {
        field: String,
        expected: String,
        found: String,
    },
    /// The record omits who decided, when, or on what basis.
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

/// An operator's explicit reference-qualification decision for ONE execution
/// identity.
///
/// ```json
/// {
///   "model_hash": "<64 hex>", "profile_hash": "<64 hex>",
///   "generation_hash": "<64 hex>",
///   "reference_generation_qualified": true,
///   "decided_by": "release engineering",
///   "decided_at": "2026-09-20",
///   "evidence": "path or reference to the reference-output comparison"
/// }
/// ```
///
/// This is a decision record, not a proof. It does not make a model good; it
/// records that a named party decided, on a stated basis, that this exact
/// execution identity passed reference qualification.
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
    /// BLAKE3 of the approved package manifest (`arc.model-package.v1`). A
    /// reviewer approves a package by this hash; startup refuses to execute
    /// a loaded artifact the manifest does not describe exactly.
    #[serde(default)]
    pub package_manifest_hash: Option<Hash256>,
}

/// A complete decision for real-model execution: the qualified identity and
/// the package manifest it approved.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RealExecutionDecision {
    pub qualification: CanonicalI8Qualification,
    pub package_manifest_hash: Hash256,
}

/// Resolve the qualification for REAL-model execution, bound to `allowed`.
/// See [`resolve_real_execution_decision`], which also returns the pinned
/// package manifest hash.
pub fn resolve_real_execution_qualification(
    path: Option<&std::path::Path>,
    allowed: &AllowedExecution,
) -> Result<CanonicalI8Qualification, QualificationError> {
    resolve_real_execution_decision(path, allowed).map(|decision| decision.qualification)
}

/// Resolve the decision for REAL-model execution, bound to `allowed`.
///
/// Returns `Err` for every path that is not an explicit, complete, matching,
/// affirmative decision. Callers must run this **before** hashing or loading an
/// artifact, so a missing decision costs nothing.
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

/// Why an operator activation config was rejected.
#[derive(Debug)]
pub enum ActivationConfigError {
    /// The file could not be read.
    Unreadable(String),
    /// The file is not valid JSON for a [`NativeActivationRequest`].
    Malformed(String),
    /// The assembled context failed the existing commitment validation.
    InvalidContext(String),
    /// The chain is not a fresh private genesis.
    ChainNotEmpty(u64),
    /// The node could not determine its own genesis binding or validator set.
    StateUnavailable(String),
    /// An operator-supplied pin did not match what the node actually has.
    ExpectationMismatch {
        field: String,
        expected: String,
        actual: String,
    },
    /// An operator config differs from the already-persisted activation.
    ReconfigurationRejected {
        field: String,
        persisted: String,
        requested: String,
    },
    /// Activation itself refused.
    Refused(String),
    /// A migration is authorised and waiting for its coordinated height. Not
    /// a failure: the node keeps running and activates when the chain gets
    /// there.
    MigrationPending { at: u64, height: u64 },
    /// A binding update is authorised and waiting for its coordinated
    /// height. Also not a failure: the binding in force is unchanged and the
    /// node keeps serving under it until the chain gets there.
    BindingUpdatePending { at: u64, height: u64 },
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

/// Optional operator pins. When present, the context the node assembles must
/// match these exactly or activation is refused.
///
/// This exists so an approved activation is reproducible: an operator who has
/// reviewed a specific validator set and genesis can require the node to
/// confirm it rather than silently activating against whatever it happens to
/// hold. Every pin is optional; pinning nothing is allowed but unreviewed.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActivationExpectations {
    #[serde(default)]
    pub chain_genesis: Option<Hash256>,
    #[serde(default)]
    pub validator_set_hash: Option<Hash256>,
    #[serde(default)]
    pub members: Option<Vec<ValidatorMember>>,
}

/// The operator-facing activation request.
///
/// Deliberately NOT the full `InferenceAdmissionContext`. Three of that type's
/// four inputs - the genesis binding, the member list and the validator-set
/// hash - are facts the node already holds authoritatively, and requiring an
/// operator to transcribe them produces exactly one outcome: transcription
/// errors that surface as an opaque refusal at activation time. The node fills
/// those in and the operator declares the one thing only they can decide, the
/// execution allowlist, plus optional pins to make the result reviewable.
///
/// This introduces no commitment scheme and relaxes no admission rule: the
/// assembled context goes through the same `commitment()` and the same
/// `activate_native_inference()` gate as before.
///
/// ```json
/// {
///   "recovery_epoch": 0,
///   "allowed_executions": [{
///     "model_hash": "<64 hex>", "profile_hash": "<64 hex>",
///     "generation_hash": "<64 hex>", "assignment_hash": "<64 hex>"
///   }],
///   "expect": { "chain_genesis": "<64 hex>" }
/// }
/// ```
/// Unknown fields are refused rather than ignored: an activation config is
/// how a chain's inference contract is pinned at height 0, and a binary that
/// silently dropped a field it did not understand would commit a different
/// contract commitment than its peers and fork the chain at its first block.
/// Failing to parse stops that node instead.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NativeActivationRequest {
    pub allowed_executions: Vec<AllowedExecution>,
    #[serde(default)]
    pub recovery_epoch: u64,
    #[serde(default)]
    pub expect: Option<ActivationExpectations>,
    /// Commit-time selection rule (decision D20), fixed for the chain's life.
    /// Absent: a new activation uses `skip-used-nonces-v2`, and a restart
    /// resumes whatever the chain was activated with (chains from before D20
    /// resume `count-every-candidate-v1`). Stated: it must match on restart.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selection_rule: Option<arc_state::NativeSelectionRule>,
    /// Authorisation to activate on a chain that is ALREADY RUNNING. Absent
    /// means fresh private genesis only, exactly as before. Every validator
    /// must be given the identical record, because each of them performs the
    /// same write at the height it names.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub migration: Option<MigrationAuthorization>,
    /// Publish this configuration as a NEW binding over the one already
    /// active, naming the binding being replaced (hex of its commitment).
    ///
    /// Absent, any difference from the active binding is a reconfiguration
    /// attempt and is refused, exactly as before - a typo must never change
    /// a live binding. Present and matching, the new configuration governs
    /// requests admitted from here on, while work already admitted settles
    /// under the binding that accepted it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub update_from: Option<String>,
    /// The coordinated height for that update: the single block at which
    /// every validator publishes the new binding. Publishing writes state,
    /// so doing it whenever each node happens to restart would leave them
    /// deriving different roots. Required whenever `update_from` is set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub update_at_height: Option<u64>,
}

/// The operator's half of an existing-chain migration, as it appears in the
/// activation config. It restates the chain it is for so a record cannot be
/// copied to another chain, and the exact binding so it cannot authorise a
/// different one than the operator reviewed.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MigrationAuthorization {
    /// Hex of the chain's genesis network hash.
    pub chain_genesis: String,
    pub recovery_epoch: u64,
    pub validator_set_id: u64,
    /// The coordinated height. Every validator activates exactly here.
    pub activation_height: u64,
    /// Hex of the commitment of the binding being frozen.
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

/// Parse an operator activation request from JSON.
pub fn load_activation_request(
    path: &std::path::Path,
) -> Result<NativeActivationRequest, ActivationConfigError> {
    let raw = fs::read_to_string(path)
        .map_err(|e| ActivationConfigError::Unreadable(format!("{}: {e}", path.display())))?;
    serde_json::from_str(&raw).map_err(|e| ActivationConfigError::Malformed(e.to_string()))
}

/// Assemble the full admission context from node state plus the operator's
/// allowlist, checking any pins the operator supplied.
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
        if let Some(pin) = expect.chain_genesis {
            if pin != chain_genesis {
                return Err(ActivationConfigError::ExpectationMismatch {
                    field: "chain_genesis".into(),
                    expected: pin.to_hex(),
                    actual: chain_genesis.to_hex(),
                });
            }
        }
        if let Some(pin) = expect.validator_set_hash {
            if pin != validator_set_hash {
                return Err(ActivationConfigError::ExpectationMismatch {
                    field: "validator_set_hash".into(),
                    expected: pin.to_hex(),
                    actual: validator_set_hash.to_hex(),
                });
            }
        }
        if let Some(pin) = &expect.members {
            if pin != &members {
                return Err(ActivationConfigError::ExpectationMismatch {
                    field: "members".into(),
                    expected: format!("{} member(s)", pin.len()),
                    actual: format!("{} member(s)", members.len()),
                });
            }
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

/// Activate the private native-inference contract from an operator config.
///
/// **Default off.** Nothing calls this unless the operator passes
/// `--native-inference-activation <PATH>`. It does not enable production model
/// execution: `CanonicalI8NativeExecutor::load_qualified` still enforces the
/// artifact hash, versioned profile/generation commitments and the explicit
/// reference-qualification flag independently of this.
///
/// The height check is redundant with the one inside
/// `activate_native_inference`, deliberately: the real gate reports "requires
/// unused private genesis", which does not tell an operator which of their
/// assumptions was wrong. This reports the observed height.
pub fn activate_native_inference_from_config(
    state: &arc_state::StateDB,
    path: &std::path::Path,
) -> Result<Hash256, ActivationConfigError> {
    let request = load_activation_request(path)?;

    // RESTART PATH. A persisted context already exists, so this is a reopen,
    // not a new activation.
    //
    // The first version of this function checked the height before looking for
    // an existing context, which made an activated node unable to restart the
    // moment it committed block 1: the operator's own flag became fatal. The
    // fresh-genesis restriction belongs to a NEW activation only.
    //
    // On this path the PERSISTED binding is authoritative and the live
    // committee is not consulted at all. Re-deriving members from
    // `active_validators()` here would let a changed committee silently replace
    // a frozen, operator-approved binding — which is the opposite of what
    // freezing it is for.
    if let Some(persisted) = state.native_inference_context() {
        return resume_persisted_activation(state, &request, &persisted);
    }

    let height = state.height();
    let context = assemble_activation_context(state, &request)?;

    // MIGRATION. An operator record authorises activation on a chain that is
    // already running. The record is checked against this chain's genesis,
    // recovery epoch and validator set before anything is written, and the
    // activation itself still happens only at the one coordinated height.
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
            // Authorised, not yet due. The node keeps running; this is the
            // normal state of every validator between being configured and
            // the chain reaching the coordinated height.
            return Err(ActivationConfigError::MigrationPending {
                at: migration.activation_height,
                height,
            });
        }
        return state
            .activate_native_inference(context)
            .map_err(|e| ActivationConfigError::Refused(e.to_string()));
    }

    // FRESH ACTIVATION. Unused private genesis only.
    if height != 0 {
        return Err(ActivationConfigError::ChainNotEmpty(height));
    }
    state
        .activate_native_inference(context)
        .map_err(|e| ActivationConfigError::Refused(e.to_string()))
}

/// Validate an operator config against an already-persisted activation and
/// resume it.
///
/// Every field the operator can state is compared against the frozen binding.
/// Anything that differs is a reconfiguration attempt and is refused: the
/// contract is explicitly not changeable after activation
/// (`activate_native_inference` returns "native inference activation cannot
/// change"), so the only honest outcomes here are "resume" or "refuse".
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
    // An explicit, pinned update: the operator names the binding being
    // replaced, so this can never fire on a mistyped config and never on a
    // binding other than the one they reviewed.
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
        // Due already, if this node is starting exactly at that height.
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

    // Operator pins are compared against the PERSISTED binding, so a pin that
    // matches the live committee but not the frozen one still fails.
    if let Some(expect) = &request.expect {
        if let Some(pin) = expect.chain_genesis {
            if pin != persisted.domain.chain_genesis {
                return Err(mismatch(
                    "expect.chain_genesis",
                    persisted.domain.chain_genesis.to_hex(),
                    pin.to_hex(),
                ));
            }
        }
        if let Some(pin) = expect.validator_set_hash {
            if pin != persisted.domain.validator_set_hash {
                return Err(mismatch(
                    "expect.validator_set_hash",
                    persisted.domain.validator_set_hash.to_hex(),
                    pin.to_hex(),
                ));
            }
        }
        if let Some(pin) = &expect.members {
            if pin != &persisted.members {
                return Err(mismatch(
                    "expect.members",
                    format!("{} member(s)", persisted.members.len()),
                    format!("{} member(s)", pin.len()),
                ));
            }
        }
    }

    // Hands the persisted context straight back, which lands on the idempotent
    // branch of `activate_native_inference` and revalidates the binding against
    // the chain rather than trusting this function.
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
    /// The one validator identity the test signer votes as. A worker refuses
    /// a vote signed by anyone but the identity its decision store was opened
    /// with, so every store paired with `Sign` is opened with this address.
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
    #[test]
    fn a_job_whose_kv_cache_exceeds_the_budget_is_refused_before_execution() {
        // Canonical 7B: 32 layers x 4096 KV width -> 2 MiB per position.
        assert_eq!(native_kv_bytes(1, 32, 4096), Some(2 << 20));
        let budget = DEFAULT_NATIVE_KV_BUDGET_BYTES;
        // 1 BOS + 1023 prompt + 1024 generated = 2048 positions = 4 GiB: fits.
        assert!(check_native_kv_budget(1023, 1024, 32, 4096, budget).is_ok());
        // One more position does not.
        assert!(matches!(
            check_native_kv_budget(1024, 1024, 32, 4096, budget),
            Err(NativeInferenceError::Executor(message)) if message.contains("KV cache")
        ));
        // The largest request the protocol admits (4096 positions) needs 8 GiB.
        assert!(check_native_kv_budget(2047, 2048, 32, 4096, budget).is_err());
        assert!(check_native_kv_budget(2047, 2048, 32, 4096, 8 << 30).is_ok());
        assert!(check_native_kv_budget(usize::MAX, 1, 32, 4096, u64::MAX).is_err());
        // The cache grows by doubling: 3,000 positions hold 4,096 (8 GiB), so
        // a 6 GiB budget refuses them although 3,000 x 2 MiB would fit.
        assert!(check_native_kv_budget(1499, 1500, 32, 4096, 6 << 30).is_err());
        assert!(check_native_kv_budget(1023, 1024, 32, 4096, 6 << 30).is_ok());
        // What a node publishes as its limit follows the same rule.
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
        // A decision that approves no package is incomplete.
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
        // A canonical executor whose tokenizer could not be read serves
        // execution but not tokenization.
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
        // A different validator's otherwise valid vote cannot be written in
        // this validator-scoped anti-equivocation store.
        assert!(s.persist_signed(bad, &j).is_err());
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
        // An unreadable record is kept: it is not evidence of anything.
        fs::write(dir.path().join("unreadable.decision"), b"bad").unwrap();

        let removed = s.prune_terminal(|id| id == settled.0.request_id).unwrap();
        assert_eq!(removed, 1);
        assert!(
            !s.path(settled.0.request_id).exists(),
            "the settled request's file is gone"
        );
        assert!(
            s.path(still_pending.0.request_id).exists(),
            "a pending request keeps its record"
        );
        assert!(dir.path().join("unreadable.decision").exists());
        // The kept record still protects: a different output for the pending
        // request is refused as equivocation.
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
        // Another validator's store over the same directory prunes nothing of
        // this validator's.
        let other =
            DecisionStore::open(dir.path(), Hash256([0xEE; 32]), j0.genesis, j0.context).unwrap();
        assert_eq!(other.prune_terminal(|_| true).unwrap(), 0);
        assert!(s.path(still_pending.0.request_id).exists());
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
        fs::write(s.path(j.request_id), b"bad").unwrap();
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

        // Expiry: a job at or past its signed expiry height is refused before
        // the executor runs, whatever the queue says.
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
        // Alice queues three requests before Bob queues one: Bob does not
        // wait behind all of Alice's.
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
        // At its expiry height a queued request is dropped unrun and frees
        // its slot; nothing else was queued, so the worker is idle.
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
        // More due requests than queue slots used to fail the whole poll
        // before anything ran, so the runtime stopped after repeated
        // failures. The queue refuses the extra request; running one frees
        // a slot for it.
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

    /// Synthetic CI-only executor. It proves the signed StateDB/mempool
    /// settlement path; it is explicitly not a model-quality or P2P proof.
    struct FixtureExecutor;
    impl NativeExecutor for FixtureExecutor {
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
        // Fixture input uses the same pre-qualified LE-u32 wire shape required
        // by CanonicalI8NativeExecutor, although this test uses FixtureExecutor.
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

    // ── Operator activation config ──────────────────────────────────────────

    fn marker() -> AllowedExecution {
        let m = hash_bytes(b"activation-config-test-marker");
        AllowedExecution {
            model_hash: m,
            profile_hash: m,
            generation_hash: m,
            assignment_hash: m,
        }
    }

    /// A persistent StateDB with a seeded validator registry, which is what the
    /// activation domain binds to.
    fn activatable_state(dir: &std::path::Path) -> (arc_state::StateDB, Vec<ValidatorMember>) {
        let genesis = hash_bytes(b"activation-config-test-genesis");
        let mut members: Vec<ValidatorMember> = (0u8..6)
            .map(|i| ValidatorMember {
                address: hash_bytes(&[i; 4]),
                stake: 1_000_000,
            })
            .collect();
        // validate_members requires strictly ascending addresses
        // (arc-types/src/inference_contract.rs:173).
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

    /// The operator seam for an existing-chain migration. The config is
    /// parsed, the record is built and offered to the state, and the state's
    /// own checks decide - here, that this chain is not recovery-bound, so
    /// the record does not authorise it. Nothing is activated either way.
    /// The coordinated-height behaviour itself is qualified in arc-state
    /// against a recovery-bound chain.
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

    /// A migration section that is not a chain identity at all is a config
    /// error, named by field, not a silent zero hash.
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

    /// Without a migration section the fresh-genesis rule is exactly as it
    /// was: a running chain is refused, by height.
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

    /// The migration section survives a round trip and rejects a field the
    /// operator misspelled, rather than ignoring it.
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

    /// The operator seam for a model upgrade: name the binding being
    /// replaced, and the new configuration governs new work.
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

        // The same file with another allowed execution, unpinned: still a
        // reconfiguration attempt, still refused.
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

        // Pinned to the wrong binding: refused, and still changes nothing.
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

        // Pinned, but with no coordinated height: refused, because every
        // validator has to publish in the same block.
        let mut unscheduled = changed.clone();
        unscheduled.update_from = Some(v1.to_hex());
        write_request(&path, &unscheduled);
        match activate_native_inference_from_config(&state, &path) {
            Err(ActivationConfigError::Malformed(reason)) => {
                assert!(reason.contains("update_at_height"), "{reason}");
            }
            other => panic!("expected a missing-height refusal, got {other:?}"),
        }

        // Pinned and scheduled: authorised, and waiting for its height.
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

        // The chain reaches that height and publishes it, in that block.
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

        // Replaying the same file is now a wrong-pin refusal, because the
        // binding it names is no longer the one in force.
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

        // A stake-0 observer has no committee to bind, and must say so rather
        // than producing an opaque refusal deeper in the stack.
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
        // The regression this exists for: the first version of the wrapper
        // checked the height before looking for an existing context, so a node
        // that activated at genesis and then committed block 1 could never be
        // restarted with its own activation file.
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

        // A different allowlist is a reconfiguration attempt, not a restart.
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

        // A pin that does not match the FROZEN binding is refused too.
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
        // A chain activated with rule v1 (as every chain before D20 was).
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
        // An unchanged config that does not mention the rule resumes it.
        write_request(
            &path,
            &NativeActivationRequest {
                selection_rule: None,
                ..v1.clone()
            },
        );
        activate_native_inference_from_config(&state, &path).expect("resume keeps rule v1");
        // Asking for another rule is a reconfiguration, refused.
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

        // A new activation that does not mention the rule gets v2.
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
        // The dangerous case: the live validator registry moves on, and a
        // restart re-derives members from it, silently replacing an
        // operator-approved binding.
        //
        // That cannot happen, and the refusal comes from two independent
        // places. The restart path here never consults `active_validators()`
        // at all, and `validate_native_inference_activation` re-checks the
        // context against the live registry BEFORE the idempotent branch, so
        // even handing back the persisted context is refused once the registry
        // has drifted.
        //
        // The operational consequence is worth stating plainly: changing the
        // validator registry after activation makes the node unable to restart
        // with its activation file. That is arc-state's existing fail-closed
        // behaviour, not something this wrapper can or should paper over - a
        // binding that no longer describes the chain should not be resumed.
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

        // The drift is refused at the first point it matters: block execution
        // itself fails once the live registry no longer matches the frozen
        // binding. The chain cannot quietly move on under a committee the
        // contract was not bound to.
        let block = state.execute_block_verified_at(&[], Hash256::ZERO, 1);
        assert!(
            block.is_err(),
            "block execution must refuse to proceed under a drifted committee, got {block:?}"
        );

        // And a restart attempt is refused as well, rather than re-deriving
        // members from the drifted registry.
        let outcome = activate_native_inference_from_config(&state, &path);
        assert!(
            outcome.is_err(),
            "a drifted committee must refuse the restart, not absorb the new set"
        );

        // Whatever happened, the frozen binding is untouched: the intruder
        // never appears in it.
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
        // D2 in its real form. On a native-inference chain a block may carry
        // only one native transaction, so a staking transaction can never move
        // the registry. What CAN move it is a restart: the registry is
        // re-seeded from the genesis file on every ordinary start. The check
        // refuses a differing genesis before that re-seed, names each
        // difference, and says what fixes it.
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

        // The activation-time committee is accepted, in any order.
        let mut shuffled = as_genesis(&original_members);
        shuffled.reverse();
        arc_state::inference_contract_state::genesis_committee_matches_binding(&shuffled, &bound)
            .expect("the same committee must be accepted");

        // A member added, one removed, and one stake changed - all named.
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
        // The refusal still names a remedy, and since versioned bindings
        // made a committee change supported, that remedy is the supported
        // path rather than "you cannot do this".
        assert!(
            message.contains("Restart with a genesis file that matches the current binding"),
            "{message}"
        );
        assert!(
            message.contains("A committee change IS supported"),
            "{message}"
        );

        // Refusing touched nothing: the binding still resumes.
        assert_eq!(
            state.native_inference_context().unwrap().members,
            original_members
        );
        activate_native_inference_from_config(&state, &path)
            .expect("the unchanged state still resumes its binding");
    }

    #[test]
    fn a_registry_change_is_refused_by_the_executor_while_a_binding_is_active() {
        // Defence in depth. On today's protocol-4 chains block admission
        // refuses any non-native transaction first, so this guard cannot fire
        // there; it exists so that a future admission change cannot quietly
        // turn a staking transaction back into a chain-wide wedge.
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
        // Advance the chain with an empty block; there is deliberately no
        // height setter on StateDB.
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
        // `remove` transfers each secret key into its independent worker while
        // retaining the fixture's persistent-state metadata for reopen checks.
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

    /// Admit `requests` (one per block, as protocol 4 requires) and return
    /// their ids.
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
        // The collector used to keep ONE output per request, set by whichever
        // vote arrived first. A byzantine member voting first for invented
        // tokens made every honest vote an "equivocation": no certificate,
        // and every honest node's own vote refused.
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
        // The honest five agree on the real output: a strict supermajority of six.
        for honest in &fixture.validators[..5] {
            sink.accept_peer_vote(&vote(request_id, &[71, 72], honest))
                .expect("an honest vote is never refused because of someone else's");
        }
        assert_eq!(
            mempool.len(),
            1,
            "the honest certificate produced one finalize"
        );
        // The byzantine member changing its vote is its own equivocation.
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
        // Before any certificate exists, a vote must carry the output it
        // signs: present, within the request's byte bound, hashing to the
        // declared output hash, and signed over exactly those tokens, by a
        // frozen member. A refused vote leaves no trace.
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

    /// Refuses one request (as the KV budget would), runs the rest.
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
        // Before: the refused request was queued again on every poll, every
        // poll returned an error, nothing reset the runtime's error count,
        // and after 60 polls the worker stopped for every other request too.
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
    fn a_request_settled_before_this_member_runs_it_is_a_refusal_not_a_failure() {
        // Before: a queued request the chain settled first (finalized by the
        // rest of the committee, or refunded) came back from the pending
        // source as `Source(..)`, which counted toward the runtime's
        // consecutive-error limit, so a member slower than the rest could
        // stop its worker for good.
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
        // A live committee that cannot reach a strict two-thirds: four of six
        // equal members vote, the request expires, the requester's refund
        // lands, and the fifth vote that would have completed a certificate
        // arrives too late to create one.
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
        // Both finalizes used to be signed at the finalizer's current nonce:
        // one executed, the other was dropped as stale, and its request -
        // marked submitted - was never signed again.
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
        // The second request's votes come round again (they are re-emitted
        // every few seconds); its finalize now goes out at the next nonce.
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
        // A byte-for-byte replay cannot produce a second terminal receipt.
        assert!(
            fixture
                .state
                .execute_block_verified(&[refund], fixture.finalizer.address())
                .is_err()
        );
    }
}
