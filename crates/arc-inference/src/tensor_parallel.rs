//! Private-cohort tensor-row parallelism for one inference query.
//!
//! This module is intentionally separate from the public layer-shard RPC.
//! Every worker owns disjoint rows of one canonical-I8 matrix, returns those
//! rows for one authenticated call, and the coordinator restores the tensor in
//! row order.  It is useful on trusted, pinned-host-key stdio transports; it
//! does not advertise a public listener or a validator capability.

use crate::cached_integer_model::{
    I8Weights, matmul_i8_canonical_row_range, matmul_i8_canonical_rows,
};
use arc_crypto::Hash256;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::io::Write;
use std::path::Path;
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering as AtomicOrdering};
use std::time::{Duration, Instant};
use thiserror::Error;

pub const ROW_PROTOCOL_DOMAIN: &str = "ARC-tensor-row-v1";
pub const MAX_ROW_INPUT_ELEMENTS: usize = 131_072;
pub const MAX_ROW_OUTPUT_ELEMENTS: usize = 131_072;
pub const MAX_ROW_WORKERS_PER_STAGE: usize = 32;
pub const MAX_ROW_ID_BYTES: usize = 256;
/// Deliberately below the sidecar's 1536 MiB cgroup budget, leaving space for
/// the worker, input frame, and OS.  A deployment must split a larger slice.
pub const MAX_CANONICAL_ROW_FILE_BYTES: usize = 1_073_741_824;
const ROW_FILE_MAGIC: &[u8; 8] = b"ARCROW01";
/// Request frames carry `ROW_FRAME_MAGIC`, responses `ROW_RESPONSE_MAGIC`, so a
/// request frame (or a worker that echoes one) is never read as a response.
const ROW_FRAME_MAGIC: &[u8; 8] = b"ARCTP001";
const ROW_RESPONSE_MAGIC: &[u8; 8] = b"ARCTR001";

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TensorKey {
    Wq,
    Wk,
    Wv,
    Wo,
    WGate,
    WUp,
    WDown,
    LmHead,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RowAssignment {
    pub artifact_id: Hash256,
    pub execution_profile: String,
    pub layer: Option<usize>,
    pub tensor: TensorKey,
    /// Logical canonical output rows, inclusive/exclusive.  Q/K means rows
    /// after the loader's interleaved-to-split-half permutation.
    pub row_start: usize,
    pub row_end: usize,
    /// Pinned transport identity, never a display name or network address.
    pub worker_id: String,
}

impl RowAssignment {
    pub fn rows(&self) -> usize {
        self.row_end.saturating_sub(self.row_start)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RowProjectionRequest {
    pub call_id: Hash256,
    pub input_hash: Hash256,
    pub assignment: RowAssignment,
    pub input: Vec<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RowProjectionResponse {
    pub call_id: Hash256,
    pub input_hash: Hash256,
    pub assignment: RowAssignment,
    pub values: Vec<i64>,
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum TensorParallelError {
    #[error("row assignment has invalid range [{start}, {end})")]
    InvalidRange { start: usize, end: usize },
    #[error("row coverage is incomplete: expected {expected}, covered {covered}")]
    IncompleteCoverage { expected: usize, covered: usize },
    #[error("row coverage overlaps or is unordered at row {row}")]
    Overlap { row: usize },
    #[error("row assignment identity does not match the requested model/profile/tensor")]
    WrongIdentity,
    #[error("row projection input/output exceeds protocol bounds")]
    Bounds,
    #[error("row projection shape mismatch")]
    WrongShape,
    #[error("row response does not bind the requested call or input")]
    WrongCall,
    #[error("no worker is pinned for {0}")]
    MissingWorker(String),
    #[error("worker rejected row projection: {0}")]
    Worker(String),
    /// The worker's connection had already closed: nothing was sent.
    #[error("the row worker's connection is closed")]
    Closed,
}

/// Validate that a stage has exactly one owner for every output row.  This is
/// deliberately strict: missing output rows, duplicate rows, and different
/// artifact/profile identities all fail before a worker is contacted.
pub fn validate_exact_coverage(
    assignments: &[RowAssignment],
    artifact_id: Hash256,
    profile: &str,
    layer: Option<usize>,
    tensor: TensorKey,
    output_rows: usize,
) -> Result<Vec<RowAssignment>, TensorParallelError> {
    let mut ordered = assignments.to_vec();
    ordered.sort_by_key(|a| (a.row_start, a.row_end, a.worker_id.clone()));
    let mut cursor = 0usize;
    for a in &ordered {
        if a.artifact_id != artifact_id
            || a.execution_profile != profile
            || a.layer != layer
            || a.tensor != tensor
            || a.worker_id.is_empty()
            || a.execution_profile.len() > MAX_ROW_ID_BYTES
            || a.worker_id.len() > MAX_ROW_ID_BYTES
        {
            return Err(TensorParallelError::WrongIdentity);
        }
        if a.row_start >= a.row_end || a.row_end > output_rows {
            return Err(TensorParallelError::InvalidRange {
                start: a.row_start,
                end: a.row_end,
            });
        }
        if a.row_start < cursor {
            return Err(TensorParallelError::Overlap { row: a.row_start });
        }
        if a.row_start != cursor {
            return Err(TensorParallelError::IncompleteCoverage {
                expected: output_rows,
                covered: cursor,
            });
        }
        cursor = a.row_end;
    }
    if cursor != output_rows {
        return Err(TensorParallelError::IncompleteCoverage {
            expected: output_rows,
            covered: cursor,
        });
    }
    if ordered.len() > MAX_ROW_WORKERS_PER_STAGE {
        return Err(TensorParallelError::Bounds);
    }
    Ok(ordered)
}

/// A transport adapter.  Production wiring uses a persistent SSH stdio child
/// with a pinned host key; this trait keeps the numeric/coverage proof
/// testable without opening a socket or changing validator RPC.
pub trait RowWorker: Send + Sync {
    fn project(
        &self,
        request: RowProjectionRequest,
    ) -> Result<RowProjectionResponse, TensorParallelError>;

    /// Whether the worker can still take a call. A transport that closes
    /// itself after a failure (as [`SshStdioRowWorker`] does) says so here,
    /// so its owner can reconnect it.
    fn is_open(&self) -> bool {
        true
    }
}

/// Fallible projection interface consumed by the canonical-I8 forward adapter.
/// A backend receives only a projection stage and its activation; it cannot
/// replace local layernorm, RoPE, attention, residuals, or token selection.
pub trait ProjectionBackend: Send + Sync {
    fn project_rows(
        &self,
        call_id: Hash256,
        layer: Option<usize>,
        tensor: TensorKey,
        input: &[i64],
        output_rows: usize,
    ) -> Result<Vec<i64>, TensorParallelError>;

    /// Independent projections from the same activation (Q/K/V and gate/up)
    /// are dispatched concurrently.  The result order is the input order;
    /// each individual projection still performs exact row coverage checks.
    fn project_group(
        &self,
        call_id: Hash256,
        layer: Option<usize>,
        calls: &[(TensorKey, usize)],
        input: &[i64],
    ) -> Result<Vec<Vec<i64>>, TensorParallelError> {
        std::thread::scope(|scope| {
            let mut joins = Vec::with_capacity(calls.len());
            for &(tensor, rows) in calls {
                joins.push(
                    scope.spawn(move || self.project_rows(call_id, layer, tensor, input, rows)),
                );
            }
            let mut out = Vec::with_capacity(joins.len());
            for join in joins {
                out.push(join.join().map_err(|_| {
                    TensorParallelError::Worker("projection task panicked".into())
                })??);
            }
            Ok(out)
        })
    }
}

/// A row shard used by the portable stdio worker and deterministic tests.
pub struct LocalRowWorker {
    pub worker_id: String,
    pub assignment: RowAssignment,
    /// Contains only `assignment.rows()` canonical matrix rows and their Q16
    /// scales; it is never a resident full model.
    pub weights: I8Weights,
}

/// Coordinator-local view of one resident canonical model.  Unlike
/// `LocalRowWorker`, it does not retain a second copy of a tensor shard; it
/// slices the coordinator's verified model only for the bounded projection.
pub struct ModelRowWorker {
    pub worker_id: String,
    pub assignment: RowAssignment,
    pub model: Arc<crate::cached_integer_model::CachedIntegerModel>,
}

impl RowWorker for ModelRowWorker {
    fn project(
        &self,
        request: RowProjectionRequest,
    ) -> Result<RowProjectionResponse, TensorParallelError> {
        if request.assignment != self.assignment
            || request.assignment.worker_id != self.worker_id
            || hash_i64(&request.input) != request.input_hash
            || self.model.canonical_execution_profile()
                != Some(self.assignment.execution_profile.as_str())
        {
            return Err(TensorParallelError::WrongIdentity);
        }
        let weights = match (self.assignment.layer, self.assignment.tensor) {
            (Some(layer), TensorKey::Wq) => self.model.layers.get(layer).map(|l| &l.wq),
            (Some(layer), TensorKey::Wk) => self.model.layers.get(layer).map(|l| &l.wk),
            (Some(layer), TensorKey::Wv) => self.model.layers.get(layer).map(|l| &l.wv),
            (Some(layer), TensorKey::Wo) => self.model.layers.get(layer).map(|l| &l.wo),
            (Some(layer), TensorKey::WGate) => self.model.layers.get(layer).map(|l| &l.w_gate),
            (Some(layer), TensorKey::WUp) => self.model.layers.get(layer).map(|l| &l.w_up),
            (Some(layer), TensorKey::WDown) => self.model.layers.get(layer).map(|l| &l.w_down),
            (None, TensorKey::LmHead) => Some(&self.model.output_weight),
            _ => None,
        }
        .ok_or(TensorParallelError::WrongIdentity)?;
        let shard = weights
            .copy_rows(self.assignment.row_start, self.assignment.row_end)
            .map_err(|_| TensorParallelError::WrongShape)?;
        let mut values = vec![0; shard.n_rows];
        matmul_i8_canonical_rows(&shard, &request.input, &mut values)
            .map_err(|_| TensorParallelError::WrongShape)?;
        Ok(RowProjectionResponse {
            call_id: request.call_id,
            input_hash: request.input_hash,
            assignment: self.assignment.clone(),
            values,
        })
    }
}

impl RowWorker for LocalRowWorker {
    fn project(
        &self,
        request: RowProjectionRequest,
    ) -> Result<RowProjectionResponse, TensorParallelError> {
        if request.assignment != self.assignment || request.assignment.worker_id != self.worker_id {
            return Err(TensorParallelError::WrongIdentity);
        }
        if request.input.len() > MAX_ROW_INPUT_ELEMENTS
            || self.assignment.rows() > MAX_ROW_OUTPUT_ELEMENTS
            || hash_i64(&request.input) != request.input_hash
        {
            return Err(TensorParallelError::Bounds);
        }
        if self.weights.n_rows != self.assignment.rows()
            || self.weights.n_cols != request.input.len()
        {
            return Err(TensorParallelError::WrongShape);
        }
        let mut values = vec![0; self.weights.n_rows];
        matmul_i8_canonical_rows(&self.weights, &request.input, &mut values)
            .map_err(|_| TensorParallelError::WrongShape)?;
        Ok(RowProjectionResponse {
            call_id: request.call_id,
            input_hash: request.input_hash,
            assignment: self.assignment.clone(),
            values,
        })
    }
}

/// Configuration for a single persistent, private SSH stdio sidecar.  The
/// command is constructed as argv (never a shell string), host-key checking is
/// mandatory, and a protocol failure kills the child instead of retrying a
/// different endpoint or silently falling back to local projection.
pub struct SshStdioConfig {
    pub ssh_program: std::path::PathBuf,
    pub target: String,
    pub known_hosts: std::path::PathBuf,
    pub remote_command: Vec<String>,
    pub timeout: Duration,
}

struct SshChild {
    child: Child,
    stdin: ChildStdin,
    stdout: ChildStdout,
}

/// The longest a single call to an SSH row worker may take, whatever it is
/// configured with.
pub const MAX_SSH_CALL_TIMEOUT_MS: u64 = 3_600_000;

pub struct SshStdioRowWorker {
    worker_id: String,
    child: Mutex<Option<SshChild>>,
    timeout_ms: AtomicU64,
    /// Set when the worker closes itself; read without waiting on a call.
    closed: AtomicBool,
}

/// The remote command as the one string OpenSSH hands the remote shell: each
/// argv element single-quoted, an embedded `'` written as `'\''` (close,
/// escaped quote, reopen), so no element can end its quoting.
fn remote_shell_command(args: &[String]) -> String {
    args.iter()
        .map(|arg| format!("'{}'", arg.replace('\'', "'\\''")))
        .collect::<Vec<_>>()
        .join(" ")
}

/// An absolute path of letters, digits and `._/-` only: OpenSSH reads it
/// verbatim in a file-name option, with nothing to split, expand or unescape.
pub fn is_plain_absolute_path(path: &str) -> bool {
    path.starts_with('/')
        && path
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '/' | '-'))
}

impl SshStdioRowWorker {
    pub fn connect(worker_id: String, config: SshStdioConfig) -> Result<Self, TensorParallelError> {
        if worker_id.is_empty()
            || worker_id.len() > MAX_ROW_ID_BYTES
            || config.target.is_empty()
            || config.target.starts_with('-')
            || config.target.contains('\0')
            || !config.known_hosts.is_file()
            || config.remote_command.is_empty()
            || config.timeout.is_zero()
        {
            return Err(TensorParallelError::Bounds);
        }
        if config.remote_command.iter().any(|arg| arg.contains('\0')) {
            return Err(TensorParallelError::Bounds);
        }
        // ssh splits UserKnownHostsFile on whitespace, expands `%`, `~` and
        // `${VAR}`, and collapses backslashes: accept only a plain absolute
        // path it reads as exactly this one file.
        let known_hosts = config
            .known_hosts
            .to_str()
            .filter(|path| is_plain_absolute_path(path))
            .ok_or(TensorParallelError::Bounds)?;
        let remote = remote_shell_command(&config.remote_command);
        let mut command = Command::new(&config.ssh_program);
        command
            // No configuration file: a ControlMaster, ProxyCommand,
            // KnownHostsCommand or host-key setting there could widen or
            // bypass the pin. The target names the user (and, as
            // ssh://user@host:port, the port); the key comes from ssh's
            // default identities or an agent.
            .arg("-F")
            .arg("none")
            .arg("-oBatchMode=yes")
            .arg("-oStrictHostKeyChecking=yes")
            .arg(format!("-oUserKnownHostsFile={known_hosts}"))
            .arg("-oGlobalKnownHostsFile=/dev/null")
            .arg("-oUpdateHostKeys=no")
            .arg("-oCheckHostIP=no")
            .arg("-oControlPath=none")
            .arg("-oPasswordAuthentication=no")
            // Bounded without a config file: an unreachable host fails within
            // 10 s, a dead session within about 45 s (15 s keepalives, 2 missed
            // before the next one fails).
            .arg("-oConnectTimeout=10")
            .arg("-oServerAliveInterval=15")
            .arg("-oServerAliveCountMax=2")
            .arg("--")
            .arg(&config.target)
            .arg(remote)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        let mut child = command
            .spawn()
            .map_err(|e| TensorParallelError::Worker(format!("start pinned SSH worker: {e}")))?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| TensorParallelError::Worker("SSH worker stdin unavailable".into()))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| TensorParallelError::Worker("SSH worker stdout unavailable".into()))?;
        set_nonblocking_stdin(&stdin)
            .map_err(|e| TensorParallelError::Worker(format!("configure SSH worker stdin: {e}")))?;
        Ok(Self {
            worker_id,
            child: Mutex::new(Some(SshChild {
                child,
                stdin,
                stdout,
            })),
            timeout_ms: AtomicU64::new(
                u64::try_from(config.timeout.as_millis()).unwrap_or(u64::MAX),
            ),
            closed: AtomicBool::new(false),
        })
    }

    /// Change the per-call timeout, e.g. after a first call that had to cover
    /// the remote model load.
    pub fn set_timeout(&self, timeout: Duration) {
        self.timeout_ms.store(
            u64::try_from(timeout.as_millis())
                .unwrap_or(u64::MAX)
                .max(1),
            AtomicOrdering::Relaxed,
        );
    }

    fn fail_closed(
        &self,
        slot: &mut Option<SshChild>,
        message: impl Into<String>,
    ) -> TensorParallelError {
        self.closed.store(true, AtomicOrdering::Release);
        if let Some(mut child) = slot.take() {
            let _ = child.child.kill();
            let _ = child.child.wait();
        }
        TensorParallelError::Worker(message.into())
    }
}

impl Drop for SshStdioRowWorker {
    fn drop(&mut self) {
        // Even after a panic poisoned the lock, the ssh child is killed and
        // reaped rather than leaked.
        let slot = match self.child.get_mut() {
            Ok(slot) => slot,
            Err(poisoned) => poisoned.into_inner(),
        };
        if let Some(mut c) = slot.take() {
            let _ = c.child.kill();
            let _ = c.child.wait();
        }
    }
}

impl RowWorker for SshStdioRowWorker {
    fn is_open(&self) -> bool {
        !self.closed.load(AtomicOrdering::Acquire)
    }

    fn project(
        &self,
        request: RowProjectionRequest,
    ) -> Result<RowProjectionResponse, TensorParallelError> {
        if request.assignment.worker_id != self.worker_id
            || hash_i64(&request.input) != request.input_hash
        {
            return Err(TensorParallelError::WrongCall);
        }
        let frame = encode_row_request(&request)?;
        let mut slot = self.child.lock().map_err(|_| {
            // A panic during a call left the session unusable, and this call
            // sends nothing: the connection is closed.
            self.closed.store(true, AtomicOrdering::Release);
            TensorParallelError::Closed
        })?;
        let child = slot.as_mut().ok_or(TensorParallelError::Closed)?;
        // Clamped, so no configured timeout can overflow the deadline.
        let timeout = self
            .timeout_ms
            .load(AtomicOrdering::Relaxed)
            .min(MAX_SSH_CALL_TIMEOUT_MS);
        let deadline = Instant::now() + Duration::from_millis(timeout);
        let mut framed = Vec::with_capacity(frame.len() + 4);
        framed.extend_from_slice(&(frame.len() as u32).to_le_bytes());
        framed.extend_from_slice(&frame);
        if let Err(e) = write_timeout(&mut child.stdin, &framed, deadline) {
            return Err(self.fail_closed(&mut slot, format!("write SSH row frame: {e}")));
        }
        let raw = match read_framed_timeout(&mut child.stdout, deadline) {
            Ok(value) => value,
            Err(error) => return Err(self.fail_closed(&mut slot, error)),
        };
        let response = match decode_row_response(&raw) {
            Ok(value) => value,
            Err(error) => {
                return Err(
                    self.fail_closed(&mut slot, format!("decode SSH row response: {error}"))
                );
            }
        };
        if response.call_id != request.call_id
            || response.input_hash != request.input_hash
            || response.assignment != request.assignment
            || response.values.len() != request.assignment.rows()
        {
            return Err(self.fail_closed(
                &mut slot,
                "SSH row response did not bind the request exactly",
            ));
        }
        Ok(response)
    }
}

fn push_bytes(out: &mut Vec<u8>, bytes: &[u8]) {
    out.extend_from_slice(bytes);
}
fn push_short(out: &mut Vec<u8>, value: &[u8]) -> Result<(), TensorParallelError> {
    if value.len() > u16::MAX as usize {
        return Err(TensorParallelError::Bounds);
    }
    push_bytes(out, &(value.len() as u16).to_le_bytes());
    push_bytes(out, value);
    Ok(())
}
fn pull<'a>(bytes: &mut &'a [u8], n: usize) -> Result<&'a [u8], TensorParallelError> {
    if bytes.len() < n {
        return Err(TensorParallelError::WrongShape);
    }
    let (head, rest) = bytes.split_at(n);
    *bytes = rest;
    Ok(head)
}
fn pull_u16(bytes: &mut &[u8]) -> Result<usize, TensorParallelError> {
    Ok(u16::from_le_bytes(
        pull(bytes, 2)?
            .try_into()
            .map_err(|_| TensorParallelError::WrongShape)?,
    ) as usize)
}
fn pull_u32(bytes: &mut &[u8]) -> Result<usize, TensorParallelError> {
    Ok(u32::from_le_bytes(
        pull(bytes, 4)?
            .try_into()
            .map_err(|_| TensorParallelError::WrongShape)?,
    ) as usize)
}
fn pull_u64(bytes: &mut &[u8]) -> Result<u64, TensorParallelError> {
    Ok(u64::from_le_bytes(
        pull(bytes, 8)?
            .try_into()
            .map_err(|_| TensorParallelError::WrongShape)?,
    ))
}
fn pull_i64(bytes: &mut &[u8]) -> Result<i64, TensorParallelError> {
    Ok(i64::from_le_bytes(
        pull(bytes, 8)?
            .try_into()
            .map_err(|_| TensorParallelError::WrongShape)?,
    ))
}
fn tensor_from_byte(byte: u8) -> Result<TensorKey, TensorParallelError> {
    match byte {
        0 => Ok(TensorKey::Wq),
        1 => Ok(TensorKey::Wk),
        2 => Ok(TensorKey::Wv),
        3 => Ok(TensorKey::Wo),
        4 => Ok(TensorKey::WGate),
        5 => Ok(TensorKey::WUp),
        6 => Ok(TensorKey::WDown),
        7 => Ok(TensorKey::LmHead),
        _ => Err(TensorParallelError::WrongIdentity),
    }
}

