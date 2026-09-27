//! Row-partitioned execution of native paid requests on this operator's own
//! machines: S1-S8 connected to the running node under option (a) of
//! docs/design/assignment-node-integration.md.
//!
//! Every row a vote depends on is computed by this node, or by a machine the
//! operator lists here and reaches over SSH with a pinned host key: the trust
//! model's hard rule. Nothing here changes consensus. A slice from a machine
//! is used only after its checks, and a failed or faulty one is recomputed
//! here, so the output equals local execution unless one of the operator's
//! own machines returns wrong rows that no check sampled. That is the
//! operator's own fault, and costs only its own vote: the other members
//! compute independently. The assignment certificate is this node's audit
//! record, kept in a bounded ring and served to the operator only. Machines
//! are connected and measured on a background thread, never while a paid
//! request waits. S10, the multi-machine comparison, stays FAIL until a run
//! on real machines passes.

use crate::native_inference::NativeInferenceError;
use arc_assign::book::{
    self, ChallengeBook, ChallengeState, Exclusion, Exclusions, Offer, ProbeBook,
};
use arc_assign::certificate::AssignmentCertificate;
use arc_assign::lease::{ChallengeResult, LeaseError};
use arc_assign::link::Probe;
use arc_assign::placement::{Participant, Policy, Stage};
use arc_assign::reservation::{MAX_SLOTS_PER_WORKER, ReservationLedger};
use arc_assign::resident::{ResidentCertificate, WorkerResidency};
use arc_assign::verify::{Finding, VerificationRule, plan as verification_plan};
use arc_crypto::Hash256;
use arc_inference::cached_integer_model::{
    BackendGenerationError, CachedIntegerModel, ModelConfig,
};
use arc_inference::low_residency::{CanonicalRowSource, LowResidencyModel};
use arc_inference::tensor_parallel::{
    PlannedSlice, ProjectionPlan, RowAssignment, RowEvent, RowEventSink, RowProjectionRequest,
    RowWorker, SliceOwner, SshStdioConfig, SshStdioRowWorker, TensorKey, TensorParallelError,
    VerifiedPartitionBackend, hash_i64, is_plain_absolute_path,
};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Weak};
use std::time::{Duration, Instant};

/// The coordinator's residency policy is explicit and shared by execution,
/// placement and verification. A low-residency model has no local generator.
#[derive(Clone)]
pub(crate) enum CoordinatorModel {
    Resident(Arc<CachedIntegerModel>),
    LowResidency(Arc<LowResidencyModel>),
}
impl CoordinatorModel {
    pub(crate) fn source(&self) -> &dyn CanonicalRowSource {
        match self {
            Self::Resident(m) => m.as_ref(),
            Self::LowResidency(m) => m.as_ref(),
        }
    }
    pub(crate) fn config(&self) -> &ModelConfig {
        self.source().config()
    }
    pub(crate) fn is_low_residency(&self) -> bool {
        matches!(self, Self::LowResidency(_))
    }
    pub(crate) fn local_generate(&self, prompt: &[u32], max_tokens: u32, eos: &[u32]) -> Generated {
        match self {
            Self::Resident(m) => m.try_generate_v2(prompt, max_tokens, eos).map_err(|e| NativeInferenceError::Executor(e.to_string())),
            Self::LowResidency(_) => Err(NativeInferenceError::Executor("low-residency execution requires complete verified remote coverage; local generation is unavailable".into())),
        }
    }
    fn generate_with_backend(
        &self,
        request: Hash256,
        prompt: &[u32],
        max_tokens: u32,
        eos: &[u32],
        backend: &impl arc_inference::tensor_parallel::ProjectionBackend,
    ) -> Result<(Vec<u32>, Hash256), BackendGenerationError> {
        match self {
            Self::Resident(m) => {
                m.try_generate_v2_with_backend(request, prompt, max_tokens, eos, backend)
            }
            Self::LowResidency(m) => {
                m.try_generate_v2_with_backend(request, prompt, max_tokens, eos, backend)
            }
        }
    }
}

/// Most machines one cohort lists.
pub const MAX_COHORT_WORKERS: usize = 32;
/// Heights a link measurement stays usable for placement.
pub const LINK_MAX_AGE_HEIGHTS: u64 = 600;
/// Failures per mille beyond which a link is not used.
pub const LINK_MAX_FAILURE_PER_MILLE: u32 = 100;
/// Heights per assignment epoch. Challenges and exclusions reset at each, and
/// every machine is measured and challenged again, in the background.
pub const EPOCH_HEIGHTS: u64 = 3_600;
/// Records kept for the operator's view.
pub const RECENT_RECORDS: usize = 256;
/// Rows of the challenge projection (layer 0's gate, the widest): enough
/// work (about 17M multiply-accumulates at 4096 columns) that link jitter
/// cannot inflate the measured rate.
const CHALLENGE_ROWS: usize = 4096;
/// Wide calls per measurement; their median gives the bandwidth.
const BULK_CALLS: usize = 3;
/// Round-trip samples per measurement: one-row calls on a zero input.
const PINGS: usize = 8;
/// A link is measured again once its newest probe is this old, so an idle
/// cohort's links do not go stale; a closed machine is retried as often.
const PROBE_REFRESH_HEIGHTS: u64 = LINK_MAX_AGE_HEIGHTS / 2;
const MAX_CONFIG_BYTES: u64 = 1 << 20;
const MAX_CALL_TIMEOUT_MS: u64 = 600_000;
const MAX_STARTUP_TIMEOUT_MS: u64 = 3_600_000;
const MAX_SPOT_ROWS: u32 = 64;

/// A generation's tokens and output hash, or why it stopped.
type Generated = Result<(Vec<u32>, Hash256), NativeInferenceError>;

fn default_max_workers() -> usize {
    4
}
fn default_duplicate_per_mille() -> u32 {
    50
}
fn default_spot_rows() -> u32 {
    2
}
fn default_startup_timeout_ms() -> u64 {
    600_000
}

/// This operator's own row-worker machines (`--native-row-workers <path>`,
/// JSON). Each runs `tensor_row_model_worker` on the same artifact and is
/// reached over SSH with a pinned host key.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RowCohortConfig {
    pub workers: Vec<RowWorkerEntry>,
    /// Most machines one request is placed on.
    #[serde(default = "default_max_workers")]
    pub max_workers: usize,
    /// Per mille of machine slices recomputed by another participant.
    #[serde(default = "default_duplicate_per_mille")]
    pub duplicate_per_mille: u32,
    /// Rows of each projection this node recomputes itself, per request.
    #[serde(default = "default_spot_rows")]
    pub spot_rows_per_stage: u32,
    /// Explicit fixed per-projection ownership; absent preserves the legacy
    /// full-layer config and stable policy bytes. Requires low residency.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub partial_rows: Option<crate::row_residency::PartialRowConfig>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RowWorkerEntry {
    /// The operator's name for the machine, also its transport identity.
    pub id: String,
    pub ssh_program: PathBuf,
    /// `user@host`, or `ssh://user@host:port`. ssh reads no configuration
    /// file for these machines (the host-key pin must be exclusive), so the
    /// key must be one of ssh's default identities or held by an agent.
    pub target: String,
    pub known_hosts: PathBuf,
    pub remote_command: Vec<String>,
    /// Per call, once the machine has answered its first call.
    pub timeout_ms: u64,
    /// The first call after connecting, which also covers the remote model
    /// load.
    #[serde(default = "default_startup_timeout_ms")]
    pub startup_timeout_ms: u64,
    pub ram_headroom_bytes: u64,
    pub max_concurrency: u32,
    /// Layers whose weights this machine holds, as half-open `[start, end)`
    /// ranges - the same grid as `--shard-range`. Empty (the default) means
    /// it loads the whole model, which is what every existing config does.
    /// Declaring ranges lets a machine serve only the layers it can hold, so
    /// its memory bounds its assignment instead of the model's total size.
    #[serde(default)]
    pub resident_layers: Vec<(u32, u32)>,
    /// This ranged worker also holds the complete output head.
    #[serde(default)]
    pub resident_output: bool,
}

impl RowCohortConfig {
    fn placement_policy(&self, low_residency: bool, now: u64) -> (Policy, VerificationRule) {
        (
            Policy {
                max_workers: self.max_workers,
                max_link_age: LINK_MAX_AGE_HEIGHTS,
                now,
                max_failure_per_mille: LINK_MAX_FAILURE_PER_MILLE,
                allow_simulated_links: false,
                input_element_bytes: 8,
                output_element_bytes: 8,
                weight_bytes_per_element: 1,
                include_coordinator: !low_residency,
            },
            VerificationRule {
                duplicate_per_mille: self.duplicate_per_mille,
                spot_rows_per_stage: self.spot_rows_per_stage,
            },
        )
    }

    /// The actual private-cohort policy authorized by a native request.
    /// Workers and live observations remain certificate inputs, not policy.
    pub fn assignment_hash(&self, low_residency: bool) -> Hash256 {
        let (policy, verification) = self.placement_policy(low_residency, 0);
        if self.partial_rows.is_some() {
            arc_assign::resident::policy_hash(&policy, &verification)
        } else {
            arc_assign::certificate::policy_hash_v2(&policy, &verification)
        }
    }

    pub fn load(path: &Path) -> Result<Self, String> {
        let metadata = std::fs::metadata(path)
            .map_err(|error| format!("row cohort config {}: {error}", path.display()))?;
        if !metadata.is_file() || metadata.len() > MAX_CONFIG_BYTES {
            return Err(format!(
                "row cohort config {} is not a regular file of at most {MAX_CONFIG_BYTES} bytes",
                path.display()
            ));
        }
        let bytes = std::fs::read(path)
            .map_err(|error| format!("row cohort config {}: {error}", path.display()))?;
        let config: Self = serde_json::from_slice(&bytes)
            .map_err(|error| format!("row cohort config {}: {error}", path.display()))?;
        config.validate()?;
        Ok(config)
    }

    pub fn validate(&self) -> Result<(), String> {
        if self.workers.is_empty() || self.workers.len() > MAX_COHORT_WORKERS {
            return Err(format!(
                "a row cohort lists 1 to {MAX_COHORT_WORKERS} machines"
            ));
        }
        if self.max_workers == 0 || self.max_workers > MAX_COHORT_WORKERS {
            return Err(format!("max_workers must be 1 to {MAX_COHORT_WORKERS}"));
        }
        if self.duplicate_per_mille > 1_000 {
            return Err("duplicate_per_mille is at most 1000".into());
        }
        if self.spot_rows_per_stage > MAX_SPOT_ROWS {
            return Err(format!("spot_rows_per_stage is at most {MAX_SPOT_ROWS}"));
        }
        if let Some(partial) = &self.partial_rows {
            partial.validate(&self.workers)?;
            if self.max_workers < self.workers.len() || self.spot_rows_per_stage == 0 {
                return Err(
                    "partial rows require all configured owners and per-stage canonical checks"
                        .into(),
                );
            }
        }
        let mut ids = std::collections::BTreeSet::new();
        for entry in &self.workers {
            let id = &entry.id;
            if id.is_empty() || id.len() > 64 || id.chars().any(char::is_whitespace) {
                return Err(format!(
                    "machine id {id:?} must be 1 to 64 characters without spaces"
                ));
            }
            if !ids.insert(id.as_str()) {
                return Err(format!("machine id {id} is listed twice"));
            }
            if entry.target.is_empty() || entry.target.starts_with('-') {
                return Err(format!("machine {id}: target must be user@host"));
            }
            if entry.remote_command.is_empty() {
                return Err(format!("machine {id}: remote_command is empty"));
            }
            if !entry
                .known_hosts
                .to_str()
                .is_some_and(is_plain_absolute_path)
            {
                return Err(format!(
                    "machine {id}: known_hosts must be an absolute path of letters, digits and ._/-"
                ));
            }
            if entry.startup_timeout_ms == 0 || entry.startup_timeout_ms > MAX_STARTUP_TIMEOUT_MS {
                return Err(format!(
                    "machine {id}: startup_timeout_ms must be 1 to {MAX_STARTUP_TIMEOUT_MS}"
                ));
            }
            for (start, end) in &entry.resident_layers {
                if start >= end {
                    return Err(format!(
                        "machine {id}: resident_layers range {start}:{end} is empty; \
                         ranges are half-open [start, end)"
                    ));
                }
            }
            if entry.timeout_ms == 0 || entry.timeout_ms > MAX_CALL_TIMEOUT_MS {
                return Err(format!(
                    "machine {id}: timeout_ms must be 1 to {MAX_CALL_TIMEOUT_MS}"
                ));
            }
            if entry.max_concurrency == 0 || entry.max_concurrency > MAX_SLOTS_PER_WORKER {
                return Err(format!(
                    "machine {id}: max_concurrency must be 1 to {MAX_SLOTS_PER_WORKER}"
                ));
            }
        }
        Ok(())
    }

