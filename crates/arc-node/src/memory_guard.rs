//! Memory headroom guard for the community worker (ARC-410 L7, design section A).
//!
//! Off unless the operator turns it on with explicit values (`--memory-guard` and its
//! `--memory-guard-*` settings in `main.rs`). **A disabled or unconfigured guard protects
//! nothing:** the worker then claims and computes exactly as it did before this module.
//!
//! What it does when on, after the model is loaded (the model load itself is not covered;
//! that is the deferred launch gate):
//! * before each claim round, it pauses claiming while available memory is below the
//!   configured headroom, and resumes only after it has been above headroom + hysteresis
//!   for the configured number of consecutive reads;
//! * at compute admission (after the prompt is tokenized and the context preflight passed),
//!   it declines a job whose KV cache + working memory + headroom does not fit in available
//!   memory, through the worker's existing declined result: nothing is computed and the job
//!   is never retried here;
//! * optionally, after N consecutive memory declines it also pauses claims for a while.
//!
//! Memory is read fresh for every decision by [`OsMemoryProbe`], which calls the OS
//! directly and reports a failed call as [`ProbeOutcome::Failed`], never as a low reading.
//! All requirement arithmetic is checked; a requirement that does not fit in a `u64` is
//! declined, never saturated. No figure here is a measured footprint or a RAM requirement.

use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

/// One memory reading.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MemoryReading {
    /// Bytes the OS reports as available for new allocations.
    pub available: u64,
    /// Total physical memory in bytes.
    pub total: u64,
    /// Where `available` came from (for example `meminfo_available`).
    pub source: &'static str,
    /// Linux only: this process's cgroup memory limit minus its usage, or `None` when the
    /// cgroup has no limit ("max") or there is no memory cgroup.
    pub cgroup_headroom: Option<u64>,
}

impl MemoryReading {
    /// The figure decisions use: the smaller of host available memory and cgroup headroom.
    pub fn effective_available(&self) -> u64 {
        match self.cgroup_headroom {
            Some(headroom) => self.available.min(headroom),
            None => self.available,
        }
    }
}

/// The result of one probe call.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ProbeOutcome {
    Reading(MemoryReading),
    /// The OS call failed or returned something unusable. Never a low reading.
    Failed(String),
}

/// Reads memory. The production implementation is [`OsMemoryProbe`]; tests inject fakes.
pub trait MemoryProbe: Send + Sync {
    fn read(&self) -> ProbeOutcome;
}

// ---------------------------------------------------------------------------------------
// Requirement accounting (pure, checked)
// ---------------------------------------------------------------------------------------

/// The model dimensions the requirement depends on, from the loaded model's config.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ModelDims {
    pub n_layers: usize,
    pub d_model: usize,
    pub d_kv: usize,
    pub d_ff: usize,
    pub vocab_size: usize,
}

impl ModelDims {
    pub fn from_config(config: &arc_inference::cached_integer_model::ModelConfig) -> Self {
        Self {
            n_layers: config.n_layers,
            d_model: config.d_model,
            d_kv: config.d_kv,
            d_ff: config.d_ff,
            vocab_size: config.vocab_size,
        }
    }
}

fn u(value: usize) -> Option<u64> {
    u64::try_from(value).ok()
}

/// KV-cache bytes for `positions` positions: every position holds one K and one V row of
/// `d_kv` i64 values in every layer (`KVCache` in `cached_integer_model.rs`). With the cache
/// reserved up front for exactly these positions, this is the cache's size.
pub fn kv_bytes(dims: &ModelDims, positions: usize) -> Option<u64> {
    u(positions)?
        .checked_mul(u(dims.n_layers)?)?
        .checked_mul(u(dims.d_kv)?)?
        .checked_mul(2)?
        .checked_mul(8)
}

/// Working memory of one token-at-a-time job (`try_generate`, the community worker's path),
/// from `forward_one_token_observed`'s own allocations in `cached_integer_model.rs`:
/// * the per-call buffers `hidden, q, k_buf, v_buf, attn_out, projected, gate, up, ff_out`:
///   `(6·d_model + 2·d_kv + 2·d_ff)` i64;
/// * per layer, alive together at most: `normed` and the attention `head_results`
///   (`n_heads × d_head` = `d_model` i64, each head's `out` collected into it), then
///   `normed_ff`: charged together as `3·d_model` i64 (an overestimate: the two phases do
///   not overlap);
/// * the `QuantizedInput`s of `normed`, `attn_out` and `normed_ff` (i8 each), and the FFN
///   down projection's input: charged as `2·d_model + d_ff` bytes;
/// * the final `normed` (`d_model` i64) and the logits (`vocab_size` i64);
/// * the generated tokens and their hash input: `max_tokens × 8` bytes.
pub fn token_path_workspace_bytes(dims: &ModelDims, max_tokens: u32) -> Option<u64> {
    let d = u(dims.d_model)?;
    let kv = u(dims.d_kv)?;
    let ff = u(dims.d_ff)?;
    let buffers = d
        .checked_mul(6)?
        .checked_add(kv.checked_mul(2)?)?
        .checked_add(ff.checked_mul(2)?)?;
    let per_layer = d.checked_mul(3)?;
    let final_and_logits = d.checked_add(u(dims.vocab_size)?)?;
    let i64_elements = buffers
        .checked_add(per_layer)?
        .checked_add(final_and_logits)?;
    let quantized_bytes = d.checked_mul(2)?.checked_add(ff)?;
    let outputs = u64::from(max_tokens).checked_mul(8)?;
    i64_elements
        .checked_mul(8)?
        .checked_add(quantized_bytes)?
        .checked_add(outputs)
}

/// KV cache + working memory of one token-at-a-time job. `None` when it does not fit in a
/// `u64` (declined as "not representable", never saturated).
pub fn job_requirement_bytes(
    dims: &ModelDims,
    required_positions: usize,
    max_tokens: u32,
) -> Option<u64> {
    kv_bytes(dims, required_positions)?.checked_add(token_path_workspace_bytes(dims, max_tokens)?)
}

// ---------------------------------------------------------------------------------------
// Policy and guard
// ---------------------------------------------------------------------------------------

/// What to do when the probe fails. No default: the operator chooses.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProbeFailurePolicy {
    /// Claim and compute as if the guard were off (no protection while the probe fails).
    Proceed,
    /// Pause claims, and decline at admission, until the probe works again.
    Pause,
}