/// Encode a row request frame (coordinator to worker).
pub fn encode_row_request(request: &RowProjectionRequest) -> Result<Vec<u8>, TensorParallelError> {
    if request.input.len() > MAX_ROW_INPUT_ELEMENTS {
        return Err(TensorParallelError::Bounds);
    }
    let a = &request.assignment;
    let mut out = Vec::with_capacity(
        128 + a.execution_profile.len() + a.worker_id.len() + request.input.len() * 8,
    );
    push_bytes(&mut out, ROW_FRAME_MAGIC);
    push_bytes(&mut out, &request.call_id.0);
    push_bytes(&mut out, &request.input_hash.0);
    push_bytes(&mut out, &a.artifact_id.0);
    push_short(&mut out, a.execution_profile.as_bytes())?;
    push_bytes(
        &mut out,
        &(a.layer.map(|v| v as i64).unwrap_or(-1)).to_le_bytes(),
    );
    out.push(a.tensor as u8);
    push_bytes(&mut out, &(a.row_start as u64).to_le_bytes());
    push_bytes(&mut out, &(a.row_end as u64).to_le_bytes());
    push_short(&mut out, a.worker_id.as_bytes())?;
    push_bytes(&mut out, &(request.input.len() as u32).to_le_bytes());
    for value in &request.input {
        push_bytes(&mut out, &value.to_le_bytes());
    }
    if out.len() > 4 * 1024 * 1024 {
        return Err(TensorParallelError::Bounds);
    }
    Ok(out)
}