    fn validate_for_model(&self, layer_count: usize) -> Result<(), String> {
        self.validate()?;
        for worker in &self.workers {
            probe_layer_for_residency(&worker.resident_layers, layer_count)
                .map_err(|error| format!("machine {}: {error}", worker.id))?;
        }
        Ok(())
    }
}

/// Pick the lowest layer a worker declares resident. Empty residency retains
/// the legacy full-model meaning and therefore probes layer zero.
fn probe_layer_for_residency(
    resident_layers: &[(u32, u32)],
    layer_count: usize,
) -> Result<usize, String> {
    if layer_count == 0 {
        return Err("the model has no layers to probe".into());
    }
    if resident_layers.is_empty() {
        return Ok(0);
    }
    let mut selected = None;
    for &(start, end) in resident_layers {
        if start >= end {
            return Err(format!(
                "resident layer range [{start}, {end}) is empty or reversed"
            ));
        }
        if u64::from(end) > layer_count as u64 {
            return Err(format!(
                "resident layer range [{start}, {end}) exceeds the model's {layer_count} layers"
            ));
        }
        selected = Some(selected.map_or(start, |current: u32| current.min(start)));
    }
    selected
        .map(|layer| layer as usize)
        .ok_or_else(|| "resident layer set is empty".into())
}

/// Whether a request's `Host` header names this machine itself (`localhost`,
/// a 127/8 address or `[::1]`, with or without a port). A browser page that
/// reaches the node through a rebound name carries that name instead.
pub fn host_header_is_loopback(host: &str) -> bool {
    let digits = |port: &str| port.chars().all(|c| c.is_ascii_digit());
    if let Some(rest) = host.strip_prefix('[') {
        // An IPv6 literal: `[addr]` or `[addr]:port`, nothing else.
        let Some((name, after)) = rest.split_once(']') else {
            return false;
        };
        let port_fine = after.is_empty() || after.strip_prefix(':').is_some_and(digits);
        return port_fine
            && name
                .parse::<std::net::Ipv6Addr>()
                .is_ok_and(|ip| ip.is_loopback());
    }
    // A name or an IPv4 address, optionally `:port`; a bare IPv6 address has
    // more than one colon and fails the port check.
    let (name, port) = host.split_once(':').unwrap_or((host, ""));
    digits(port)
        && (name.eq_ignore_ascii_case("localhost")
            || name
                .parse::<std::net::Ipv4Addr>()
                .is_ok_and(|ip| ip.is_loopback()))
}

/// The identity a machine is placed, reserved and excluded under: this
/// validator's address, the operator's id for the machine and where it is
/// reached. Two validators' machines never share one.
pub fn machine_address(validator: &Hash256, entry: &RowWorkerEntry) -> Hash256 {
    let mut hasher = blake3::Hasher::new_derive_key("ARC-operator-row-machine-v1");
    hasher.update(&validator.0);
    for part in [entry.id.as_bytes(), entry.target.as_bytes()] {
        hasher.update(&(part.len() as u64).to_le_bytes());
        hasher.update(part);
    }
    Hash256(*hasher.finalize().as_bytes())
}

/// The digest a certificate binds for one machine's configuration entry.
fn entry_digest(entry: &RowWorkerEntry) -> Hash256 {
    let mut hasher = blake3::Hasher::new_derive_key("ARC-operator-row-entry-v1");
    hasher.update(&serde_json::to_vec(entry).expect("a config entry serialises"));
    Hash256(*hasher.finalize().as_bytes())
}

fn verify_row_certificate(
    certificate: &AssignmentCertificate,
    request: Hash256,
    artifact: Hash256,
    profile: &str,
    assignment: Hash256,
    now: u64,
) -> Result<(), NativeInferenceError> {
    if certificate.version != 2
        || certificate.request_id != request
        || certificate.artifact_id != artifact
        || certificate.execution_profile != profile
        || certificate.policy.now != now
        || certificate.epoch != now / EPOCH_HEIGHTS
    {
        return Err(NativeInferenceError::Executor(
            "row assignment certificate does not match this job and observation".into(),
        ));
    }
    certificate
        .verify(&assignment)
        .map_err(|error| NativeInferenceError::Executor(error.to_string()))
}

fn tensor_name(tensor: TensorKey) -> &'static str {
    match tensor {
        TensorKey::Wq => "wq",
        TensorKey::Wk => "wk",
        TensorKey::Wv => "wv",
        TensorKey::Wo => "wo",
        TensorKey::WGate => "w_gate",
        TensorKey::WUp => "w_up",
        TensorKey::WDown => "w_down",
        TensorKey::LmHead => "lm_head",
    }
}

/// Every projection the canonical forward dispatches, in forward order: the
/// backend's keys and the placement's stages, index for index.
pub fn projection_stages(
    model: &dyn CanonicalRowSource,
) -> (Vec<(Option<usize>, TensorKey)>, Vec<Stage>) {
    let mut keys = Vec::new();
    for layer in 0..model.config().n_layers {
        for tensor in [
            TensorKey::Wq,
            TensorKey::Wk,
            TensorKey::Wv,
            TensorKey::Wo,
            TensorKey::WGate,
            TensorKey::WUp,
            TensorKey::WDown,
        ] {
            keys.push((Some(layer), tensor));
        }
    }
    keys.push((None, TensorKey::LmHead));
    let mut kept = Vec::with_capacity(keys.len());
    let mut stages = Vec::with_capacity(keys.len());
    for (layer, tensor) in keys {
        if let Some((rows, cols)) = model.projection_shape(layer, tensor) {
            kept.push((layer, tensor));
            stages.push(Stage {
                layer: layer.map(|l| l as u32),
                tensor: tensor_name(tensor).into(),
                rows: rows as u64,
                cols: cols as u64,
            });
        }
    }
    (kept, stages)
}

/// The verified backend's plans for a certificate: each stage's slices in
/// row order (this node's rows local, a machine's remote), with the
/// certificate's verification plan applied. A duplicated slice is recomputed
/// by its checker; a spot row is recomputed by this node.
pub fn plans_from(
    certificate: &AssignmentCertificate,
    keys: &[(Option<usize>, TensorKey)],
    id_of: &BTreeMap<[u8; 32], String>,
) -> Result<BTreeMap<(Option<usize>, TensorKey), ProjectionPlan>, String> {
    plans_with_checks(
        certificate,
        keys,
        id_of,
        verification_plan(
            certificate.hash(),
            &certificate.placement,
            &certificate.verification,
        ),
    )
}

fn plans_with_checks(
    certificate: &AssignmentCertificate,
    keys: &[(Option<usize>, TensorKey)],
    id_of: &BTreeMap<[u8; 32], String>,
    checks: arc_assign::verify::VerificationPlan,
) -> Result<BTreeMap<(Option<usize>, TensorKey), ProjectionPlan>, String> {
    let owner = |participant: &Participant| match participant {
        Participant::Coordinator => Ok(SliceOwner::Local),
        Participant::Worker(address) => id_of
            .get(&address.0)
            .cloned()
            .map(SliceOwner::Remote)
            .ok_or_else(|| "the placement names a machine this cohort does not list".to_string()),
    };
    let key_of = |stage: usize| {
        keys.get(stage)
            .copied()
            .ok_or_else(|| format!("the placement names stage {stage}, beyond the model"))
    };
    let mut plans = BTreeMap::new();
    for stage_plan in &certificate.placement.stages {
        let mut slices = Vec::with_capacity(stage_plan.slices.len());
        for slice in &stage_plan.slices {
            slices.push(PlannedSlice {
                owner: owner(&slice.participant)?,
                row_start: usize::try_from(slice.row_start).map_err(|e| e.to_string())?,
                row_end: usize::try_from(slice.row_end).map_err(|e| e.to_string())?,
                duplicate_on: None,
            });
        }
        plans.insert(
            key_of(stage_plan.stage)?,
            ProjectionPlan {
                slices,
                spot_rows: Vec::new(),
            },
        );
    }
    for duplicate in &checks.duplicates {
        let key = key_of(duplicate.stage)?;
        let slice = plans
            .get_mut(&key)
            .and_then(|plan| plan.slices.get_mut(duplicate.slice))
            .ok_or("the verification plan names a slice the placement lacks")?;
        slice.duplicate_on = Some(owner(&duplicate.checker)?);
    }
    for (stage, row) in &checks.spot_rows {
        let key = key_of(*stage)?;
        let plan = plans
            .get_mut(&key)
            .ok_or("the verification plan names a stage the placement lacks")?;
        plan.spot_rows
            .push(usize::try_from(*row).map_err(|e| e.to_string())?);
    }
    Ok(plans)
}

