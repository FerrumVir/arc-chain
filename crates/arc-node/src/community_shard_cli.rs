use anyhow::{Context, Result, anyhow, bail, ensure};
use arc_crypto::{Hash256, KeyPair};
use arc_inference::cached_integer_model::{
    CANONICAL_REWARD_INFERENCE_PROFILE, CachedIntegerModel, load_cached_model_shard_canonical_i8,
};
use arc_inference::canonical_shard_admission::{
    ArtifactTrustBoundary, CanonicalShardMetadata, ShardAdmissionError, ShardAdmissionEstimate,
};
use arc_inference::model_artifact::{ModelArtifactCommitment, ModelArtifactLoadError};
use arc_node::community_shard_executor::{DEFAULT_MAX_KV_CACHE_BYTES, MAX_KV_CACHE_BYTES_HARD_CAP};
use arc_node::community_shard_worker::{
    CommunityShardWorkerConfig, CommunityShardWorkerError, PartialModelLoader, WorkerRunSummary,
    run_community_shard_worker,
};
use arc_node::community_sharding::{
    MAX_EOS_TOKENS, MAX_HIDDEN_LEN_HARD_CAP, MAX_MODEL_LAYERS_HARD_CAP,
    MAX_SEQUENCE_POSITIONS_HARD_CAP, ShardLeaseAssignment, TrustedModelSpec,
};
use clap::Args;
use serde::Serialize;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use tokio::sync::watch;
use tokio::task::JoinHandle;

pub const DEFAULT_WORKER_THREADS: usize = 1;
pub const MAX_WORKER_THREADS_HARD_CAP: usize = 64;
const HEADER_ADMISSION_ONLY_MODE: &str = "header-admission-only";
const ESTIMATE_BASIS_NOTE: &str = "conservative_payload_budget_not_exact_rss";
const ROLE_PLACEMENT_LIMITATION_NOTE: &str = "conservative_all_roles_required: coordinator protocol does not support role-aware placement; admitted_max_resident_layers must fit first (Q16+I8 embeddings), last (output head + final norm), and interior shard roles within max_working_set_bytes";

#[derive(Clone, Debug, Args)]
pub struct CommunityShardWorkerArgs {
    /// Pinned coordinator base URL (HTTPS required unless --allow-insecure-for-loopback with literal loopback).
    #[arg(long, value_name = "URL")]
    pub coordinator_url: String,

    /// Pinned nonzero 64-hex coordinator identity (32-byte Hash256).
    #[arg(long, value_name = "HEX64", value_parser = parse_nonzero_hash256)]
    pub coordinator_id: Hash256,

    /// Pinned nonzero 64-hex recovered transaction domain (32-byte Hash256).
    #[arg(long, value_name = "HEX64", value_parser = parse_nonzero_hash256)]
    pub transaction_domain: Hash256,

    /// Pinned nonzero 64-hex expected full-artifact BLAKE3 model_id.
    #[arg(long, value_name = "HEX64", value_parser = parse_nonzero_hash256)]
    pub expected_model_id: Hash256,

    /// Local path to a regular GGUF model artifact file (no downloads).
    #[arg(long, value_name = "PATH")]
    pub model_path: PathBuf,

    /// Mode-0600 Ed25519 worker keyfile (required unless --check-only or --check-header-only).
    #[arg(
        long,
        value_name = "PATH",
        required_unless_present_any = ["check_only", "check_header_only"]
    )]
    pub worker_key_file: Option<PathBuf>,

    /// Human-readable worker name registered with the coordinator.
    #[arg(long, value_name = "NAME")]
    pub worker_name: String,

    /// Explicit conservative peak working-set payload budget in bytes.
    #[arg(long, value_name = "BYTES")]
    pub max_working_set_bytes: u64,

    /// Maximum reserved KV cache budget in bytes (default 256 MiB, hard cap 1 GiB).
    #[arg(long, value_name = "BYTES", default_value_t = DEFAULT_MAX_KV_CACHE_BYTES)]
    pub max_kv_cache_bytes: usize,

    /// Optional upper bound on resident layers (must be in 1..=ceil(n_layers/2) and < n_layers).
    #[arg(long, value_name = "LAYERS")]
    pub max_resident_layers: Option<usize>,

    /// Global Rayon thread count (default 1, positive, <= available_parallelism and <= 64).
    #[arg(long, value_name = "COUNT", default_value_t = DEFAULT_WORKER_THREADS)]
    pub threads: usize,

    /// Allow plaintext HTTP only for literal loopback coordinator URLs (127.0.0.1 / [::1]).
    #[arg(long)]
    pub allow_insecure_for_loopback: bool,

    /// Keep the host awake only while executing an assigned partial-model step.
    #[arg(long)]
    pub prevent_sleep_during_jobs: bool,

    /// Verify pinned artifact and conservative all-roles admission, print JSON report, and exit without key loading, Rayon init, weights, or network.
    #[arg(long, conflicts_with = "check_header_only")]
    pub check_only: bool,

    /// Validate pins, bounded GGUF header geometry, and conservative all-roles admission without full-file BLAKE3 hashing, key loading, Rayon init, weights, or network.
    #[arg(long, conflicts_with = "check_only")]
    pub check_header_only: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ShardWorkerLaunchSpec {
    pub(crate) coordinator_url: String,
    pub(crate) coordinator_id: Hash256,
    pub(crate) transaction_domain: Hash256,
    pub(crate) expected_model_id: Hash256,
    pub(crate) model_path: PathBuf,
    pub(crate) worker_name: String,
    pub(crate) max_working_set_bytes: u64,
    pub(crate) max_kv_cache_bytes: usize,
    pub(crate) max_resident_layers: Option<usize>,
    pub(crate) threads: usize,
    pub(crate) allow_insecure_for_loopback: bool,
    pub(crate) prevent_sleep_during_jobs: bool,
    pub(crate) report_mode: &'static str,
}