/// Decode a row response frame (worker to coordinator).
pub fn decode_row_response(raw: &[u8]) -> Result<RowProjectionResponse, TensorParallelError> {
    let mut bytes = raw;
    if pull(&mut bytes, 8)? != ROW_RESPONSE_MAGIC {
        return Err(TensorParallelError::WrongIdentity);
    }
    let call_id = Hash256(
        pull(&mut bytes, 32)?
            .try_into()
            .map_err(|_| TensorParallelError::WrongShape)?,
    );
    let input_hash = Hash256(
        pull(&mut bytes, 32)?
            .try_into()
            .map_err(|_| TensorParallelError::WrongShape)?,
    );
    let artifact_id = Hash256(
        pull(&mut bytes, 32)?
            .try_into()
            .map_err(|_| TensorParallelError::WrongShape)?,
    );
    let profile_len = pull_u16(&mut bytes)?;
    let execution_profile = std::str::from_utf8(pull(&mut bytes, profile_len)?)
        .map_err(|_| TensorParallelError::WrongShape)?
        .to_string();
    let layer = pull_i64(&mut bytes)?;
    let tensor = tensor_from_byte(pull(&mut bytes, 1)?[0])?;
    let row_start = pull_u64(&mut bytes)? as usize;
    let row_end = pull_u64(&mut bytes)? as usize;
    let worker_len = pull_u16(&mut bytes)?;
    let worker_id = std::str::from_utf8(pull(&mut bytes, worker_len)?)
        .map_err(|_| TensorParallelError::WrongShape)?
        .to_string();
    let count = pull_u32(&mut bytes)?;
    if count > MAX_ROW_OUTPUT_ELEMENTS
        || execution_profile.len() > MAX_ROW_ID_BYTES
        || worker_id.len() > MAX_ROW_ID_BYTES
        || count != row_end.saturating_sub(row_start)
    {
        return Err(TensorParallelError::WrongShape);
    }
    let mut values = Vec::with_capacity(count);
    for _ in 0..count {
        values.push(pull_i64(&mut bytes)?);
    }
    if !bytes.is_empty() {
        return Err(TensorParallelError::WrongShape);
    }
    Ok(RowProjectionResponse {
        call_id,
        input_hash,
        assignment: RowAssignment {
            artifact_id,
            execution_profile,
            layer: (layer >= 0).then_some(layer as usize),
            tensor,
            row_start,
            row_end,
            worker_id,
        },
        values,
    })
}

/// Largest frame either side sends; the SSH reader refuses anything larger.
pub const MAX_ROW_FRAME_BYTES: usize = 4 * 1024 * 1024;

/// The header both frame directions carry: the call, the input it binds and
/// the exact assignment. Byte layout identical to [`encode_row_request`].
fn push_frame_header(
    out: &mut Vec<u8>,
    magic: &[u8; 8],
    call_id: &Hash256,
    input_hash: &Hash256,
    a: &RowAssignment,
) -> Result<(), TensorParallelError> {
    push_bytes(out, magic);
    push_bytes(out, &call_id.0);
    push_bytes(out, &input_hash.0);
    push_bytes(out, &a.artifact_id.0);
    push_short(out, a.execution_profile.as_bytes())?;
    push_bytes(
        out,
        &(a.layer.map(|v| v as i64).unwrap_or(-1)).to_le_bytes(),
    );
    out.push(a.tensor as u8);
    push_bytes(out, &(a.row_start as u64).to_le_bytes());
    push_bytes(out, &(a.row_end as u64).to_le_bytes());
    push_short(out, a.worker_id.as_bytes())
}

fn pull_hash(bytes: &mut &[u8]) -> Result<Hash256, TensorParallelError> {
    Ok(Hash256(
        pull(bytes, 32)?
            .try_into()
            .map_err(|_| TensorParallelError::WrongShape)?,
    ))
}

/// Inverse of [`push_frame_header`], with the same checks as
/// [`decode_row_response`].
fn pull_frame_header(
    bytes: &mut &[u8],
    magic: &[u8; 8],
) -> Result<(Hash256, Hash256, RowAssignment), TensorParallelError> {
    if pull(bytes, 8)? != magic {
        return Err(TensorParallelError::WrongIdentity);
    }
    let call_id = pull_hash(bytes)?;
    let input_hash = pull_hash(bytes)?;
    let artifact_id = pull_hash(bytes)?;
    let profile_len = pull_u16(bytes)?;
    let execution_profile = std::str::from_utf8(pull(bytes, profile_len)?)
        .map_err(|_| TensorParallelError::WrongShape)?
        .to_string();
    let layer = pull_i64(bytes)?;
    let tensor = tensor_from_byte(pull(bytes, 1)?[0])?;
    let row_start = pull_u64(bytes)? as usize;
    let row_end = pull_u64(bytes)? as usize;
    let worker_len = pull_u16(bytes)?;
    let worker_id = std::str::from_utf8(pull(bytes, worker_len)?)
        .map_err(|_| TensorParallelError::WrongShape)?
        .to_string();
    if execution_profile.len() > MAX_ROW_ID_BYTES || worker_id.len() > MAX_ROW_ID_BYTES {
        return Err(TensorParallelError::WrongShape);
    }
    let assignment = RowAssignment {
        artifact_id,
        execution_profile,
        layer: (layer >= 0).then_some(layer as usize),
        tensor,
        row_start,
        row_end,
        worker_id,
    };
    Ok((call_id, input_hash, assignment))
}

/// Decode a row request frame (the worker's side of [`encode_row_request`]).
pub fn decode_row_request(raw: &[u8]) -> Result<RowProjectionRequest, TensorParallelError> {
    let mut bytes = raw;
    let (call_id, input_hash, assignment) = pull_frame_header(&mut bytes, ROW_FRAME_MAGIC)?;
    let count = pull_u32(&mut bytes)?;
    if count > MAX_ROW_INPUT_ELEMENTS || assignment.row_end <= assignment.row_start {
        return Err(TensorParallelError::WrongShape);
    }
    let mut input = Vec::with_capacity(count);
    for _ in 0..count {
        input.push(pull_i64(&mut bytes)?);
    }
    if !bytes.is_empty() {
        return Err(TensorParallelError::WrongShape);
    }
    Ok(RowProjectionRequest {
        call_id,
        input_hash,
        assignment,
        input,
    })
}

/// Encode a row response frame (the worker's side of [`decode_row_response`]).
pub fn encode_row_response(
    response: &RowProjectionResponse,
) -> Result<Vec<u8>, TensorParallelError> {
    let a = &response.assignment;
    if response.values.len() > MAX_ROW_OUTPUT_ELEMENTS || response.values.len() != a.rows() {
        return Err(TensorParallelError::Bounds);
    }
    let mut out = Vec::with_capacity(
        128 + a.execution_profile.len() + a.worker_id.len() + response.values.len() * 8,
    );
    push_frame_header(
        &mut out,
        ROW_RESPONSE_MAGIC,
        &response.call_id,
        &response.input_hash,
        a,
    )?;
    push_bytes(&mut out, &(response.values.len() as u32).to_le_bytes());
    for value in &response.values {
        push_bytes(&mut out, &value.to_le_bytes());
    }
    if out.len() > MAX_ROW_FRAME_BYTES {
        return Err(TensorParallelError::Bounds);
    }
    Ok(out)
}

/// The resident matrix of one projection of `model`.
pub fn model_projection_weights(
    model: &crate::cached_integer_model::CachedIntegerModel,
    layer: Option<usize>,
    tensor: TensorKey,
) -> Option<&I8Weights> {
    match (layer, tensor) {
        (Some(layer), TensorKey::Wq) => model.layers.get(layer).map(|l| &l.wq),
        (Some(layer), TensorKey::Wk) => model.layers.get(layer).map(|l| &l.wk),
        (Some(layer), TensorKey::Wv) => model.layers.get(layer).map(|l| &l.wv),
        (Some(layer), TensorKey::Wo) => model.layers.get(layer).map(|l| &l.wo),
        (Some(layer), TensorKey::WGate) => model.layers.get(layer).map(|l| &l.w_gate),
        (Some(layer), TensorKey::WUp) => model.layers.get(layer).map(|l| &l.w_up),
        (Some(layer), TensorKey::WDown) => model.layers.get(layer).map(|l| &l.w_down),
        (None, TensorKey::LmHead) => Some(&model.output_weight),
        _ => None,
    }
}