enum CohortAssignment {
    Legacy(AssignmentCertificate),
    Resident(ResidentCertificate),
}
impl std::ops::Deref for CohortAssignment {
    type Target = AssignmentCertificate;
    fn deref(&self) -> &Self::Target {
        match self {
            Self::Legacy(c) => c,
            Self::Resident(c) => &c.assignment,
        }
    }
}
impl CohortAssignment {
    fn hash(&self) -> Hash256 {
        match self {
            Self::Legacy(c) => c.hash(),
            Self::Resident(c) => c.hash(),
        }
    }
    fn plans(
        &self,
        keys: &[(Option<usize>, TensorKey)],
        id_of: &BTreeMap<[u8; 32], String>,
    ) -> Result<BTreeMap<(Option<usize>, TensorKey), ProjectionPlan>, String> {
        match self {
            Self::Legacy(c) => plans_from(c, keys, id_of),
            Self::Resident(c) => {
                plans_with_checks(&c.assignment, keys, id_of, c.verification_plan())
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct ProbeLayout {
    layer: usize,
    narrow_row: usize,
    wide_row: usize,
    challenge_start: usize,
    challenge_end: usize,
}
fn probe_layout(
    model: &dyn CanonicalRowSource,
    layer: usize,
    residency: Option<&WorkerResidency>,
    keys: &[(Option<usize>, TensorKey)],
) -> Result<ProbeLayout, String> {
    let range = |tensor| -> Result<(usize, usize), String> {
        let (rows, _) = model
            .projection_shape(Some(layer), tensor)
            .ok_or("missing probe projection")?;
        match residency {
            None => Ok((0, rows)),
            Some(owner) => {
                let stage = keys
                    .iter()
                    .position(|key| *key == (Some(layer), tensor))
                    .ok_or("missing probe stage")?;
                let range = owner
                    .ranges
                    .iter()
                    .find(|range| range.stage == stage)
                    .ok_or("owner lacks probe rows")?;
                let start = usize::try_from(range.row_start).map_err(|e| e.to_string())?;
                let end = usize::try_from(range.row_end).map_err(|e| e.to_string())?;
                if start >= end || end > rows {
                    return Err("probe rows outside projection".into());
                }
                Ok((start, end))
            }
        }
    };
    let (gate_start, gate_end) = range(TensorKey::WGate)?;
    Ok(ProbeLayout {
        layer,
        narrow_row: range(TensorKey::Wq)?.0,
        wide_row: range(TensorKey::WDown)?.0,
        challenge_start: gate_start,
        challenge_end: gate_end.min(gate_start + CHALLENGE_ROWS),
    })
}

struct Machine {
    entry: RowWorkerEntry,
    address: Hash256,
    digest: Hash256,
    probe: ProbeLayout,
}

struct Books {
    epoch: u64,
    challenges: ChallengeBook,
    probes: ProbeBook,
    ledger: ReservationLedger,
    exclusions: Exclusions,
    /// Height of each machine's last completed measurement.
    attempted: BTreeMap<[u8; 32], u64>,
    /// Machines with a measurement in flight on the background thread.
    measuring: BTreeSet<[u8; 32]>,
    /// Height of the last round whose challenge could not be prepared; the
    /// immediate challenge of an unchallenged machine waits a refresh
    /// interval after it, rather than retrying on every request.
    material_failed_at: Option<u64>,
    /// Machines whose failures a reconnect forgave this epoch: at most once
    /// each, so a machine that answers pings but fails its real calls is not
    /// placed again and again.
    forgiven: BTreeSet<[u8; 32]>,
}

/// One request's placement and what happened to it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CohortRecord {
    pub request_id: String,
    pub height: u64,
    /// The certificate's hash; `None` when nothing was placed.
    pub certificate: Option<String>,
    /// The machines the placement used, by id.
    pub machines: Vec<String>,
    pub predicted_token_us: u64,
    pub coordinator_only_token_us: u64,
    /// Calls a machine answered.
    pub answered: u64,
    /// Calls that failed and were computed here instead.
    pub fallbacks: u64,
    /// Slices of excluded or disconnected machines, computed here without a
    /// call.
    pub skipped: u64,
    pub faults: u64,
    pub elapsed_ms: u64,
    /// `partitioned`, or `local: <why>`. A placed run in which no machine
    /// answered says so.
    pub outcome: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct MachineView {
    pub id: String,
    pub connected: bool,
    pub measured_macs_per_s: Option<u64>,
    pub excluded: bool,
    pub free_slots: u32,
}

/// The operator's view of a cohort (`/assignment/cohort`, loopback only).
/// Every count is bounded and is a growth gauge.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CohortView {
    pub low_residency: bool,
    pub local_fallback_enabled: bool,
    pub partial_row_residency: bool,
    /// Partial fixed placement has no modeled total latency. Canonical checks
    /// read the local artifact and consume CPU/I/O, including full selected duplicates.
    pub placement_timing_prediction_available: bool,
    pub machines: Vec<MachineView>,
    pub coordinator_macs_per_s: u64,
    pub epoch: u64,
    pub challenges: usize,
    pub probes: usize,
    pub reservations: usize,
    pub exclusions: usize,
    pub recent: Vec<CohortRecord>,
}

/// This operator's row cohort, connected and measured.
pub struct RowCohort {
    /// This cohort, for its background measurements.
    me: Weak<RowCohort>,
    model: CoordinatorModel,
    artifact: Hash256,
    config: RowCohortConfig,
    machines: Vec<Machine>,
    residency: Option<Vec<WorkerResidency>>,
    /// Open connections, by machine id. A machine whose connection closed
    /// (the SSH worker closes itself on any failure) is reconnected at its
    /// next measurement.
    workers: Mutex<BTreeMap<String, Arc<SshStdioRowWorker>>>,
    coordinator_macs_per_s: u64,
    books: Mutex<Books>,
    recent: Mutex<VecDeque<CohortRecord>>,
    /// The latest chain height a request brought; measurements finishing on
    /// the background thread are recorded at it.
    last_height: AtomicU64,
}

/// One round's challenge: rows of a resident layer's gate for a height-seeded
/// input, and the answer this node computed for it. Machines resident on the
/// same layer share the material, so it is computed once per layer.
struct ChallengeMaterial {
    layer: usize,
    tensor: TensorKey,
    rows: usize,
    row_start: usize,
    input: Vec<i64>,
    expected: Vec<i64>,
}

fn challenge_material(
    model: &dyn CanonicalRowSource,
    layer: usize,
    height: u64,
) -> Result<ChallengeMaterial, String> {
    let (rows, _) = model
        .projection_shape(Some(layer), TensorKey::WGate)
        .ok_or("the model has no challenge layer")?;
    challenge_material_range(model, layer, 0, CHALLENGE_ROWS.min(rows), height)
}

fn challenge_material_range(
    model: &dyn CanonicalRowSource,
    layer: usize,
    row_start: usize,
    row_end: usize,
    height: u64,
) -> Result<ChallengeMaterial, String> {
    let tensor = TensorKey::WGate;
    let (total_rows, cols) = model
        .projection_shape(Some(layer), tensor)
        .ok_or("the model has no challenge layer")?;
    if row_start >= row_end || row_end > total_rows || row_end - row_start > CHALLENGE_ROWS {
        return Err("challenge rows outside residency or bounded work budget".into());
    }
    let rows = row_end - row_start;
    let input: Vec<i64> = (0..cols as u64)
        .map(|i| {
            let h = arc_crypto::hash_bytes(
                &[
                    height.to_le_bytes().as_slice(),
                    (layer as u64).to_le_bytes().as_slice(),
                    i.to_le_bytes().as_slice(),
                ]
                .concat(),
            );
            // Q16 activations in [-2, 2).
            i64::from(u32::from_le_bytes([h.0[0], h.0[1], h.0[2], h.0[3]]) % (4 << 16)) - (2 << 16)
        })
        .collect();
    let expected = model
        .projection_rows(Some(layer), tensor, row_start, row_end, &input)
        .map_err(|e| e.to_string())?;
    Ok(ChallengeMaterial {
        layer,
        tensor,
        rows,
        row_start,
        input,
        expected,
    })
}

impl ChallengeMaterial {
    /// The challenge call to one machine.
    fn request(
        &self,
        execution_profile: &str,
        artifact: Hash256,
        worker_id: &str,
        height: u64,
    ) -> Result<RowProjectionRequest, String> {
        Ok(RowProjectionRequest {
            call_id: arc_crypto::hash_bytes(
                &[
                    b"ARC-row-challenge".as_slice(),
                    &height.to_le_bytes(),
                    worker_id.as_bytes(),
                ]
                .concat(),
            ),
            input_hash: hash_i64(&self.input),
            assignment: RowAssignment {
                artifact_id: artifact,
                execution_profile: execution_profile.to_string(),
                layer: Some(self.layer),
                tensor: self.tensor,
                row_start: self.row_start,
                row_end: self.row_start + self.rows,
                worker_id: worker_id.to_string(),
            },
            input: self.input.clone(),
        })
    }
}

fn elapsed_us(elapsed: Duration) -> u64 {
    u64::try_from(elapsed.as_micros())
        .unwrap_or(u64::MAX)
        .max(1)
}

/// This node's own rate on the challenge projection: the kernel alone, timed.
fn local_rate(model: &dyn CanonicalRowSource, height: u64) -> Result<u64, String> {
    let material = challenge_material(model, 0, height)?;
    let started = Instant::now();
    model
        .projection_rows(
            Some(material.layer),
            material.tensor,
            0,
            material.rows,
            &material.input,
        )
        .map_err(|e| e.to_string())?;
    let measured = ChallengeResult {
        macs: (material.rows * material.input.len()) as u64,
        elapsed_us: elapsed_us(started.elapsed()),
        correct: true,
    };
    Ok(measured.measured_macs_per_s().max(1))
}

/// A one-row call on a zero input, which must answer a zero row. Its time is
/// the link's round trip for an input of that projection's width.
#[allow(clippy::too_many_arguments)]
fn zero_call(
    model: &dyn CanonicalRowSource,
    artifact: Hash256,
    worker: &SshStdioRowWorker,
    worker_id: &str,
    layer: usize,
    tensor: TensorKey,
    row_start: usize,
    seed: &[u8],
) -> Result<u64, String> {
    let request = zero_request(model, artifact, worker_id, layer, tensor, row_start, seed)?;
    let started = Instant::now();
    let response = worker.project(request).map_err(|error| error.to_string())?;
    let elapsed = elapsed_us(started.elapsed());
    if response.values != [0] {
        return Err("a zero input did not give a zero row".into());
    }
    Ok(elapsed)
}

fn zero_request(
    model: &dyn CanonicalRowSource,
    artifact: Hash256,
    worker_id: &str,
    layer: usize,
    tensor: TensorKey,
    row_start: usize,
    seed: &[u8],
) -> Result<RowProjectionRequest, String> {
    let (rows, cols) = model
        .projection_shape(Some(layer), tensor)
        .ok_or("the model has no probe layer")?;
    if row_start >= rows {
        return Err("probe row outside projection".into());
    }
    let input = vec![0i64; cols];
    Ok(RowProjectionRequest {
        call_id: arc_crypto::hash_bytes(
            &[b"ARC-row-ping".as_slice(), seed, worker_id.as_bytes()].concat(),
        ),
        input_hash: hash_i64(&input),
        assignment: RowAssignment {
            artifact_id: artifact,
            execution_profile: model
                .canonical_execution_profile()
                .ok_or("the model has no canonical profile")?
                .to_string(),
            layer: Some(layer),
            tensor,
            row_start,
            row_end: row_start + 1,
            worker_id: worker_id.to_string(),
        },
        input,
    })
}

fn failed_probe() -> Probe {
    Probe {
        rtt_us: 0,
        bytes: 0,
        transfer_us: 0,
        failed: true,
    }
}

/// `elapsed` less the round trip, when the call took clearly longer than one
/// round trip; otherwise `elapsed` itself, so jitter never inflates a rate.
fn beyond_round_trip(elapsed: u64, rtt: Option<u64>) -> u64 {
    match rtt {
        Some(rtt) if elapsed > rtt.saturating_mul(2) => elapsed - rtt,
        _ => elapsed,
    }
    .max(1)
}

/// What one measurement of a machine found.
struct Measurement {
    index: usize,
    /// The connection, if one is open after the measurement.
    worker: Option<Arc<SshStdioRowWorker>>,
    /// A new connection was made and answered.
    reconnected: bool,
    probes: Vec<Probe>,
    /// When a challenge was due and the machine was reachable: the measured
    /// rate, or why it was refused. An unreachable machine gets none, so it
    /// can still take this epoch's challenge once it answers.
    challenge: Option<Result<u64, String>>,
}

/// Marks one machine as being measured, and clears the mark when dropped,
/// however the measurement ends: recorded, panicked, or never started because
/// no thread could be created.
struct Measuring {
    cohort: Weak<RowCohort>,
    address: [u8; 32],
}

impl Drop for Measuring {
    fn drop(&mut self) {
        if let Some(cohort) = self.cohort.upgrade() {
            cohort.books.lock().measuring.remove(&self.address);
        }
    }
}

/// A placed request's reservations, released when this is dropped: on the
/// normal return, an early one, or an unwind.
struct Reserved<'a> {
    cohort: &'a RowCohort,
    request: Hash256,
}

impl Drop for Reserved<'_> {
    fn drop(&mut self) {
        self.cohort.books.lock().ledger.release(&self.request);
    }
}

impl RowCohort {
    pub fn assignment_hash(&self) -> Hash256 {
        self.config.assignment_hash(self.model.is_low_residency())
    }

    #[cfg(test)]
    pub(crate) fn disconnected_policy_fixture(
        config: RowCohortConfig,
        model: Arc<CachedIntegerModel>,
        artifact: Hash256,
    ) -> Arc<Self> {
        // No model load, connections, measurement threads or qualification.
        // Tests exercise policy refusal before any of those may be used.
        Arc::new_cyclic(|me| Self {
            me: me.clone(),
            model: CoordinatorModel::Resident(model),
            artifact,
            config,
            machines: vec![],
            residency: None,
            workers: Mutex::new(BTreeMap::new()),
            coordinator_macs_per_s: 1,
            books: Mutex::new(Books {
                epoch: 0,
                challenges: ChallengeBook::new(0),
                probes: ProbeBook::new(false),
                ledger: ReservationLedger::new(),
                exclusions: Exclusions::new(0),
                attempted: BTreeMap::new(),
                measuring: BTreeSet::new(),
                material_failed_at: None,
                forgiven: BTreeSet::new(),
            }),
            recent: Mutex::new(VecDeque::new()),
            last_height: AtomicU64::new(0),
        })
    }

    fn verify_certificate(
        &self,
        certificate: &CohortAssignment,
        request: Hash256,
        assignment: Hash256,
        now: u64,
    ) -> Result<(), NativeInferenceError> {
        if assignment != self.assignment_hash() {
            return Err(NativeInferenceError::Executor(
                "row assignment certificate does not match this job and runtime policy".into(),
            ));
        }
        match certificate {
            CohortAssignment::Legacy(c) if self.residency.is_none() => verify_row_certificate(
                c,
                request,
                self.artifact,
                self.model
                    .source()
                    .canonical_execution_profile()
                    .unwrap_or_default(),
                assignment,
                now,
            ),
            CohortAssignment::Resident(c) if self.residency.as_ref() == Some(&c.residency) => {
                if c.assignment.request_id != request
                    || c.assignment.artifact_id != self.artifact
                    || c.assignment.execution_profile
                        != self
                            .model
                            .source()
                            .canonical_execution_profile()
                            .unwrap_or_default()
                    || c.assignment.policy.now != now
                    || c.assignment.epoch != now / EPOCH_HEIGHTS
                {
                    return Err(NativeInferenceError::Executor(
                        "resident certificate job or observation mismatch".into(),
                    ));
                }
                c.verify(&assignment)
                    .map_err(NativeInferenceError::Executor)
            }
            _ => Err(NativeInferenceError::Executor(
                "certificate does not match the runtime resident layout".into(),
            )),
        }
    }