/// Every value the guard uses. All are supplied by the operator; none has a built-in default,
/// because no production value has been decided or measured.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MemoryGuardPolicy {
    /// Memory to leave free for the OS and other programs, in bytes.
    pub headroom_bytes: u64,
    /// Extra bytes above `headroom_bytes` required before a pause ends.
    pub resume_hysteresis_bytes: u64,
    /// Consecutive reads at or above headroom + hysteresis before a pause ends (at least 1).
    pub resume_consecutive_reads: u32,
    /// How long to wait between reads while paused.
    pub recheck_interval: Duration,
    /// Optional decline back-off: after this many consecutive memory declines, pause claims
    /// for `decline_backoff`. `None` turns the back-off off.
    pub decline_backoff_after: Option<u32>,
    pub decline_backoff: Duration,
    pub on_probe_failure: ProbeFailurePolicy,
}

/// Why claims are paused.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PauseReason {
    LowMemory,
    ProbeFailed,
    DeclineBackoff,
}

/// The guard's answer before a claim round.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ClaimDecision {
    Claim,
    Pause {
        reason: PauseReason,
        retry_after: Duration,
    },
}

/// Why a job was declined at admission.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DeclineKind {
    LowMemory,
    NotRepresentable,
    ProbeFailed,
}

/// The guard's answer at compute admission.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AdmissionDecision {
    Admit,
    Decline { kind: DeclineKind, reason: String },
}

/// What the worker status reports about the guard.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct MemoryGuardStatus {
    /// `ok`, or the reason claims are paused.
    pub state: &'static str,
    /// `ok`, `failed` (the last probe call failed) or `not_read_yet`.
    pub memory_check: &'static str,
    pub last_available_bytes: Option<u64>,
    pub last_total_bytes: Option<u64>,
    pub last_source: Option<&'static str>,
    pub last_cgroup_headroom_bytes: Option<u64>,
    pub headroom_bytes: u64,
    pub paused_rounds: u64,
    pub declined_low_memory: u64,
    pub declined_not_representable: u64,
    pub declined_probe_failed: u64,
    pub probe_failures: u64,
    pub backoffs: u64,
}

#[derive(Default)]
struct GuardState {
    paused: Option<PauseReason>,
    good_reads: u32,
    consecutive_memory_declines: u32,
    backoff_until: Option<Instant>,
    last: Option<MemoryReading>,
    last_probe_failed: bool,
    read_once: bool,
}

/// The guard. One per worker; `before_claim` and `admit` are called from the claim loop.
pub struct MemoryGuard {
    policy: MemoryGuardPolicy,
    probe: Box<dyn MemoryProbe>,
    state: Mutex<GuardState>,
    paused_rounds: AtomicU64,
    declined_low_memory: AtomicU64,
    declined_not_representable: AtomicU64,
    declined_probe_failed: AtomicU64,
    probe_failures: AtomicU64,
    backoffs: AtomicU64,
}

impl std::fmt::Debug for MemoryGuard {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MemoryGuard")
            .field("policy", &self.policy)
            .field("status", &self.status())
            .finish()
    }
}

impl MemoryGuard {
    pub fn new(policy: MemoryGuardPolicy, probe: Box<dyn MemoryProbe>) -> Self {
        let policy = MemoryGuardPolicy {
            resume_consecutive_reads: policy.resume_consecutive_reads.max(1),
            ..policy
        };
        Self {
            policy,
            probe,
            state: Mutex::new(GuardState::default()),
            paused_rounds: AtomicU64::new(0),
            declined_low_memory: AtomicU64::new(0),
            declined_not_representable: AtomicU64::new(0),
            declined_probe_failed: AtomicU64::new(0),
            probe_failures: AtomicU64::new(0),
            backoffs: AtomicU64::new(0),
        }
    }