/// Rows `[start, end)` of `weights` applied by the coordinator. Validate the
/// geometry before allocating so a malformed plan cannot request a huge vec.
pub(crate) fn rows_of(
    weights: &I8Weights,
    start: usize,
    end: usize,
    input: &[i64],
) -> Result<Vec<i64>, TensorParallelError> {
    if start >= end
        || end > weights.n_rows
        || weights.n_cols == 0
        || input.len() != weights.n_cols
        || weights.n_rows.checked_mul(weights.n_cols) != Some(weights.data.len())
        || weights.scales.len() != weights.n_rows
    {
        return Err(TensorParallelError::WrongShape);
    }
    let mut values = vec![0; end - start];
    matmul_i8_canonical_row_range(weights, start, end, input, &mut values)
        .map_err(|_| TensorParallelError::WrongShape)?;
    Ok(values)
}

/// The remote frame path retains its assignment-scoped row copy.
fn rows_of_copy(
    weights: &I8Weights,
    start: usize,
    end: usize,
    input: &[i64],
) -> Result<Vec<i64>, TensorParallelError> {
    let shard = weights
        .copy_rows(start, end)
        .map_err(|_| TensorParallelError::WrongShape)?;
    if shard.n_cols != input.len() {
        return Err(TensorParallelError::WrongShape);
    }
    let mut values = vec![0; shard.n_rows];
    matmul_i8_canonical_rows(&shard, input, &mut values)
        .map_err(|_| TensorParallelError::WrongShape)?;
    Ok(values)
}

/// A contiguous row range of ONE canonical tensor, held by a worker that does
/// not hold the model.
///
/// This is the whole resident state a row worker needs: the i8 rows, their
/// per-row scales and the column count, plus the identity those rows belong
/// to. No embeddings, norms, vocabulary, KV cache or `ModelConfig` - a
/// full-model worker carries well over a gigabyte of those and never reads
/// them while serving.
pub struct RowShard {
    pub artifact: Hash256,
    pub profile: String,
    pub layer: Option<usize>,
    pub tensor: TensorKey,
    /// The canonical row range these weights are, half-open.
    pub row_start: usize,
    pub row_end: usize,
    /// Exactly the held rows, already in canonical (post-permutation) order.
    pub weights: I8Weights,
}

/// Answer one projection from held shards, serving any SUB-RANGE of a shard.
///
/// Placement decides row ranges when a request arrives and cannot know, when
/// the weights were placed, how it will later divide them. A worker that
/// insisted on bounds equal to what it holds would force a re-export for every
/// re-placement, which is what kept memory-bounded workers out of automatic
/// placement.
///
/// `worker_id` is deliberately not matched. It is placement's label for a
/// machine, not an identity: the machine is authenticated by its transport,
/// and these weights are identified by artifact, profile, layer, tensor and
/// row range, every one of which is still matched exactly.
///
/// The arithmetic is the canonical kernel, not a second implementation of it,
/// so a shard's answer is bit-identical to the same rows computed by a
/// coordinator holding the whole model.
pub fn project_from_shards(
    shards: &[RowShard],
    request: &RowProjectionRequest,
) -> Result<Vec<i64>, TensorParallelError> {
    let assignment = &request.assignment;
    if assignment.row_start >= assignment.row_end {
        return Err(TensorParallelError::WrongShape);
    }
    let shard = shards
        .iter()
        .find(|shard| {
            shard.artifact == assignment.artifact_id
                && shard.profile == assignment.execution_profile
                && shard.layer == assignment.layer
                && shard.tensor == assignment.tensor
                && shard.row_start <= assignment.row_start
                && assignment.row_end <= shard.row_end
        })
        .ok_or(TensorParallelError::WrongIdentity)?;
    let offset = assignment.row_start - shard.row_start;
    let rows = assignment.row_end - assignment.row_start;
    let end = offset
        .checked_add(rows)
        .ok_or(TensorParallelError::WrongShape)?;
    if end > shard.weights.n_rows {
        return Err(TensorParallelError::WrongShape);
    }
    rows_of(&shard.weights, offset, end, &request.input)
}

/// Serve row projections of a resident canonical model over length-prefixed
/// frames: the private-cohort protocol [`SshStdioRowWorker`] speaks, for any
/// assignment of this artifact and profile. For an operator's own machine
/// that holds the whole artifact (the `tensor_row_model_worker` example), so
/// a coordinator can place any rows on it. Returns at a clean end of input;
/// the first malformed or mismatched request ends the session with an error,
/// which the coordinator sees as a closed worker.
pub fn serve_row_frames(
    model: &crate::cached_integer_model::CachedIntegerModel,
    artifact_id: Hash256,
    input: &mut impl std::io::Read,
    output: &mut impl std::io::Write,
) -> Result<(), TensorParallelError> {
    let io_error =
        |what: &str, error: std::io::Error| TensorParallelError::Worker(format!("{what}: {error}"));
    let profile = model
        .canonical_execution_profile()
        .ok_or(TensorParallelError::WrongIdentity)?;
    loop {
        let mut length = [0u8; 4];
        let first = loop {
            match input.read(&mut length[..1]) {
                Ok(read) => break read,
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
                Err(error) => return Err(io_error("read frame length", error)),
            }
        };
        if first == 0 {
            return Ok(());
        }
        input
            .read_exact(&mut length[1..])
            .map_err(|error| io_error("read frame length", error))?;
        let length = u32::from_le_bytes(length) as usize;
        if length == 0 || length > MAX_ROW_FRAME_BYTES {
            return Err(TensorParallelError::Bounds);
        }
        let mut frame = vec![0u8; length];
        input
            .read_exact(&mut frame)
            .map_err(|error| io_error("read frame", error))?;
        let request = decode_row_request(&frame)?;
        let a = &request.assignment;
        if a.artifact_id != artifact_id || a.execution_profile != profile {
            return Err(TensorParallelError::WrongIdentity);
        }
        if hash_i64(&request.input) != request.input_hash {
            return Err(TensorParallelError::WrongCall);
        }
        let weights = model_projection_weights(model, a.layer, a.tensor)
            .ok_or(TensorParallelError::WrongIdentity)?;
        let values = rows_of_copy(weights, a.row_start, a.row_end, &request.input)?;
        let response = RowProjectionResponse {
            call_id: request.call_id,
            input_hash: request.input_hash,
            assignment: request.assignment,
            values,
        };
        let frame = encode_row_response(&response)?;
        output
            .write_all(&(frame.len() as u32).to_le_bytes())
            .and_then(|()| output.write_all(&frame))
            .and_then(|()| output.flush())
            .map_err(|error| io_error("write frame", error))?;
    }
}

#[cfg(unix)]
fn set_nonblocking_stdin(writer: &ChildStdin) -> Result<(), String> {
    use std::os::fd::AsRawFd;
    let fd = writer.as_raw_fd();
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
        return Err("fcntl O_NONBLOCK failed".into());
    }
    Ok(())
}
#[cfg(not(unix))]
fn set_nonblocking_stdin(_: &ChildStdin) -> Result<(), String> {
    Err("SSH row worker requires Unix nonblocking pipe support".into())
}

#[cfg(unix)]
fn write_timeout(writer: &mut ChildStdin, bytes: &[u8], until: Instant) -> Result<(), String> {
    use std::io::Write;
    use std::os::fd::AsRawFd;
    let mut offset = 0;
    while offset < bytes.len() {
        let remain = until
            .checked_duration_since(Instant::now())
            .ok_or_else(|| "SSH row request timed out".to_string())?;
        let mut fd = libc::pollfd {
            fd: writer.as_raw_fd(),
            events: libc::POLLOUT,
            revents: 0,
        };
        let ready =
            unsafe { libc::poll(&mut fd, 1, remain.as_millis().min(i32::MAX as u128) as i32) };
        if ready <= 0 {
            return Err(if ready == 0 {
                "SSH row request timed out".into()
            } else {
                "poll SSH row request failed".into()
            });
        }
        match writer.write(&bytes[offset..]) {
            Ok(0) => return Err("SSH row worker closed stdin".into()),
            Ok(n) => offset += n,
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => continue,
            Err(e) => return Err(format!("write SSH row request: {e}")),
        }
    }
    Ok(())
}
#[cfg(not(unix))]
fn write_timeout(_: &mut ChildStdin, _: &[u8], _: Instant) -> Result<(), String> {
    Err("SSH row worker requires Unix nonblocking pipe support".into())
}