    /// Build the cohort and measure every listed machine on a background
    /// thread (connecting, timing and challenging it). Until a machine is
    /// measured it is not placed, and the node computes those rows itself, so
    /// neither startup nor a request waits on a connect or a model load.
    pub fn connect(
        config: RowCohortConfig,
        validator: Hash256,
        model: Arc<CachedIntegerModel>,
        artifact: Hash256,
        height: u64,
    ) -> Result<Arc<Self>, String> {
        Self::connect_model(
            config,
            validator,
            CoordinatorModel::Resident(model),
            artifact,
            height,
        )
    }

    pub(crate) fn connect_model(
        config: RowCohortConfig,
        validator: Hash256,
        model: CoordinatorModel,
        artifact: Hash256,
        height: u64,
    ) -> Result<Arc<Self>, String> {
        if model.is_low_residency() && config.spot_rows_per_stage == 0 {
            return Err(
                "low-residency execution requires numerical spot checks on every stage".into(),
            );
        }
        config.validate_for_model(model.config().n_layers)?;
        if config.partial_rows.is_some() && !model.is_low_residency() {
            return Err(
                "partial rows require low residency; no full-model fallback mode is supported"
                    .into(),
            );
        }
        let (keys, stages) = projection_stages(model.source());
        let residency = config
            .partial_rows
            .as_ref()
            .map(|partial| {
                let mut owners =
                    partial.load(&config.workers, validator, model.source(), artifact)?;
                owners.sort_by_key(|owner| owner.worker.0);
                for owner in &mut owners {
                    owner.ranges.sort_by_key(|range| range.stage);
                }
                arc_assign::resident::validate_layout(&stages, &owners, config.max_workers)?;
                Ok::<_, String>(owners)
            })
            .transpose()?;
        if model.is_low_residency() && residency.is_none() {
            // Declaration is not readiness: measured workers must still pass
            // exact placement and per-call verification before execution.
            for layer in 0..model.config().n_layers as u32 {
                if !config.workers.iter().any(|w| {
                    w.resident_layers.is_empty()
                        || w.resident_layers
                            .iter()
                            .any(|&(start, end)| (start..end).contains(&layer))
                }) {
                    return Err(format!(
                        "low-residency cohort has no declared coverage for layer {layer}"
                    ));
                }
            }
            if !config
                .workers
                .iter()
                .any(|w| w.resident_layers.is_empty() || w.resident_output)
            {
                return Err("low-residency cohort has no declared output-head coverage".into());
            }
        }

        let machines: Vec<Machine> = config
            .workers
            .iter()
            .map(|entry| {
                Ok(Machine {
                    entry: entry.clone(),
                    address: machine_address(&validator, entry),
                    digest: entry_digest(entry),
                    probe: probe_layout(
                        model.source(),
                        probe_layer_for_residency(&entry.resident_layers, model.config().n_layers)?,
                        residency.as_ref().and_then(|owners| {
                            owners
                                .iter()
                                .find(|owner| owner.worker == machine_address(&validator, entry))
                        }),
                        &keys,
                    )?,
                })
            })
            .collect::<Result<_, String>>()?;
        let coordinator_macs_per_s = if model.is_low_residency() {
            0
        } else {
            local_rate(model.source(), height)?
        };
        let epoch = height / EPOCH_HEIGHTS;
        let cohort = Arc::new_cyclic(|me| Self {
            me: me.clone(),
            model,
            artifact,
            config,
            machines,
            residency,
            workers: Mutex::new(BTreeMap::new()),
            coordinator_macs_per_s,
            books: Mutex::new(Books {
                epoch,
                challenges: ChallengeBook::new(epoch),
                probes: ProbeBook::new(false),
                ledger: ReservationLedger::new(),
                exclusions: Exclusions::new(epoch),
                attempted: BTreeMap::new(),
                measuring: BTreeSet::new(),
                material_failed_at: None,
                forgiven: BTreeSet::new(),
            }),
            recent: Mutex::new(VecDeque::new()),
            last_height: AtomicU64::new(height),
        });
        let all: Vec<(usize, bool)> = (0..cohort.machines.len())
            .map(|index| (index, true))
            .collect();
        cohort.spawn_measure(all, height);
        Ok(cohort)
    }

    fn address_of(&self, id: &str) -> Option<Hash256> {
        self.machines
            .iter()
            .find(|machine| machine.entry.id == id)
            .map(|machine| machine.address)
    }

    /// An open connection to machine `index`, and whether it is new: the
    /// current one, or a new one whose first call, under the startup timeout
    /// that covers the remote model load, has answered.
    fn open_worker(
        &self,
        index: usize,
        height: u64,
    ) -> Result<(Arc<SshStdioRowWorker>, bool), String> {
        let entry = &self.machines[index].entry;
        let current = self.workers.lock().get(&entry.id).cloned();
        if let Some(worker) = current.filter(|worker| worker.is_open()) {
            return Ok((worker, false));
        }
        let ssh = SshStdioConfig {
            ssh_program: entry.ssh_program.clone(),
            target: entry.target.clone(),
            known_hosts: entry.known_hosts.clone(),
            remote_command: entry.remote_command.clone(),
            timeout: Duration::from_millis(entry.startup_timeout_ms),
        };
        let worker =
            SshStdioRowWorker::connect(entry.id.clone(), ssh).map_err(|error| error.to_string())?;
        let seed = [b"warm-up".as_slice(), &height.to_le_bytes()].concat();
        zero_call(
            self.model.source(),
            self.artifact,
            &worker,
            &entry.id,
            self.machines[index].probe.layer,
            TensorKey::Wq,
            self.machines[index].probe.narrow_row,
            &seed,
        )?;
        worker.set_timeout(Duration::from_millis(entry.timeout_ms));
        Ok((Arc::new(worker), true))
    }

    /// Measure one machine: connect or reuse it, time `PINGS` zero-input
    /// calls for the round trip and `BULK_CALLS` wide ones for bandwidth, and
    /// run the challenge when one is given.
    fn measure_one(
        &self,
        height: u64,
        index: usize,
        material: Option<&ChallengeMaterial>,
    ) -> Measurement {
        let (model, artifact) = (self.model.source(), self.artifact);
        let entry = &self.machines[index].entry;
        let probe = self.machines[index].probe;
        let probe_layer = probe.layer;
        let (worker, reconnected) = match self.open_worker(index, height) {
            Ok(opened) => opened,
            Err(error) => {
                tracing::warn!(machine = %entry.id, %error, "row machine unreachable; not placed");
                return Measurement {
                    index,
                    worker: None,
                    reconnected: false,
                    probes: vec![failed_probe()],
                    challenge: None,
                };
            }
        };
        let mut probes = Vec::new();
        let mut rtts = Vec::new();
        for ping in 0..PINGS as u64 {
            // Once a failure has closed the session, further calls send
            // nothing: they are not failed probes of the link.
            if !worker.is_open() {
                break;
            }
            let seed = [height.to_le_bytes(), ping.to_le_bytes()].concat();
            match zero_call(
                model,
                artifact,
                &worker,
                &entry.id,
                probe_layer,
                TensorKey::Wq,
                probe.narrow_row,
                &seed,
            ) {
                Ok(rtt_us) => {
                    rtts.push(rtt_us);
                    // No bytes: a ping says nothing about bandwidth.
                    probes.push(Probe {
                        rtt_us,
                        bytes: 0,
                        transfer_us: 0,
                        failed: false,
                    });
                }
                Err(_) => probes.push(failed_probe()),
            }
        }
        rtts.sort_unstable();
        let rtt = rtts.get(rtts.len() / 2).copied();
        // Bandwidth: the down projection's wider input, over the median of
        // several wide calls less the round trip a ping of the model's width
        // already costs.
        if let (Some(rtt), Some(narrow), Some(wide)) = (
            rtt,
            model.projection_shape(Some(probe_layer), TensorKey::Wq),
            model.projection_shape(Some(probe_layer), TensorKey::WDown),
        ) && wide.1 > narrow.1
        {
            let mut wide_calls = Vec::new();
            for call in 0..BULK_CALLS as u64 {
                if !worker.is_open() {
                    break;
                }
                let seed = [height.to_le_bytes(), (u64::MAX - call).to_le_bytes()].concat();
                match zero_call(
                    model,
                    artifact,
                    &worker,
                    &entry.id,
                    probe_layer,
                    TensorKey::WDown,
                    probe.wide_row,
                    &seed,
                ) {
                    Ok(elapsed) => wide_calls.push(elapsed),
                    Err(_) => probes.push(failed_probe()),
                }
            }
            wide_calls.sort_unstable();
            if let Some(elapsed) = wide_calls.get(wide_calls.len() / 2) {
                probes.push(Probe {
                    rtt_us: rtt,
                    bytes: ((wide.1 - narrow.1) * 8) as u64,
                    transfer_us: beyond_round_trip(*elapsed, Some(rtt)),
                    failed: false,
                });
            }
        }
        // Only on a connection still open: a challenge that was never sent is
        // no refusal, and the machine can take it once it answers.
        let challenge = match material {
            Some(material) if worker.is_open() => {
                self.run_challenge(&worker, material, &entry.id, height, rtt)
            }
            _ => None,
        };
        let worker = worker.is_open().then_some(worker);
        Measurement {
            index,
            reconnected: reconnected && worker.is_some(),
            worker,
            probes,
            challenge,
        }
    }

    /// Send one machine its challenge. `None` when it was not delivered (the
    /// connection had closed); otherwise the measured rate, or why the
    /// machine failed it.
    fn run_challenge(
        &self,
        worker: &SshStdioRowWorker,
        material: &ChallengeMaterial,
        worker_id: &str,
        height: u64,
        rtt: Option<u64>,
    ) -> Option<Result<u64, String>> {
        let profile = match self.model.source().canonical_execution_profile() {
            Some(profile) => profile,
            None => return Some(Err("the model has no canonical profile".to_string())),
        };
        let request = match material.request(profile, self.artifact, worker_id, height) {
            Ok(request) => request,
            Err(error) => return Some(Err(error)),
        };
        let macs = (material.expected.len() * material.input.len()) as u64;
        let started = Instant::now();
        let response = match worker.project(request) {
            Ok(response) => response,
            Err(TensorParallelError::Closed) => return None,
            Err(error) => return Some(Err(error.to_string())),
        };
        let elapsed = elapsed_us(started.elapsed());
        if response.values != material.expected {
            return Some(Err("answered its challenge wrongly".to_string()));
        }
        let measured = ChallengeResult {
            macs,
            elapsed_us: beyond_round_trip(elapsed, rtt),
            correct: true,
        };
        Some(Ok(measured.measured_macs_per_s()))
    }

    /// Measure `due` machines, as `(index, challenge due)`, in the background:
    /// one thread per machine, each recording its own result as soon as it
    /// ends, so a slow machine delays no other. A machine already being
    /// measured is left to that measurement.
    fn spawn_measure(&self, due: Vec<(usize, bool)>, height: u64) {
        let marked: Vec<(usize, bool, Measuring)> = {
            let mut books = self.books.lock();
            due.into_iter()
                .filter(|(index, _)| books.measuring.insert(self.machines[*index].address.0))
                .map(|(index, challenge)| {
                    let guard = Measuring {
                        cohort: self.me.clone(),
                        address: self.machines[index].address.0,
                    };
                    (index, challenge, guard)
                })
                .collect()
        };
        if marked.is_empty() {
            return;
        }
        let me = self.me.clone();
        let epoch = height / EPOCH_HEIGHTS;
        // If no thread starts, the closure (and every guard in it) is dropped,
        // which clears the marks, and the machines are measured next time.
        let spawned = std::thread::Builder::new()
            .name("arc-row-cohort".into())
            .spawn(move || {
                let Some(cohort) = me.upgrade() else {
                    return;
                };
                let material = if marked.iter().any(|(_, challenge, _)| *challenge) {
                    let ranges: BTreeSet<(usize, usize, usize)> = marked
                        .iter()
                        .filter(|(_, challenge, _)| *challenge)
                        .map(|(index, _, _)| {
                            let probe = cohort.machines[*index].probe;
                            (probe.layer, probe.challenge_start, probe.challenge_end)
                        })
                        .collect();
                    let prepared = ranges
                        .into_iter()
                        .map(|range @ (layer, start, end)| {
                            challenge_material_range(cohort.model.source(), layer, start, end, height)
                                .map(|material| (range, material))
                        })
                        .collect::<Result<BTreeMap<_, _>, _>>();
                    cohort.books.lock().material_failed_at = prepared.is_err().then_some(height);
                    prepared
                        .map_err(|error| {
                            tracing::warn!(%error, "could not prepare resident-layer row challenges; no machine is challenged this round");
                        })
                        .ok()
                } else {
                    None
                };
                std::thread::scope(|scope| {
                    for (index, challenge, guard) in marked {
                        let cohort = &cohort;
                        let material = material
                            .as_ref()
                            .and_then(|materials| {
                                let probe = cohort.machines[index].probe;
                                materials.get(&(probe.layer, probe.challenge_start, probe.challenge_end))
                            })
                            .filter(|_| challenge);
                        let spawned = std::thread::Builder::new()
                            .name("arc-row-machine".into())
                            .spawn_scoped(scope, move || {
                                let _guard = guard;
                                let measurement = cohort.measure_one(height, index, material);
                                cohort.record(measurement, epoch);
                            });
                        if let Err(error) = spawned {
                            tracing::warn!(%error, "could not start a row machine measurement; it is retried later");
                        }
                    }
                });
            });
        if let Err(error) = spawned {
            tracing::warn!(%error, "could not start the row cohort's measurement; it is retried later");
        }
    }