    pub fn policy(&self) -> &MemoryGuardPolicy {
        &self.policy
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, GuardState> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Read the probe and record the outcome. `None` means the call failed.
    fn read(&self, state: &mut GuardState) -> Option<u64> {
        state.read_once = true;
        match self.probe.read() {
            ProbeOutcome::Reading(reading) => {
                let available = reading.effective_available();
                state.last = Some(reading);
                state.last_probe_failed = false;
                Some(available)
            }
            ProbeOutcome::Failed(reason) => {
                tracing::warn!(%reason, "memory guard: the memory probe failed");
                self.probe_failures.fetch_add(1, Ordering::Relaxed);
                state.last_probe_failed = true;
                None
            }
        }
    }

    fn pause(
        &self,
        state: &mut GuardState,
        reason: PauseReason,
        retry_after: Duration,
    ) -> ClaimDecision {
        state.paused = Some(reason);
        self.paused_rounds.fetch_add(1, Ordering::Relaxed);
        ClaimDecision::Pause {
            reason,
            retry_after,
        }
    }

    /// Before a claim round: claim, or pause for `retry_after`.
    pub fn before_claim(&self, now: Instant) -> ClaimDecision {
        let mut state = self.lock();
        if let Some(until) = state.backoff_until {
            if now < until {
                return self.pause(&mut state, PauseReason::DeclineBackoff, until - now);
            }
            state.backoff_until = None;
            if state.paused == Some(PauseReason::DeclineBackoff) {
                state.paused = None;
            }
        }
        let recheck = self.policy.recheck_interval;
        let Some(available) = self.read(&mut state) else {
            state.good_reads = 0;
            return match self.policy.on_probe_failure {
                // As if the guard were off while the probe fails: no protection.
                ProbeFailurePolicy::Proceed => {
                    state.paused = None;
                    ClaimDecision::Claim
                }
                ProbeFailurePolicy::Pause => {
                    self.pause(&mut state, PauseReason::ProbeFailed, recheck)
                }
            };
        };
        if state.paused == Some(PauseReason::LowMemory) {
            // Resume only after enough consecutive reads at or above headroom + hysteresis.
            // A threshold that does not fit in a u64 can never be met.
            let resume_at = self
                .policy
                .headroom_bytes
                .checked_add(self.policy.resume_hysteresis_bytes);
            if resume_at.is_some_and(|threshold| available >= threshold) {
                state.good_reads = state.good_reads.saturating_add(1);
                if state.good_reads >= self.policy.resume_consecutive_reads {
                    state.paused = None;
                    state.good_reads = 0;
                    return ClaimDecision::Claim;
                }
            } else {
                state.good_reads = 0;
            }
            return self.pause(&mut state, PauseReason::LowMemory, recheck);
        }
        // Not paused, or paused only because the probe failed and it works again.
        state.paused = None;
        if available < self.policy.headroom_bytes {
            state.good_reads = 0;
            return self.pause(&mut state, PauseReason::LowMemory, recheck);
        }
        ClaimDecision::Claim
    }

    /// At compute admission, after the context preflight: admit, or decline with a reason.
    /// `requirement` is [`job_requirement_bytes`] (`None` when it was not representable).
    pub fn admit(&self, requirement: Option<u64>, now: Instant) -> AdmissionDecision {
        let mut state = self.lock();
        let Some(needed) =
            requirement.and_then(|bytes| bytes.checked_add(self.policy.headroom_bytes))
        else {
            self.declined_not_representable
                .fetch_add(1, Ordering::Relaxed);
            return AdmissionDecision::Decline {
                kind: DeclineKind::NotRepresentable,
                reason: "memory guard: job memory requirement not representable".into(),
            };
        };
        let Some(available) = self.read(&mut state) else {
            return match self.policy.on_probe_failure {
                ProbeFailurePolicy::Proceed => AdmissionDecision::Admit,
                ProbeFailurePolicy::Pause => {
                    self.declined_probe_failed.fetch_add(1, Ordering::Relaxed);
                    AdmissionDecision::Decline {
                        kind: DeclineKind::ProbeFailed,
                        reason: "memory guard: memory check failed; not computing".into(),
                    }
                }
            };
        };
        if available < needed {
            self.declined_low_memory.fetch_add(1, Ordering::Relaxed);
            state.consecutive_memory_declines = state.consecutive_memory_declines.saturating_add(1);
            if let Some(after) = self.policy.decline_backoff_after
                && after > 0
                && state.consecutive_memory_declines >= after
            {
                state.backoff_until = now.checked_add(self.policy.decline_backoff);
                state.consecutive_memory_declines = 0;
                self.backoffs.fetch_add(1, Ordering::Relaxed);
            }
            return AdmissionDecision::Decline {
                kind: DeclineKind::LowMemory,
                reason: format!(
                    "memory guard: worker memory below headroom for this job (needs {needed} bytes including headroom, has {available})"
                ),
            };
        }
        state.consecutive_memory_declines = 0;
        AdmissionDecision::Admit
    }

    pub fn status(&self) -> MemoryGuardStatus {
        let state = self.lock();
        let paused_state = match state.paused {
            None => "ok",
            Some(PauseReason::LowMemory) => "paused_low_memory",
            Some(PauseReason::ProbeFailed) => "paused_memory_check_failed",
            Some(PauseReason::DeclineBackoff) => "paused_decline_backoff",
        };
        MemoryGuardStatus {
            state: paused_state,
            memory_check: if !state.read_once {
                "not_read_yet"
            } else if state.last_probe_failed {
                "failed"
            } else {
                "ok"
            },
            last_available_bytes: state.last.as_ref().map(|r| r.available),
            last_total_bytes: state.last.as_ref().map(|r| r.total),
            last_source: state.last.as_ref().map(|r| r.source),
            last_cgroup_headroom_bytes: state.last.as_ref().and_then(|r| r.cgroup_headroom),
            headroom_bytes: self.policy.headroom_bytes,
            paused_rounds: self.paused_rounds.load(Ordering::Relaxed),
            declined_low_memory: self.declined_low_memory.load(Ordering::Relaxed),
            declined_not_representable: self.declined_not_representable.load(Ordering::Relaxed),
            declined_probe_failed: self.declined_probe_failed.load(Ordering::Relaxed),
            probe_failures: self.probe_failures.load(Ordering::Relaxed),
            backoffs: self.backoffs.load(Ordering::Relaxed),
        }
    }
}

// ---------------------------------------------------------------------------------------
// OS probe
// ---------------------------------------------------------------------------------------

/// Reads memory straight from the OS, fresh on every call, with explicit failure:
/// * macOS: `host_statistics64(HOST_VM_INFO64)` must return `KERN_SUCCESS` and the
///   `hw.memsize` sysctl must succeed; available = (free + inactive + purgeable −
///   compressor) pages, saturating at 0 (a genuine low reading, not a failure);
/// * Windows: `GlobalMemoryStatusEx` must return TRUE with a non-zero total;
/// * Linux: `/proc/meminfo` must parse (`MemAvailable`, or the pre-3.14 estimate), and the
///   process's memory cgroup (v2 `memory.max`/`memory.current`, or v1) is applied;
/// * anything else: `Failed`.
pub struct OsMemoryProbe;

impl MemoryProbe for OsMemoryProbe {
    fn read(&self) -> ProbeOutcome {
        os::read()
    }
}

/// Parse `/proc/meminfo`: `(available, total, source)`. `MemAvailable` when present,
/// otherwise the pre-3.14 estimate free + buffers + cached + reclaimable slab − shmem.
pub fn parse_meminfo(text: &str) -> Result<(u64, u64, &'static str), String> {
    let mut fields = std::collections::HashMap::new();
    for line in text.lines() {
        let mut parts = line.split_whitespace();
        let (Some(name), Some(value)) = (parts.next(), parts.next()) else {
            continue;
        };
        let Ok(kib) = value.parse::<u64>() else {
            return Err(format!("/proc/meminfo: unparsable value in {line:?}"));
        };
        let bytes = kib
            .checked_mul(1024)
            .ok_or_else(|| format!("/proc/meminfo: {name} overflows"))?;
        fields.insert(name.trim_end_matches(':').to_owned(), bytes);
    }
    let total = *fields.get("MemTotal").ok_or("/proc/meminfo: no MemTotal")?;
    if total == 0 {
        return Err("/proc/meminfo: MemTotal is 0".into());
    }
    if let Some(&available) = fields.get("MemAvailable") {
        return Ok((available, total, "meminfo_available"));
    }
    let get = |name: &str| {
        fields
            .get(name)
            .copied()
            .ok_or_else(|| format!("/proc/meminfo: no {name}"))
    };
    let estimate = get("MemFree")?
        .saturating_add(get("Buffers")?)
        .saturating_add(get("Cached")?)
        .saturating_add(fields.get("SReclaimable").copied().unwrap_or(0))
        .saturating_sub(fields.get("Shmem").copied().unwrap_or(0));
    Ok((estimate, total, "meminfo_estimate_pre_3_14"))
}

/// Where this process's memory cgroup lives, from `/proc/self/cgroup`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CgroupLocation {
    /// cgroup v2: the unified hierarchy path (`0::/path`).
    V2(String),
    /// cgroup v1: the memory controller's path.
    V1(String),
    None,
}