#[cfg(unix)]
fn read_framed_timeout(reader: &mut ChildStdout, until: Instant) -> Result<Vec<u8>, String> {
    fn exact(reader: &mut ChildStdout, out: &mut [u8], until: Instant) -> Result<(), String> {
        use std::io::Read;
        use std::os::fd::AsRawFd;
        let mut offset = 0;
        while offset < out.len() {
            let remain = until
                .checked_duration_since(Instant::now())
                .ok_or_else(|| "SSH row response timed out".to_string())?;
            let mut fd = libc::pollfd {
                fd: reader.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            let ms = remain.as_millis().min(i32::MAX as u128) as i32;
            let ready = unsafe { libc::poll(&mut fd, 1, ms) };
            if ready <= 0 {
                return Err(if ready == 0 {
                    "SSH row response timed out".into()
                } else {
                    "poll SSH row response failed".into()
                });
            }
            let read = reader
                .read(&mut out[offset..])
                .map_err(|e| format!("read SSH row response: {e}"))?;
            if read == 0 {
                return Err("SSH row worker closed stdout".into());
            }
            offset += read;
        }
        Ok(())
    }
    let mut len = [0u8; 4];
    exact(reader, &mut len, until)?;
    let len = u32::from_le_bytes(len) as usize;
    if len == 0 || len > 4 * 1024 * 1024 {
        return Err("SSH row response frame exceeds bound".into());
    }
    let mut raw = vec![0; len];
    exact(reader, &mut raw, until)?;
    Ok(raw)
}
#[cfg(not(unix))]
fn read_framed_timeout(_: &mut ChildStdout, _: Instant) -> Result<Vec<u8>, String> {
    Err("SSH row worker requires Unix poll timeout support".into())
}

/// Coordinator-side backend for the fallible forward adapter.  Workers run
/// concurrently for a stage; deterministic merge remains local and happens
/// only after every bounded response has passed identity, call, shape, and
/// exact-coverage checks.
pub struct PartitionedProjectionBackend {
    artifact_id: Hash256,
    profile: String,
    assignments: BTreeMap<(Option<usize>, TensorKey), Vec<RowAssignment>>,
    workers: BTreeMap<String, Arc<dyn RowWorker>>,
}

impl PartitionedProjectionBackend {
    pub fn new(
        artifact_id: Hash256,
        profile: String,
        assignments: Vec<RowAssignment>,
        workers: BTreeMap<String, Arc<dyn RowWorker>>,
    ) -> Self {
        let mut by_tensor: BTreeMap<(Option<usize>, TensorKey), Vec<RowAssignment>> =
            BTreeMap::new();
        for assignment in assignments {
            by_tensor
                .entry((assignment.layer, assignment.tensor))
                .or_default()
                .push(assignment);
        }
        Self {
            artifact_id,
            profile,
            assignments: by_tensor,
            workers,
        }
    }

    pub fn project(
        &self,
        call_id: Hash256,
        layer: Option<usize>,
        tensor: TensorKey,
        input: &[i64],
        output_rows: usize,
    ) -> Result<Vec<i64>, TensorParallelError> {
        if input.len() > MAX_ROW_INPUT_ELEMENTS || output_rows > MAX_ROW_OUTPUT_ELEMENTS {
            return Err(TensorParallelError::Bounds);
        }
        let assignments = self.assignments.get(&(layer, tensor)).ok_or(
            TensorParallelError::IncompleteCoverage {
                expected: output_rows,
                covered: 0,
            },
        )?;
        let ordered = validate_exact_coverage(
            assignments,
            self.artifact_id,
            &self.profile,
            layer,
            tensor,
            output_rows,
        )?;
        let input_hash = hash_i64(input);
        let results: Vec<Result<RowProjectionResponse, TensorParallelError>> =
            std::thread::scope(|scope| {
                let mut joins = Vec::with_capacity(ordered.len());
                for assignment in &ordered {
                    let worker = self.workers.get(&assignment.worker_id).ok_or_else(|| {
                        TensorParallelError::MissingWorker(assignment.worker_id.clone())
                    })?;
                    let req = RowProjectionRequest {
                        call_id,
                        input_hash,
                        assignment: assignment.clone(),
                        input: input.to_vec(),
                    };
                    joins.push(scope.spawn(move || worker.project(req)));
                }
                Ok::<_, TensorParallelError>(
                    joins
                        .into_iter()
                        .map(|j| {
                            j.join()
                                .map_err(|_| {
                                    TensorParallelError::Worker("worker thread panicked".into())
                                })
                                .and_then(|r| r)
                        })
                        .collect(),
                )
            })?;
        let mut responses = Vec::with_capacity(results.len());
        for response in results {
            responses.push(response?);
        }
        responses.sort_by_key(|r| r.assignment.row_start);
        let mut values = Vec::with_capacity(output_rows);
        for (expected, response) in ordered.iter().zip(responses) {
            if response.call_id != call_id || response.input_hash != input_hash {
                return Err(TensorParallelError::WrongCall);
            }
            if response.assignment != *expected || response.values.len() != expected.rows() {
                return Err(TensorParallelError::WrongShape);
            }
            values.extend(response.values);
        }
        if values.len() != output_rows {
            return Err(TensorParallelError::WrongShape);
        }
        Ok(values)
    }
}

impl ProjectionBackend for PartitionedProjectionBackend {
    fn project_rows(
        &self,
        call_id: Hash256,
        layer: Option<usize>,
        tensor: TensorKey,
        input: &[i64],
        output_rows: usize,
    ) -> Result<Vec<i64>, TensorParallelError> {
        self.project(call_id, layer, tensor, input, output_rows)
    }
}

/// Who computes one slice of a placed projection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SliceOwner {
    /// The coordinator, from its own resident rows.
    Local,
    /// A pinned worker, by its transport identity.
    Remote(String),
}

/// One slice of a placed projection, and how it is checked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlannedSlice {
    pub owner: SliceOwner,
    pub row_start: usize,
    pub row_end: usize,
    /// A second participant that recomputes this slice at every call. Any
    /// difference is a fault (a duplicate check).
    pub duplicate_on: Option<SliceOwner>,
}

/// A placed projection: slices in row order that cover every output row
/// once, and the rows the coordinator recomputes itself at every call (spot
/// checks).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ProjectionPlan {
    pub slices: Vec<PlannedSlice>,
    pub spot_rows: Vec<usize>,
}

/// What the coordinator saw a remote worker do. Every fallback and every
/// fault is reported; none is silent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RowEvent {
    /// The worker answered a call: a timing sample for link measurement.
    Answered {
        worker: String,
        rows: usize,
        bytes: usize,
        elapsed: Duration,
    },
    /// The worker is excluded for the epoch, or its connection is closed, so
    /// the coordinator computed its slice without calling it. Reported, but
    /// not a failure: a closed connection's calls would all fail at once.
    Skipped {
        worker: String,
        layer: Option<usize>,
        tensor: TensorKey,
    },
    /// The call failed, timed out or was refused; the coordinator computed
    /// the slice from its own rows.
    Fallback {
        worker: String,
        layer: Option<usize>,
        tensor: TensorKey,
        error: String,
    },
    /// Strict coordinator refused a transport/identity/shape failure. No
    /// local fallback was performed.
    Refused {
        worker: String,
        layer: Option<usize>,
        tensor: TensorKey,
        error: String,
    },
    /// Exact arithmetic disagreed on a checked slice or row; the coordinator
    /// recomputed the slice and used its own rows.
    Fault {
        worker: String,
        layer: Option<usize>,
        tensor: TensorKey,
        row_start: usize,
        row_end: usize,
        expected: Hash256,
        found: Hash256,
    },
}

/// Receives a backend's events and says which workers to skip.
pub trait RowEventSink: Send + Sync {
    fn record(&self, event: RowEvent);
    /// Excluded for the rest of the epoch: its slices are computed locally.
    fn is_excluded(&self, worker: &str) -> bool;
}

/// Row-partitioned projections with exact checks and per-call fallback (S5,
/// S7 and S8 in the running node). A remote slice that fails is computed from
/// the coordinator's own rows. A duplicate or spot check that disagrees makes
/// the coordinator recompute the slice and use its own rows. Both are
/// reported through the sink, so the output equals the local forward whenever
/// every row that fed it passed its checks or was computed locally. Rows that
/// no check covered are trusted, which is why only this operator's own
/// machines may feed a vote (docs/design/assignment-node-integration.md,
/// trust model).
pub struct VerifiedPartitionBackend<'a> {
    source: &'a dyn crate::low_residency::CanonicalRowSource,
    strict: bool,
    artifact_id: Hash256,
    profile: String,
    plans: BTreeMap<(Option<usize>, TensorKey), ProjectionPlan>,
    workers: BTreeMap<String, Arc<dyn RowWorker>>,
    events: &'a dyn RowEventSink,
}

impl<'a> VerifiedPartitionBackend<'a> {
    pub fn new(
        model: &'a crate::cached_integer_model::CachedIntegerModel,
        artifact_id: Hash256,
        plans: BTreeMap<(Option<usize>, TensorKey), ProjectionPlan>,
        workers: BTreeMap<String, Arc<dyn RowWorker>>,
        events: &'a dyn RowEventSink,
    ) -> Result<Self, TensorParallelError> {
        let profile = model
            .canonical_execution_profile()
            .ok_or(TensorParallelError::WrongIdentity)?
            .to_string();
        Ok(Self {
            source: model,
            strict: false,
            artifact_id,
            profile,
            plans,
            workers,
            events,
        })
    }

    /// All primary slices must be remote. Local rows remain available only
    /// for the certificate's numerical checks. A failed check or transport
    /// refuses the request; no primary slice is recomputed as a fallback.
    pub fn new_strict(
        source: &'a dyn crate::low_residency::CanonicalRowSource,
        artifact_id: Hash256,
        plans: BTreeMap<(Option<usize>, TensorKey), ProjectionPlan>,
        workers: BTreeMap<String, Arc<dyn RowWorker>>,
        events: &'a dyn RowEventSink,
    ) -> Result<Self, TensorParallelError> {
        let profile = source
            .canonical_execution_profile()
            .ok_or(TensorParallelError::WrongIdentity)?
            .to_string();
        // Validate every required stage before the first token is forwarded.
        let keys = (0..source.config().n_layers)
            .flat_map(|layer| {
                [
                    TensorKey::Wq,
                    TensorKey::Wk,
                    TensorKey::Wv,
                    TensorKey::Wo,
                    TensorKey::WGate,
                    TensorKey::WUp,
                    TensorKey::WDown,
                ]
                .into_iter()
                .map(move |tensor| (Some(layer), tensor))
            })
            .chain(std::iter::once((None, TensorKey::LmHead)));
        for (layer, tensor) in keys {
            let (rows, _) = source
                .projection_shape(layer, tensor)
                .ok_or(TensorParallelError::WrongShape)?;
            let plan =
                plans
                    .get(&(layer, tensor))
                    .ok_or(TensorParallelError::IncompleteCoverage {
                        expected: rows,
                        covered: 0,
                    })?;
            let assignments = plan
                .slices
                .iter()
                .map(|slice| {
                    let SliceOwner::Remote(worker) = &slice.owner else {
                        return Err(TensorParallelError::WrongIdentity);
                    };
                    if events.is_excluded(worker)
                        || !workers.get(worker).is_some_and(|w| w.is_open())
                    {
                        return Err(TensorParallelError::MissingWorker(worker.clone()));
                    }
                    Ok(RowAssignment {
                        artifact_id,
                        execution_profile: profile.clone(),
                        layer,
                        tensor,
                        row_start: slice.row_start,
                        row_end: slice.row_end,
                        worker_id: worker.clone(),
                    })
                })
                .collect::<Result<Vec<_>, _>>()?;
            validate_exact_coverage(&assignments, artifact_id, &profile, layer, tensor, rows)?;
            if plan.spot_rows.is_empty() || plan.spot_rows.iter().any(|&row| row >= rows) {
                return Err(TensorParallelError::WrongShape);
            }
            for slice in &plan.slices {
                if let Some(SliceOwner::Remote(checker)) = &slice.duplicate_on {
                    if events.is_excluded(checker)
                        || !workers.get(checker).is_some_and(|w| w.is_open())
                    {
                        return Err(TensorParallelError::MissingWorker(checker.clone()));
                    }
                }
            }
        }
        Ok(Self {
            source,
            strict: true,
            artifact_id,
            profile,
            plans,
            workers,
            events,
        })
    }

    fn local(
        &self,
        layer: Option<usize>,
        tensor: TensorKey,
        start: usize,
        end: usize,
        input: &[i64],
    ) -> Result<Vec<i64>, TensorParallelError> {
        self.source
            .projection_rows(layer, tensor, start, end, input)
    }