    /// Record one machine's measurement as soon as it ends, at the latest
    /// height. A challenge answered for an epoch that has since ended is
    /// dropped: the machine takes the new epoch's.
    fn record(&self, measurement: Measurement, started_epoch: u64) {
        let machine = &self.machines[measurement.index];
        let (lifted, refused) = {
            let mut books = self.books.lock();
            let mut workers = self.workers.lock();
            let height = self.last_height.load(Ordering::Relaxed);
            books.attempted.insert(machine.address.0, height);
            books
                .ledger
                .set_capacity(machine.address, machine.entry.max_concurrency);
            for probe in measurement.probes {
                books.probes.record_probe(&machine.address, height, probe);
            }
            match measurement.worker {
                Some(worker) => {
                    workers.insert(machine.entry.id.clone(), worker);
                }
                None => {
                    workers.remove(&machine.entry.id);
                }
            }
            // Only a real exclusion for failures, in the epoch the measurement
            // started in, and at most once per machine per epoch. A reconnect
            // of a machine that is not excluded leaves its failure count, so
            // repeated failures still add up to an exclusion.
            let lifted = measurement.reconnected
                && books.epoch == started_epoch
                && matches!(
                    books.exclusions.get(&machine.address),
                    Some(Exclusion::Failures(_))
                )
                && books.forgiven.insert(machine.address.0)
                && books.exclusions.forgive_failures(&machine.address);
            let refused = match measurement.challenge {
                Some(outcome) if books.epoch == started_epoch => {
                    let _ = books.challenges.issue(&machine.address, height);
                    let checked =
                        outcome
                            .as_ref()
                            .copied()
                            .map_err(|_| LeaseError::DishonestCapacity {
                                claimed: 0,
                                measured: 0,
                            });
                    let state = books.challenges.answer(&machine.address, checked, height);
                    // A second answer in the same epoch is ignored by the book;
                    // only a refusal it recorded is reported.
                    match (state, outcome) {
                        (Ok(ChallengeState::Refused), Err(error)) => Some(error),
                        _ => None,
                    }
                }
                _ => None,
            };
            (lifted, refused)
        };
        if lifted {
            tracing::info!(machine = %machine.entry.id, "row machine reconnected; its exclusion for failures is lifted");
        }
        if let Some(error) = refused {
            tracing::warn!(machine = %machine.entry.id, %error, "row machine failed its challenge; not placed this epoch");
        }
    }

    /// Sweep reservations of refund-eligible requests, and schedule
    /// measurements without waiting for them. At a new epoch, challenges and
    /// exclusions reset and every machine is measured again. Between epochs,
    /// a machine is measured again once its link has gone stale or its
    /// connection closed, at most once per `PROBE_REFRESH_HEIGHTS`.
    fn begin(&self, now: u64) {
        self.last_height.fetch_max(now, Ordering::Relaxed);
        let epoch = now / EPOCH_HEIGHTS;
        let due: Vec<(usize, bool)> = {
            let mut books = self.books.lock();
            books.ledger.sweep(now);
            if books.epoch == epoch {
                let workers = self.workers.lock();
                self.machines
                    .iter()
                    .enumerate()
                    .filter_map(|(index, machine)| {
                        let open = workers
                            .get(&machine.entry.id)
                            .is_some_and(|worker| worker.is_open());
                        let unchallenged = books.challenges.state(&machine.address).is_none();
                        let material_ok = books
                            .material_failed_at
                            .is_none_or(|at| now.saturating_sub(at) >= PROBE_REFRESH_HEIGHTS);
                        // An open machine that has not taken this epoch's
                        // challenge (it was unreachable, or its measurement
                        // spanned the epoch change) takes it now.
                        if open && unchallenged && material_ok {
                            return Some((index, true));
                        }
                        let recent = books
                            .attempted
                            .get(&machine.address.0)
                            .is_some_and(|at| now.saturating_sub(*at) < PROBE_REFRESH_HEIGHTS);
                        if recent {
                            return None;
                        }
                        let stale = books.probes.link(&machine.address).is_none_or(|link| {
                            now.saturating_sub(link.measured_at) >= PROBE_REFRESH_HEIGHTS
                        });
                        (!open || stale).then_some((index, unchallenged))
                    })
                    .collect()
            } else {
                books.epoch = epoch;
                books.challenges.begin_epoch(epoch);
                books.exclusions.begin_epoch(epoch);
                books.forgiven.clear();
                (0..self.machines.len())
                    .map(|index| (index, true))
                    .collect()
            }
        };
        if !due.is_empty() {
            self.spawn_measure(due, now);
        }
    }

    fn offers(&self, open: &BTreeMap<String, Arc<dyn RowWorker>>) -> Vec<Offer> {
        self.machines
            .iter()
            .filter(|machine| open.contains_key(&machine.entry.id))
            .map(|machine| Offer {
                worker: machine.address,
                transport_id: machine.entry.id.clone(),
                ram_headroom_bytes: machine.entry.ram_headroom_bytes,
                claimed_macs_per_s: None,
                resident_layers: machine.entry.resident_layers.clone(),
                resident_output: machine.entry.resident_output,
                digest: machine.digest,
            })
            .collect()
    }

    fn remember(&self, record: CohortRecord) {
        let mut recent = self.recent.lock();
        if recent.len() == RECENT_RECORDS {
            recent.pop_front();
        }
        recent.push_back(record);
    }

    fn issue_placement(
        &self,
        request: Hash256,
        now: u64,
        stages: Vec<Stage>,
        open: &BTreeMap<String, Arc<dyn RowWorker>>,
    ) -> Result<CohortAssignment, String> {
        let books = self.books.lock();
        let (candidates, digests) = book::candidates_from(
            self.offers(open),
            &books.challenges,
            &books.probes,
            &books.ledger,
            &books.exclusions,
        );
        let (policy, rule) = self
            .config
            .placement_policy(self.model.is_low_residency(), now);
        if let Some(residency) = &self.residency {
            let assignment = AssignmentCertificate {
                version: 3,
                request_id: request,
                artifact_id: self.artifact,
                execution_profile: self
                    .model
                    .source()
                    .canonical_execution_profile()
                    .unwrap_or_default()
                    .into(),
                epoch: books.epoch,
                stages,
                coordinator_macs_per_s: 0,
                candidates,
                lease_digests: digests,
                policy,
                verification: rule,
                placement: arc_assign::placement::Placement {
                    workers: vec![],
                    stages: vec![],
                    predicted_token_us: 0,
                    coordinator_only_token_us: 0,
                },
            };
            return ResidentCertificate::issue(assignment, residency.clone())
                .map(CohortAssignment::Resident);
        }
        AssignmentCertificate::issue_v2(
            request,
            self.artifact,
            self.model
                .source()
                .canonical_execution_profile()
                .unwrap_or_default(),
            books.epoch,
            stages,
            self.coordinator_macs_per_s,
            candidates,
            digests,
            policy,
            rule,
        )
        .map(CohortAssignment::Legacy)
        .map_err(|error| error.to_string())
    }

    /// Refresh measurements even on an idle node. Low residency is ready only
    /// when the same measured placement and strict coverage preflight used by
    /// execution succeed. This reserves no slots and loads no model matrices.
    pub(crate) fn ready_for_requests(&self, now: u64) -> bool {
        if !self.model.is_low_residency() {
            return true;
        }
        self.begin(now);
        let open: BTreeMap<String, Arc<dyn RowWorker>> = self
            .workers
            .lock()
            .iter()
            .filter(|(_, worker)| worker.is_open())
            .map(|(id, worker)| (id.clone(), worker.clone() as Arc<dyn RowWorker>))
            .collect();
        let (keys, stages) = projection_stages(self.model.source());
        let Ok(certificate) = self.issue_placement(Hash256::ZERO, now, stages, &open) else {
            return false;
        };
        if self
            .verify_certificate(&certificate, Hash256::ZERO, self.assignment_hash(), now)
            .is_err()
        {
            return false;
        }
        let id_of = self
            .machines
            .iter()
            .map(|machine| (machine.address.0, machine.entry.id.clone()))
            .collect();
        let Ok(plans) = certificate.plans(&keys, &id_of) else {
            return false;
        };
        let stage_of = keys
            .iter()
            .enumerate()
            .map(|(index, key)| (*key, index))
            .collect();
        let events = CohortEvents {
            cohort: self,
            certificate: certificate.hash(),
            height: now,
            stage_of: &stage_of,
            answered: AtomicU64::new(0),
            fallbacks: AtomicU64::new(0),
            skipped: AtomicU64::new(0),
            faults: AtomicU64::new(0),
        };
        VerifiedPartitionBackend::new_strict(
            self.model.source(),
            self.artifact,
            plans,
            open,
            &events,
        )
        .is_ok()
    }

    /// Generate one request's tokens: placed on this cohort when placement
    /// predicts the machines make it faster, locally otherwise. Either way the
    /// tokens equal local execution's (see the module documentation). Never
    /// waits on a machine's connect or measurement.
    #[allow(clippy::too_many_arguments)]
    pub fn generate(
        &self,
        request: Hash256,
        assignment: Hash256,
        expires_at: u64,
        now: u64,
        prompt: &[u32],
        max_tokens: u32,
        eos_tokens: &[u32],
    ) -> Generated {
        if assignment != self.assignment_hash() {
            return Err(NativeInferenceError::Executor(
                "native request assignment does not authorize this row-cohort policy".into(),
            ));
        }
        let started = Instant::now();
        self.begin(now);
        let model = self.model.source();
        let local = |why: String| {
            if self.model.is_low_residency() {
                let reason = format!("refused without local fallback: {why}");
                return (Err(NativeInferenceError::Executor(reason.clone())), reason);
            }
            (
                self.model.local_generate(prompt, max_tokens, eos_tokens),
                why,
            )
        };
        let (keys, stages) = projection_stages(model);
        let open: BTreeMap<String, Arc<dyn RowWorker>> = self
            .workers
            .lock()
            .iter()
            .filter(|(_, worker)| worker.is_open())
            .map(|(id, worker)| (id.clone(), worker.clone() as Arc<dyn RowWorker>))
            .collect();
        let issued = self.issue_placement(request, now, stages, &open);
        let mut record = CohortRecord {
            request_id: request.to_hex(),
            height: now,
            certificate: None,
            machines: Vec::new(),
            predicted_token_us: 0,
            coordinator_only_token_us: 0,
            answered: 0,
            fallbacks: 0,
            skipped: 0,
            faults: 0,
            elapsed_ms: 0,
            outcome: String::new(),
        };
        let (result, outcome) = match issued {
            Err(error) => local(format!("local: placement failed: {error}")),
            Ok(certificate) => {
                // Authorization failures never enter the local-fallback
                // branch and precede every plan, reservation and generation.
                self.verify_certificate(&certificate, request, assignment, now)?;
                record.certificate = Some(certificate.hash().to_hex());
                record.predicted_token_us = certificate.placement.predicted_token_us;
                record.coordinator_only_token_us = certificate.placement.coordinator_only_token_us;
                let id_of: BTreeMap<[u8; 32], String> = self
                    .machines
                    .iter()
                    .map(|machine| (machine.address.0, machine.entry.id.clone()))
                    .collect();
                record.machines = certificate
                    .placement
                    .workers
                    .iter()
                    .filter_map(|address| id_of.get(&address.0).cloned())
                    .collect();
                if certificate.placement.workers.is_empty() {
                    local("local: this node alone is fastest".into())
                } else {
                    self.run_placed(
                        &certificate,
                        &keys,
                        &id_of,
                        &open,
                        (expires_at, now),
                        (prompt, max_tokens, eos_tokens),
                        &mut record,
                    )
                    .unwrap_or_else(local)
                }
            }
        };
        record.outcome = outcome;
        record.elapsed_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
        self.remember(record);
        result
    }

