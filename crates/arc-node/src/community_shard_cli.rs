use anyhow::{Context, Result, anyhow, bail, ensure};
use arc_crypto::Hash256;
use arc_inference::cached_integer_model::{
    CANONICAL_REWARD_INFERENCE_PROFILE, CachedIntegerModel, load_cached_model_shard_canonical_i8,
};
use arc_inference::canonical_shard_admission::{
    ArtifactTrustBoundary, CanonicalShardMetadata, ShardAdmissionError, ShardAdmissionEstimate,
};
use arc_inference::model_artifact::ModelArtifactCommitment;
use arc_node::community_shard_executor::{DEFAULT_MAX_KV_CACHE_BYTES, MAX_KV_CACHE_BYTES_HARD_CAP};
use arc_node::community_shard_worker::{
    CommunityShardWorkerConfig, PartialModelLoader, WorkerRunSummary, run_community_shard_worker,
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

    /// Mode-0600 Ed25519 worker keyfile (required unless --check-only).
    #[arg(long, value_name = "PATH", required_unless_present = "check_only")]
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

    /// Verify pinned artifact and conservative all-roles admission, print JSON report, and exit without key loading, Rayon init, weights, or network.
    #[arg(long)]
    pub check_only: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ValidatedInputs {
    normalized_coordinator_url: String,
    worker_name: String,
    platform: String,
    max_kv_cache_bytes_u64: u64,
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

struct PreparedWorker {
    artifact: Arc<ModelArtifactCommitment>,
    metadata: Arc<CanonicalShardMetadata>,
    model_spec: TrustedModelSpec,
    config: CommunityShardWorkerConfig,
    report: AdmissionReport,
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

fn parse_nonzero_hash256(encoded: &str) -> std::result::Result<Hash256, String> {
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

fn host_available_parallelism() -> usize {
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

fn validate_args_with_parallelism(
    args: &CommunityShardWorkerArgs,
    available_parallelism: usize,
) -> Result<ValidatedInputs> {
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
        args.check_only || args.worker_key_file.is_some(),
        "--worker-key-file is required unless --check-only is set"
    );
    ensure!(
        args.max_working_set_bytes > 0,
        "--max-working-set-bytes must be nonzero"
    );
    ensure!(
        args.max_kv_cache_bytes > 0 && args.max_kv_cache_bytes <= MAX_KV_CACHE_BYTES_HARD_CAP,
        "--max-kv-cache-bytes {} must be in 1..={MAX_KV_CACHE_BYTES_HARD_CAP}",
        args.max_kv_cache_bytes
    );
    let max_kv_cache_bytes_u64 =
        u64::try_from(args.max_kv_cache_bytes).context("--max-kv-cache-bytes exceeds u64 range")?;
    ensure!(
        args.max_working_set_bytes >= max_kv_cache_bytes_u64,
        "--max-working-set-bytes ({}) must be at least --max-kv-cache-bytes ({})",
        args.max_working_set_bytes,
        args.max_kv_cache_bytes
    );
    if let Some(limit) = args.max_resident_layers {
        let protocol_cap = MAX_MODEL_LAYERS_HARD_CAP.div_ceil(2);
        ensure!(
            limit >= 1 && limit <= protocol_cap,
            "--max-resident-layers {limit} must be in 1..={protocol_cap}"
        );
    }
    validate_thread_count(args.threads, available_parallelism)?;
    let worker_name = args.worker_name.trim().to_string();
    ensure!(
        !worker_name.is_empty() && !worker_name.chars().any(char::is_control),
        "--worker-name must be non-empty without control characters"
    );
    let platform = current_platform();
    let probe_spec = TrustedModelSpec {
        model_id: args.expected_model_id,
        execution_profile: CANONICAL_REWARD_INFERENCE_PROFILE.to_string(),
        n_layers: 2,
        d_model: 64,
        vocab_size: 32,
        max_seq: 64,
        eos_tokens: vec![2],
        bos_token: 1,
    };
    let mut probe_config = CommunityShardWorkerConfig::new(
        args.coordinator_url.clone(),
        args.coordinator_id,
        Some(args.transaction_domain),
        1,
        worker_name.clone(),
        "arc-2L-64d-1h-32v",
        platform.clone(),
    );
    probe_config.allow_insecure_for_loopback = args.allow_insecure_for_loopback;
    probe_config.executor_config.max_kv_cache_bytes = args.max_kv_cache_bytes;
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

fn ensure_regular_model_file(path: &Path) -> Result<()> {
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
        n_layers >= 2 && n_layers <= MAX_MODEL_LAYERS_HARD_CAP,
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

fn prepare_model_blocking(
    args: &CommunityShardWorkerArgs,
    validated: &ValidatedInputs,
    cancel: &AtomicBool,
) -> Result<PreparedWorker> {
    ensure!(
        !cancel.load(Ordering::Acquire),
        "model preparation canceled"
    );
    ensure_regular_model_file(&args.model_path)?;
    let artifact = ModelArtifactCommitment::from_path(&args.model_path).with_context(|| {
        format!(
            "failed to compute BLAKE3 commitment for {}",
            args.model_path.display()
        )
    })?;
    ensure!(
        artifact.model_id() == args.expected_model_id,
        "model artifact {} BLAKE3 digest {} does not match pinned --expected-model-id {}; \
         refusing metadata parsing, key loading, or coordinator registration",
        args.model_path.display(),
        artifact.model_id().to_hex(),
        args.expected_model_id.to_hex()
    );
    ensure!(
        !cancel.load(Ordering::Acquire),
        "model preparation canceled"
    );
    let metadata =
        CanonicalShardMetadata::from_committed_artifact(&artifact).with_context(|| {
            format!(
                "failed to inspect committed model artifact {}",
                args.model_path.display()
            )
        })?;
    ensure!(
        !cancel.load(Ordering::Acquire),
        "model preparation canceled"
    );
    validate_metadata_against_protocol(&metadata, args.expected_model_id, artifact.size_bytes())?;
    let admission = derive_all_roles_admission(
        &metadata,
        args.max_resident_layers,
        args.max_working_set_bytes,
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
        args.coordinator_id,
        Some(args.transaction_domain),
        admission.admitted_max_resident_layers,
        validated.worker_name.clone(),
        model_name.clone(),
        validated.platform.clone(),
    );
    config.allow_insecure_for_loopback = args.allow_insecure_for_loopback;
    config.executor_config.max_kv_cache_bytes = args.max_kv_cache_bytes;
    let normalized_coordinator_url = config
        .validate(&model_spec)
        .context("worker configuration failed validation against committed model spec")?;
    let report = AdmissionReport {
        mode: if args.check_only { "check-only" } else { "run" },
        estimate_basis: ESTIMATE_BASIS_NOTE,
        role_placement_limitation: ROLE_PLACEMENT_LIMITATION_NOTE,
        coordinator_url: normalized_coordinator_url,
        coordinator_id: args.coordinator_id.to_hex(),
        transaction_domain: args.transaction_domain.to_hex(),
        allow_insecure_for_loopback: args.allow_insecure_for_loopback,
        worker_name: validated.worker_name.clone(),
        platform: validated.platform.clone(),
        threads: args.threads,
        model_name,
        model_path: args.model_path.display().to_string(),
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
        max_working_set_bytes: args.max_working_set_bytes,
        max_kv_cache_bytes: args.max_kv_cache_bytes,
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

fn configure_global_rayon_pool(threads: usize) -> Result<()> {
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
        result = &mut handle => result.context("model preparation task panicked")?,
        signal_name = shutdown_signal => {
            cancel.store(true, Ordering::Release);
            let _ = handle.await;
            bail!("shutdown requested ({signal_name}) during model preparation; blocking task drained");
        }
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
        result = &mut worker_future => result,
        signal_name = shutdown_signal => {
            eprintln!(
                "received {signal_name}; signaling community shard worker shutdown and awaiting drain"
            );
            let _ = shutdown_tx.send(true);
            worker_future.await
        }
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
    let validated = validate_args_with_parallelism(&args, host_available_parallelism())?;
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
    let keyfile_path = args
        .worker_key_file
        .as_deref()
        .ok_or_else(|| anyhow!("--worker-key-file is required unless --check-only is set"))?;
    let keypair = Arc::new(crate::validator_identity::load_ed25519_keyfile(
        keyfile_path,
    )?);
    let worker_address = keypair.address();
    configure_global_rayon_pool(args.threads)?;
    let loader = build_partial_model_loader(
        Arc::clone(&prepared.artifact),
        Arc::clone(&prepared.metadata),
        args.expected_model_id,
        prepared.report.admission.admitted_max_resident_layers,
        args.max_working_set_bytes,
        validated.max_kv_cache_bytes_u64,
    );
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

#[cfg(test)]
mod tests {
    use super::*;
    use arc_inference::canonical_shard_admission::ShardLayerRange as AdmissionLayerRange;
    use arc_node::community_sharding::{LeaseId, ShardLayerRange};
    use clap::Parser;
    use std::sync::atomic::AtomicU64;

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
            .err()
            .expect("first embedding exceeding 1-layer budget must fail")
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
}