pub fn parse_proc_self_cgroup(text: &str) -> CgroupLocation {
    let mut v2 = None;
    for line in text.lines() {
        let mut parts = line.splitn(3, ':');
        let (Some(id), Some(controllers), Some(path)) = (parts.next(), parts.next(), parts.next())
        else {
            continue;
        };
        if controllers.split(',').any(|c| c == "memory") {
            return CgroupLocation::V1(path.to_owned());
        }
        if id == "0" && controllers.is_empty() {
            v2 = Some(path.to_owned());
        }
    }
    v2.map_or(CgroupLocation::None, CgroupLocation::V2)
}

/// A cgroup memory limit file: `"max"` (v2) means no limit (`None`), never `u64::MAX`.
pub fn parse_cgroup_limit(text: &str) -> Result<Option<u64>, String> {
    let value = text.trim();
    if value == "max" {
        return Ok(None);
    }
    value
        .parse::<u64>()
        .map(Some)
        .map_err(|_| format!("cgroup memory limit: unparsable {value:?}"))
}

pub fn parse_cgroup_usage(text: &str) -> Result<u64, String> {
    let value = text.trim();
    value
        .parse::<u64>()
        .map_err(|_| format!("cgroup memory usage: unparsable {value:?}"))
}

/// Headroom inside a cgroup: `None` for no limit, else limit − usage (0 when over the limit).
pub fn cgroup_headroom(limit: Option<u64>, usage: u64) -> Option<u64> {
    limit.map(|limit| limit.saturating_sub(usage))
}

/// The Linux reading from file contents; `read_file` returns `Ok(None)` for a file that does
/// not exist. Split out so tests can feed any `/proc` and cgroup contents.
pub fn linux_reading(read_file: &dyn Fn(&str) -> std::io::Result<Option<String>>) -> ProbeOutcome {
    let meminfo = match read_file("/proc/meminfo") {
        Ok(Some(text)) => text,
        Ok(None) => return ProbeOutcome::Failed("/proc/meminfo is missing".into()),
        Err(error) => return ProbeOutcome::Failed(format!("/proc/meminfo: {error}")),
    };
    let (available, total, source) = match parse_meminfo(&meminfo) {
        Ok(parsed) => parsed,
        Err(error) => return ProbeOutcome::Failed(error),
    };
    let location = match read_file("/proc/self/cgroup") {
        Ok(Some(text)) => parse_proc_self_cgroup(&text),
        Ok(None) => CgroupLocation::None,
        Err(error) => return ProbeOutcome::Failed(format!("/proc/self/cgroup: {error}")),
    };
    let (limit_path, usage_path) = match &location {
        CgroupLocation::V2(path) => {
            let base = format!("/sys/fs/cgroup{}", path.trim_end_matches('/'));
            (
                format!("{base}/memory.max"),
                format!("{base}/memory.current"),
            )
        }
        CgroupLocation::V1(path) => {
            let base = format!("/sys/fs/cgroup/memory{}", path.trim_end_matches('/'));
            (
                format!("{base}/memory.limit_in_bytes"),
                format!("{base}/memory.usage_in_bytes"),
            )
        }
        CgroupLocation::None => {
            return ProbeOutcome::Reading(MemoryReading {
                available,
                total,
                source,
                cgroup_headroom: None,
            });
        }
    };
    let limit = match read_file(&limit_path) {
        // The controller is listed but its files are not visible here (for example a
        // cgroup namespace without the memory controller mounted): no limit to apply.
        Ok(None) => None,
        Ok(Some(text)) => match parse_cgroup_limit(&text) {
            Ok(limit) => limit,
            Err(error) => return ProbeOutcome::Failed(error),
        },
        Err(error) => return ProbeOutcome::Failed(format!("{limit_path}: {error}")),
    };
    let cgroup_headroom = match limit {
        None => None,
        Some(limit) => match read_file(&usage_path) {
            Ok(Some(text)) => match parse_cgroup_usage(&text) {
                Ok(usage) => cgroup_headroom(Some(limit), usage),
                Err(error) => return ProbeOutcome::Failed(error),
            },
            Ok(None) => {
                return ProbeOutcome::Failed(format!(
                    "{usage_path} is missing while a limit is set"
                ));
            }
            Err(error) => return ProbeOutcome::Failed(format!("{usage_path}: {error}")),
        },
    };
    ProbeOutcome::Reading(MemoryReading {
        available,
        total,
        source,
        cgroup_headroom,
    })
}

/// macOS available bytes from `host_statistics64` page counts, as sysinfo computes it:
/// (free + inactive + purgeable − compressor) pages, saturating at 0.
pub fn macos_available_bytes(
    free: u64,
    inactive: u64,
    purgeable: u64,
    compressor: u64,
    page_size: u64,
) -> u64 {
    free.saturating_add(inactive)
        .saturating_add(purgeable)
        .saturating_sub(compressor)
        .saturating_mul(page_size)
}

/// The macOS reading from the two calls' results; split out so tests can feed both a failed
/// call and a successful call whose counts saturate to 0.
pub fn macos_reading(
    kern_return: i32,
    counts: Option<(u64, u64, u64, u64)>,
    page_size: u64,
    total: Option<u64>,
) -> ProbeOutcome {
    const KERN_SUCCESS: i32 = 0;
    if kern_return != KERN_SUCCESS {
        return ProbeOutcome::Failed(format!("host_statistics64 returned {kern_return}"));
    }
    let Some((free, inactive, purgeable, compressor)) = counts else {
        return ProbeOutcome::Failed("host_statistics64 returned no counts".into());
    };
    let Some(total) = total.filter(|&t| t > 0) else {
        return ProbeOutcome::Failed("sysctl hw.memsize failed".into());
    };
    if page_size == 0 {
        return ProbeOutcome::Failed("page size unavailable".into());
    }
    ProbeOutcome::Reading(MemoryReading {
        available: macos_available_bytes(free, inactive, purgeable, compressor, page_size),
        total,
        source: "host_statistics64",
        cgroup_headroom: None,
    })
}