    /// Run a placed request: reserve its slots, generate on the verified
    /// backend, and release the slots on every exit path. `Err(why)` when it
    /// could not run placed, so the caller runs it locally.
    #[allow(clippy::too_many_arguments)]
    fn run_placed(
        &self,
        certificate: &CohortAssignment,
        keys: &[(Option<usize>, TensorKey)],
        id_of: &BTreeMap<[u8; 32], String>,
        open: &BTreeMap<String, Arc<dyn RowWorker>>,
        (expires_at, now): (u64, u64),
        (prompt, max_tokens, eos_tokens): (&[u32], u32, &[u32]),
        record: &mut CohortRecord,
    ) -> Result<(Generated, String), String> {
        let plans = certificate
            .plans(keys, id_of)
            .map_err(|error| format!("local: {error}"))?;
        self.books
            .lock()
            .ledger
            .reserve_all(
                &certificate.placement.workers,
                certificate.request_id,
                expires_at,
                now,
            )
            .map_err(|error| format!("local: {error}"))?;
        let _reserved = Reserved {
            cohort: self,
            request: certificate.request_id,
        };
        let stage_of: BTreeMap<(Option<usize>, TensorKey), usize> = keys
            .iter()
            .enumerate()
            .map(|(index, key)| (*key, index))
            .collect();
        let sink = CohortEvents {
            cohort: self,
            certificate: certificate.hash(),
            height: now,
            stage_of: &stage_of,
            answered: AtomicU64::new(0),
            fallbacks: AtomicU64::new(0),
            skipped: AtomicU64::new(0),
            faults: AtomicU64::new(0),
        };
        let backend = match &self.model {
            CoordinatorModel::Resident(model) => {
                VerifiedPartitionBackend::new(model, self.artifact, plans, open.clone(), &sink)
            }
            CoordinatorModel::LowResidency(model) => VerifiedPartitionBackend::new_strict(
                model.as_ref(),
                self.artifact,
                plans,
                open.clone(),
                &sink,
            ),
        };
        let generated = backend
            .map_err(|error| error.to_string())
            .and_then(|backend| {
                self.model
                    .generate_with_backend(
                        certificate.request_id,
                        prompt,
                        max_tokens,
                        eos_tokens,
                        &backend,
                    )
                    .map_err(|error| error.to_string())
            });
        record.answered = sink.answered.load(Ordering::Relaxed);
        record.fallbacks = sink.fallbacks.load(Ordering::Relaxed);
        record.skipped = sink.skipped.load(Ordering::Relaxed);
        record.faults = sink.faults.load(Ordering::Relaxed);
        match generated {
            Ok(output) if record.answered == 0 => Ok((
                Ok(output),
                format!(
                    "partitioned, but no machine answered: every machine slice was computed \
                     here ({} fallbacks, {} skipped)",
                    record.fallbacks, record.skipped
                ),
            )),
            Ok(output) => Ok((
                Ok(output),
                if self.residency.is_some() {
                    "partitioned fixed resident rows; timing prediction unavailable; canonical checks consume local artifact I/O and compute".into()
                } else {
                    "partitioned".into()
                },
            )),
            // The backend recomputes failed and faulty slices itself, so an
            // error here is this node's own; run the request locally.
            Err(error) => Err(format!("local: the partitioned run stopped: {error}")),
        }
    }

    pub fn is_low_residency(&self) -> bool {
        self.model.is_low_residency()
    }

    /// The operator's view: machines, bounded books and recent records.
    pub fn view(&self) -> CohortView {
        let books = self.books.lock();
        let workers = self.workers.lock();
        let machines = self
            .machines
            .iter()
            .map(|machine| MachineView {
                id: machine.entry.id.clone(),
                connected: workers
                    .get(&machine.entry.id)
                    .is_some_and(|worker| worker.is_open()),
                measured_macs_per_s: books.challenges.measured(&machine.address),
                excluded: books.exclusions.is_excluded(&machine.address),
                free_slots: books.ledger.free(&machine.address),
            })
            .collect();
        drop(workers);
        CohortView {
            partial_row_residency: self.residency.is_some(),
            placement_timing_prediction_available: self.residency.is_none(),
            low_residency: self.model.is_low_residency(),
            local_fallback_enabled: !self.model.is_low_residency(),
            machines,
            coordinator_macs_per_s: self.coordinator_macs_per_s,
            epoch: books.epoch,
            challenges: books.challenges.len(),
            probes: books.probes.len(),
            reservations: books.ledger.len(),
            exclusions: books.exclusions.len(),
            recent: self.recent.lock().iter().cloned().collect(),
        }
    }
}

/// Feeds the verified backend's events into the cohort's books. A failure
/// counts toward exclusion and is a failed probe; a fault excludes the
/// machine for the epoch with its evidence. An answered call's time mixes
/// compute and queueing into the round trip, so it is not a link probe; it
/// only ends a run of failures.
struct CohortEvents<'a> {
    cohort: &'a RowCohort,
    certificate: Hash256,
    height: u64,
    stage_of: &'a BTreeMap<(Option<usize>, TensorKey), usize>,
    answered: AtomicU64,
    fallbacks: AtomicU64,
    skipped: AtomicU64,
    faults: AtomicU64,
}