    #[allow(clippy::too_many_arguments)]
    fn remote(
        &self,
        worker: &str,
        call_id: Hash256,
        input_hash: Hash256,
        layer: Option<usize>,
        tensor: TensorKey,
        start: usize,
        end: usize,
        input: &[i64],
    ) -> Result<Vec<i64>, TensorParallelError> {
        let client = self
            .workers
            .get(worker)
            .ok_or_else(|| TensorParallelError::MissingWorker(worker.to_string()))?;
        let assignment = RowAssignment {
            artifact_id: self.artifact_id,
            execution_profile: self.profile.clone(),
            layer,
            tensor,
            row_start: start,
            row_end: end,
            worker_id: worker.to_string(),
        };
        let started = Instant::now();
        let response = client.project(RowProjectionRequest {
            call_id,
            input_hash,
            assignment: assignment.clone(),
            input: input.to_vec(),
        })?;
        if response.call_id != call_id || response.input_hash != input_hash {
            return Err(TensorParallelError::WrongCall);
        }
        if response.assignment != assignment || response.values.len() != end - start {
            return Err(TensorParallelError::WrongShape);
        }
        self.events.record(RowEvent::Answered {
            worker: worker.to_string(),
            rows: end - start,
            bytes: (input.len() + (end - start)) * std::mem::size_of::<i64>(),
            elapsed: started.elapsed(),
        });
        Ok(response.values)
    }

    /// One participant's rows for a slice: `Ok(None)` when a remote call
    /// failed (reported as a fallback), so the caller computes it locally.
    #[allow(clippy::too_many_arguments)]
    fn attempt(
        &self,
        owner: &SliceOwner,
        call_id: Hash256,
        input_hash: Hash256,
        layer: Option<usize>,
        tensor: TensorKey,
        start: usize,
        end: usize,
        input: &[i64],
    ) -> Result<Option<Vec<i64>>, TensorParallelError> {
        match owner {
            SliceOwner::Local => self.local(layer, tensor, start, end, input).map(Some),
            SliceOwner::Remote(worker)
                if self.events.is_excluded(worker)
                    || !self
                        .workers
                        .get(worker)
                        .is_some_and(|client| client.is_open()) =>
            {
                self.events.record(RowEvent::Skipped {
                    worker: worker.clone(),
                    layer,
                    tensor,
                });
                if self.strict {
                    Err(TensorParallelError::Closed)
                } else {
                    Ok(None)
                }
            }
            SliceOwner::Remote(worker) => {
                match self.remote(
                    worker, call_id, input_hash, layer, tensor, start, end, input,
                ) {
                    Ok(values) => Ok(Some(values)),
                    // The connection closed between the check and the call:
                    // nothing was sent, so this is a skip, not a failure.
                    Err(TensorParallelError::Closed) => {
                        self.events.record(RowEvent::Skipped {
                            worker: worker.clone(),
                            layer,
                            tensor,
                        });
                        if self.strict {
                            Err(TensorParallelError::Closed)
                        } else {
                            Ok(None)
                        }
                    }
                    Err(error) => {
                        if self.strict {
                            self.events.record(RowEvent::Refused {
                                worker: worker.clone(),
                                layer,
                                tensor,
                                error: error.to_string(),
                            });
                            return Err(error);
                        }
                        self.events.record(RowEvent::Fallback {
                            worker: worker.clone(),
                            layer,
                            tensor,
                            error: error.to_string(),
                        });
                        Ok(None)
                    }
                }
            }
        }
    }

    /// A slice's rows after its checks.
    #[allow(clippy::too_many_arguments)]
    fn slice_rows(
        &self,
        slice: &PlannedSlice,
        spot_rows: &[usize],
        call_id: Hash256,
        input_hash: Hash256,
        layer: Option<usize>,
        tensor: TensorKey,
        input: &[i64],
    ) -> Result<Vec<i64>, TensorParallelError> {
        let (start, end) = (slice.row_start, slice.row_end);
        let Some(values) = self.attempt(
            &slice.owner,
            call_id,
            input_hash,
            layer,
            tensor,
            start,
            end,
            input,
        )?
        else {
            return self.local(layer, tensor, start, end, input);
        };
        let SliceOwner::Remote(worker) = &slice.owner else {
            // The coordinator's own rows need no check.
            return Ok(values);
        };
        let fault = |expected: &[i64]| {
            self.events.record(RowEvent::Fault {
                worker: worker.clone(),
                layer,
                tensor,
                row_start: start,
                row_end: end,
                expected: hash_i64(expected),
                found: hash_i64(&values),
            });
        };
        if let Some(checker) = &slice.duplicate_on {
            let second = self.attempt(
                checker, call_id, input_hash, layer, tensor, start, end, input,
            )?;
            if second.as_deref() != Some(values.as_slice()) {
                if self.strict {
                    // A bounded local check identifies faulty participants,
                    // but its result can never replace the failed primary.
                    let own = self.local(layer, tensor, start, end, input)?;
                    if own != values {
                        fault(&own);
                    }
                    if let (Some(second), SliceOwner::Remote(checker_id)) = (&second, checker)
                        && *second != own
                    {
                        self.events.record(RowEvent::Fault {
                            worker: checker_id.clone(),
                            layer,
                            tensor,
                            row_start: start,
                            row_end: end,
                            expected: hash_i64(&own),
                            found: hash_i64(second),
                        });
                    }
                    return Err(TensorParallelError::Worker(
                        "duplicate row verification disagreed; request refused".into(),
                    ));
                }
                // Disagreement, or no second answer: the coordinator's own
                // rows decide, and are what the forward uses.
                let own = self.local(layer, tensor, start, end, input)?;
                if own != values {
                    fault(&own);
                }
                if let (Some(second), SliceOwner::Remote(checker_id)) = (&second, checker)
                    && *second != own
                {
                    self.events.record(RowEvent::Fault {
                        worker: checker_id.clone(),
                        layer,
                        tensor,
                        row_start: start,
                        row_end: end,
                        expected: hash_i64(&own),
                        found: hash_i64(second),
                    });
                }
                return Ok(own);
            }
        }
        for &row in spot_rows.iter().filter(|row| (start..end).contains(*row)) {
            let own_row = self.local(layer, tensor, row, row + 1, input)?;
            if own_row[0] != values[row - start] {
                if self.strict {
                    self.events.record(RowEvent::Fault {
                        worker: worker.clone(),
                        layer,
                        tensor,
                        row_start: row,
                        row_end: row + 1,
                        expected: hash_i64(&own_row),
                        found: hash_i64(&values[row - start..row - start + 1]),
                    });
                    return Err(TensorParallelError::Worker(
                        "spot row verification disagreed; request refused".into(),
                    ));
                }
                let own = self.local(layer, tensor, start, end, input)?;
                fault(&own);
                return Ok(own);
            }
        }
        Ok(values)
    }
}

impl ProjectionBackend for VerifiedPartitionBackend<'_> {
    fn project_rows(
        &self,
        call_id: Hash256,
        layer: Option<usize>,
        tensor: TensorKey,
        input: &[i64],
        output_rows: usize,
    ) -> Result<Vec<i64>, TensorParallelError> {
        if input.len() > MAX_ROW_INPUT_ELEMENTS || output_rows > MAX_ROW_OUTPUT_ELEMENTS {
            return Err(TensorParallelError::Bounds);
        }
        let plan =
            self.plans
                .get(&(layer, tensor))
                .ok_or(TensorParallelError::IncompleteCoverage {
                    expected: output_rows,
                    covered: 0,
                })?;
        let mut cursor = 0usize;
        for slice in &plan.slices {
            if slice.row_end <= slice.row_start {
                return Err(TensorParallelError::InvalidRange {
                    start: slice.row_start,
                    end: slice.row_end,
                });
            }
            if slice.row_start != cursor {
                return Err(TensorParallelError::Overlap {
                    row: slice.row_start,
                });
            }
            cursor = slice.row_end;
        }
        if cursor != output_rows {
            return Err(TensorParallelError::IncompleteCoverage {
                expected: output_rows,
                covered: cursor,
            });
        }
        let input_hash = hash_i64(input);
        let results: Vec<Result<Vec<i64>, TensorParallelError>> = std::thread::scope(|scope| {
            let joins: Vec<_> = plan
                .slices
                .iter()
                .map(|slice| {
                    scope.spawn(move || {
                        self.slice_rows(
                            slice,
                            &plan.spot_rows,
                            call_id,
                            input_hash,
                            layer,
                            tensor,
                            input,
                        )
                    })
                })
                .collect();
            joins
                .into_iter()
                .map(|join| {
                    join.join()
                        .map_err(|_| TensorParallelError::Worker("slice task panicked".into()))
                        .and_then(|rows| rows)
                })
                .collect()
        });
        let mut values = Vec::with_capacity(output_rows);
        for rows in results {
            values.extend(rows?);
        }
        Ok(values)
    }
}

pub fn hash_i64(values: &[i64]) -> Hash256 {
    let mut hasher = blake3::Hasher::new();
    for value in values {
        hasher.update(&value.to_le_bytes());
    }
    Hash256(*hasher.finalize().as_bytes())
}

/// Export one already-canonicalized tensor range for a private sidecar.  The
/// source `I8Weights` must come from the corrected canonical loader, so Q/K
/// row order and their scales are exported together after permutation.
/// This file is data-only; it contains no model-wide embedding, norms, cache,
/// tokenizer, or public endpoint configuration.
pub(crate) fn export_canonical_row_file(
    path: impl AsRef<Path>,
    assignment: &RowAssignment,
    weights: &I8Weights,
) -> Result<(), TensorParallelError> {
    if assignment.rows() != weights.n_rows
        || weights.n_cols == 0
        || weights.data.len() != weights.n_rows.saturating_mul(weights.n_cols)
        || weights.scales.len() != weights.n_rows
    {
        return Err(TensorParallelError::WrongShape);
    }
    let payload = weights
        .data
        .len()
        .checked_add(weights.scales.len().saturating_mul(8))
        .ok_or(TensorParallelError::Bounds)?;
    if payload > MAX_CANONICAL_ROW_FILE_BYTES {
        return Err(TensorParallelError::Bounds);
    }
    let profile = assignment.execution_profile.as_bytes();
    let worker = assignment.worker_id.as_bytes();
    if profile.len() > u16::MAX as usize || worker.len() > u16::MAX as usize {
        return Err(TensorParallelError::Bounds);
    }
    let mut file =
        std::fs::File::create(path).map_err(|e| TensorParallelError::Worker(e.to_string()))?;
    file.write_all(ROW_FILE_MAGIC)
        .and_then(|_| file.write_all(&assignment.artifact_id.0))
        .and_then(|_| file.write_all(&(profile.len() as u16).to_le_bytes()))
        .and_then(|_| file.write_all(profile))
        .and_then(|_| {
            file.write_all(&(assignment.layer.map(|v| v as i64).unwrap_or(-1)).to_le_bytes())
        })
        .and_then(|_| file.write_all(&[assignment.tensor as u8]))
        .and_then(|_| file.write_all(&(assignment.row_start as u64).to_le_bytes()))
        .and_then(|_| file.write_all(&(assignment.row_end as u64).to_le_bytes()))
        .and_then(|_| file.write_all(&(worker.len() as u16).to_le_bytes()))
        .and_then(|_| file.write_all(worker))
        .and_then(|_| file.write_all(&(weights.n_cols as u64).to_le_bytes()))
        .and_then(|_| file.write_all(&(weights.n_rows as u64).to_le_bytes()))
        .map_err(|e| TensorParallelError::Worker(e.to_string()))?;
    for scale in &weights.scales {
        file.write_all(&scale.to_le_bytes())
            .map_err(|e| TensorParallelError::Worker(e.to_string()))?;
    }
    let data: Vec<u8> = weights.data.iter().map(|v| *v as u8).collect();
    file.write_all(&data)
        .map_err(|e| TensorParallelError::Worker(e.to_string()))?;
    Ok(())
}