/// The Windows reading from `GlobalMemoryStatusEx`'s result.
pub fn windows_reading(call_succeeded: bool, total_phys: u64, avail_phys: u64) -> ProbeOutcome {
    if !call_succeeded {
        return ProbeOutcome::Failed("GlobalMemoryStatusEx failed".into());
    }
    if total_phys == 0 {
        return ProbeOutcome::Failed("GlobalMemoryStatusEx reported 0 total memory".into());
    }
    ProbeOutcome::Reading(MemoryReading {
        available: avail_phys,
        total: total_phys,
        source: "GlobalMemoryStatusEx",
        cgroup_headroom: None,
    })
}

#[cfg(target_os = "linux")]
mod os {
    pub fn read() -> super::ProbeOutcome {
        super::linux_reading(&|path| match std::fs::read_to_string(path) {
            Ok(text) => Ok(Some(text)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error),
        })
    }
}

#[cfg(target_os = "macos")]
mod os {
    use std::sync::OnceLock;

    /// The host port, taken once: every `mach_host_self` call adds a send-right reference.
    fn host_port() -> libc::mach_port_t {
        static HOST: OnceLock<libc::mach_port_t> = OnceLock::new();
        // SAFETY: mach_host_self takes no arguments and returns this task's host port.
        #[allow(deprecated)]
        *HOST.get_or_init(|| unsafe { libc::mach_host_self() })
    }

    pub fn read() -> super::ProbeOutcome {
        let mut stat = std::mem::MaybeUninit::<libc::vm_statistics64>::zeroed();
        let mut count = libc::HOST_VM_INFO64_COUNT;
        // SAFETY: `stat` is a zeroed vm_statistics64 and `count` is its size in integer
        // units, as host_statistics64 requires; it writes at most `count` units.
        let kern_return = unsafe {
            libc::host_statistics64(
                host_port(),
                libc::HOST_VM_INFO64,
                stat.as_mut_ptr().cast(),
                &mut count,
            )
        };
        // SAFETY: zero-initialized, and fully written on KERN_SUCCESS; read only then.
        let stat = unsafe { stat.assume_init() };
        let counts = (kern_return == libc::KERN_SUCCESS).then(|| {
            (
                u64::from(stat.free_count),
                u64::from(stat.inactive_count),
                u64::from(stat.purgeable_count),
                u64::from(stat.compressor_page_count),
            )
        });
        // SAFETY: sysconf has no preconditions.
        let page_size = u64::try_from(unsafe { libc::sysconf(libc::_SC_PAGESIZE) }).unwrap_or(0);
        let mut total: u64 = 0;
        let mut size = std::mem::size_of::<u64>();
        // SAFETY: the name is NUL-terminated; `total` is a u64 of `size` bytes, as hw.memsize needs.
        let sysctl = unsafe {
            libc::sysctlbyname(
                c"hw.memsize".as_ptr(),
                (&mut total as *mut u64).cast(),
                &mut size,
                std::ptr::null_mut(),
                0,
            )
        };
        let total = (sysctl == 0 && size == std::mem::size_of::<u64>()).then_some(total);
        super::macos_reading(kern_return, counts, page_size, total)
    }
}