impl RowEventSink for CohortEvents<'_> {
    fn record(&self, event: RowEvent) {
        let was_fallback = matches!(&event, RowEvent::Fallback { .. });
        match event {
            RowEvent::Answered { worker, .. } => {
                self.answered.fetch_add(1, Ordering::Relaxed);
                if let Some(address) = self.cohort.address_of(&worker) {
                    self.cohort.books.lock().exclusions.call_succeeded(&address);
                }
            }
            RowEvent::Skipped { .. } => {
                self.skipped.fetch_add(1, Ordering::Relaxed);
            }
            RowEvent::Fallback { worker, error, .. } | RowEvent::Refused { worker, error, .. } => {
                if was_fallback {
                    self.fallbacks.fetch_add(1, Ordering::Relaxed);
                }
                let Some(address) = self.cohort.address_of(&worker) else {
                    return;
                };
                let mut books = self.cohort.books.lock();
                books
                    .probes
                    .record_probe(&address, self.height, failed_probe());
                if books.exclusions.call_failed(&address) {
                    tracing::warn!(machine = %worker, %error, "row machine excluded for the epoch after repeated failures");
                }
            }
            RowEvent::Fault {
                worker,
                layer,
                tensor,
                expected,
                found,
                ..
            } => {
                self.faults.fetch_add(1, Ordering::Relaxed);
                let Some(address) = self.cohort.address_of(&worker) else {
                    return;
                };
                let stage = self
                    .stage_of
                    .get(&(layer, tensor))
                    .copied()
                    .unwrap_or(usize::MAX);
                self.cohort.books.lock().exclusions.fault(
                    self.certificate,
                    &Finding::Fault {
                        stage,
                        participant: Participant::Worker(address),
                        expected_digest: expected,
                        found_digest: found,
                    },
                );
                tracing::error!(machine = %worker, ?layer, ?tensor, "row machine returned wrong rows and was excluded for the epoch");
            }
        }
    }

    fn is_excluded(&self, worker: &str) -> bool {
        self.cohort
            .address_of(worker)
            .is_some_and(|address| self.cohort.books.lock().exclusions.is_excluded(&address))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use arc_assign::link::LinkMeasurement;
    use arc_assign::placement::Candidate;

    fn entry(id: &str) -> RowWorkerEntry {
        RowWorkerEntry {
            id: id.into(),
            ssh_program: "/usr/bin/ssh".into(),
            target: format!("arc@{id}.lan"),
            known_hosts: "/etc/arc/known_hosts".into(),
            remote_command: vec!["/opt/arc/tensor_row_model_worker".into()],
            timeout_ms: 5_000,
            startup_timeout_ms: 600_000,
            ram_headroom_bytes: 16 << 30,
            resident_layers: vec![],
            resident_output: false,
            max_concurrency: 2,
        }
    }

    fn config(entries: Vec<RowWorkerEntry>) -> RowCohortConfig {
        RowCohortConfig {
            workers: entries,
            max_workers: default_max_workers(),
            duplicate_per_mille: default_duplicate_per_mille(),
            spot_rows_per_stage: default_spot_rows(),
            partial_rows: None,
        }
    }

    #[test]
    fn native_row_certificate_binds_the_job_policy_and_live_observation() {
        let cfg = config(vec![entry("fixture")]);
        let (policy, rule) = cfg.placement_policy(false, 10);
        let request = Hash256([1; 32]);
        let artifact = Hash256([2; 32]);
        let assignment = cfg.assignment_hash(false);
        let cert = AssignmentCertificate::issue_v2(
            request,
            artifact,
            "fixture-profile",
            0,
            vec![Stage {
                layer: None,
                tensor: "lm_head".into(),
                rows: 2,
                cols: 1,
            }],
            1_000_000,
            vec![],
            vec![],
            policy,
            rule,
        )
        .unwrap();
        verify_row_certificate(&cert, request, artifact, "fixture-profile", assignment, 10)
            .unwrap();
        for (r, a, p, authorized, now) in [
            (Hash256::ZERO, artifact, "fixture-profile", assignment, 10),
            (request, Hash256::ZERO, "fixture-profile", assignment, 10),
            (request, artifact, "other-profile", assignment, 10),
            (request, artifact, "fixture-profile", Hash256::ZERO, 10),
            (request, artifact, "fixture-profile", assignment, 11),
        ] {
            assert!(verify_row_certificate(&cert, r, a, p, authorized, now).is_err());
        }
        let mut wrong = cert.clone();
        wrong.epoch += 1;
        assert!(
            verify_row_certificate(&wrong, request, artifact, "fixture-profile", assignment, 10)
                .is_err()
        );
        wrong = cert;
        wrong.placement.stages[0].slices[0].row_end -= 1;
        assert!(
            verify_row_certificate(&wrong, request, artifact, "fixture-profile", assignment, 10)
                .is_err()
        );
    }

    /// The operator-facing template must be a config the real loader accepts.
    /// It is the artifact handed to whoever sets up the second machine, so a
    /// typo in it would be discovered on their host instead of in CI.
    #[test]
    fn the_documented_cohort_template_is_accepted_by_the_real_loader() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../docs/operations/row-cohort.example.json");
        let config = RowCohortConfig::load(&path)
            .unwrap_or_else(|error| panic!("{}: {error}", path.display()));
        assert_eq!(config.workers.len(), 1);
        let worker = &config.workers[0];
        assert_eq!(worker.id, "machine-b");
        assert_eq!(worker.max_concurrency, 1);
        // The remote command must name the worker binary and pin the artifact
        // the chain activated, or the machine would serve rows of some other
        // model.
        assert!(
            worker
                .remote_command
                .iter()
                .any(|part| part.ends_with("tensor_row_model_worker")),
            "{:?}",
            worker.remote_command
        );
        let artifact = worker
            .remote_command
            .iter()
            .position(|part| part == "--artifact")
            .and_then(|at| worker.remote_command.get(at + 1))
            .expect("the template pins --artifact");
        assert_eq!(artifact.len(), 64, "the artifact pin is a 32-byte hex hash");
        assert!(artifact.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn a_cohort_config_is_parsed_strictly_and_validated() {
        let json = serde_json::json!({
            "workers": [{
                "id": "rack-1", "ssh_program": "/usr/bin/ssh", "target": "arc@10.0.0.11",
                "known_hosts": "/etc/arc/known_hosts",
                "remote_command": ["/opt/arc/tensor_row_model_worker", "--model", "/data/m.gguf"],
                "timeout_ms": 5000, "ram_headroom_bytes": 17179869184u64, "max_concurrency": 2
            }]
        });
        let parsed: RowCohortConfig = serde_json::from_value(json.clone()).unwrap();
        parsed.validate().unwrap();
        assert_eq!(parsed.max_workers, 4);
        assert_eq!(parsed.duplicate_per_mille, 50);
        let mut unknown = json;
        unknown["workers"][0]["password"] = serde_json::json!("x");
        assert!(serde_json::from_value::<RowCohortConfig>(unknown).is_err());

        assert!(config(vec![]).validate().is_err());
        assert!(config(vec![entry("a"), entry("a")]).validate().is_err());
        let mut bad = entry("b");
        bad.target = "-oProxyCommand=x".into();
        assert!(config(vec![bad]).validate().is_err());
        let mut bad = entry("c");
        bad.max_concurrency = MAX_SLOTS_PER_WORKER + 1;
        assert!(config(vec![bad]).validate().is_err());
        let mut bad = entry("d");
        bad.timeout_ms = 0;
        assert!(config(vec![bad]).validate().is_err());
        let mut over = config(vec![entry("e")]);
        over.duplicate_per_mille = 1_001;
        assert!(over.validate().is_err());
        let mut over = config(vec![entry("f")]);
        over.spot_rows_per_stage = MAX_SPOT_ROWS + 1;
        assert!(over.validate().is_err());
        let mut bad = entry("g");
        bad.startup_timeout_ms = 0;
        assert!(config(vec![bad]).validate().is_err());
        // A known_hosts path ssh would split, expand or read relative to a
        // directory is refused at load, not at the first connect.
        for path in [
            "/etc/arc/known hosts",
            "known_hosts",
            "/home/${USER}/known_hosts",
            "~/.ssh/pin",
        ] {
            let mut bad = entry("h");
            bad.known_hosts = path.into();
            assert!(config(vec![bad]).validate().is_err(), "{path}");
        }
    }

    #[test]
    fn probe_and_challenge_use_a_declared_nonzero_resident_layer() {
        assert_eq!(probe_layer_for_residency(&[], 16).unwrap(), 0);
        // Choose the lowest resident layer independent of declaration order.
        assert_eq!(
            probe_layer_for_residency(&[(8, 12), (4, 6)], 16).unwrap(),
            4
        );

        let material = ChallengeMaterial {
            layer: 7,
            tensor: TensorKey::WGate,
            rows: 2,
            row_start: 0,
            input: vec![0, 1],
            expected: vec![0, 1],
        };
        let request = material
            .request(
                "canonical-profile",
                arc_crypto::hash_bytes(b"artifact"),
                "rack-7",
                9,
            )
            .unwrap();
        assert_eq!(request.assignment.layer, Some(7));
        assert_eq!(request.assignment.execution_profile, "canonical-profile");
        assert_eq!(request.assignment.worker_id, "rack-7");
    }

    #[test]
    fn model_residency_validation_rejects_missing_or_invalid_ranges() {
        assert!(probe_layer_for_residency(&[], 0).is_err());
        assert!(probe_layer_for_residency(&[(3, 3)], 8).is_err());
        assert!(probe_layer_for_residency(&[(6, 4)], 8).is_err());
        assert!(probe_layer_for_residency(&[(4, 9)], 8).is_err());

        let mut worker = entry("partial");
        worker.resident_layers = vec![(4, 8)];
        assert!(config(vec![worker.clone()]).validate_for_model(7).is_err());
        assert!(config(vec![worker]).validate_for_model(8).is_ok());
    }

    #[test]
    fn only_a_loopback_host_header_is_the_operators_own() {
        for host in [
            "127.0.0.1:9090",
            "localhost:9090",
            "LOCALHOST",
            "[::1]:9090",
            "127.3.4.5",
        ] {
            assert!(host_header_is_loopback(host), "{host}");
        }
        for host in [
            "attacker.example:9090",
            "10.0.0.1:9090",
            "[2001:db8::1]:9090",
            "",
            "127.0.0.1.nip.io",
            "::1:9090",
            "[::1",
            "[::1]junk",
            "127.0.0.1:junk",
            "[localhost]:9090",
            "localhost.",
        ] {
            assert!(!host_header_is_loopback(host), "{host}");
        }
    }

    #[test]
    fn machines_are_distinct_per_validator_id_and_target() {
        let validator = arc_crypto::hash_bytes(b"validator-1");
        let other = arc_crypto::hash_bytes(b"validator-2");
        let a = entry("rack-1");
        assert_eq!(
            machine_address(&validator, &a),
            machine_address(&validator, &a)
        );
        assert_ne!(machine_address(&validator, &a), machine_address(&other, &a));
        assert_ne!(
            machine_address(&validator, &a),
            machine_address(&validator, &entry("rack-2"))
        );
        let mut moved = a.clone();
        moved.target = "arc@elsewhere".into();
        assert_ne!(
            machine_address(&validator, &a),
            machine_address(&validator, &moved)
        );
    }

    fn link() -> LinkMeasurement {
        LinkMeasurement {
            samples: 32,
            rtt_median_us: 50,
            rtt_p95_us: 50,
            jitter_us: 0,
            bandwidth_bps: 10_000_000_000,
            failure_per_mille: 0,
            measured_at: 100,
            simulated: false,
        }
    }

    #[test]
    fn a_certificate_maps_to_backend_plans_with_its_checks() {
        let machine = arc_crypto::hash_bytes(b"machine-1");
        let keys = vec![(Some(0), TensorKey::Wq), (None, TensorKey::LmHead)];
        let stages = vec![
            Stage {
                layer: Some(0),
                tensor: "wq".into(),
                rows: 4096,
                cols: 4096,
            },
            Stage {
                layer: None,
                tensor: "lm_head".into(),
                rows: 32000,
                cols: 4096,
            },
        ];
        let candidate = Candidate {
            worker: machine,
            transport_id: "rack-1".into(),
            macs_per_s: 8_000_000_000,
            ram_headroom_bytes: 64 << 30,
            max_concurrency: 2,
            link: link(),
            resident_layers: vec![],
            resident_output: false,
        };
        let policy = Policy {
            max_workers: 4,
            max_link_age: 50,
            now: 110,
            max_failure_per_mille: 20,
            allow_simulated_links: false,
            input_element_bytes: 8,
            output_element_bytes: 8,
            weight_bytes_per_element: 1,
            include_coordinator: true,
        };
        let rule = VerificationRule {
            duplicate_per_mille: 1_000,
            spot_rows_per_stage: 2,
        };
        let certificate = AssignmentCertificate::issue(
            arc_crypto::hash_bytes(b"request"),
            arc_crypto::hash_bytes(b"artifact"),
            "profile",
            0,
            stages,
            1_000_000_000,
            vec![candidate],
            vec![arc_crypto::hash_bytes(b"entry")],
            policy,
            rule,
        )
        .unwrap();
        assert_eq!(
            certificate.placement.workers,
            [machine],
            "a fast LAN machine is used"
        );
        let id_of: BTreeMap<[u8; 32], String> = [(machine.0, "rack-1".to_string())].into();
        let plans = plans_from(&certificate, &keys, &id_of).unwrap();
        assert_eq!(plans.len(), 2);
        for (key, plan) in &plans {
            let rows = if *key == (None, TensorKey::LmHead) {
                32000
            } else {
                4096
            };
            assert_eq!(plan.slices.first().unwrap().row_start, 0);
            assert_eq!(plan.slices.last().unwrap().row_end, rows);
            assert_eq!(plan.spot_rows.len(), 2);
            for slice in &plan.slices {
                if slice.owner == SliceOwner::Remote("rack-1".into()) {
                    // Every machine slice is duplicated at 1000 per mille,
                    // by this node when no other machine is placed.
                    assert_eq!(slice.duplicate_on, Some(SliceOwner::Local));
                }
            }
        }
        // A placement that names a machine the cohort does not list is refused.
        assert!(plans_from(&certificate, &keys, &BTreeMap::new()).is_err());
        // So is one that names more stages than the model has.
        assert!(plans_from(&certificate, &keys[..1], &id_of).is_err());
    }

    #[test]
    fn a_machine_holding_one_layer_is_only_planned_for_that_layer() {
        let machine = arc_crypto::hash_bytes(b"machine-1");
        let keys = vec![(Some(0usize), TensorKey::Wq), (Some(1usize), TensorKey::Wq)];
        let stages: Vec<Stage> = (0..2u32)
            .map(|layer| Stage {
                layer: Some(layer),
                tensor: "wq".into(),
                rows: 4096,
                cols: 4096,
            })
            .collect();
        // The machine declares, in its signed offer, that it holds layer 0
        // only - it never loaded layer 1 and cannot serve a row of it.
        let candidate = Candidate {
            worker: machine,
            transport_id: "rack-1".into(),
            macs_per_s: 8_000_000_000,
            ram_headroom_bytes: 64 << 30,
            max_concurrency: 2,
            link: link(),
            resident_layers: vec![(0, 1)],
            resident_output: false,
        };
        let policy = Policy {
            max_workers: 4,
            max_link_age: 50,
            now: 110,
            max_failure_per_mille: 20,
            allow_simulated_links: false,
            input_element_bytes: 8,
            output_element_bytes: 8,
            weight_bytes_per_element: 1,
            include_coordinator: true,
        };
        let rule = VerificationRule {
            duplicate_per_mille: 1_000,
            spot_rows_per_stage: 2,
        };
        let certificate = AssignmentCertificate::issue(
            arc_crypto::hash_bytes(b"request"),
            arc_crypto::hash_bytes(b"artifact"),
            "profile",
            0,
            stages,
            1_000_000_000,
            vec![candidate],
            vec![arc_crypto::hash_bytes(b"entry")],
            policy,
            rule,
        )
        .unwrap();
        let id_of: BTreeMap<[u8; 32], String> = [(machine.0, "rack-1".to_string())].into();
        let plans = plans_from(&certificate, &keys, &id_of).unwrap();
        let remote = SliceOwner::Remote("rack-1".into());
        let rows_for = |key: (Option<usize>, TensorKey)| -> usize {
            plans
                .iter()
                .find(|entry| *entry.0 == key)
                .expect("a plan per key")
                .1
                .slices
                .iter()
                .filter(|s| s.owner == remote)
                .map(|s| s.row_end - s.row_start)
                .sum()
        };
        assert!(
            rows_for((Some(0usize), TensorKey::Wq)) > 0,
            "the machine must serve the layer it holds"
        );
        assert_eq!(
            rows_for((Some(1usize), TensorKey::Wq)),
            0,
            "and must never be planned a row of the layer it does not"
        );
        // This node still covers every row of both layers.
        for plan in plans.values() {
            assert_eq!(plan.slices.first().unwrap().row_start, 0);
            assert_eq!(plan.slices.last().unwrap().row_end, 4096);
        }
    }

    #[test]
    fn declared_layer_residency_is_parsed_and_an_empty_range_is_refused() {
        let base = serde_json::json!({
            "workers": [{
                "id": "rack-1", "ssh_program": "/usr/bin/ssh", "target": "arc@10.0.0.11",
                "known_hosts": "/etc/arc/known_hosts",
                "remote_command": ["/opt/arc/tensor_row_model_worker", "--model", "/data/m.gguf"],
                "timeout_ms": 5000, "ram_headroom_bytes": 17179869184u64, "max_concurrency": 2,
                "resident_layers": [[0, 6], [11, 16]]
            }]
        });
        let parsed: RowCohortConfig = serde_json::from_value(base.clone()).unwrap();
        parsed.validate().unwrap();
        assert_eq!(parsed.workers[0].resident_layers, vec![(0, 6), (11, 16)]);
        // A half-open range that holds nothing is a configuration mistake,
        // not a machine that silently serves no rows.
        let mut empty = base;
        empty["workers"][0]["resident_layers"] = serde_json::json!([[3, 3]]);
        let parsed: RowCohortConfig = serde_json::from_value(empty).unwrap();
        assert!(parsed.validate().is_err());
    }
    // Synthetic row arithmetic, deliberately not a qualified model. These
    // tests prove residency, verification and refusal at the real backend seam.
    struct TinyRowSource {
        config: ModelConfig,
        checks: Mutex<Vec<(Option<usize>, TensorKey, usize, usize)>>,
    }
    impl TinyRowSource {
        fn new() -> Self {
            Self {
                config: ModelConfig {
                    n_layers: 1, d_model: 2, n_heads: 1, n_kv_heads: 1,
                    d_ff: 4, d_head: 2, d_kv: 2, vocab_size: 6, attn_scale: 1,
                    rope_cos: vec![], rope_sin: vec![], max_seq: 8, eos_tokens: vec![],
                    bos_token: 1, chat_template: String::new(),
                    arithmetic_profile: arc_inference::cached_integer_model::ArithmeticProfile::GgufInterleavedRowsV1,
                }, checks: Mutex::new(vec![]),
            }
        }
        fn rows(start: usize, end: usize, input: &[i64]) -> Vec<i64> {
            let sum: i64 = input.iter().sum();
            (start..end).map(|row| (row as i64 + 1) * sum).collect()
        }
    }
    impl CanonicalRowSource for TinyRowSource {
        fn config(&self) -> &ModelConfig {
            &self.config
        }
        fn canonical_execution_profile(&self) -> Option<&'static str> {
            Some("synthetic-partial-row-fixture")
        }
        fn projection_shape(
            &self,
            layer: Option<usize>,
            tensor: TensorKey,
        ) -> Option<(usize, usize)> {
            if (layer == Some(0) && tensor != TensorKey::LmHead)
                || (layer.is_none() && tensor == TensorKey::LmHead)
            {
                Some((6, if tensor == TensorKey::WDown { 4 } else { 2 }))
            } else {
                None
            }
        }
        fn projection_rows(
            &self,
            layer: Option<usize>,
            tensor: TensorKey,
            start: usize,
            end: usize,
            input: &[i64],
        ) -> Result<Vec<i64>, TensorParallelError> {
            let (rows, cols) = self
                .projection_shape(layer, tensor)
                .ok_or(TensorParallelError::WrongShape)?;
            if start >= end || end > rows || input.len() != cols {
                return Err(TensorParallelError::WrongShape);
            }
            self.checks.lock().push((layer, tensor, start, end));
            Ok(Self::rows(start, end, input))
        }
    }
    struct RangeWorker {
        id: String,
        start: usize,
        end: usize,
        open: std::sync::atomic::AtomicBool,
        corrupt: std::sync::atomic::AtomicBool,
        calls: Mutex<Vec<RowAssignment>>,
    }
    impl RowWorker for RangeWorker {
        fn project(
            &self,
            request: RowProjectionRequest,
        ) -> Result<arc_inference::tensor_parallel::RowProjectionResponse, TensorParallelError>
        {
            if !self.is_open() {
                return Err(TensorParallelError::Closed);
            }
            let a = &request.assignment;
            if a.worker_id != self.id
                || a.artifact_id != Hash256([9; 32])
                || a.execution_profile != "synthetic-partial-row-fixture"
                || a.row_start < self.start
                || a.row_end > self.end
                || a.row_start >= a.row_end
                || request.input_hash != hash_i64(&request.input)
            {
                return Err(TensorParallelError::WrongIdentity);
            }
            self.calls.lock().push(a.clone());
            let mut values = TinyRowSource::rows(a.row_start, a.row_end, &request.input);
            if self.corrupt.load(Ordering::Relaxed) {
                values[0] += 1;
            }
            Ok(arc_inference::tensor_parallel::RowProjectionResponse {
                call_id: request.call_id,
                input_hash: request.input_hash,
                assignment: request.assignment,
                values,
            })
        }
        fn is_open(&self) -> bool {
            self.open.load(Ordering::Relaxed)
        }
    }
    #[derive(Default)]
    struct PartialEvents(Mutex<Vec<RowEvent>>);
    impl RowEventSink for PartialEvents {
        fn record(&self, event: RowEvent) {
            self.0.lock().push(event);
        }
        fn is_excluded(&self, _: &str) -> bool {
            false
        }
    }
    fn partial_fixture(
        source: &TinyRowSource,
    ) -> (
        CohortAssignment,
        Vec<Arc<RangeWorker>>,
        BTreeMap<[u8; 32], String>,
    ) {
        let (_, stages) = projection_stages(source);
        let mut cfg = config(vec![entry("owner-1"), entry("owner-2")]);
        cfg.duplicate_per_mille = 1000;
        let (policy, verification) = cfg.placement_policy(true, 110);
        let mut owners = Vec::new();
        let mut candidates = Vec::new();
        let mut workers = Vec::new();
        let mut ids = BTreeMap::new();
        for i in 0..2 {
            let address = Hash256([i as u8 + 1; 32]);
            let id = format!("owner-{}", i + 1);
            let (start, end) = (i * 3, (i + 1) * 3);
            ids.insert(address.0, id.clone());
            owners.push(WorkerResidency {
                worker: address,
                manifest_hash: Hash256([i as u8 + 11; 32]),
                ranges: (0..stages.len())
                    .map(|stage| arc_assign::resident::ResidentRange {
                        stage,
                        row_start: start as u64,
                        row_end: end as u64,
                    })
                    .collect(),
            });
            candidates.push(Candidate {
                worker: address,
                transport_id: id.clone(),
                macs_per_s: 1000,
                ram_headroom_bytes: 4096,
                max_concurrency: 1,
                link: link(),
                resident_layers: vec![],
                resident_output: false,
            });
            workers.push(Arc::new(RangeWorker {
                id,
                start,
                end,
                open: std::sync::atomic::AtomicBool::new(true),
                corrupt: std::sync::atomic::AtomicBool::new(false),
                calls: Mutex::new(vec![]),
            }));
        }
        let assignment = AssignmentCertificate {
            version: 3,
            request_id: Hash256([8; 32]),
            artifact_id: Hash256([9; 32]),
            execution_profile: source.canonical_execution_profile().unwrap().into(),
            epoch: 0,
            stages,
            coordinator_macs_per_s: 0,
            candidates,
            lease_digests: vec![],
            policy,
            verification,
            placement: arc_assign::placement::Placement {
                workers: vec![],
                stages: vec![],
                predicted_token_us: 0,
                coordinator_only_token_us: 0,
            },
        };
        (
            CohortAssignment::Resident(ResidentCertificate::issue(assignment, owners).unwrap()),
            workers,
            ids,
        )
    }

    #[test]
    fn partial_resident_plans_execute_disjoint_rows_and_refuse_missing_or_faulty_owners() {
        use arc_inference::tensor_parallel::ProjectionBackend;
        let source = TinyRowSource::new();
        let (certificate, workers, ids) = partial_fixture(&source);
        let (keys, _) = projection_stages(&source);
        let plans = certificate.plans(&keys, &ids).unwrap();
        let open: BTreeMap<String, Arc<dyn RowWorker>> = workers
            .iter()
            .map(|worker| (worker.id.clone(), worker.clone() as Arc<dyn RowWorker>))
            .collect();
        let events = PartialEvents::default();
        let backend = VerifiedPartitionBackend::new_strict(
            &source,
            Hash256([9; 32]),
            plans.clone(),
            open.clone(),
            &events,
        )
        .unwrap();
        for &(layer, tensor) in &keys {
            let (rows, cols) = source.projection_shape(layer, tensor).unwrap();
            let input = vec![1; cols];
            assert_eq!(
                backend
                    .project_rows(Hash256([8; 32]), layer, tensor, &input, rows)
                    .unwrap(),
                TinyRowSource::rows(0, rows, &input)
            );
        }
        for worker in &workers {
            let calls = worker.calls.lock();
            assert_eq!(
                calls.len(),
                keys.len(),
                "one primary call per owner per projection"
            );
            assert!(
                calls
                    .iter()
                    .all(|a| a.row_start == worker.start && a.row_end == worker.end)
            );
            assert!(
                calls
                    .iter()
                    .any(|a| a.layer.is_none() && a.tensor == TensorKey::LmHead)
            );
        }
        assert!(
            events
                .0
                .lock()
                .iter()
                .all(|event| matches!(event, RowEvent::Answered { .. }))
        );
        assert!(
            !source.checks.lock().is_empty(),
            "canonical verification is real work"
        );
        assert!(
            source
                .checks
                .lock()
                .iter()
                .all(|(_, _, start, end)| end - start <= 3)
        );
        let mut missing = open.clone();
        missing.remove("owner-2");
        assert!(
            VerifiedPartitionBackend::new_strict(
                &source,
                Hash256([9; 32]),
                plans.clone(),
                missing,
                &events
            )
            .is_err()
        );
        workers[1].open.store(false, Ordering::Relaxed);
        assert!(
            backend
                .project_rows(Hash256([8; 32]), Some(0), TensorKey::Wq, &[1, 1], 6)
                .is_err(),
            "loss after preflight cannot cause local fallback"
        );
        workers[1].open.store(true, Ordering::Relaxed);
        workers[1].corrupt.store(true, Ordering::Relaxed);
        assert!(
            backend
                .project_rows(Hash256([8; 32]), Some(0), TensorKey::Wq, &[1, 1], 6)
                .is_err(),
            "canonical duplicate mismatch refuses"
        );
    }

    #[test]
    fn partial_warmup_and_challenges_use_owned_nonzero_rows() {
        let source = TinyRowSource::new();
        let (certificate, workers, _) = partial_fixture(&source);
        let CohortAssignment::Resident(certificate) = certificate else {
            unreachable!()
        };
        let (keys, _) = projection_stages(&source);
        let probe = probe_layout(&source, 0, Some(&certificate.residency[1]), &keys).unwrap();
        assert_eq!(
            (
                probe.narrow_row,
                probe.wide_row,
                probe.challenge_start,
                probe.challenge_end
            ),
            (3, 3, 3, 6)
        );
        for (tensor, row) in [
            (TensorKey::Wq, probe.narrow_row),
            (TensorKey::WDown, probe.wide_row),
        ] {
            let request = zero_request(
                &source,
                Hash256([9; 32]),
                "owner-2",
                0,
                tensor,
                row,
                b"warmup",
            )
            .unwrap();
            assert_eq!(workers[1].project(request).unwrap().values, vec![0]);
        }
        let material =
            challenge_material_range(&source, 0, probe.challenge_start, probe.challenge_end, 110)
                .unwrap();
        let request = material
            .request(
                source.canonical_execution_profile().unwrap(),
                Hash256([9; 32]),
                "owner-2",
                110,
            )
            .unwrap();
        assert_eq!(
            workers[1].project(request).unwrap().values,
            material.expected
        );
        assert!(
            workers[1]
                .project(
                    zero_request(
                        &source,
                        Hash256([9; 32]),
                        "owner-2",
                        0,
                        TensorKey::Wq,
                        0,
                        b"wrong"
                    )
                    .unwrap()
                )
                .is_err()
        );
        let other = probe_layout(&source, 0, Some(&certificate.residency[0]), &keys).unwrap();
        assert_ne!(
            (probe.layer, probe.challenge_start, probe.challenge_end),
            (other.layer, other.challenge_start, other.challenge_end),
            "challenge cache must distinguish disjoint ranges on the same layer"
        );
    }

    #[test]
    fn legacy_config_identity_is_unchanged_and_partial_mode_requires_explicit_policy() {
        assert_eq!(
            arc_assign::resident::MAX_WORKERS,
            arc_inference::tensor_parallel::MAX_ROW_WORKERS_PER_STAGE
        );
        assert_eq!(MAX_COHORT_WORKERS, arc_assign::resident::MAX_WORKERS);
        let mut cfg = config(vec![entry("owner-1"), entry("owner-2")]);
        let (policy, rule) = cfg.placement_policy(true, 10);
        assert_eq!(
            cfg.assignment_hash(true),
            arc_assign::certificate::policy_hash_v2(&policy, &rule)
        );
        assert!(
            serde_json::to_value(&cfg)
                .unwrap()
                .get("partial_rows")
                .is_none()
        );
        cfg.partial_rows = Some(crate::row_residency::PartialRowConfig {
            format: "arc.private-row-residency.v1".into(),
            manifests: (1..=2)
                .map(|i| crate::row_residency::ManifestPin {
                    worker_id: format!("owner-{i}"),
                    path: PathBuf::from(format!("/manifests/owner-{i}.json")),
                    blake3: Hash256([i; 32]),
                })
                .collect(),
        });
        cfg.validate().unwrap();
        assert_ne!(
            cfg.assignment_hash(true),
            arc_assign::certificate::policy_hash_v2(&policy, &rule)
        );
        assert!(crate::native_inference::runtime_assignment_hash(Some(&cfg), false).is_err());
        assert_eq!(
            crate::native_inference::runtime_assignment_hash(Some(&cfg), true).unwrap(),
            cfg.assignment_hash(true)
        );
        cfg.max_workers = 1;
        assert!(
            cfg.validate().is_err(),
            "fixed ownership requires every configured owner"
        );
    }
}