impl ShardWorkerLaunchSpec {
    fn from_cli_args(args: &CommunityShardWorkerArgs) -> Self {
        Self {
            coordinator_url: args.coordinator_url.clone(),
            coordinator_id: args.coordinator_id,
            transaction_domain: args.transaction_domain,
            expected_model_id: args.expected_model_id,
            model_path: args.model_path.clone(),
            worker_name: args.worker_name.clone(),
            max_working_set_bytes: args.max_working_set_bytes,
            max_kv_cache_bytes: args.max_kv_cache_bytes,
            max_resident_layers: args.max_resident_layers,
            threads: args.threads,
            allow_insecure_for_loopback: args.allow_insecure_for_loopback,
            prevent_sleep_during_jobs: args.prevent_sleep_during_jobs,
            report_mode: if args.check_header_only {
                HEADER_ADMISSION_ONLY_MODE
            } else if args.check_only {
                "check-only"
            } else {
                "run"
            },
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ValidatedInputs {
    pub(crate) normalized_coordinator_url: String,
    pub(crate) worker_name: String,
    pub(crate) platform: String,
    pub(crate) max_kv_cache_bytes_u64: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RoleBudgetReport {
    pub start_layer: usize,
    pub end_layer: usize,
    pub owned_layers: usize,
    pub is_first_shard: bool,
    pub is_last_shard: bool,
    pub layer_weights_bytes: u64,
    pub first_embedding_bytes: u64,
    pub output_head_and_norm_bytes: u64,
    pub reserved_kv_cache_bytes: u64,
    pub resident_payload_bytes: u64,
    pub resident_budget_bytes: u64,
    pub peak_loading_transient_bytes: u64,
    pub peak_loading_budget_bytes: u64,
}

impl RoleBudgetReport {
    fn from_estimate(estimate: &ShardAdmissionEstimate) -> Self {
        Self {
            start_layer: estimate.range.start_layer,
            end_layer: estimate.range.end_layer,
            owned_layers: estimate.owned_layers,
            is_first_shard: estimate.is_first_shard,
            is_last_shard: estimate.is_last_shard,
            layer_weights_bytes: estimate.layer_weights_bytes,
            first_embedding_bytes: estimate.first_embedding_bytes,
            output_head_and_norm_bytes: estimate.output_head_and_norm_bytes,
            reserved_kv_cache_bytes: estimate.reserved_kv_cache_bytes,
            resident_payload_bytes: estimate.resident_payload_bytes,
            resident_budget_bytes: estimate.resident_budget_bytes,
            peak_loading_transient_bytes: estimate.peak_loading_transient_bytes,
            peak_loading_budget_bytes: estimate.peak_loading_budget_bytes,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AllRolesAdmissionReport {
    pub protocol_max_resident_layers: usize,
    pub requested_max_resident_layers: Option<usize>,
    pub admitted_max_resident_layers: usize,
    pub worst_case_resident_budget_bytes: u64,
    pub worst_case_peak_loading_budget_bytes: u64,
    pub first_role: RoleBudgetReport,
    pub last_role: RoleBudgetReport,
    pub interior_role: Option<RoleBudgetReport>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AdmissionReport {
    pub mode: &'static str,
    pub estimate_basis: &'static str,
    pub role_placement_limitation: &'static str,
    pub coordinator_url: String,
    pub coordinator_id: String,
    pub transaction_domain: String,
    pub allow_insecure_for_loopback: bool,
    pub worker_name: String,
    pub platform: String,
    pub threads: usize,
    pub model_name: String,
    pub model_path: String,
    pub model_id: String,
    pub artifact_size_bytes: u64,
    pub execution_profile: String,
    pub architecture: String,
    pub n_layers: usize,
    pub d_model: usize,
    pub n_heads: usize,
    pub n_kv_heads: usize,
    pub d_ff: usize,
    pub d_kv: usize,
    pub d_head: usize,
    pub vocab_size: usize,
    pub max_seq: usize,
    pub bos_token: u32,
    pub eos_tokens: Vec<u32>,
    pub has_tied_output_head: bool,
    pub header_bytes: u64,
    pub max_working_set_bytes: u64,
    pub max_kv_cache_bytes: usize,
    pub admission: AllRolesAdmissionReport,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct HeaderOnlyAdmissionReport {
    pub mode: &'static str,
    pub artifact_verified: bool,
    pub coordinator_url: String,
    pub coordinator_id: String,
    pub transaction_domain: String,
    pub expected_model_id: String,
    pub max_working_set_bytes: u64,
    pub admission: AllRolesAdmissionReport,
}

pub(crate) struct PreparedWorker {
    artifact: Arc<ModelArtifactCommitment>,
    metadata: Arc<CanonicalShardMetadata>,
    model_spec: TrustedModelSpec,
    config: CommunityShardWorkerConfig,
    pub(crate) report: AdmissionReport,
}

impl PreparedWorker {
    pub(crate) fn build_loader(&self) -> Result<PartialModelLoader> {
        let max_kv_cache_bytes_u64 = u64::try_from(self.report.max_kv_cache_bytes)
            .context("--max-kv-cache-bytes exceeds u64 range")?;
        Ok(build_partial_model_loader(
            Arc::clone(&self.artifact),
            Arc::clone(&self.metadata),
            self.model_spec.model_id,
            self.report.admission.admitted_max_resident_layers,
            self.report.max_working_set_bytes,
            max_kv_cache_bytes_u64,
        ))
    }
}

struct WidthRoleEstimates {
    first: ShardAdmissionEstimate,
    last: ShardAdmissionEstimate,
    interior: Option<ShardAdmissionEstimate>,
}

trait ShardBudgetAdmitter {
    fn n_layers(&self) -> usize;
    fn max_shard_layers(&self) -> usize;
    fn admit_range(
        &self,
        range: std::ops::Range<usize>,
        max_working_set_bytes: u64,
        max_kv_cache_bytes: u64,
    ) -> std::result::Result<ShardAdmissionEstimate, ShardAdmissionError>;
}

impl ShardBudgetAdmitter for CanonicalShardMetadata {
    fn n_layers(&self) -> usize {
        self.n_layers()
    }

    fn max_shard_layers(&self) -> usize {
        self.max_shard_layers()
    }

    fn admit_range(
        &self,
        range: std::ops::Range<usize>,
        max_working_set_bytes: u64,
        max_kv_cache_bytes: u64,
    ) -> std::result::Result<ShardAdmissionEstimate, ShardAdmissionError> {
        self.admit(range, max_working_set_bytes, max_kv_cache_bytes)
    }
}

struct ShutdownSignals {
    #[cfg(unix)]
    sigint: tokio::signal::unix::Signal,
    #[cfg(unix)]
    sigterm: tokio::signal::unix::Signal,
}

impl ShutdownSignals {
    fn install() -> Result<Self> {
        #[cfg(unix)]
        {
            let sigint = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())
                .context("failed to install SIGINT handler")?;
            let sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                .context("failed to install SIGTERM handler")?;
            Ok(Self { sigint, sigterm })
        }
        #[cfg(not(unix))]
        {
            Ok(Self {})
        }
    }

    async fn recv(&mut self) -> &'static str {
        #[cfg(unix)]
        {
            tokio::select! {
                _ = self.sigint.recv() => "SIGINT",
                _ = self.sigterm.recv() => "SIGTERM",
            }
        }
        #[cfg(not(unix))]
        {
            match tokio::signal::ctrl_c().await {
                Ok(()) => "CTRL-C",
                Err(_) => "CTRL-C-ERROR",
            }
        }
    }
}

pub(crate) fn parse_nonzero_hash256(encoded: &str) -> std::result::Result<Hash256, String> {
    if encoded.len() != 64 {
        return Err("must contain exactly 64 hexadecimal characters (32 bytes)".to_string());
    }
    let mut bytes = [0u8; 32];
    hex::decode_to_slice(encoded, &mut bytes)
        .map_err(|_| "must contain exactly 64 hexadecimal characters (32 bytes)".to_string())?;
    let hash = Hash256(bytes);
    if hash == Hash256::ZERO {
        return Err("must be nonzero".to_string());
    }
    Ok(hash)
}

fn current_platform() -> String {
    format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH)
}

fn canonical_model_name(metadata: &CanonicalShardMetadata) -> String {
    format!(
        "arc-{}L-{}d-{}h-{}v",
        metadata.n_layers(),
        metadata.d_model(),
        metadata.n_heads(),
        metadata.vocab_size(),
    )
}

pub(crate) fn host_available_parallelism() -> usize {
    std::thread::available_parallelism()
        .map(usize::from)
        .unwrap_or(1)
        .max(1)
}

fn validate_thread_count(threads: usize, available_parallelism: usize) -> Result<()> {
    let available = available_parallelism.max(1);
    ensure!(threads >= 1, "--threads must be at least 1");
    ensure!(
        threads <= MAX_WORKER_THREADS_HARD_CAP,
        "--threads {threads} exceeds hard cap {MAX_WORKER_THREADS_HARD_CAP}"
    );
    ensure!(
        threads <= available,
        "--threads {threads} exceeds host available parallelism ({available})"
    );
    Ok(())
}

pub(crate) fn validate_launch_spec_with_parallelism(
    spec: &ShardWorkerLaunchSpec,
    available_parallelism: usize,
) -> Result<ValidatedInputs> {
    ensure!(
        spec.coordinator_id != Hash256::ZERO,
        "--coordinator-id must be nonzero"
    );
    ensure!(
        spec.transaction_domain != Hash256::ZERO,
        "--transaction-domain must be nonzero"
    );
    ensure!(
        spec.expected_model_id != Hash256::ZERO,
        "--expected-model-id must be nonzero"
    );
    ensure!(
        spec.max_working_set_bytes > 0,
        "--max-working-set-bytes must be nonzero"
    );
    ensure!(
        spec.max_kv_cache_bytes > 0 && spec.max_kv_cache_bytes <= MAX_KV_CACHE_BYTES_HARD_CAP,
        "--max-kv-cache-bytes {} must be in 1..={MAX_KV_CACHE_BYTES_HARD_CAP}",
        spec.max_kv_cache_bytes
    );
    let max_kv_cache_bytes_u64 =
        u64::try_from(spec.max_kv_cache_bytes).context("--max-kv-cache-bytes exceeds u64 range")?;
    ensure!(
        spec.max_working_set_bytes >= max_kv_cache_bytes_u64,
        "--max-working-set-bytes ({}) must be at least --max-kv-cache-bytes ({})",
        spec.max_working_set_bytes,
        spec.max_kv_cache_bytes
    );
    if let Some(limit) = spec.max_resident_layers {
        let protocol_cap = MAX_MODEL_LAYERS_HARD_CAP.div_ceil(2);
        ensure!(
            limit >= 1 && limit <= protocol_cap,
            "--max-resident-layers {limit} must be in 1..={protocol_cap}"
        );
    }
    validate_thread_count(spec.threads, available_parallelism)?;
    let worker_name = spec.worker_name.trim().to_string();
    ensure!(
        !worker_name.is_empty() && !worker_name.chars().any(char::is_control),
        "--worker-name must be non-empty without control characters"
    );
    let platform = current_platform();
    let probe_spec = TrustedModelSpec {
        model_id: spec.expected_model_id,
        execution_profile: CANONICAL_REWARD_INFERENCE_PROFILE.to_string(),
        n_layers: 2,
        d_model: 64,
        vocab_size: 32,
        max_seq: 64,
        eos_tokens: vec![2],
        bos_token: 1,
    };
    let mut probe_config = CommunityShardWorkerConfig::new(
        spec.coordinator_url.clone(),
        spec.coordinator_id,
        Some(spec.transaction_domain),
        1,
        worker_name.clone(),
        "arc-2L-64d-1h-32v",
        platform.clone(),
    );
    probe_config.allow_insecure_for_loopback = spec.allow_insecure_for_loopback;
    probe_config.executor_config.max_kv_cache_bytes = spec.max_kv_cache_bytes;
    let normalized_coordinator_url = probe_config
        .validate(&probe_spec)
        .context("invalid community shard worker CLI configuration")?;
    Ok(ValidatedInputs {
        normalized_coordinator_url,
        worker_name,
        platform,
        max_kv_cache_bytes_u64,
    })
}

fn validate_args_with_parallelism(
    args: &CommunityShardWorkerArgs,
    available_parallelism: usize,
) -> Result<ValidatedInputs> {
    ensure!(
        !(args.check_only && args.check_header_only),
        "--check-only and --check-header-only are mutually exclusive"
    );
    ensure!(
        args.coordinator_id != Hash256::ZERO,
        "--coordinator-id must be nonzero"
    );
    ensure!(
        args.transaction_domain != Hash256::ZERO,
        "--transaction-domain must be nonzero"
    );
    ensure!(
        args.expected_model_id != Hash256::ZERO,
        "--expected-model-id must be nonzero"
    );
    ensure!(
        args.check_only || args.check_header_only || args.worker_key_file.is_some(),
        "--worker-key-file is required unless --check-only or --check-header-only is set"
    );
    let spec = ShardWorkerLaunchSpec::from_cli_args(args);
    validate_launch_spec_with_parallelism(&spec, available_parallelism)
}

pub(crate) fn ensure_regular_model_file(path: &Path) -> Result<()> {
    ensure!(
        path.to_str().is_some(),
        "model artifact path {} is not valid UTF-8",
        path.display()
    );
    let meta = std::fs::metadata(path).with_context(|| {
        format!(
            "model path {} must reference an existing local regular file (model downloads are disabled)",
            path.display()
        )
    })?;
    ensure!(
        meta.is_file(),
        "model path {} must be a regular file, not a directory, pipe, socket, or device (model downloads are disabled)",
        path.display()
    );
    ensure!(meta.len() > 0, "model artifact {} is empty", path.display());
    Ok(())
}

fn validate_metadata_protocol_geometry(metadata: &CanonicalShardMetadata) -> Result<()> {
    ensure!(
        metadata.canonical_profile() == CANONICAL_REWARD_INFERENCE_PROFILE,
        "unsupported execution profile {:?}; expected {:?}",
        metadata.canonical_profile(),
        CANONICAL_REWARD_INFERENCE_PROFILE
    );
    ensure!(
        metadata.n_layers() >= 2 && metadata.n_layers() <= MAX_MODEL_LAYERS_HARD_CAP,
        "model n_layers {} must be in 2..={MAX_MODEL_LAYERS_HARD_CAP}",
        metadata.n_layers()
    );
    ensure!(
        metadata.d_model() > 0 && metadata.d_model() <= MAX_HIDDEN_LEN_HARD_CAP,
        "model d_model {} must be in 1..={MAX_HIDDEN_LEN_HARD_CAP}",
        metadata.d_model()
    );
    ensure!(metadata.n_heads() > 0, "model n_heads must be nonzero");
    ensure!(
        metadata.vocab_size() > 0,
        "model vocab_size must be nonzero"
    );
    ensure!(
        metadata.max_seq() > 0 && metadata.max_seq() <= MAX_SEQUENCE_POSITIONS_HARD_CAP,
        "model max_seq {} must be in 1..={MAX_SEQUENCE_POSITIONS_HARD_CAP}",
        metadata.max_seq()
    );
    ensure!(
        metadata.eos_tokens().len() <= MAX_EOS_TOKENS,
        "model eos_tokens count {} exceeds maximum {MAX_EOS_TOKENS}",
        metadata.eos_tokens().len()
    );
    ensure!(
        usize::try_from(metadata.bos_token()).is_ok_and(|token| token < metadata.vocab_size()),
        "model bos_token {} out of range for vocab_size {}",
        metadata.bos_token(),
        metadata.vocab_size()
    );
    for &eos in metadata.eos_tokens() {
        ensure!(
            usize::try_from(eos).is_ok_and(|token| token < metadata.vocab_size()),
            "model eos_token {eos} out of range for vocab_size {}",
            metadata.vocab_size()
        );
    }
    Ok(())
}

fn validate_metadata_against_protocol(
    metadata: &CanonicalShardMetadata,
    expected_model_id: Hash256,
    expected_size_bytes: u64,
) -> Result<()> {
    ensure!(
        metadata.trust_boundary()
            == ArtifactTrustBoundary::VerifiedBlake3Commitment {
                model_id: expected_model_id,
                size_bytes: expected_size_bytes,
            },
        "model metadata trust boundary does not match verified BLAKE3 commitment"
    );
    validate_metadata_protocol_geometry(metadata)
}

pub(crate) fn prevalidate_early_shard_model_header(
    spec: &ShardWorkerLaunchSpec,
    validated: &ValidatedInputs,
) -> Result<CanonicalShardMetadata> {
    ensure_regular_model_file(&spec.model_path)?;
    let metadata = CanonicalShardMetadata::from_path(&spec.model_path).with_context(|| {
        format!(
            "bounded pre-bind GGUF header validation failed for {}",
            spec.model_path.display()
        )
    })?;
    ensure!(
        metadata.trust_boundary() == ArtifactTrustBoundary::UnverifiedHeaderOnly,
        "pre-bind shard header inspection must remain unverified until BLAKE3 commitment completes"
    );
    validate_metadata_protocol_geometry(&metadata)?;
    let _ = derive_all_roles_admission(
        &metadata,
        spec.max_resident_layers,
        spec.max_working_set_bytes,
        validated.max_kv_cache_bytes_u64,
    )?;
    Ok(metadata)
}

fn check_header_only_with_parallelism(
    args: &CommunityShardWorkerArgs,
    available_parallelism: usize,
) -> Result<HeaderOnlyAdmissionReport> {
    ensure!(
        args.check_header_only,
        "--check-header-only must be set for header-only admission check"
    );
    let validated = validate_args_with_parallelism(args, available_parallelism)?;
    let spec = ShardWorkerLaunchSpec::from_cli_args(args);
    let metadata = prevalidate_early_shard_model_header(&spec, &validated)?;
    ensure!(
        metadata.trust_boundary() == ArtifactTrustBoundary::UnverifiedHeaderOnly,
        "header-only admission check must keep model metadata unverified"
    );
    let admission = derive_all_roles_admission(
        &metadata,
        spec.max_resident_layers,
        spec.max_working_set_bytes,
        validated.max_kv_cache_bytes_u64,
    )?;
    Ok(HeaderOnlyAdmissionReport {
        mode: HEADER_ADMISSION_ONLY_MODE,
        artifact_verified: false,
        coordinator_url: validated.normalized_coordinator_url,
        coordinator_id: spec.coordinator_id.to_hex(),
        transaction_domain: spec.transaction_domain.to_hex(),
        expected_model_id: spec.expected_model_id.to_hex(),
        max_working_set_bytes: spec.max_working_set_bytes,
        admission,
    })
}

fn evaluate_roles_at_width<A: ShardBudgetAdmitter>(
    admitter: &A,
    width: usize,
    max_working_set_bytes: u64,
    max_kv_cache_bytes: u64,
) -> std::result::Result<WidthRoleEstimates, ShardAdmissionError> {
    let n_layers = admitter.n_layers();
    let first = admitter.admit_range(0..width, max_working_set_bytes, max_kv_cache_bytes)?;
    let last = admitter.admit_range(
        (n_layers - width)..n_layers,
        max_working_set_bytes,
        max_kv_cache_bytes,
    )?;
    let interior = if 1 + width < n_layers {
        Some(admitter.admit_range(1..(1 + width), max_working_set_bytes, max_kv_cache_bytes)?)
    } else {
        None
    };
    Ok(WidthRoleEstimates {
        first,
        last,
        interior,
    })
}

fn derive_all_roles_admission<A: ShardBudgetAdmitter>(
    admitter: &A,
    requested_max_resident_layers: Option<usize>,
    max_working_set_bytes: u64,
    max_kv_cache_bytes: u64,
) -> Result<AllRolesAdmissionReport> {
    let n_layers = admitter.n_layers();
    ensure!(
        (2..=MAX_MODEL_LAYERS_HARD_CAP).contains(&n_layers),
        "model n_layers {n_layers} must be in 2..={MAX_MODEL_LAYERS_HARD_CAP} for partial sharding"
    );
    let protocol_max = admitter.max_shard_layers().min(n_layers - 1);
    ensure!(
        protocol_max >= 1 && protocol_max < n_layers && protocol_max <= n_layers.div_ceil(2),
        "invalid protocol max_resident_layers {protocol_max} for n_layers={n_layers}"
    );
    let upper_bound = match requested_max_resident_layers {
        Some(limit) => {
            ensure!(
                limit >= 1 && limit <= protocol_max && limit < n_layers,
                "--max-resident-layers {limit} must be in 1..={protocol_max} and strictly less than n_layers ({n_layers})"
            );
            limit
        }
        None => protocol_max,
    };
    let width_one = match evaluate_roles_at_width(
        admitter,
        1,
        max_working_set_bytes,
        max_kv_cache_bytes,
    ) {
        Ok(estimates) => estimates,
        Err(ShardAdmissionError::BudgetExceeded {
            start_layer,
            end_layer,
            resident_budget_bytes,
            peak_loading_budget_bytes,
            reserved_kv_cache_bytes,
            max_working_set_bytes,
            shortfall_bytes,
            role_note,
        }) => {
            bail!(
                "cannot admit even 1 resident layer for shard [{start_layer}, {end_layer}){role_note}: \
                 conservative peak_loading_budget_bytes={peak_loading_budget_bytes} \
                 (resident_budget_bytes={resident_budget_bytes}, reserved_kv_cache_bytes={reserved_kv_cache_bytes}) \
                 exceeds --max-working-set-bytes={max_working_set_bytes} by {shortfall_bytes} bytes; \
                 {ROLE_PLACEMENT_LIMITATION_NOTE}; increase --max-working-set-bytes to at least \
                 {peak_loading_budget_bytes}, lower --max-kv-cache-bytes, or run on a host with more RAM"
            );
        }
        Err(other) => {
            return Err(anyhow::Error::from(other))
                .context("failed to evaluate conservative shard admission for 1 resident layer");
        }
    };
    let mut low = 1usize;
    let mut high = upper_bound;
    let mut best = width_one;
    while low < high {
        let mid = low + (high - low).div_ceil(2);
        match evaluate_roles_at_width(admitter, mid, max_working_set_bytes, max_kv_cache_bytes) {
            Ok(estimates) => {
                best = estimates;
                low = mid;
            }
            Err(ShardAdmissionError::BudgetExceeded { .. }) => {
                high = mid - 1;
            }
            Err(other) => {
                return Err(anyhow::Error::from(other)).with_context(|| {
                    format!("failed to evaluate conservative shard admission at width {mid}")
                });
            }
        }
    }
    let mut worst_resident = best
        .first
        .resident_budget_bytes
        .max(best.last.resident_budget_bytes);
    let mut worst_peak = best
        .first
        .peak_loading_budget_bytes
        .max(best.last.peak_loading_budget_bytes);
    if let Some(interior) = &best.interior {
        worst_resident = worst_resident.max(interior.resident_budget_bytes);
        worst_peak = worst_peak.max(interior.peak_loading_budget_bytes);
    }
    Ok(AllRolesAdmissionReport {
        protocol_max_resident_layers: protocol_max,
        requested_max_resident_layers,
        admitted_max_resident_layers: low,
        worst_case_resident_budget_bytes: worst_resident,
        worst_case_peak_loading_budget_bytes: worst_peak,
        first_role: RoleBudgetReport::from_estimate(&best.first),
        last_role: RoleBudgetReport::from_estimate(&best.last),
        interior_role: best.interior.as_ref().map(RoleBudgetReport::from_estimate),
    })
}

#[derive(Debug, thiserror::Error)]
#[error("model preparation canceled")]
pub(crate) struct ModelPreparationCanceled;

pub(crate) fn prepare_launch_spec_blocking(
    spec: &ShardWorkerLaunchSpec,
    validated: &ValidatedInputs,
    cancel: &AtomicBool,
) -> Result<PreparedWorker> {
    prepare_launch_spec_blocking_with_after_commit_hook(spec, validated, cancel, || {})
}

fn prepare_launch_spec_blocking_with_after_commit_hook(
    spec: &ShardWorkerLaunchSpec,
    validated: &ValidatedInputs,
    cancel: &AtomicBool,
    after_first_commit: impl FnOnce(),
) -> Result<PreparedWorker> {
    if cancel.load(Ordering::Acquire) {
        return Err(ModelPreparationCanceled.into());
    }
    ensure_regular_model_file(&spec.model_path)?;
    let artifact = match ModelArtifactCommitment::from_path_cancellable(&spec.model_path, cancel) {
        Ok(artifact) => artifact,
        Err(ModelArtifactLoadError::Cancelled) => return Err(ModelPreparationCanceled.into()),
        Err(ModelArtifactLoadError::Inference(error)) => {
            return Err(error).with_context(|| {
                format!(
                    "failed to compute BLAKE3 commitment for {}",
                    spec.model_path.display()
                )
            });
        }
    };
    ensure!(
        artifact.model_id() == spec.expected_model_id,
        "model artifact {} BLAKE3 digest {} does not match pinned --expected-model-id {}; \
         refusing metadata parsing, key loading, or coordinator registration",
        spec.model_path.display(),
        artifact.model_id().to_hex(),
        spec.expected_model_id.to_hex()
    );
    after_first_commit();
    let metadata =
        match CanonicalShardMetadata::from_committed_artifact_cancellable(&artifact, cancel) {
            Ok(metadata) => metadata,
            Err(ShardAdmissionError::Cancelled) => return Err(ModelPreparationCanceled.into()),
            Err(error) => {
                return Err(error).with_context(|| {
                    format!(
                        "failed to inspect committed model artifact {}",
                        spec.model_path.display()
                    )
                });
            }
        };
    if cancel.load(Ordering::Acquire) {
        return Err(ModelPreparationCanceled.into());
    }
    validate_metadata_against_protocol(&metadata, spec.expected_model_id, artifact.size_bytes())?;
    let admission = derive_all_roles_admission(
        &metadata,
        spec.max_resident_layers,
        spec.max_working_set_bytes,
        validated.max_kv_cache_bytes_u64,
    )?;
    let model_name = canonical_model_name(&metadata);
    let model_spec = TrustedModelSpec {
        model_id: artifact.model_id(),
        execution_profile: metadata.canonical_profile().to_string(),
        n_layers: metadata.n_layers(),
        d_model: metadata.d_model(),
        vocab_size: metadata.vocab_size(),
        max_seq: metadata.max_seq(),
        eos_tokens: metadata.eos_tokens().to_vec(),
        bos_token: metadata.bos_token(),
    };
    let mut config = CommunityShardWorkerConfig::new(
        validated.normalized_coordinator_url.clone(),
        spec.coordinator_id,
        Some(spec.transaction_domain),
        admission.admitted_max_resident_layers,
        validated.worker_name.clone(),
        model_name.clone(),
        validated.platform.clone(),
    );
    config.allow_insecure_for_loopback = spec.allow_insecure_for_loopback;
    config.prevent_sleep_during_jobs = spec.prevent_sleep_during_jobs;
    config.executor_config.max_kv_cache_bytes = spec.max_kv_cache_bytes;
    let normalized_coordinator_url = config
        .validate(&model_spec)
        .context("worker configuration failed validation against committed model spec")?;
    let report = AdmissionReport {
        mode: spec.report_mode,
        estimate_basis: ESTIMATE_BASIS_NOTE,
        role_placement_limitation: ROLE_PLACEMENT_LIMITATION_NOTE,
        coordinator_url: normalized_coordinator_url,
        coordinator_id: spec.coordinator_id.to_hex(),
        transaction_domain: spec.transaction_domain.to_hex(),
        allow_insecure_for_loopback: spec.allow_insecure_for_loopback,
        worker_name: validated.worker_name.clone(),
        platform: validated.platform.clone(),
        threads: spec.threads,
        model_name,
        model_path: spec.model_path.display().to_string(),
        model_id: artifact.model_id().to_hex(),
        artifact_size_bytes: artifact.size_bytes(),
        execution_profile: metadata.canonical_profile().to_string(),
        architecture: metadata.architecture().to_string(),
        n_layers: metadata.n_layers(),
        d_model: metadata.d_model(),
        n_heads: metadata.n_heads(),
        n_kv_heads: metadata.n_kv_heads(),
        d_ff: metadata.d_ff(),
        d_kv: metadata.d_kv(),
        d_head: metadata.d_head(),
        vocab_size: metadata.vocab_size(),
        max_seq: metadata.max_seq(),
        bos_token: metadata.bos_token(),
        eos_tokens: metadata.eos_tokens().to_vec(),
        has_tied_output_head: metadata.has_tied_output_head(),
        header_bytes: metadata.header_bytes(),
        max_working_set_bytes: spec.max_working_set_bytes,
        max_kv_cache_bytes: spec.max_kv_cache_bytes,
        admission,
    };
    Ok(PreparedWorker {
        artifact: Arc::new(artifact),
        metadata: Arc::new(metadata),
        model_spec,
        config,
        report,
    })
}

fn prepare_model_blocking(
    args: &CommunityShardWorkerArgs,
    validated: &ValidatedInputs,
    cancel: &AtomicBool,
) -> Result<PreparedWorker> {
    ensure!(
        !args.check_header_only,
        "--check-header-only must not enter full verified model preparation"
    );
    let spec = ShardWorkerLaunchSpec::from_cli_args(args);
    prepare_launch_spec_blocking(&spec, validated, cancel)
}

#[derive(Clone, Copy)]
struct LeaseLoadPolicy<'a> {
    expected_model_id: Hash256,
    expected_profile: &'a str,
    admitted_max_resident_layers: usize,
    max_working_set_bytes: u64,
    max_kv_cache_bytes: u64,
}

fn load_and_verify_lease<M, A, F>(
    artifact: &ModelArtifactCommitment,
    admitter: &A,
    policy: &LeaseLoadPolicy<'_>,
    lease: &ShardLeaseAssignment,
    load_fn: F,
) -> Result<M>
where
    A: ShardBudgetAdmitter,
    F: FnOnce(&Path, usize, usize) -> Result<M>,
{
    let LeaseLoadPolicy {
        expected_model_id,
        expected_profile,
        admitted_max_resident_layers,
        max_working_set_bytes,
        max_kv_cache_bytes,
    } = *policy;
    ensure!(
        lease.model_id == expected_model_id && lease.model_id == artifact.model_id(),
        "lease model_id {} does not match pinned artifact {}",
        lease.model_id.to_hex(),
        expected_model_id.to_hex()
    );
    ensure!(
        lease.execution_profile == expected_profile,
        "lease execution_profile {:?} does not match canonical profile {:?}",
        lease.execution_profile,
        expected_profile
    );
    let n_layers = admitter.n_layers();
    ensure!(
        lease.range.is_valid_for(n_layers),
        "lease shard range [{}, {}) is invalid for n_layers={n_layers}",
        lease.range.start_layer,
        lease.range.end_layer
    );
    let owned = lease.range.len();
    ensure!(
        owned >= 1 && owned <= admitted_max_resident_layers && owned < n_layers,
        "lease shard range [{}, {}) owns {owned} layer(s), exceeding admitted_max_resident_layers={admitted_max_resident_layers} (n_layers={n_layers})",
        lease.range.start_layer,
        lease.range.end_layer
    );
    admitter
        .admit_range(
            lease.range.start_layer..lease.range.end_layer,
            max_working_set_bytes,
            max_kv_cache_bytes,
        )
        .with_context(|| {
            format!(
                "lease shard [{}, {}) rejected by conservative admission before loading weights",
                lease.range.start_layer, lease.range.end_layer
            )
        })?;
    artifact
        .verify_unchanged()
        .context("pinned model artifact changed before partial shard load")?;
    let loaded = load_fn(
        artifact.path(),
        lease.range.start_layer,
        lease.range.end_layer,
    )?;
    artifact
        .verify_unchanged()
        .context("pinned model artifact changed during partial shard load")?;
    Ok(loaded)
}

fn load_strict_partial_shard(
    path: &Path,
    start_layer: usize,
    end_layer: usize,
) -> Result<CachedIntegerModel> {
    let path_str = path
        .to_str()
        .ok_or_else(|| anyhow!("model artifact path {} is not valid UTF-8", path.display()))?;
    load_cached_model_shard_canonical_i8(path_str, start_layer, end_layer).with_context(|| {
        format!(
            "failed to load canonical partial shard [{start_layer}, {end_layer}) from {}",
            path.display()
        )
    })
}

fn build_partial_model_loader(
    artifact: Arc<ModelArtifactCommitment>,
    metadata: Arc<CanonicalShardMetadata>,
    expected_model_id: Hash256,
    admitted_max_resident_layers: usize,
    max_working_set_bytes: u64,
    max_kv_cache_bytes: u64,
) -> PartialModelLoader {
    let load_lock = Arc::new(Mutex::new(()));
    Arc::new(move |lease: &ShardLeaseAssignment| {
        let _guard = load_lock
            .lock()
            .map_err(|_| anyhow!("partial model loader lock poisoned"))?;
        let model = load_and_verify_lease(
            artifact.as_ref(),
            metadata.as_ref(),
            &LeaseLoadPolicy {
                expected_model_id,
                expected_profile: metadata.canonical_profile(),
                admitted_max_resident_layers,
                max_working_set_bytes,
                max_kv_cache_bytes,
            },
            lease,
            load_strict_partial_shard,
        )?;
        Ok(Arc::new(model))
    })
}

pub(crate) fn configure_global_rayon_pool(threads: usize) -> Result<()> {
    rayon::ThreadPoolBuilder::new()
        .num_threads(threads)
        .thread_name(|index| format!("arc-shard-rayon-{index}"))
        .build_global()
        .with_context(|| {
            format!(
                "failed to initialize global Rayon pool with {threads} thread(s); \
                 refusing to run with an unmanaged thread pool"
            )
        })
}

async fn await_prepare_with_cancel<T, S>(
    mut handle: JoinHandle<Result<T>>,
    cancel: Arc<AtomicBool>,
    shutdown_signal: S,
) -> Result<T>
where
    S: Future<Output = &'static str>,
{
    tokio::select! {
        biased;
        signal_name = shutdown_signal => {
            cancel.store(true, Ordering::Release);
            let _ = handle.await;
            bail!("shutdown requested ({signal_name}) during model preparation; blocking task drained");
        }
        result = &mut handle => result.context("model preparation task panicked")?,
    }
}

async fn drive_worker_with_shutdown<T, E, W, S>(
    worker_future: W,
    shutdown_tx: watch::Sender<bool>,
    shutdown_signal: S,
) -> std::result::Result<T, E>
where
    W: Future<Output = std::result::Result<T, E>>,
    S: Future<Output = &'static str>,
{
    tokio::pin!(worker_future);
    tokio::select! {
        biased;
        signal_name = shutdown_signal => {
            eprintln!(
                "received {signal_name}; signaling community shard worker shutdown and awaiting drain"
            );
            let _ = shutdown_tx.send(true);
            worker_future.await
        }
        result = &mut worker_future => result,
    }
}

fn format_exit_report(
    worker_address: Hash256,
    report: &AdmissionReport,
    summary: &WorkerRunSummary,
) -> serde_json::Value {
    serde_json::json!({
        "worker_id": worker_address.to_hex(),
        "worker_name": report.worker_name,
        "coordinator_url": report.coordinator_url,
        "coordinator_id": report.coordinator_id,
        "transaction_domain": report.transaction_domain,
        "model_id": report.model_id,
        "model_name": report.model_name,
        "admitted_max_resident_layers": report.admission.admitted_max_resident_layers,
        "threads": report.threads,
        "summary": {
            "registrations": summary.registrations,
            "leases_acquired": summary.leases_acquired,
            "models_loaded": summary.models_loaded,
            "heartbeats_sent": summary.heartbeats_sent,
            "steps_executed": summary.steps_executed,
            "steps_submitted": summary.steps_submitted,
            "submit_retries": summary.submit_retries,
            "submit_stale_conflicts": summary.submit_stale_conflicts,
            "cancellations_applied": summary.cancellations_applied,
            "rebalances_completed": summary.rebalances_completed,
            "clean_release_sent": summary.clean_release_sent,
        }
    })
}

pub async fn run(args: CommunityShardWorkerArgs) -> Result<()> {
    let available_parallelism = host_available_parallelism();
    if args.check_header_only {
        let report = check_header_only_with_parallelism(&args, available_parallelism)?;
        println!("{}", serde_json::to_string_pretty(&report)?);
        return Ok(());
    }
    let validated = validate_args_with_parallelism(&args, available_parallelism)?;
    let mut signals = ShutdownSignals::install()?;
    let cancel_prepare = Arc::new(AtomicBool::new(false));
    let prepare_args = args.clone();
    let prepare_validated = validated.clone();
    let prepare_cancel = Arc::clone(&cancel_prepare);
    let prepare_handle = tokio::task::spawn_blocking(move || {
        prepare_model_blocking(&prepare_args, &prepare_validated, &prepare_cancel)
    });
    let prepared =
        await_prepare_with_cancel(prepare_handle, cancel_prepare, signals.recv()).await?;
    if args.check_only {
        println!("{}", serde_json::to_string_pretty(&prepared.report)?);
        return Ok(());
    }
    let keyfile_path = args.worker_key_file.as_deref().ok_or_else(|| {
        anyhow!("--worker-key-file is required unless --check-only or --check-header-only is set")
    })?;
    let keypair = Arc::new(crate::validator_identity::load_ed25519_keyfile(
        keyfile_path,
    )?);
    let worker_address = keypair.address();
    configure_global_rayon_pool(args.threads)?;
    let loader = prepared.build_loader()?;
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let worker_future = run_community_shard_worker(
        prepared.config,
        keypair,
        prepared.model_spec,
        loader,
        shutdown_rx,
    );
    let summary = drive_worker_with_shutdown(worker_future, shutdown_tx, signals.recv())
        .await
        .map_err(anyhow::Error::from)?;
    println!(
        "{}",
        serde_json::to_string_pretty(&format_exit_report(
            worker_address,
            &prepared.report,
            &summary,
        ))?
    );
    Ok(())
}

async fn drive_integrated_worker_task<F, Fut>(
    report: AdmissionReport,
    status: Option<Arc<arc_node::community_worker::CommunityWorkerStatus>>,
    mut shutdown_rx: watch::Receiver<bool>,
    run_worker: F,
) where
    F: FnOnce(watch::Receiver<bool>) -> Fut,
    Fut: Future<Output = std::result::Result<WorkerRunSummary, CommunityShardWorkerError>>,
{
    if *shutdown_rx.borrow_and_update() {
        if let Some(status) = status.as_ref() {
            status.record_shard_stopped();
        }
        tracing::info!(
            coordinator_url = %report.coordinator_url,
            "Integrated community shard worker skipped start because shutdown was already requested"
        );
        return;
    }
    tracing::info!(
        coordinator_url = %report.coordinator_url,
        coordinator_id = %report.coordinator_id,
        transaction_domain = %report.transaction_domain,
        model_id = %report.model_id,
        model_name = %report.model_name,
        admitted_max_resident_layers = report.admission.admitted_max_resident_layers,
        threads = report.threads,
        worker_name = %report.worker_name,
        "Integrated community shard worker started"
    );
    match run_worker(shutdown_rx).await {
        Ok(summary) => {
            tracing::info!(
                registrations = summary.registrations,
                leases_acquired = summary.leases_acquired,
                models_loaded = summary.models_loaded,
                heartbeats_sent = summary.heartbeats_sent,
                steps_executed = summary.steps_executed,
                steps_submitted = summary.steps_submitted,
                submit_retries = summary.submit_retries,
                submit_stale_conflicts = summary.submit_stale_conflicts,
                cancellations_applied = summary.cancellations_applied,
                rebalances_completed = summary.rebalances_completed,
                clean_release_sent = summary.clean_release_sent,
                reconnect_attempts = summary.reconnect_attempts,
                "Integrated community shard worker stopped at the lifecycle barrier"
            );
        }
        Err(error) => {
            tracing::error!(
                %error,
                coordinator_url = %report.coordinator_url,
                "Integrated community shard worker exited with error"
            );
        }
    }
}

fn configure_integrated_worker_config(
    mut config: CommunityShardWorkerConfig,
    status: Arc<arc_node::community_worker::CommunityWorkerStatus>,
    worker_execution_gate: Arc<arc_node::native_inference::WorkerExecutionGate>,
) -> CommunityShardWorkerConfig {
    config.auto_reconnect = true;
    config.telemetry = Some(status.into());
    config.worker_execution_gate = Some(worker_execution_gate.into());
    config
}

pub(crate) fn spawn_integrated_worker(
    prepared: PreparedWorker,
    keypair: Arc<KeyPair>,
    worker_execution_gate: Arc<arc_node::native_inference::WorkerExecutionGate>,
    shutdown_rx: watch::Receiver<bool>,
) -> Result<JoinHandle<()>> {
    let loader = prepared.build_loader()?;
    let report = prepared.report;
    let status = arc_node::community_worker::install(Arc::new(
        arc_node::community_worker::CommunityWorkerStatus::new_sharded(
            format!("0x{}", keypair.address().to_hex()),
            &report.worker_name,
            &report.model_name,
            &report.model_id,
            report.n_layers,
            prepared.config.prevent_sleep_during_jobs,
        ),
    ));
    let config = configure_integrated_worker_config(
        prepared.config,
        Arc::clone(&status),
        worker_execution_gate,
    );
    let model_spec = prepared.model_spec;
    Ok(tokio::spawn(drive_integrated_worker_task(
        report,
        Some(status),
        shutdown_rx,
        move |worker_shutdown_rx| {
            run_community_shard_worker(config, keypair, model_spec, loader, worker_shutdown_rx)
        },
    )))
}

#[cfg(test)]
mod tests {
    use super::*;
    use arc_inference::canonical_shard_admission::ShardLayerRange as AdmissionLayerRange;
    use arc_node::community_sharding::{LeaseId, ShardLayerRange};
    use clap::Parser;
    use std::sync::atomic::AtomicU64;

    fn dummy_integrated_admission_report() -> AdmissionReport {
        let role = RoleBudgetReport {
            start_layer: 0,
            end_layer: 1,
            owned_layers: 1,
            is_first_shard: true,
            is_last_shard: false,
            layer_weights_bytes: 1024,
            first_embedding_bytes: 1024,
            output_head_and_norm_bytes: 0,
            reserved_kv_cache_bytes: 1024,
            resident_payload_bytes: 2048,
            resident_budget_bytes: 3072,
            peak_loading_transient_bytes: 512,
            peak_loading_budget_bytes: 3584,
        };
        AdmissionReport {
            mode: "integrated",
            estimate_basis: ESTIMATE_BASIS_NOTE,
            role_placement_limitation: ROLE_PLACEMENT_LIMITATION_NOTE,
            coordinator_url: "https://coordinator.example.com".to_string(),
            coordinator_id: Hash256([0x11; 32]).to_hex(),
            transaction_domain: Hash256([0x22; 32]).to_hex(),
            allow_insecure_for_loopback: false,
            worker_name: "worker-integrated".to_string(),
            platform: current_platform(),
            threads: DEFAULT_WORKER_THREADS,
            model_name: "arc-2L-64d-1h-32v".to_string(),
            model_path: "/tmp/model.gguf".to_string(),
            model_id: Hash256([0x33; 32]).to_hex(),
            artifact_size_bytes: 4096,
            execution_profile: CANONICAL_REWARD_INFERENCE_PROFILE.to_string(),
            architecture: "llama".to_string(),
            n_layers: 2,
            d_model: 64,
            n_heads: 1,
            n_kv_heads: 1,
            d_ff: 128,
            d_kv: 64,
            d_head: 64,
            vocab_size: 32,
            max_seq: 64,
            bos_token: 1,
            eos_tokens: vec![2],
            has_tied_output_head: false,
            header_bytes: 512,
            max_working_set_bytes: 512 * 1024 * 1024,
            max_kv_cache_bytes: DEFAULT_MAX_KV_CACHE_BYTES,
            admission: AllRolesAdmissionReport {
                protocol_max_resident_layers: 1,
                requested_max_resident_layers: None,
                admitted_max_resident_layers: 1,
                worst_case_resident_budget_bytes: 3072,
                worst_case_peak_loading_budget_bytes: 3584,
                first_role: role.clone(),
                last_role: role,
                interior_role: None,
            },
        }
    }

    fn sample_integrated_launch_spec(model_path: PathBuf) -> ShardWorkerLaunchSpec {
        ShardWorkerLaunchSpec {
            coordinator_url: "https://coordinator.example.com".to_string(),
            coordinator_id: Hash256([0x11; 32]),
            transaction_domain: Hash256([0x22; 32]),
            expected_model_id: Hash256([0x33; 32]),
            model_path,
            worker_name: "worker-integrated".to_string(),
            max_working_set_bytes: 512 * 1024 * 1024,
            max_kv_cache_bytes: DEFAULT_MAX_KV_CACHE_BYTES,
            max_resident_layers: None,
            threads: DEFAULT_WORKER_THREADS,
            allow_insecure_for_loopback: false,
            report_mode: "integrated",
            prevent_sleep_during_jobs: false,
        }
    }

    #[test]
    fn integrated_shard_worker_shared_prepare_cancels_when_shutdown_flag_set() {
        let spec = sample_integrated_launch_spec(PathBuf::from("/nonexistent/model.gguf"));
        let validated = validate_launch_spec_with_parallelism(&spec, 4).unwrap();
        let cancel = AtomicBool::new(true);
        let error = prepare_launch_spec_blocking(&spec, &validated, &cancel)
            .err()
            .expect("canceled preparation must fail");
        assert!(error.to_string().contains("model preparation canceled"));
    }

    #[test]
    fn integrated_shard_worker_shared_prepare_rejects_unpinned_model_before_metadata_parse() {
        static TEMP_SEQ: AtomicU64 = AtomicU64::new(0);
        let seq = TEMP_SEQ.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "arc-integrated-shard-mismatch-{}-{seq}.gguf",
            std::process::id()
        ));
        std::fs::write(&path, b"not-a-valid-gguf-header").unwrap();
        let spec = sample_integrated_launch_spec(path.clone());
        let validated = validate_launch_spec_with_parallelism(&spec, 4).unwrap();
        let cancel = AtomicBool::new(false);
        let result = prepare_launch_spec_blocking(&spec, &validated, &cancel);
        let _ = std::fs::remove_file(&path);
        let error = result.err().expect("mismatched digest must fail");
        assert!(
            error
                .to_string()
                .contains("does not match pinned --expected-model-id")
        );
    }

    #[test]
    fn integrated_shard_worker_shared_prepare_cancels_during_committed_metadata_verification() {
        let model_file = TempPath::new("committed-cancel.gguf");
        std::fs::write(&model_file.0, minimal_canonical_llama_gguf_header_bytes(4)).unwrap();
        let commitment = ModelArtifactCommitment::from_path(&model_file.0).unwrap();
        let mut spec = sample_integrated_launch_spec(model_file.0.clone());
        spec.expected_model_id = commitment.model_id();
        let validated = validate_launch_spec_with_parallelism(&spec, 4).unwrap();

        let uncancelled = AtomicBool::new(false);
        let prepared = prepare_launch_spec_blocking(&spec, &validated, &uncancelled)
            .expect("pinned canonical GGUF should prepare when uncancelled");
        assert_eq!(prepared.report.model_id, commitment.model_id().to_hex());

        let cancel = AtomicBool::new(false);
        let mut first_commit_completed = false;
        let canceled =
            prepare_launch_spec_blocking_with_after_commit_hook(&spec, &validated, &cancel, || {
                first_commit_completed = true;
                cancel.store(true, Ordering::Release);
            })
            .err()
            .expect("cancel during committed metadata verification must abort");
        assert!(first_commit_completed);
        assert!(
            canceled
                .downcast_ref::<ModelPreparationCanceled>()
                .is_some()
        );
    }

    #[tokio::test]
    async fn integrated_shard_worker_runtime_task_drains_before_join_returns() {
        let (runtime_shutdown_tx, runtime_shutdown_rx) = watch::channel(false);
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let drained = Arc::new(AtomicBool::new(false));
        let drained_task = Arc::clone(&drained);
        let runtime_task = tokio::spawn(drive_integrated_worker_task(
            dummy_integrated_admission_report(),
            None,
            runtime_shutdown_rx,
            move |mut shutdown_rx| async move {
                let _ = started_tx.send(());
                let _ = shutdown_rx.changed().await;
                assert!(*shutdown_rx.borrow());
                drained_task.store(true, Ordering::Release);
                Ok(WorkerRunSummary {
                    clean_release_sent: true,
                    ..Default::default()
                })
            },
        ));
        started_rx.await.unwrap();
        assert!(!drained.load(Ordering::Acquire));
        let _ = runtime_shutdown_tx.send(true);
        runtime_task.await.unwrap();
        assert!(drained.load(Ordering::Acquire));
    }

    #[tokio::test]
    async fn integrated_shard_worker_skips_start_when_shutdown_already_requested() {
        let (runtime_shutdown_tx, runtime_shutdown_rx) = watch::channel(false);
        let _ = runtime_shutdown_tx.send(true);
        let entered = Arc::new(AtomicBool::new(false));
        let entered_task = Arc::clone(&entered);
        drive_integrated_worker_task(
            dummy_integrated_admission_report(),
            None,
            runtime_shutdown_rx,
            move |_shutdown_rx| async move {
                entered_task.store(true, Ordering::Release);
                Ok(WorkerRunSummary::default())
            },
        )
        .await;
        assert!(!entered.load(Ordering::Acquire));
    }

    #[test]
    fn integrated_worker_config_wires_execution_gate_while_standalone_leaves_it_none() {
        use arc_node::community_worker::CommunityWorkerStatus;
        use arc_node::native_inference::WorkerExecutionGate;

        let base = CommunityShardWorkerConfig::new(
            "https://coordinator.example.com",
            Hash256([0x11; 32]),
            Some(Hash256([0x22; 32])),
            1,
            "worker-integrated",
            "arc-2L-64d-1h-32v",
            "linux-x86_64",
        );
        assert!(!base.auto_reconnect);
        assert!(base.telemetry.is_none());
        assert!(base.worker_execution_gate.is_none());

        let status = Arc::new(CommunityWorkerStatus::new_sharded(
            format!("0x{}", Hash256([0x44; 32]).to_hex()),
            "worker-integrated",
            "arc-2L-64d-1h-32v",
            Hash256([0x33; 32]).to_hex(),
            2,
            false,
        ));
        let gate = Arc::new(WorkerExecutionGate::default());
        let status_arg = Arc::clone(&status);
        let gate_arg = Arc::clone(&gate);
        let integrated = configure_integrated_worker_config(base, status_arg, gate_arg);
        assert!(integrated.auto_reconnect);
        assert_eq!(integrated.telemetry, Some(status.into()));
        assert_eq!(integrated.worker_execution_gate, Some(gate.into()));
    }

    static TEMP_COUNTER: AtomicU64 = AtomicU64::new(1);

    struct TempPath(PathBuf);

    impl TempPath {
        fn new(label: &str) -> Self {
            let seq = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
            Self(std::env::temp_dir().join(format!(
                "arc-community-shard-cli-{}-{label}-{seq}",
                std::process::id()
            )))
        }
    }

    impl Drop for TempPath {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    struct TestAdmitter {
        n_layers: usize,
        per_layer_bytes: u64,
        first_extra_bytes: u64,
        last_extra_bytes: u64,
        base_overhead_bytes: u64,
    }

    impl ShardBudgetAdmitter for TestAdmitter {
        fn n_layers(&self) -> usize {
            self.n_layers
        }

        fn max_shard_layers(&self) -> usize {
            self.n_layers.div_ceil(2)
        }

        fn admit_range(
            &self,
            range: std::ops::Range<usize>,
            max_working_set_bytes: u64,
            max_kv_cache_bytes: u64,
        ) -> std::result::Result<ShardAdmissionEstimate, ShardAdmissionError> {
            let owned = range.end.saturating_sub(range.start);
            if range.start >= range.end
                || range.end > self.n_layers
                || owned == self.n_layers
                || owned > self.max_shard_layers()
            {
                return Err(ShardAdmissionError::InvalidRange {
                    start_layer: range.start,
                    end_layer: range.end,
                    n_layers: self.n_layers,
                    reason: "invalid test range".into(),
                });
            }
            let is_first = range.start == 0;
            let is_last = range.end == self.n_layers;
            let layer_weights_bytes = (owned as u64) * self.per_layer_bytes;
            let first_embedding_bytes = if is_first { self.first_extra_bytes } else { 0 };
            let output_head_and_norm_bytes = if is_last { self.last_extra_bytes } else { 0 };
            let resident_payload_bytes = layer_weights_bytes
                + first_embedding_bytes
                + output_head_and_norm_bytes
                + self.base_overhead_bytes;
            let resident_budget_bytes = resident_payload_bytes + max_kv_cache_bytes;
            let peak_loading_budget_bytes = resident_budget_bytes;
            if peak_loading_budget_bytes > max_working_set_bytes {
                let role_note = if is_first {
                    " (includes first-shard Q16+I8 embeddings)"
                } else if is_last {
                    " (includes last-shard output head + final norm)"
                } else {
                    ""
                };
                return Err(ShardAdmissionError::BudgetExceeded {
                    start_layer: range.start,
                    end_layer: range.end,
                    resident_budget_bytes,
                    peak_loading_budget_bytes,
                    reserved_kv_cache_bytes: max_kv_cache_bytes,
                    max_working_set_bytes,
                    shortfall_bytes: peak_loading_budget_bytes - max_working_set_bytes,
                    role_note,
                });
            }
            Ok(ShardAdmissionEstimate {
                range: AdmissionLayerRange::new(range.start, range.end),
                owned_layers: owned,
                is_first_shard: is_first,
                is_last_shard: is_last,
                layer_weights_bytes,
                layer_norms_bytes: 0,
                first_embedding_bytes,
                output_head_and_norm_bytes,
                rope_tables_bytes: 0,
                vocab_and_metadata_storage_bytes: 0,
                placeholder_struct_overhead_bytes: 0,
                execution_scratch_bytes: 0,
                fixed_overhead_margin_bytes: self.base_overhead_bytes,
                reserved_kv_cache_bytes: max_kv_cache_bytes,
                resident_payload_bytes,
                resident_budget_bytes,
                max_loaded_tensor_elements: 0,
                worst_tensor_load_transient_bytes: 0,
                loader_temporary_overhead_bytes: 0,
                peak_loading_transient_bytes: 0,
                peak_loading_budget_bytes,
            })
        }
    }

    fn hex_id(byte: u8) -> String {
        hex::encode([byte; 32])
    }

    fn base_argv() -> Vec<String> {
        vec![
            "arc-node".into(),
            "community-shard-worker".into(),
            "--coordinator-url".into(),
            "https://coordinator.example.invalid:9944".into(),
            "--coordinator-id".into(),
            hex_id(0x11),
            "--transaction-domain".into(),
            hex_id(0x22),
            "--expected-model-id".into(),
            hex_id(0x33),
            "--model-path".into(),
            "/tmp/nonexistent.gguf".into(),
            "--worker-key-file".into(),
            "/tmp/nonexistent-key.json".into(),
            "--worker-name".into(),
            "worker-alpha".into(),
            "--max-working-set-bytes".into(),
            (4u64 * 1024 * 1024 * 1024).to_string(),
        ]
    }

    fn parse_worker_args(
        argv: &[String],
    ) -> std::result::Result<CommunityShardWorkerArgs, clap::Error> {
        let cli = crate::Cli::try_parse_from(argv)?;
        match cli.operator_command {
            Some(crate::OperatorCommand::CommunityShardWorker(args)) => Ok(args),
            _ => panic!("expected CommunityShardWorker subcommand"),
        }
    }

    #[test]
    fn cli_parses_top_level_subcommand_and_rejects_operator_prefix() {
        let args = parse_worker_args(&base_argv()).expect("valid CLI must parse");
        assert_eq!(args.coordinator_id, Hash256([0x11; 32]));
        assert_eq!(args.transaction_domain, Hash256([0x22; 32]));
        assert_eq!(args.expected_model_id, Hash256([0x33; 32]));
        assert_eq!(args.threads, DEFAULT_WORKER_THREADS);
        assert_eq!(args.max_kv_cache_bytes, DEFAULT_MAX_KV_CACHE_BYTES);
        assert!(!args.allow_insecure_for_loopback);
        assert!(!args.check_only);

        let mut wrong_prefix = base_argv();
        wrong_prefix.insert(1, "operator".into());
        assert!(crate::Cli::try_parse_from(&wrong_prefix).is_err());
    }

    #[test]
    fn cli_requires_pins_and_allows_missing_keyfile_only_for_check_only() {
        for flag in [
            "--coordinator-url",
            "--coordinator-id",
            "--transaction-domain",
            "--expected-model-id",
            "--model-path",
            "--worker-key-file",
            "--worker-name",
            "--max-working-set-bytes",
        ] {
            let mut argv = base_argv();
            let pos = argv.iter().position(|item| item == flag).unwrap();
            argv.drain(pos..=pos + 1);
            assert!(
                crate::Cli::try_parse_from(&argv).is_err(),
                "expected missing {flag} to fail"
            );
        }

        let mut check_only_argv = base_argv();
        let pos = check_only_argv
            .iter()
            .position(|item| item == "--worker-key-file")
            .unwrap();
        check_only_argv.drain(pos..=pos + 1);
        check_only_argv.push("--check-only".into());
        let args = parse_worker_args(&check_only_argv).expect("check-only should omit keyfile");
        assert!(args.check_only);
        assert!(args.worker_key_file.is_none());
    }

    #[test]
    fn cli_rejects_bad_identity_and_domain_hex() {
        for flag in [
            "--coordinator-id",
            "--transaction-domain",
            "--expected-model-id",
        ] {
            for bad in [
                "00".repeat(32),
                "11".repeat(31),
                format!("0x{}", "11".repeat(32)),
                "zz".repeat(32),
            ] {
                let mut argv = base_argv();
                let pos = argv.iter().position(|item| item == flag).unwrap();
                argv[pos + 1] = bad.clone();
                assert!(
                    crate::Cli::try_parse_from(&argv).is_err(),
                    "expected {flag} {bad} to fail"
                );
            }
        }
    }

    #[test]
    fn validate_args_rejects_zero_and_hard_cap_budgets_threads_and_insecure_urls() {
        let base = parse_worker_args(&base_argv()).unwrap();
        assert!(validate_args_with_parallelism(&base, 8).is_ok());

        let mut zero_ws = base.clone();
        zero_ws.max_working_set_bytes = 0;
        assert!(validate_args_with_parallelism(&zero_ws, 8).is_err());

        let mut zero_kv = base.clone();
        zero_kv.max_kv_cache_bytes = 0;
        assert!(validate_args_with_parallelism(&zero_kv, 8).is_err());

        let mut over_kv = base.clone();
        over_kv.max_kv_cache_bytes = MAX_KV_CACHE_BYTES_HARD_CAP + 1;
        assert!(validate_args_with_parallelism(&over_kv, 8).is_err());

        let mut kv_gt_ws = base.clone();
        kv_gt_ws.max_working_set_bytes = (DEFAULT_MAX_KV_CACHE_BYTES as u64) - 1;
        assert!(validate_args_with_parallelism(&kv_gt_ws, 8).is_err());

        let mut zero_layers = base.clone();
        zero_layers.max_resident_layers = Some(0);
        assert!(validate_args_with_parallelism(&zero_layers, 8).is_err());

        let mut zero_threads = base.clone();
        zero_threads.threads = 0;
        assert!(validate_args_with_parallelism(&zero_threads, 8).is_err());

        let mut too_many_threads = base.clone();
        too_many_threads.threads = 9;
        assert!(validate_args_with_parallelism(&too_many_threads, 8).is_err());

        let mut over_cap_threads = base.clone();
        over_cap_threads.threads = MAX_WORKER_THREADS_HARD_CAP + 1;
        assert!(validate_args_with_parallelism(&over_cap_threads, 128).is_err());

        let mut http_without_optin = base.clone();
        http_without_optin.coordinator_url = "http://127.0.0.1:9944".into();
        assert!(validate_args_with_parallelism(&http_without_optin, 8).is_err());

        let mut http_loopback_optin = http_without_optin.clone();
        http_loopback_optin.allow_insecure_for_loopback = true;
        assert!(validate_args_with_parallelism(&http_loopback_optin, 8).is_ok());

        let mut http_remote_optin = base.clone();
        http_remote_optin.coordinator_url = "http://192.0.2.10:9944".into();
        http_remote_optin.allow_insecure_for_loopback = true;
        assert!(validate_args_with_parallelism(&http_remote_optin, 8).is_err());
    }

    #[test]
    fn prepare_rejects_wrong_artifact_pin_before_metadata_keys_or_network() {
        let model_file = TempPath::new("not-gguf");
        std::fs::write(&model_file.0, b"not a gguf header at all").unwrap();
        let mut args = parse_worker_args(&base_argv()).unwrap();
        args.model_path = model_file.0.clone();
        args.expected_model_id = Hash256([0x99; 32]);
        let validated = validate_args_with_parallelism(&args, 4).unwrap();
        let err = prepare_model_blocking(&args, &validated, &AtomicBool::new(false))
            .err()
            .expect("wrong artifact pin must fail")
            .to_string();
        assert!(
            err.contains("does not match pinned --expected-model-id"),
            "{err}"
        );

        let dir_path = TempPath::new("dir");
        std::fs::create_dir(&dir_path.0).unwrap();
        args.model_path = dir_path.0.clone();
        let dir_err = prepare_model_blocking(&args, &validated, &AtomicBool::new(false))
            .err()
            .expect("directory model path must fail")
            .to_string();
        assert!(dir_err.contains("regular file"), "{dir_err}");
    }

    #[test]
    fn derive_all_roles_admission_enforces_budget_caps_and_first_embedding_floor() {
        let admitter = TestAdmitter {
            n_layers: 32,
            per_layer_bytes: 100,
            first_extra_bytes: 500,
            last_extra_bytes: 200,
            base_overhead_bytes: 50,
        };
        let derived = derive_all_roles_admission(&admitter, None, 1_100, 100).unwrap();
        assert_eq!(derived.protocol_max_resident_layers, 16);
        assert_eq!(derived.admitted_max_resident_layers, 4);
        assert_eq!(derived.worst_case_peak_loading_budget_bytes, 1_050);
        assert!(derived.interior_role.is_some());

        let capped = derive_all_roles_admission(&admitter, Some(2), 1_100, 100).unwrap();
        assert_eq!(capped.admitted_max_resident_layers, 2);

        let budget_bound = derive_all_roles_admission(&admitter, Some(10), 1_100, 100).unwrap();
        assert_eq!(budget_bound.admitted_max_resident_layers, 4);

        assert!(derive_all_roles_admission(&admitter, Some(17), 10_000, 100).is_err());

        let too_tight = derive_all_roles_admission(&admitter, None, 700, 100)
            .expect_err("first embedding exceeding 1-layer budget must fail")
            .to_string();
        assert!(
            too_tight.contains("cannot admit even 1 resident layer"),
            "{too_tight}"
        );
        assert!(
            too_tight.contains("first-shard Q16+I8 embeddings"),
            "{too_tight}"
        );
        assert!(
            too_tight.contains("conservative_all_roles_required"),
            "{too_tight}"
        );

        let two_layer = TestAdmitter {
            n_layers: 2,
            per_layer_bytes: 100,
            first_extra_bytes: 500,
            last_extra_bytes: 200,
            base_overhead_bytes: 50,
        };
        let two_report = derive_all_roles_admission(&two_layer, None, 1_000, 100).unwrap();
        assert_eq!(two_report.admitted_max_resident_layers, 1);
        assert!(two_report.interior_role.is_none());

        let over_cap_layers = TestAdmitter {
            n_layers: MAX_MODEL_LAYERS_HARD_CAP + 1,
            per_layer_bytes: 10,
            first_extra_bytes: 10,
            last_extra_bytes: 10,
            base_overhead_bytes: 10,
        };
        assert!(derive_all_roles_admission(&over_cap_layers, None, 1_000_000, 10).is_err());
    }

    #[test]
    fn partial_loader_admits_before_weights_and_verifies_artifact_before_and_after_load() {
        let model_file = TempPath::new("artifact");
        std::fs::write(&model_file.0, b"pinned artifact payload bytes").unwrap();
        let artifact = ModelArtifactCommitment::from_path(&model_file.0).unwrap();
        let admitter = TestAdmitter {
            n_layers: 8,
            per_layer_bytes: 100,
            first_extra_bytes: 500,
            last_extra_bytes: 200,
            base_overhead_bytes: 50,
        };
        let lease = ShardLeaseAssignment {
            lease_id: LeaseId {
                boot_nonce: Hash256([0x44; 32]),
                seq: 1,
            },
            lease_epoch: 1,
            model_id: artifact.model_id(),
            execution_profile: CANONICAL_REWARD_INFERENCE_PROFILE.to_string(),
            range: ShardLayerRange {
                start_layer: 0,
                end_layer: 2,
            },
            provisioning_deadline_unix_ms: 10_000,
        };

        let over_budget: Result<()> = load_and_verify_lease(
            &artifact,
            &admitter,
            &LeaseLoadPolicy {
                expected_model_id: artifact.model_id(),
                expected_profile: CANONICAL_REWARD_INFERENCE_PROFILE,
                admitted_max_resident_layers: 2,
                max_working_set_bytes: 700,
                max_kv_cache_bytes: 100,
            },
            &lease,
            |_, _, _| panic!("loader must not run when admission rejects lease"),
        );
        assert!(over_budget.is_err());

        let mut loaded_range = None;
        load_and_verify_lease(
            &artifact,
            &admitter,
            &LeaseLoadPolicy {
                expected_model_id: artifact.model_id(),
                expected_profile: CANONICAL_REWARD_INFERENCE_PROFILE,
                admitted_max_resident_layers: 2,
                max_working_set_bytes: 1_000,
                max_kv_cache_bytes: 100,
            },
            &lease,
            |path, start, end| {
                assert_eq!(path, artifact.path());
                loaded_range = Some((start, end));
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(loaded_range, Some((0, 2)));

        let mutated_during_load = load_and_verify_lease(
            &artifact,
            &admitter,
            &LeaseLoadPolicy {
                expected_model_id: artifact.model_id(),
                expected_profile: CANONICAL_REWARD_INFERENCE_PROFILE,
                admitted_max_resident_layers: 2,
                max_working_set_bytes: 1_000,
                max_kv_cache_bytes: 100,
            },
            &lease,
            |path, _, _| {
                std::fs::write(path, b"tampered during partial load").unwrap();
                Ok(())
            },
        );
        assert!(
            mutated_during_load
                .err()
                .unwrap()
                .to_string()
                .contains("changed during partial shard load")
        );

        let mutated_before_load: Result<()> = load_and_verify_lease(
            &artifact,
            &admitter,
            &LeaseLoadPolicy {
                expected_model_id: artifact.model_id(),
                expected_profile: CANONICAL_REWARD_INFERENCE_PROFILE,
                admitted_max_resident_layers: 2,
                max_working_set_bytes: 1_000,
                max_kv_cache_bytes: 100,
            },
            &lease,
            |_, _, _| panic!("loader must not run when artifact changed before load"),
        );
        assert!(
            mutated_before_load
                .err()
                .unwrap()
                .to_string()
                .contains("changed before partial shard load")
        );
    }

    #[tokio::test]
    async fn shutdown_signals_watch_and_drains_worker_and_prepare_without_abort() {
        let (shutdown_tx, mut shutdown_rx) = watch::channel(false);
        let worker_drained = Arc::new(AtomicBool::new(false));
        let worker_drained_inner = Arc::clone(&worker_drained);
        let worker_future = async move {
            shutdown_rx.changed().await.unwrap();
            assert!(*shutdown_rx.borrow());
            tokio::task::spawn_blocking(move || {
                std::thread::sleep(std::time::Duration::from_millis(15));
                worker_drained_inner.store(true, Ordering::Release);
            })
            .await
            .unwrap();
            Ok::<WorkerRunSummary, anyhow::Error>(WorkerRunSummary {
                clean_release_sent: true,
                ..WorkerRunSummary::default()
            })
        };
        let summary = drive_worker_with_shutdown(worker_future, shutdown_tx, async { "SIGTERM" })
            .await
            .unwrap();
        assert!(worker_drained.load(Ordering::Acquire));
        assert!(summary.clean_release_sent);

        let cancel = Arc::new(AtomicBool::new(false));
        let cancel_inner = Arc::clone(&cancel);
        let prepare_drained = Arc::new(AtomicBool::new(false));
        let prepare_drained_inner = Arc::clone(&prepare_drained);
        let handle = tokio::task::spawn_blocking(move || {
            while !cancel_inner.load(Ordering::Acquire) {
                std::thread::sleep(std::time::Duration::from_millis(2));
            }
            prepare_drained_inner.store(true, Ordering::Release);
            Ok::<(), anyhow::Error>(())
        });
        let prep_err = await_prepare_with_cancel(handle, cancel, async { "SIGINT" })
            .await
            .err()
            .unwrap()
            .to_string();
        assert!(prepare_drained.load(Ordering::Acquire));
        assert!(prep_err.contains("blocking task drained"), "{prep_err}");
    }
    #[tokio::test]
    async fn pending_shutdown_signal_preempts_worker_first_poll_and_prepare_completion() {
        for _ in 0..64 {
            let (shutdown_tx, shutdown_rx) = watch::channel(false);
            let saw_shutdown_on_first_poll = Arc::new(AtomicBool::new(false));
            let saw_flag = Arc::clone(&saw_shutdown_on_first_poll);
            let worker_future = async move {
                saw_flag.store(*shutdown_rx.borrow(), Ordering::Release);
                Ok::<WorkerRunSummary, anyhow::Error>(WorkerRunSummary::default())
            };
            drive_worker_with_shutdown(worker_future, shutdown_tx, std::future::ready("SIGTERM"))
                .await
                .unwrap();
            assert!(
                saw_shutdown_on_first_poll.load(Ordering::Acquire),
                "already-pending shutdown signal must set watch before first worker poll"
            );

            let cancel = Arc::new(AtomicBool::new(false));
            let (done_tx, done_rx) = tokio::sync::oneshot::channel();
            let handle = tokio::task::spawn_blocking(move || {
                let _ = done_tx.send(());
                Ok::<(), anyhow::Error>(())
            });
            let _ = done_rx.await;
            tokio::task::yield_now().await;
            let err = await_prepare_with_cancel(
                handle,
                Arc::clone(&cancel),
                std::future::ready("SIGINT"),
            )
            .await
            .expect_err("already-pending shutdown signal must abort preparation");
            assert!(cancel.load(Ordering::Acquire));
            assert!(err.to_string().contains("shutdown requested (SIGINT)"));
        }
    }

    #[cfg(any(unix, windows))]
    #[test]
    fn prepare_rejects_non_utf8_model_path_before_hashing_or_check_only_pass() {
        use std::ffi::OsString;
        #[cfg(unix)]
        use std::os::unix::ffi::OsStringExt;
        #[cfg(windows)]
        use std::os::windows::ffi::OsStringExt;

        let dir = TempPath::new("non-utf8-dir");
        std::fs::create_dir(&dir.0).unwrap();
        #[cfg(unix)]
        let bad_name = OsString::from_vec(b"model-\xff.gguf".to_vec());
        #[cfg(windows)]
        let bad_name = OsString::from_wide(&[0x6d, 0x6f, 0x64, 0x65, 0x6c, 0xd800]);
        let bad_file = dir.0.join(bad_name);
        assert!(bad_file.to_str().is_none());

        let mut args = parse_worker_args(&base_argv()).unwrap();
        args.model_path = bad_file;
        for check_only in [false, true] {
            args.check_only = check_only;
            args.worker_key_file = (!check_only).then(|| dir.0.join("unread-key.json"));
            let validated = validate_args_with_parallelism(&args, 4).unwrap();
            let err = prepare_model_blocking(&args, &validated, &AtomicBool::new(false))
                .err()
                .expect("non-UTF-8 model path must fail before filesystem access")
                .to_string();
            assert!(err.contains("not valid UTF-8"), "{err}");
        }
    }

    fn minimal_canonical_llama_gguf_header_bytes(n_layers: usize) -> Vec<u8> {
        fn push_str(buf: &mut Vec<u8>, s: &str) {
            buf.extend_from_slice(&(s.len() as u64).to_le_bytes());
            buf.extend_from_slice(s.as_bytes());
        }
        let d_model = 16usize;
        let n_heads = 4usize;
        let n_kv_heads = 2usize;
        let d_kv = 8usize;
        let d_ff = 28usize;
        let vocab_size = 24usize;
        let tokens: Vec<String> = (0..vocab_size).map(|idx| format!("tok_{idx}")).collect();
        let mut tensors: Vec<(String, Vec<u64>)> = vec![
            (
                "token_embd.weight".to_string(),
                vec![d_model as u64, vocab_size as u64],
            ),
            (
                "output.weight".to_string(),
                vec![d_model as u64, vocab_size as u64],
            ),
            ("output_norm.weight".to_string(), vec![d_model as u64]),
        ];
        for layer in 0..n_layers {
            let p = format!("blk.{layer}");
            tensors.push((
                format!("{p}.attn_q.weight"),
                vec![d_model as u64, d_model as u64],
            ));
            tensors.push((
                format!("{p}.attn_k.weight"),
                vec![d_model as u64, d_kv as u64],
            ));
            tensors.push((
                format!("{p}.attn_v.weight"),
                vec![d_model as u64, d_kv as u64],
            ));
            tensors.push((
                format!("{p}.attn_output.weight"),
                vec![d_model as u64, d_model as u64],
            ));
            tensors.push((
                format!("{p}.ffn_gate.weight"),
                vec![d_model as u64, d_ff as u64],
            ));
            tensors.push((
                format!("{p}.ffn_up.weight"),
                vec![d_model as u64, d_ff as u64],
            ));
            tensors.push((
                format!("{p}.ffn_down.weight"),
                vec![d_ff as u64, d_model as u64],
            ));
            tensors.push((format!("{p}.attn_norm.weight"), vec![d_model as u64]));
            tensors.push((format!("{p}.ffn_norm.weight"), vec![d_model as u64]));
        }
        let mut buf = Vec::new();
        buf.extend_from_slice(b"GGUF");
        buf.extend_from_slice(&3u32.to_le_bytes());
        buf.extend_from_slice(&(tensors.len() as u64).to_le_bytes());
        buf.extend_from_slice(&9u64.to_le_bytes());

        push_str(&mut buf, "general.architecture");
        buf.extend_from_slice(&8u32.to_le_bytes());
        push_str(&mut buf, "llama");

        for (key, val) in [
            ("llama.block_count", n_layers as u32),
            ("llama.embedding_length", d_model as u32),
            ("llama.attention.head_count", n_heads as u32),
            ("llama.attention.head_count_kv", n_kv_heads as u32),
            ("llama.feed_forward_length", d_ff as u32),
            ("tokenizer.ggml.bos_token_id", 1u32),
        ] {
            push_str(&mut buf, key);
            buf.extend_from_slice(&4u32.to_le_bytes());
            buf.extend_from_slice(&val.to_le_bytes());
        }

        push_str(&mut buf, "tokenizer.ggml.eos_token_id");
        buf.extend_from_slice(&9u32.to_le_bytes());
        buf.extend_from_slice(&4u32.to_le_bytes());
        buf.extend_from_slice(&1u64.to_le_bytes());
        buf.extend_from_slice(&2u32.to_le_bytes());

        push_str(&mut buf, "tokenizer.ggml.tokens");
        buf.extend_from_slice(&9u32.to_le_bytes());
        buf.extend_from_slice(&8u32.to_le_bytes());
        buf.extend_from_slice(&(tokens.len() as u64).to_le_bytes());
        for token in &tokens {
            push_str(&mut buf, token);
        }

        for (name, dims) in &tensors {
            push_str(&mut buf, name);
            buf.extend_from_slice(&(dims.len() as u32).to_le_bytes());
            for dim in dims {
                buf.extend_from_slice(&dim.to_le_bytes());
            }
            buf.extend_from_slice(&0u32.to_le_bytes());
            buf.extend_from_slice(&0u64.to_le_bytes());
        }
        buf
    }

    #[test]
    fn early_shard_header_prevalidation_rejects_bad_headers_and_budgets_before_bind() {
        let missing_spec =
            sample_integrated_launch_spec(PathBuf::from("/nonexistent/early-shard.gguf"));
        let validated = validate_launch_spec_with_parallelism(&missing_spec, 4).unwrap();
        let missing_err = prevalidate_early_shard_model_header(&missing_spec, &validated)
            .err()
            .expect("missing model file must fail before bind")
            .to_string();
        assert!(missing_err.contains("regular file"), "{missing_err}");

        let empty_file = TempPath::new("empty.gguf");
        std::fs::write(&empty_file.0, b"").unwrap();
        let mut spec = sample_integrated_launch_spec(empty_file.0.clone());
        let empty_err = prevalidate_early_shard_model_header(&spec, &validated)
            .err()
            .expect("empty model file must fail before bind")
            .to_string();
        assert!(empty_err.contains("is empty"), "{empty_err}");

        let invalid_file = TempPath::new("invalid.gguf");
        std::fs::write(&invalid_file.0, b"not-a-gguf-header").unwrap();
        spec.model_path = invalid_file.0.clone();
        let invalid_err = prevalidate_early_shard_model_header(&spec, &validated)
            .err()
            .expect("invalid GGUF must fail before bind")
            .to_string();
        assert!(
            invalid_err.contains("bounded pre-bind GGUF header validation failed"),
            "{invalid_err}"
        );

        let truncated_file = TempPath::new("truncated.gguf");
        let valid_bytes = minimal_canonical_llama_gguf_header_bytes(4);
        std::fs::write(&truncated_file.0, &valid_bytes[..28]).unwrap();
        spec.model_path = truncated_file.0.clone();
        let truncated_err = prevalidate_early_shard_model_header(&spec, &validated)
            .err()
            .expect("truncated GGUF header must fail before bind")
            .to_string();
        assert!(
            truncated_err.contains("bounded pre-bind GGUF header validation failed"),
            "{truncated_err}"
        );

        let one_layer_file = TempPath::new("one-layer.gguf");
        std::fs::write(
            &one_layer_file.0,
            minimal_canonical_llama_gguf_header_bytes(1),
        )
        .unwrap();
        spec.model_path = one_layer_file.0.clone();
        let one_layer_err = prevalidate_early_shard_model_header(&spec, &validated)
            .err()
            .expect("1-layer GGUF must fail protocol geometry before bind")
            .to_string();
        assert!(
            one_layer_err.contains("model n_layers 1"),
            "{one_layer_err}"
        );

        let valid_file = TempPath::new("valid-4l.gguf");
        std::fs::write(&valid_file.0, &valid_bytes).unwrap();
        spec.model_path = valid_file.0.clone();
        spec.max_working_set_bytes = 1_024;
        let budget_err = prevalidate_early_shard_model_header(&spec, &validated)
            .err()
            .expect("undersized working-set budget must fail admission before bind")
            .to_string();
        assert!(
            budget_err.contains("cannot admit even 1 resident layer"),
            "{budget_err}"
        );

        spec.max_working_set_bytes = 512 * 1024 * 1024;
        let meta = prevalidate_early_shard_model_header(&spec, &validated)
            .expect("valid canonical header with sufficient budget should pass prevalidation");
        assert_eq!(
            meta.trust_boundary(),
            ArtifactTrustBoundary::UnverifiedHeaderOnly
        );
        let unpinned_err = prepare_launch_spec_blocking(&spec, &validated, &AtomicBool::new(false))
            .err()
            .expect("prevalidated unverified header must still enforce pinned BLAKE3 digest")
            .to_string();
        assert!(
            unpinned_err.contains("does not match pinned --expected-model-id"),
            "{unpinned_err}"
        );

        #[cfg(unix)]
        {
            let dangling_link = TempPath::new("dangling-link.gguf");
            std::os::unix::fs::symlink("/nonexistent/dangling-target.gguf", &dangling_link.0)
                .unwrap();
            spec.model_path = dangling_link.0.clone();
            assert!(prevalidate_early_shard_model_header(&spec, &validated).is_err());

            let dir_target = TempPath::new("dir-target");
            std::fs::create_dir(&dir_target.0).unwrap();
            let dir_link = TempPath::new("dir-link.gguf");
            std::os::unix::fs::symlink(&dir_target.0, &dir_link.0).unwrap();
            spec.model_path = dir_link.0.clone();
            let dir_link_err = prevalidate_early_shard_model_header(&spec, &validated)
                .err()
                .expect("symlink to directory must fail before bind")
                .to_string();
            assert!(dir_link_err.contains("regular file"), "{dir_link_err}");

            let malformed_link = TempPath::new("malformed-link.gguf");
            std::os::unix::fs::symlink(&truncated_file.0, &malformed_link.0).unwrap();
            spec.model_path = malformed_link.0.clone();
            assert!(prevalidate_early_shard_model_header(&spec, &validated).is_err());

            let valid_link = TempPath::new("valid-link.gguf");
            std::os::unix::fs::symlink(&valid_file.0, &valid_link.0).unwrap();
            spec.model_path = valid_link.0.clone();
            let link_meta = prevalidate_early_shard_model_header(&spec, &validated)
                .expect("symlink to valid canonical GGUF header should prevalidate unverified");
            assert_eq!(
                link_meta.trust_boundary(),
                ArtifactTrustBoundary::UnverifiedHeaderOnly
            );
        }
    }

    fn header_only_argv(model_path: &Path) -> Vec<String> {
        let mut argv = base_argv();
        let key_pos = argv
            .iter()
            .position(|item| item == "--worker-key-file")
            .unwrap();
        argv.drain(key_pos..=key_pos + 1);
        let model_pos = argv.iter().position(|item| item == "--model-path").unwrap();
        argv[model_pos + 1] = model_path.display().to_string();
        let budget_pos = argv
            .iter()
            .position(|item| item == "--max-working-set-bytes")
            .unwrap();
        argv[budget_pos + 1] = (512u64 * 1024 * 1024).to_string();
        argv.push("--check-header-only".into());
        argv
    }

    #[test]
    fn cli_check_header_only_allows_missing_keyfile_and_conflicts_with_check_only() {
        let argv = header_only_argv(Path::new("/tmp/nonexistent.gguf"));
        let args = parse_worker_args(&argv).expect("check-header-only should omit keyfile");
        assert!(args.check_header_only);
        assert!(!args.check_only);
        assert!(args.worker_key_file.is_none());

        let mut conflict_argv = argv.clone();
        conflict_argv.push("--check-only".into());
        assert!(
            crate::Cli::try_parse_from(&conflict_argv).is_err(),
            "--check-only and --check-header-only must conflict in CLI parsing"
        );

        let mut both = args.clone();
        both.check_only = true;
        let both_err = validate_args_with_parallelism(&both, 4)
            .expect_err("programmatic check_only + check_header_only must fail")
            .to_string();
        assert!(both_err.contains("mutually exclusive"), "{both_err}");

        let validated_header_only = validate_args_with_parallelism(&args, 4).unwrap();
        let prepare_err =
            prepare_model_blocking(&args, &validated_header_only, &AtomicBool::new(false))
                .err()
                .expect("header-only args must be refused by full model preparation")
                .to_string();
        assert!(
            prepare_err.contains("must not enter full verified model preparation"),
            "{prepare_err}"
        );

        let mut run_without_key = args.clone();
        run_without_key.check_header_only = false;
        let missing_key_err = validate_args_with_parallelism(&run_without_key, 4)
            .expect_err("normal run mode must still require worker keyfile")
            .to_string();
        assert!(
            missing_key_err.contains("--worker-key-file is required"),
            "{missing_key_err}"
        );

        for flag in [
            "--coordinator-url",
            "--coordinator-id",
            "--transaction-domain",
            "--expected-model-id",
            "--model-path",
            "--worker-name",
            "--max-working-set-bytes",
        ] {
            let mut missing_pin = argv.clone();
            let pos = missing_pin.iter().position(|item| item == flag).unwrap();
            missing_pin.drain(pos..=pos + 1);
            assert!(
                crate::Cli::try_parse_from(&missing_pin).is_err(),
                "expected missing {flag} to fail in --check-header-only mode"
            );
        }
    }

    #[test]
    fn check_header_only_rejects_malformed_headers_geometry_pins_urls_threads_and_budgets() {
        let valid_bytes = minimal_canonical_llama_gguf_header_bytes(4);
        let valid_file = TempPath::new("header-only-valid-4l.gguf");
        std::fs::write(&valid_file.0, &valid_bytes).unwrap();
        let base = parse_worker_args(&header_only_argv(&valid_file.0)).unwrap();

        let mut zero_coord = base.clone();
        zero_coord.coordinator_id = Hash256::ZERO;
        assert!(check_header_only_with_parallelism(&zero_coord, 4).is_err());

        let mut zero_domain = base.clone();
        zero_domain.transaction_domain = Hash256::ZERO;
        assert!(check_header_only_with_parallelism(&zero_domain, 4).is_err());

        let mut zero_model = base.clone();
        zero_model.expected_model_id = Hash256::ZERO;
        assert!(check_header_only_with_parallelism(&zero_model, 4).is_err());

        let mut insecure_url = base.clone();
        insecure_url.coordinator_url = "http://127.0.0.1:9944".into();
        assert!(check_header_only_with_parallelism(&insecure_url, 4).is_err());

        let mut remote_http = base.clone();
        remote_http.coordinator_url = "http://192.0.2.10:9944".into();
        remote_http.allow_insecure_for_loopback = true;
        assert!(check_header_only_with_parallelism(&remote_http, 4).is_err());

        let mut zero_threads = base.clone();
        zero_threads.threads = 0;
        assert!(check_header_only_with_parallelism(&zero_threads, 4).is_err());

        let mut excess_threads = base.clone();
        excess_threads.threads = 5;
        assert!(check_header_only_with_parallelism(&excess_threads, 4).is_err());

        let mut missing_model = base.clone();
        missing_model.model_path = PathBuf::from("/nonexistent/header-only-missing.gguf");
        assert!(check_header_only_with_parallelism(&missing_model, 4).is_err());

        let dir_path = TempPath::new("header-only-dir");
        std::fs::create_dir(&dir_path.0).unwrap();
        let mut dir_model = base.clone();
        dir_model.model_path = dir_path.0.clone();
        let dir_err = check_header_only_with_parallelism(&dir_model, 4)
            .expect_err("directory model path must fail")
            .to_string();
        assert!(dir_err.contains("regular file"), "{dir_err}");

        let empty_file = TempPath::new("header-only-empty.gguf");
        std::fs::write(&empty_file.0, b"").unwrap();
        let mut empty_model = base.clone();
        empty_model.model_path = empty_file.0.clone();
        assert!(check_header_only_with_parallelism(&empty_model, 4).is_err());

        let invalid_file = TempPath::new("header-only-bad-magic.gguf");
        std::fs::write(&invalid_file.0, b"not-a-valid-gguf-header").unwrap();
        let mut invalid_model = base.clone();
        invalid_model.model_path = invalid_file.0.clone();
        let invalid_err = check_header_only_with_parallelism(&invalid_model, 4)
            .expect_err("malformed GGUF header must fail")
            .to_string();
        assert!(
            invalid_err.contains("bounded pre-bind GGUF header validation failed"),
            "{invalid_err}"
        );

        let truncated_file = TempPath::new("header-only-truncated.gguf");
        std::fs::write(&truncated_file.0, &valid_bytes[..28]).unwrap();
        let mut truncated_model = base.clone();
        truncated_model.model_path = truncated_file.0.clone();
        assert!(check_header_only_with_parallelism(&truncated_model, 4).is_err());

        let one_layer_file = TempPath::new("header-only-one-layer.gguf");
        std::fs::write(
            &one_layer_file.0,
            minimal_canonical_llama_gguf_header_bytes(1),
        )
        .unwrap();
        let mut one_layer_model = base.clone();
        one_layer_model.model_path = one_layer_file.0.clone();
        let geometry_err = check_header_only_with_parallelism(&one_layer_model, 4)
            .expect_err("1-layer GGUF must fail protocol geometry")
            .to_string();
        assert!(geometry_err.contains("model n_layers 1"), "{geometry_err}");

        let mut below_kv = base.clone();
        below_kv.max_working_set_bytes = (DEFAULT_MAX_KV_CACHE_BYTES as u64) - 1;
        assert!(check_header_only_with_parallelism(&below_kv, 4).is_err());

        let mut tight_budget = base.clone();
        tight_budget.max_working_set_bytes = DEFAULT_MAX_KV_CACHE_BYTES as u64;
        let budget_err = check_header_only_with_parallelism(&tight_budget, 4)
            .expect_err("undersized working-set budget must fail admission")
            .to_string();
        assert!(
            budget_err.contains("cannot admit even 1 resident layer"),
            "{budget_err}"
        );

        let mut over_resident = base.clone();
        over_resident.max_resident_layers = Some(3);
        let resident_err = check_header_only_with_parallelism(&over_resident, 4)
            .expect_err("max_resident_layers above ceil(n_layers/2) must fail")
            .to_string();
        assert!(
            resident_err.contains("--max-resident-layers 3 must be in 1..=2"),
            "{resident_err}"
        );

        #[cfg(any(unix, windows))]
        {
            use std::ffi::OsString;
            #[cfg(unix)]
            use std::os::unix::ffi::OsStringExt;
            #[cfg(windows)]
            use std::os::windows::ffi::OsStringExt;

            let non_utf8_dir = TempPath::new("header-only-non-utf8");
            std::fs::create_dir(&non_utf8_dir.0).unwrap();
            #[cfg(unix)]
            let bad_name = OsString::from_vec(b"model-\xff.gguf".to_vec());
            #[cfg(windows)]
            let bad_name = OsString::from_wide(&[0x6d, 0x6f, 0x64, 0x65, 0x6c, 0xd800]);
            let mut non_utf8_model = base;
            non_utf8_model.model_path = non_utf8_dir.0.join(bad_name);
            let utf8_err = check_header_only_with_parallelism(&non_utf8_model, 4)
                .expect_err("non-UTF-8 model path must fail")
                .to_string();
            assert!(utf8_err.contains("not valid UTF-8"), "{utf8_err}");
        }
    }

    #[test]
    fn check_header_only_accepts_header_only_gguf_while_check_only_rejects_unverified() {
        use std::collections::BTreeSet;

        #[derive(serde::Deserialize)]
        struct VerifiedModelClaim {
            model_id: String,
            artifact_size_bytes: u64,
        }

        let model_file = TempPath::new("header-only-no-tensors-4l.gguf");
        let header_only_bytes = minimal_canonical_llama_gguf_header_bytes(4);
        std::fs::write(&model_file.0, &header_only_bytes).unwrap();
        let actual_commitment = ModelArtifactCommitment::from_path(&model_file.0).unwrap();
        assert_ne!(
            actual_commitment.model_id(),
            Hash256([0x33; 32]),
            "header-only fixture hash must differ from pinned expected_model_id"
        );

        let mut argv = header_only_argv(&model_file.0);
        let url_pos = argv
            .iter()
            .position(|item| item == "--coordinator-url")
            .unwrap();
        argv[url_pos + 1] = "https://COORDINATOR.EXAMPLE.INVALID:9944/".into();
        let mut args = parse_worker_args(&argv).unwrap();

        let report = check_header_only_with_parallelism(&args, 4)
            .expect("tiny header-only GGUF with no tensor payload should pass header-only check");
        assert_eq!(report.mode, HEADER_ADMISSION_ONLY_MODE);
        assert!(!report.artifact_verified);
        assert_eq!(
            report.coordinator_url,
            "https://coordinator.example.invalid:9944"
        );
        assert_eq!(report.coordinator_id, hex_id(0x11));
        assert_eq!(report.transaction_domain, hex_id(0x22));
        assert_eq!(report.expected_model_id, hex_id(0x33));
        assert_ne!(
            report.expected_model_id,
            actual_commitment.model_id().to_hex()
        );
        assert_eq!(report.max_working_set_bytes, 512u64 * 1024 * 1024);
        assert_eq!(report.admission.protocol_max_resident_layers, 2);
        assert_eq!(report.admission.admitted_max_resident_layers, 2);
        assert!(report.admission.interior_role.is_some());

        let json_value = serde_json::to_value(&report).unwrap();
        let json_obj = json_value
            .as_object()
            .expect("report must serialize as JSON object");
        let expected_keys: BTreeSet<&str> = [
            "mode",
            "artifact_verified",
            "coordinator_url",
            "coordinator_id",
            "transaction_domain",
            "expected_model_id",
            "max_working_set_bytes",
            "admission",
        ]
        .into_iter()
        .collect();
        let actual_keys: BTreeSet<&str> = json_obj.keys().map(String::as_str).collect();
        assert_eq!(actual_keys, expected_keys);
        assert_eq!(
            json_obj.get("mode"),
            Some(&serde_json::json!("header-admission-only"))
        );
        assert_eq!(
            json_obj.get("artifact_verified"),
            Some(&serde_json::json!(false))
        );
        assert_eq!(
            json_obj.get("coordinator_url"),
            Some(&serde_json::json!(
                "https://coordinator.example.invalid:9944"
            ))
        );
        assert_eq!(
            json_obj.get("coordinator_id"),
            Some(&serde_json::json!(hex_id(0x11)))
        );
        assert_eq!(
            json_obj.get("transaction_domain"),
            Some(&serde_json::json!(hex_id(0x22)))
        );
        assert_eq!(
            json_obj.get("expected_model_id"),
            Some(&serde_json::json!(hex_id(0x33)))
        );
        assert_eq!(
            json_obj.get("max_working_set_bytes"),
            Some(&serde_json::json!(512u64 * 1024 * 1024))
        );
        assert_eq!(
            json_obj.get("admission"),
            Some(&serde_json::to_value(&report.admission).unwrap())
        );
        assert!(!json_obj.contains_key("model_id"));
        assert!(!json_obj.contains_key("artifact_size_bytes"));
        assert!(
            serde_json::from_value::<VerifiedModelClaim>(json_value).is_err(),
            "header-only report must not deserialize as a verified model report"
        );

        let mut with_unread_key = args.clone();
        with_unread_key.worker_key_file = Some(PathBuf::from("/nonexistent/unread-key.json"));
        assert_eq!(
            check_header_only_with_parallelism(&with_unread_key, 4).unwrap(),
            report
        );

        args.check_header_only = false;
        args.check_only = true;
        let validated = validate_args_with_parallelism(&args, 4).unwrap();
        let full_err = prepare_model_blocking(&args, &validated, &AtomicBool::new(false))
            .err()
            .expect("full --check-only must reject unverified header-only artifact pin mismatch")
            .to_string();
        assert!(
            full_err.contains("does not match pinned --expected-model-id"),
            "{full_err}"
        );

        args.expected_model_id = actual_commitment.model_id();
        let pinned_validated = validate_args_with_parallelism(&args, 4).unwrap();
        let prepared = prepare_model_blocking(&args, &pinned_validated, &AtomicBool::new(false))
            .expect("full --check-only must preserve verified check behavior when pin matches");
        assert_eq!(prepared.report.mode, "check-only");
        let full_json = serde_json::to_value(&prepared.report).unwrap();
        let verified_claim: VerifiedModelClaim = serde_json::from_value(full_json)
            .expect("verified AdmissionReport must expose verified model_id and size");
        assert_eq!(
            verified_claim.model_id,
            actual_commitment.model_id().to_hex()
        );
        assert_eq!(
            verified_claim.artifact_size_bytes,
            actual_commitment.size_bytes()
        );
    }

    #[tokio::test]
    async fn run_check_header_only_returns_early_without_hashing_key_load_rayon_or_network() {
        let model_file = TempPath::new("run-header-only-4l.gguf");
        std::fs::write(&model_file.0, minimal_canonical_llama_gguf_header_bytes(4)).unwrap();

        let mut args = parse_worker_args(&header_only_argv(&model_file.0)).unwrap();
        args.worker_key_file = Some(PathBuf::from("/nonexistent/never-loaded-key.json"));
        run(args.clone())
            .await
            .expect("run --check-header-only must succeed offline without hashing or key load");

        args.worker_key_file = None;
        run(args.clone())
            .await
            .expect("run --check-header-only without keyfile must succeed");

        args.check_header_only = false;
        args.check_only = true;
        let full_err = run(args)
            .await
            .expect_err("run --check-only must fail BLAKE3 pin check on header-only file")
            .to_string();
        assert!(
            full_err.contains("does not match pinned --expected-model-id"),
            "{full_err}"
        );
    }
}