#[cfg(target_os = "windows")]
mod os {
    #[repr(C)]
    #[allow(non_snake_case)]
    struct MEMORYSTATUSEX {
        dwLength: u32,
        dwMemoryLoad: u32,
        ullTotalPhys: u64,
        ullAvailPhys: u64,
        ullTotalPageFile: u64,
        ullAvailPageFile: u64,
        ullTotalVirtual: u64,
        ullAvailVirtual: u64,
        ullAvailExtendedVirtual: u64,
    }

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GlobalMemoryStatusEx(buffer: *mut MEMORYSTATUSEX) -> i32;
    }

    pub fn read() -> super::ProbeOutcome {
        let mut status = MEMORYSTATUSEX {
            dwLength: std::mem::size_of::<MEMORYSTATUSEX>() as u32,
            dwMemoryLoad: 0,
            ullTotalPhys: 0,
            ullAvailPhys: 0,
            ullTotalPageFile: 0,
            ullAvailPageFile: 0,
            ullTotalVirtual: 0,
            ullAvailVirtual: 0,
            ullAvailExtendedVirtual: 0,
        };
        // SAFETY: `status` is a MEMORYSTATUSEX with dwLength set, as the API requires.
        let ok = unsafe { GlobalMemoryStatusEx(&mut status) } != 0;
        super::windows_reading(ok, status.ullTotalPhys, status.ullAvailPhys)
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
mod os {
    pub fn read() -> super::ProbeOutcome {
        super::ProbeOutcome::Failed("no memory probe for this platform".into())
    }
}

#[cfg(test)]
mod tests {
    //! Synthetic: a scripted probe, Instants as a fake clock, made-up model dimensions. No
    //! memory is read from this machine and no model is loaded.
    use super::*;
    use std::collections::VecDeque;
    use std::sync::Arc;
    use std::sync::atomic::AtomicUsize;

    const GIB: u64 = 1 << 30;
    const MIB: u64 = 1 << 20;

    /// Returns scripted outcomes in order (the last one repeats) and counts calls.
    struct ScriptedProbe {
        outcomes: Mutex<VecDeque<ProbeOutcome>>,
        last: Mutex<Option<ProbeOutcome>>,
        calls: Arc<AtomicUsize>,
    }

    impl MemoryProbe for ScriptedProbe {
        fn read(&self) -> ProbeOutcome {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let next = self.outcomes.lock().unwrap().pop_front();
            let mut last = self.last.lock().unwrap();
            if let Some(outcome) = next {
                *last = Some(outcome);
            }
            last.clone().expect("script has at least one outcome")
        }
    }

    fn avail(bytes: u64) -> ProbeOutcome {
        ProbeOutcome::Reading(MemoryReading {
            available: bytes,
            total: 16 * GIB,
            source: "fake",
            cgroup_headroom: None,
        })
    }

    fn failed() -> ProbeOutcome {
        ProbeOutcome::Failed("scripted failure".into())
    }

    fn policy(on_probe_failure: ProbeFailurePolicy) -> MemoryGuardPolicy {
        MemoryGuardPolicy {
            headroom_bytes: 2 * GIB,
            resume_hysteresis_bytes: 512 * MIB,
            resume_consecutive_reads: 2,
            recheck_interval: Duration::from_secs(30),
            decline_backoff_after: None,
            decline_backoff: Duration::ZERO,
            on_probe_failure,
        }
    }

    fn guard(
        policy: MemoryGuardPolicy,
        script: Vec<ProbeOutcome>,
    ) -> (MemoryGuard, Arc<AtomicUsize>) {
        let calls = Arc::new(AtomicUsize::new(0));
        let probe = ScriptedProbe {
            outcomes: Mutex::new(script.into()),
            last: Mutex::new(None),
            calls: calls.clone(),
        };
        (MemoryGuard::new(policy, Box::new(probe)), calls)
    }

    /// Llama-2-7B shape (32 layers, d_model 4096, d_kv 4096, d_ff 11008, vocab 32000).
    const LLAMA7B: ModelDims = ModelDims {
        n_layers: 32,
        d_model: 4096,
        d_kv: 4096,
        d_ff: 11008,
        vocab_size: 32000,
    };

    // ---- accounting --------------------------------------------------------------------

    #[test]
    fn kv_is_two_mib_per_position_for_the_7b_shape_and_follows_other_shapes() {
        assert_eq!(kv_bytes(&LLAMA7B, 1), Some(2 * MIB));
        assert_eq!(kv_bytes(&LLAMA7B, 4096), Some(8 * GIB));
        let gqa = ModelDims {
            n_layers: 28,
            d_kv: 512,
            ..LLAMA7B
        };
        assert_eq!(kv_bytes(&gqa, 1), Some(28 * 512 * 2 * 8));
    }

    #[test]
    fn token_path_workspace_counts_its_own_buffers() {
        let d = 4096u64;
        let kv = 4096u64;
        let ff = 11008u64;
        let i64s = (6 * d + 2 * kv + 2 * ff) + 3 * d + (d + 32000);
        let expected = i64s * 8 + (2 * d + ff) + 256 * 8;
        assert_eq!(token_path_workspace_bytes(&LLAMA7B, 256), Some(expected));
        // Nothing but the KV cache grows with the number of positions.
        assert_eq!(
            job_requirement_bytes(&LLAMA7B, 1001, 256).unwrap()
                - job_requirement_bytes(&LLAMA7B, 1000, 256).unwrap(),
            2 * MIB
        );
    }

    #[test]
    fn requirements_that_do_not_fit_a_u64_are_none_never_saturated() {
        assert_eq!(kv_bytes(&LLAMA7B, usize::MAX), None);
        let huge = ModelDims {
            d_model: usize::MAX,
            ..LLAMA7B
        };
        assert_eq!(token_path_workspace_bytes(&huge, 1), None);
        assert_eq!(job_requirement_bytes(&huge, 1, 1), None);
    }

    // ---- admission (G2b) -------------------------------------------------------------------

    #[test]
    fn admission_boundary_is_exact() {
        let need = job_requirement_bytes(&LLAMA7B, 300, 256).unwrap();
        let now = Instant::now();
        let p = policy(ProbeFailurePolicy::Pause);
        let (g, _) = guard(p.clone(), vec![avail(need + p.headroom_bytes)]);
        assert_eq!(g.admit(Some(need), now), AdmissionDecision::Admit);
        let (g, _) = guard(p.clone(), vec![avail(need + p.headroom_bytes - 1)]);
        match g.admit(Some(need), now) {
            AdmissionDecision::Decline { kind, reason } => {
                assert_eq!(kind, DeclineKind::LowMemory);
                assert!(reason.contains("below headroom for this job"), "{reason}");
            }
            other => panic!("expected a decline, got {other:?}"),
        }
        assert_eq!(g.status().declined_low_memory, 1);
    }

    #[test]
    fn overflow_is_declined_as_not_representable_even_with_unbounded_memory() {
        let now = Instant::now();
        // The requirement itself overflowed.
        let (g, calls) = guard(policy(ProbeFailurePolicy::Proceed), vec![avail(u64::MAX)]);
        assert!(matches!(
            g.admit(None, now),
            AdmissionDecision::Decline { kind: DeclineKind::NotRepresentable, ref reason }
                if reason.contains("job memory requirement not representable")
        ));
        // Requirement + headroom overflows: never compared against u64::MAX available.
        assert!(matches!(
            g.admit(Some(u64::MAX - 1), now),
            AdmissionDecision::Decline {
                kind: DeclineKind::NotRepresentable,
                ..
            }
        ));
        assert_eq!(
            calls.load(Ordering::SeqCst),
            0,
            "no probe read is needed to refuse"
        );
        assert_eq!(g.status().declined_not_representable, 2);
    }

    #[test]
    fn every_decision_reads_memory_fresh() {
        let now = Instant::now();
        let (g, calls) = guard(policy(ProbeFailurePolicy::Pause), vec![avail(10 * GIB)]);
        g.before_claim(now);
        g.admit(Some(GIB), now);
        g.before_claim(now);
        assert_eq!(calls.load(Ordering::SeqCst), 3);
    }

    #[test]
    fn a_later_drop_only_affects_the_next_decision() {
        // Admission is a point-in-time check: memory taken after it (another program, or
        // during compute) is not prevented; the next round sees it.
        let now = Instant::now();
        let (g, _) = guard(
            policy(ProbeFailurePolicy::Pause),
            vec![avail(10 * GIB), avail(GIB)],
        );
        assert_eq!(g.admit(Some(GIB), now), AdmissionDecision::Admit);
        assert!(matches!(
            g.before_claim(now),
            ClaimDecision::Pause {
                reason: PauseReason::LowMemory,
                ..
            }
        ));
    }

    // ---- floor pause (G2a) and hysteresis ---------------------------------------------------

    #[test]
    fn floor_pause_and_resume_need_consecutive_reads_above_headroom_plus_hysteresis() {
        let p = policy(ProbeFailurePolicy::Pause);
        let h = p.headroom_bytes;
        let resume = h + p.resume_hysteresis_bytes;
        let t0 = Instant::now();
        let (g, _) = guard(
            p,
            vec![
                avail(h),
                avail(h - 1),
                avail(resume - 1),
                avail(resume),
                avail(resume - 1),
                avail(resume),
                avail(resume),
            ],
        );
        assert_eq!(
            g.before_claim(t0),
            ClaimDecision::Claim,
            "exactly at the headroom claims"
        );
        assert!(
            matches!(g.before_claim(t0), ClaimDecision::Pause { reason: PauseReason::LowMemory, retry_after } if retry_after == Duration::from_secs(30))
        );
        assert!(
            matches!(g.before_claim(t0), ClaimDecision::Pause { .. }),
            "below resume stays paused"
        );
        assert!(
            matches!(g.before_claim(t0), ClaimDecision::Pause { .. }),
            "one good read is not enough"
        );
        assert!(
            matches!(g.before_claim(t0), ClaimDecision::Pause { .. }),
            "a bad read resets the count"
        );
        assert!(matches!(g.before_claim(t0), ClaimDecision::Pause { .. }));
        assert_eq!(
            g.before_claim(t0),
            ClaimDecision::Claim,
            "two consecutive good reads resume"
        );
        let status = g.status();
        assert_eq!(status.state, "ok");
        assert_eq!(status.paused_rounds, 5);
    }

    #[test]
    fn the_floor_pause_never_charges_a_full_context_job() {
        // 3 GiB free is far below a full-context KV cache (8 GiB) but above the headroom:
        // claims go on; only admission looks at the job.
        let (g, _) = guard(policy(ProbeFailurePolicy::Pause), vec![avail(3 * GIB)]);
        assert_eq!(g.before_claim(Instant::now()), ClaimDecision::Claim);
    }

    // ---- decline back-off ---------------------------------------------------------------------

    #[test]
    fn repeated_large_jobs_without_backoff_are_claimed_and_declined_each_time() {
        let now = Instant::now();
        let (g, _) = guard(policy(ProbeFailurePolicy::Pause), vec![avail(3 * GIB)]);
        for _ in 0..5 {
            assert_eq!(g.before_claim(now), ClaimDecision::Claim);
            assert!(matches!(
                g.admit(Some(4 * GIB), now),
                AdmissionDecision::Decline {
                    kind: DeclineKind::LowMemory,
                    ..
                }
            ));
        }
        assert_eq!(g.status().declined_low_memory, 5);
        assert_eq!(g.status().backoffs, 0);
    }

    #[test]
    fn backoff_pauses_claims_after_n_declines_and_a_fitting_job_resets_the_count() {
        let mut p = policy(ProbeFailurePolicy::Pause);
        p.decline_backoff_after = Some(2);
        p.decline_backoff = Duration::from_secs(120);
        let t0 = Instant::now();
        let (g, _) = guard(p, vec![avail(3 * GIB)]);
        assert!(matches!(
            g.admit(Some(4 * GIB), t0),
            AdmissionDecision::Decline { .. }
        ));
        assert_eq!(
            g.admit(Some(GIB / 2), t0),
            AdmissionDecision::Admit,
            "a job that fits resets the count"
        );
        assert!(matches!(
            g.admit(Some(4 * GIB), t0),
            AdmissionDecision::Decline { .. }
        ));
        assert_eq!(
            g.before_claim(t0),
            ClaimDecision::Claim,
            "one decline since the reset"
        );
        assert!(matches!(
            g.admit(Some(4 * GIB), t0),
            AdmissionDecision::Decline { .. }
        ));
        match g.before_claim(t0 + Duration::from_secs(20)) {
            ClaimDecision::Pause {
                reason: PauseReason::DeclineBackoff,
                retry_after,
            } => {
                assert_eq!(retry_after, Duration::from_secs(100))
            }
            other => panic!("expected the back-off, got {other:?}"),
        }
        assert_eq!(g.status().state, "paused_decline_backoff");
        assert_eq!(
            g.before_claim(t0 + Duration::from_secs(120)),
            ClaimDecision::Claim,
            "back-off over"
        );
        assert_eq!(g.status().backoffs, 1);
    }

    // ---- probe failure policy ----------------------------------------------------------------

    #[test]
    fn probe_failure_with_proceed_acts_as_if_off_and_is_visible() {
        let now = Instant::now();
        let (g, _) = guard(policy(ProbeFailurePolicy::Proceed), vec![failed()]);
        assert_eq!(g.before_claim(now), ClaimDecision::Claim);
        assert_eq!(g.admit(Some(GIB), now), AdmissionDecision::Admit);
        let status = g.status();
        assert_eq!(status.memory_check, "failed");
        assert_eq!(status.probe_failures, 2);
    }

    #[test]
    fn probe_failure_with_pause_stops_claims_declines_and_recovers() {
        let now = Instant::now();
        let (g, _) = guard(
            policy(ProbeFailurePolicy::Pause),
            vec![failed(), failed(), avail(10 * GIB)],
        );
        assert!(matches!(
            g.before_claim(now),
            ClaimDecision::Pause {
                reason: PauseReason::ProbeFailed,
                ..
            }
        ));
        assert_eq!(g.status().state, "paused_memory_check_failed");
        assert!(matches!(
            g.admit(Some(GIB), now),
            AdmissionDecision::Decline {
                kind: DeclineKind::ProbeFailed,
                ..
            }
        ));
        assert_eq!(
            g.before_claim(now),
            ClaimDecision::Claim,
            "a working probe with room resumes"
        );
        assert_eq!(g.status().memory_check, "ok");
        assert_eq!(g.status().declined_probe_failed, 1);
    }

    #[test]
    fn status_before_any_read() {
        let (g, _) = guard(policy(ProbeFailurePolicy::Pause), vec![avail(GIB)]);
        assert_eq!(g.status().memory_check, "not_read_yet");
        assert_eq!(g.status().state, "ok");
    }

    // ---- macOS / Windows probe results ----------------------------------------------------------

    #[test]
    fn macos_a_failed_call_is_failed_and_a_saturated_zero_is_a_real_reading() {
        // A non-success return is a failure even if the buffer holds plausible counts.
        assert!(matches!(
            macos_reading(5, Some((1000, 1000, 0, 0)), 16384, Some(16 * GIB)),
            ProbeOutcome::Failed(_)
        ));
        assert!(matches!(
            macos_reading(0, None, 16384, Some(16 * GIB)),
            ProbeOutcome::Failed(_)
        ));
        assert!(
            matches!(
                macos_reading(0, Some((1, 1, 1, 1)), 16384, None),
                ProbeOutcome::Failed(_)
            ),
            "sysctl failed"
        );
        assert!(matches!(
            macos_reading(0, Some((1, 1, 1, 1)), 0, Some(16 * GIB)),
            ProbeOutcome::Failed(_)
        ));
        // Heavy compression: compressor pages exceed free + inactive + purgeable.
        let zero = macos_reading(0, Some((100, 50, 10, 1_000)), 16384, Some(16 * GIB));
        assert_eq!(
            zero,
            ProbeOutcome::Reading(MemoryReading {
                available: 0,
                total: 16 * GIB,
                source: "host_statistics64",
                cgroup_headroom: None
            })
        );
        // A real 0 pauses and declines; it is not treated as a failure.
        let (g, _) = guard(policy(ProbeFailurePolicy::Proceed), vec![zero]);
        assert!(matches!(
            g.before_claim(Instant::now()),
            ClaimDecision::Pause {
                reason: PauseReason::LowMemory,
                ..
            }
        ));
        assert_eq!(g.status().probe_failures, 0);
        assert_eq!(macos_available_bytes(10, 20, 5, 15, 4096), 20 * 4096);
    }

    #[test]
    fn windows_false_or_zero_total_is_failed() {
        assert!(matches!(
            windows_reading(false, 16 * GIB, 8 * GIB),
            ProbeOutcome::Failed(_)
        ));
        assert!(matches!(
            windows_reading(true, 0, 0),
            ProbeOutcome::Failed(_)
        ));
        assert_eq!(
            windows_reading(true, 16 * GIB, 8 * GIB),
            ProbeOutcome::Reading(MemoryReading {
                available: 8 * GIB,
                total: 16 * GIB,
                source: "GlobalMemoryStatusEx",
                cgroup_headroom: None
            })
        );
    }

    // ---- Linux probe ------------------------------------------------------------------------------

    fn files(entries: &[(&str, &str)]) -> impl Fn(&str) -> std::io::Result<Option<String>> {
        let map: std::collections::HashMap<String, String> = entries
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect();
        move |path: &str| Ok(map.get(path).cloned())
    }

    const MEMINFO: &str = "MemTotal:       16000000 kB\nMemFree:         1000000 kB\nMemAvailable:    8000000 kB\nBuffers:          100000 kB\nCached:          2000000 kB\n";
    const MEMINFO_OLD: &str = "MemTotal: 16000000 kB\nMemFree: 1000000 kB\nBuffers: 100000 kB\nCached: 2000000 kB\nSReclaimable: 50000 kB\nShmem: 10000 kB\n";

    #[test]
    fn linux_meminfo_available_and_the_pre_3_14_estimate() {
        assert_eq!(
            parse_meminfo(MEMINFO),
            Ok((8_000_000 * 1024, 16_000_000 * 1024, "meminfo_available"))
        );
        assert_eq!(
            parse_meminfo(MEMINFO_OLD),
            Ok((
                (1_000_000 + 100_000 + 2_000_000 + 50_000 - 10_000) * 1024,
                16_000_000 * 1024,
                "meminfo_estimate_pre_3_14"
            ))
        );
        assert!(parse_meminfo("MemTotal: lots kB\n").is_err());
        assert!(parse_meminfo("MemFree: 1 kB\n").is_err(), "no MemTotal");
        assert!(
            matches!(linux_reading(&files(&[])), ProbeOutcome::Failed(_)),
            "missing /proc/meminfo"
        );
        assert!(matches!(
            linux_reading(&|_| Err(std::io::Error::other("denied"))),
            ProbeOutcome::Failed(_)
        ));
    }

    #[test]
    fn linux_cgroup_v2_limit_usage_and_max() {
        let base = [
            ("/proc/meminfo", MEMINFO),
            ("/proc/self/cgroup", "0::/worker.slice\n"),
        ];
        let read = |extra: &[(&str, &str)]| {
            let mut all = base.to_vec();
            all.extend_from_slice(extra);
            linux_reading(&files(&all))
        };
        let ProbeOutcome::Reading(r) = read(&[
            ("/sys/fs/cgroup/worker.slice/memory.max", "4294967296\n"),
            ("/sys/fs/cgroup/worker.slice/memory.current", "1073741824\n"),
        ]) else {
            panic!("expected a reading")
        };
        assert_eq!(r.cgroup_headroom, Some(3 * GIB));
        assert_eq!(r.effective_available(), 3 * GIB, "min(host, cgroup)");
        // Over the limit: 0 headroom, never a wrap.
        let ProbeOutcome::Reading(r) = read(&[
            ("/sys/fs/cgroup/worker.slice/memory.max", "1073741824"),
            ("/sys/fs/cgroup/worker.slice/memory.current", "2147483648"),
        ]) else {
            panic!("expected a reading")
        };
        assert_eq!(r.effective_available(), 0);
        // "max" is no limit: None, never u64::MAX; the host figure is used.
        let ProbeOutcome::Reading(r) = read(&[("/sys/fs/cgroup/worker.slice/memory.max", "max\n")])
        else {
            panic!("expected a reading")
        };
        assert_eq!(r.cgroup_headroom, None);
        assert_eq!(r.effective_available(), 8_000_000 * 1024);
        // Unparsable files, or a limit without usage, are failures.
        assert!(matches!(
            read(&[("/sys/fs/cgroup/worker.slice/memory.max", "lots")]),
            ProbeOutcome::Failed(_)
        ));
        assert!(matches!(
            read(&[("/sys/fs/cgroup/worker.slice/memory.max", "1024")]),
            ProbeOutcome::Failed(_)
        ));
    }

    #[test]
    fn linux_cgroup_v1_memory_controller() {
        assert_eq!(
            parse_proc_self_cgroup("12:cpu,cpuacct:/x\n9:memory:/docker/abc\n0::/\n"),
            CgroupLocation::V1("/docker/abc".into())
        );
        assert_eq!(parse_proc_self_cgroup(""), CgroupLocation::None);
        let ProbeOutcome::Reading(r) = linux_reading(&files(&[
            ("/proc/meminfo", MEMINFO),
            ("/proc/self/cgroup", "9:memory:/docker/abc\n"),
            (
                "/sys/fs/cgroup/memory/docker/abc/memory.limit_in_bytes",
                "2147483648",
            ),
            (
                "/sys/fs/cgroup/memory/docker/abc/memory.usage_in_bytes",
                "1073741824",
            ),
        ])) else {
            panic!("expected a reading")
        };
        assert_eq!(r.cgroup_headroom, Some(GIB));
    }
}