/// Export from the actual resident canonical model, binding the file to the
/// caller's independently verified artifact commitment and the model's own
/// versioned arithmetic profile.  Callers cannot label an arbitrary matrix as
/// a canonical tensor export through the public API.
pub fn export_verified_model_rows(
    path: impl AsRef<Path>,
    model: &crate::cached_integer_model::CachedIntegerModel,
    verified_artifact_id: Hash256,
    assignment: &RowAssignment,
) -> Result<(), TensorParallelError> {
    if assignment.artifact_id != verified_artifact_id
        || model.canonical_execution_profile() != Some(assignment.execution_profile.as_str())
    {
        return Err(TensorParallelError::WrongIdentity);
    }
    let weights = match (assignment.layer, assignment.tensor) {
        (Some(layer), TensorKey::Wq) => model.layers.get(layer).map(|l| &l.wq),
        (Some(layer), TensorKey::Wk) => model.layers.get(layer).map(|l| &l.wk),
        (Some(layer), TensorKey::Wv) => model.layers.get(layer).map(|l| &l.wv),
        (Some(layer), TensorKey::Wo) => model.layers.get(layer).map(|l| &l.wo),
        (Some(layer), TensorKey::WGate) => model.layers.get(layer).map(|l| &l.w_gate),
        (Some(layer), TensorKey::WUp) => model.layers.get(layer).map(|l| &l.w_up),
        (Some(layer), TensorKey::WDown) => model.layers.get(layer).map(|l| &l.w_down),
        (None, TensorKey::LmHead) => Some(&model.output_weight),
        _ => None,
    }
    .ok_or(TensorParallelError::WrongIdentity)?;
    let rows = weights
        .copy_rows(assignment.row_start, assignment.row_end)
        .map_err(|_| TensorParallelError::WrongShape)?;
    export_canonical_row_file(path, assignment, &rows)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn row_frames_round_trip_in_both_directions() {
        let input = vec![3, -4, 5];
        let request = RowProjectionRequest {
            call_id: Hash256([1; 32]),
            input_hash: hash_i64(&input),
            assignment: RowAssignment {
                artifact_id: Hash256([7; 32]),
                execution_profile: "canonical".into(),
                layer: Some(2),
                tensor: TensorKey::Wq,
                row_start: 2,
                row_end: 5,
                worker_id: "worker-a".into(),
            },
            input,
        };
        let frame = encode_row_request(&request).unwrap();
        let decoded = decode_row_request(&frame).unwrap();
        assert_eq!(
            (
                decoded.call_id,
                decoded.input_hash,
                &decoded.assignment,
                &decoded.input
            ),
            (
                request.call_id,
                request.input_hash,
                &request.assignment,
                &request.input
            )
        );
        assert!(decode_row_request(&frame[..frame.len() - 1]).is_err());
        let mut longer = frame.clone();
        longer.push(0);
        assert!(decode_row_request(&longer).is_err());

        let response = RowProjectionResponse {
            call_id: request.call_id,
            input_hash: request.input_hash,
            assignment: request.assignment.clone(),
            values: vec![7, 8, -9],
        };
        let decoded = decode_row_response(&encode_row_response(&response).unwrap()).unwrap();
        assert_eq!(
            (
                decoded.call_id,
                decoded.input_hash,
                &decoded.assignment,
                &decoded.values
            ),
            (
                response.call_id,
                response.input_hash,
                &response.assignment,
                &response.values
            )
        );
        // A response must carry exactly its assignment's rows.
        let mut short = response.clone();
        short.values.pop();
        assert!(encode_row_response(&short).is_err());
        // A request frame is not a response frame, nor the other way round,
        // even when its payload has exactly the assignment's row count.
        assert!(decode_row_response(&frame).is_err());
        assert!(decode_row_request(&encode_row_response(&response).unwrap()).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn the_remote_command_reaches_the_remote_shell_exactly() {
        // OpenSSH hands the remote shell one string; a POSIX shell parses it
        // back. Every argument must come back byte for byte, and nothing in
        // one may run: apostrophes, command substitution, quotes, spaces.
        let args: Vec<String> = [
            "/opt/arc/tensor_row_model_worker",
            "--model",
            "/data/it's a model.gguf",
            "x';echo INJECTED;'",
            "$(echo substituted)",
            "a\"b",
            "''",
        ]
        .iter()
        .map(|arg| arg.to_string())
        .collect();
        let script = format!("printf '%s\\n' {}", remote_shell_command(&args));
        let output = std::process::Command::new("sh")
            .arg("-c")
            .arg(script)
            .output()
            .unwrap();
        assert!(output.status.success());
        let printed: Vec<String> = String::from_utf8(output.stdout)
            .unwrap()
            .lines()
            .map(str::to_string)
            .collect();
        assert_eq!(printed, args);
    }

    /// A throwaway "ssh" program: it ignores its arguments, waits
    /// `delay_secs`, writes `replies` (length-prefixed frames) to stdout and
    /// then holds the session open. Returns the worker and the directory.
    #[cfg(unix)]
    fn fake_ssh_worker(
        name: &str,
        replies: &[u8],
        delay_secs: u32,
        timeout: Duration,
    ) -> (SshStdioRowWorker, std::path::PathBuf) {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("arc-row-{name}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let known_hosts = dir.join("known_hosts");
        std::fs::write(&known_hosts, "").unwrap();
        let replies_path = dir.join("replies");
        std::fs::write(&replies_path, replies).unwrap();
        let program = dir.join("fake-ssh");
        std::fs::write(
            &program,
            format!(
                "#!/bin/sh\nsleep {delay_secs}\ncat '{}'\nexec sleep 30\n",
                replies_path.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o700)).unwrap();
        let worker = SshStdioRowWorker::connect(
            name.into(),
            SshStdioConfig {
                ssh_program: program,
                target: "worker.invalid".into(),
                known_hosts,
                remote_command: vec!["/opt/arc/tensor_row_model_worker".into()],
                timeout,
            },
        )
        .unwrap();
        (worker, dir)
    }

    #[cfg(unix)]
    fn framed(response: &RowProjectionResponse, copies: usize) -> Vec<u8> {
        let frame = encode_row_response(response).unwrap();
        let mut out = Vec::new();
        for _ in 0..copies {
            out.extend_from_slice(&(frame.len() as u32).to_le_bytes());
            out.extend_from_slice(&frame);
        }
        out
    }

    #[cfg(unix)]
    #[test]
    fn a_replayed_answer_is_refused_and_the_session_closes() {
        // A misbehaving worker answers the first call, then sends the same
        // answer again. The next call reads that copy: it binds another call,
        // so it is refused, the session is closed, and nothing further is
        // sent or read.
        let a = assignment(0, 2, "replay");
        let input = vec![3i64, -4];
        let first = RowProjectionRequest {
            call_id: id(1),
            input_hash: hash_i64(&input),
            assignment: a.clone(),
            input,
        };
        let answer = RowProjectionResponse {
            call_id: first.call_id,
            input_hash: first.input_hash,
            assignment: a,
            values: vec![5, 6],
        };
        let (worker, dir) =
            fake_ssh_worker("replay", &framed(&answer, 2), 0, Duration::from_secs(10));
        assert_eq!(worker.project(first.clone()).unwrap().values, vec![5, 6]);
        let second = RowProjectionRequest {
            call_id: id(2),
            ..first
        };
        let refused = worker.project(second.clone()).unwrap_err();
        assert!(
            matches!(&refused, TensorParallelError::Worker(message) if message.contains("did not bind")),
            "{refused:?}"
        );
        assert!(!worker.is_open());
        assert_eq!(
            worker.project(second).unwrap_err(),
            TensorParallelError::Closed
        );
        drop(worker);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[cfg(unix)]
    #[test]
    fn a_late_answer_is_never_read_by_a_later_call() {
        // The answer arrives after the call's deadline. The call fails and
        // the session is killed, so the late answer can never be taken as the
        // answer to the next call.
        let a = assignment(0, 2, "late");
        let input = vec![1i64, 2];
        let request = RowProjectionRequest {
            call_id: id(3),
            input_hash: hash_i64(&input),
            assignment: a.clone(),
            input,
        };
        let answer = RowProjectionResponse {
            call_id: request.call_id,
            input_hash: request.input_hash,
            assignment: a,
            values: vec![7, 8],
        };
        let (worker, dir) =
            fake_ssh_worker("late", &framed(&answer, 1), 2, Duration::from_millis(200));
        let started = Instant::now();
        assert!(matches!(
            worker.project(request.clone()).unwrap_err(),
            TensorParallelError::Worker(_)
        ));
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "the deadline, not the answer, ended the call"
        );
        assert!(!worker.is_open());
        assert_eq!(
            worker.project(request).unwrap_err(),
            TensorParallelError::Closed
        );
        drop(worker);
        let _ = std::fs::remove_dir_all(dir);
    }

    fn id(value: u8) -> Hash256 {
        Hash256([value; 32])
    }
    fn assignment(start: usize, end: usize, worker: &str) -> RowAssignment {
        RowAssignment {
            artifact_id: id(7),
            execution_profile: "canonical".into(),
            layer: Some(2),
            tensor: TensorKey::Wq,
            row_start: start,
            row_end: end,
            worker_id: worker.into(),
        }
    }
    fn worker(a: RowAssignment, rows: Vec<f32>) -> Arc<dyn RowWorker> {
        Arc::new(LocalRowWorker {
            worker_id: a.worker_id.clone(),
            assignment: a,
            weights: I8Weights::quantize_f32(&rows, 2, 2),
        })
    }

    #[test]
    fn merge_is_exact_and_input_is_real_canonical_i8_kernel() {
        let a = assignment(0, 2, "a");
        let b = assignment(2, 4, "b");
        let mut workers = BTreeMap::new();
        workers.insert("a".into(), worker(a.clone(), vec![1., 2., 3., 4.]));
        workers.insert("b".into(), worker(b.clone(), vec![5., 6., 7., 8.]));
        let backend =
            PartitionedProjectionBackend::new(id(7), "canonical".into(), vec![b, a], workers);
        let got = backend
            .project(id(9), Some(2), TensorKey::Wq, &[65_536, 65_536], 4)
            .unwrap();
        let full = I8Weights::quantize_f32(&[1., 2., 3., 4., 5., 6., 7., 8.], 4, 2);
        let mut expected = vec![0; 4];
        matmul_i8_canonical_rows(&full, &[65_536, 65_536], &mut expected).unwrap();
        assert_eq!(got, expected);
    }

    #[test]
    fn rejects_shuffled_missing_overlap_and_wrong_identity() {
        let a = assignment(0, 2, "a");
        let b = assignment(2, 4, "b");
        assert!(
            validate_exact_coverage(
                &[b.clone(), a.clone()],
                id(7),
                "canonical",
                Some(2),
                TensorKey::Wq,
                4
            )
            .is_ok()
        );
        let missing = vec![a.clone()];
        assert!(matches!(
            validate_exact_coverage(&missing, id(7), "canonical", Some(2), TensorKey::Wq, 4),
            Err(TensorParallelError::IncompleteCoverage { .. })
        ));
        let mut overlap = b.clone();
        overlap.row_start = 1;
        assert!(matches!(
            validate_exact_coverage(
                &[a.clone(), overlap],
                id(7),
                "canonical",
                Some(2),
                TensorKey::Wq,
                4
            ),
            Err(TensorParallelError::Overlap { .. })
        ));
        let mut wrong = a;
        wrong.artifact_id = id(8);
        assert_eq!(
            validate_exact_coverage(&[wrong], id(7), "canonical", Some(2), TensorKey::Wq, 2),
            Err(TensorParallelError::WrongIdentity)
        );
    }

    #[test]
    fn rejects_wrong_response_shape_and_call() {
        struct Bad;
        impl RowWorker for Bad {
            fn project(
                &self,
                r: RowProjectionRequest,
            ) -> Result<RowProjectionResponse, TensorParallelError> {
                Ok(RowProjectionResponse {
                    call_id: id(1),
                    input_hash: r.input_hash,
                    assignment: r.assignment,
                    values: vec![1],
                })
            }
        }
        let a = assignment(0, 2, "bad");
        let mut workers = BTreeMap::new();
        workers.insert("bad".into(), Arc::new(Bad) as Arc<dyn RowWorker>);
        let backend =
            PartitionedProjectionBackend::new(id(7), "canonical".into(), vec![a], workers);
        assert_eq!(
            backend.project(id(9), Some(2), TensorKey::Wq, &[1, 2], 2),
            Err(TensorParallelError::WrongCall)
        );
    }

    #[test]
    fn rejects_wrong_input_hash_and_oversized_stage() {
        let a = assignment(0, 2, "a");
        let w = LocalRowWorker {
            worker_id: "a".into(),
            assignment: a.clone(),
            weights: I8Weights::quantize_f32(&[1., 2., 3., 4.], 2, 2),
        };
        let request = RowProjectionRequest {
            call_id: id(1),
            input_hash: id(99),
            assignment: a,
            input: vec![1, 2],
        };
        assert!(matches!(
            w.project(request),
            Err(TensorParallelError::Bounds)
        ));
        let many: Vec<_> = (0..(MAX_ROW_WORKERS_PER_STAGE + 1))
            .map(|i| RowAssignment {
                row_start: i,
                row_end: i + 1,
                worker_id: format!("w{i}"),
                ..assignment(0, 1, "x")
            })
            .collect();
        assert_eq!(
            validate_exact_coverage(
                &many,
                id(7),
                "canonical",
                Some(2),
                TensorKey::Wq,
                many.len()
            ),
            Err(TensorParallelError::Bounds)
        );
    }

    #[test]
    fn canonical_row_kernel_rejects_negative_128_overflow_before_dispatch() {
        let weights = I8Weights {
            data: vec![-128],
            scales: vec![i64::MAX],
            n_rows: 1,
            n_cols: 1,
        };
        let mut output = vec![0];
        assert!(matmul_i8_canonical_rows(&weights, &[1], &mut output).is_err());
    }

    // ---- memory-bounded row shards -------------------------------------

    fn tensor_weights(n_rows: usize, n_cols: usize) -> I8Weights {
        let data = (0..n_rows * n_cols)
            .map(|i| (((i * 31) % 251) as i64 - 125) as i8)
            .collect();
        let scales = (0..n_rows).map(|r| 1_000 + (r as i64) * 37).collect();
        I8Weights {
            data,
            scales,
            n_rows,
            n_cols,
        }
    }

    fn shard_of(whole: &I8Weights, start: usize, end: usize) -> RowShard {
        RowShard {
            artifact: id(7),
            profile: "canonical".into(),
            layer: Some(2),
            tensor: TensorKey::Wq,
            row_start: start,
            row_end: end,
            weights: whole.copy_rows(start, end).unwrap(),
        }
    }

    fn request_for(a: RowAssignment, input: &[i64]) -> RowProjectionRequest {
        RowProjectionRequest {
            call_id: id(9),
            input_hash: hash_i64(input),
            assignment: a,
            input: input.to_vec(),
        }
    }

    /// The property the whole memory-bounded design rests on: a worker holding
    /// only some rows returns, for any sub-range, exactly what a coordinator
    /// holding the whole tensor computes for those same rows.
    #[test]
    fn a_shard_serves_any_sub_range_bit_identically_to_the_whole_tensor() {
        let n_cols = 12;
        let whole = tensor_weights(32, n_cols);
        let input: Vec<i64> = (0..n_cols).map(|i| (i as i64) * 7 - 40).collect();
        // One worker holds rows 8..24 and nothing else.
        let shards = vec![shard_of(&whole, 8, 24)];

        for (start, end) in [(8, 24), (8, 9), (23, 24), (10, 18), (12, 13)] {
            let expected = rows_of(&whole, start, end, &input).unwrap();
            let served = project_from_shards(
                &shards,
                &request_for(assignment(start, end, "placement-label"), &input),
            )
            .unwrap_or_else(|error| panic!("rows {start}..{end}: {error}"));
            assert_eq!(served, expected, "rows {start}..{end} must match exactly");
            assert_eq!(served.len(), end - start);
        }
    }

    #[test]
    fn a_shard_refuses_rows_it_does_not_hold() {
        let whole = tensor_weights(32, 4);
        let input = vec![1, 2, 3, 4];
        let shards = vec![shard_of(&whole, 8, 16)];
        for (start, end) in [(0, 4), (7, 9), (15, 17), (16, 20)] {
            assert_eq!(
                project_from_shards(&shards, &request_for(assignment(start, end, "w"), &input)),
                Err(TensorParallelError::WrongIdentity),
                "rows {start}..{end} are not held and must not be answered"
            );
        }
        // An empty or inverted range is refused before anything is matched.
        assert!(
            project_from_shards(&shards, &request_for(assignment(10, 10, "w"), &input)).is_err()
        );
    }

    /// Placement renames machines between runs; the weights do not change.
    #[test]
    fn a_shard_serves_whatever_placement_label_the_request_carries() {
        let whole = tensor_weights(16, 4);
        let input = vec![5, -6, 7, -8];
        let shards = vec![shard_of(&whole, 0, 16)];
        let first = project_from_shards(
            &shards,
            &request_for(assignment(2, 6, "exported-as"), &input),
        )
        .unwrap();
        let second = project_from_shards(
            &shards,
            &request_for(assignment(2, 6, "placed-as-something-else"), &input),
        )
        .unwrap();
        assert_eq!(first, second);
        assert_eq!(first, rows_of(&whole, 2, 6, &input).unwrap());
    }

    #[test]
    fn a_shard_refuses_another_artifact_profile_layer_or_tensor() {
        let whole = tensor_weights(16, 4);
        let input = vec![1, 1, 1, 1];
        let shards = vec![shard_of(&whole, 0, 16)];
        let mut other_artifact = assignment(0, 4, "w");
        other_artifact.artifact_id = id(8);
        let mut other_profile = assignment(0, 4, "w");
        other_profile.execution_profile = "something-else".into();
        let mut other_layer = assignment(0, 4, "w");
        other_layer.layer = Some(3);
        let mut other_tensor = assignment(0, 4, "w");
        other_tensor.tensor = TensorKey::WDown;
        for bad in [other_artifact, other_profile, other_layer, other_tensor] {
            assert_eq!(
                project_from_shards(&shards, &request_for(bad, &input)),
                Err(TensorParallelError::WrongIdentity)
            );
        }
    }

    /// Several shards of different tensors on one worker, which is what a
    /// memory budget actually looks like once placement spreads work.
    #[test]
    fn a_worker_holding_several_shards_picks_the_right_one() {
        let n_cols = 6;
        let wq = tensor_weights(16, n_cols);
        let wdown = tensor_weights(24, n_cols);
        let input: Vec<i64> = (0..n_cols).map(|i| i as i64 - 3).collect();
        let mut wq_shard = shard_of(&wq, 0, 16);
        wq_shard.tensor = TensorKey::Wq;
        let mut down_shard = shard_of(&wdown, 4, 20);
        down_shard.tensor = TensorKey::WDown;
        let shards = vec![wq_shard, down_shard];

        let mut ask_wq = assignment(2, 5, "w");
        ask_wq.tensor = TensorKey::Wq;
        assert_eq!(
            project_from_shards(&shards, &request_for(ask_wq, &input)).unwrap(),
            rows_of(&wq, 2, 5, &input).unwrap()
        );

        let mut ask_down = assignment(6, 11, "w");
        ask_down.tensor = TensorKey::WDown;
        assert_eq!(
            project_from_shards(&shards, &request_for(ask_down, &input)).unwrap(),
            rows_of(&wdown, 6, 11, &input).unwrap()
        );
    }
}
