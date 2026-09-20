//! Private-cohort tensor-row parallelism for one inference query.
//!
//! This module is intentionally separate from the public layer-shard RPC.
//! Every worker owns disjoint rows of one canonical-I8 matrix, returns those
//! rows for one authenticated call, and the coordinator restores the tensor in
//! row order.  It is useful on trusted, pinned-host-key stdio transports; it
//! does not advertise a public listener or a validator capability.

use crate::cached_integer_model::{I8Weights, matmul_i8_canonical_rows};
use arc_crypto::Hash256;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::io::Write;
use std::path::Path;
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::Arc;
use std::sync::Mutex;
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
const ROW_FRAME_MAGIC: &[u8; 8] = b"ARCTP001";

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

pub struct SshStdioRowWorker {
    worker_id: String,
    child: Mutex<Option<SshChild>>,
    timeout: Duration,
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
        // OpenSSH hands the remote command to the remote shell as one string;
        // quote each reviewed argv element here rather than claiming local
        // Command argv semantics extend across SSH.
        let remote = config
            .remote_command
            .iter()
            .map(|arg| format!("'{}'", arg.replace('\'', "'\\\"'\\\"'")))
            .collect::<Vec<_>>()
            .join(" ");
        let mut command = Command::new(&config.ssh_program);
        command
            .arg("-oBatchMode=yes")
            .arg("-oStrictHostKeyChecking=yes")
            .arg(format!(
                "-oUserKnownHostsFile={}",
                config.known_hosts.display()
            ))
            .arg("-oPasswordAuthentication=no")
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
            timeout: config.timeout,
        })
    }

    fn fail_closed(slot: &mut Option<SshChild>, message: impl Into<String>) -> TensorParallelError {
        if let Some(mut child) = slot.take() {
            let _ = child.child.kill();
            let _ = child.child.wait();
        }
        TensorParallelError::Worker(message.into())
    }
}

impl Drop for SshStdioRowWorker {
    fn drop(&mut self) {
        if let Ok(slot) = self.child.get_mut() {
            if let Some(mut c) = slot.take() {
                let _ = c.child.kill();
                let _ = c.child.wait();
            }
        }
    }
}

impl RowWorker for SshStdioRowWorker {
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
        let mut slot = self
            .child
            .lock()
            .map_err(|_| TensorParallelError::Worker("SSH worker mutex poisoned".into()))?;
        let child = slot
            .as_mut()
            .ok_or_else(|| TensorParallelError::Worker("SSH worker is closed".into()))?;
        let deadline = Instant::now() + self.timeout;
        let mut framed = Vec::with_capacity(frame.len() + 4);
        framed.extend_from_slice(&(frame.len() as u32).to_le_bytes());
        framed.extend_from_slice(&frame);
        if let Err(e) = write_timeout(&mut child.stdin, &framed, deadline) {
            return Err(Self::fail_closed(
                &mut slot,
                format!("write SSH row frame: {e}"),
            ));
        }
        let raw = match read_framed_timeout(&mut child.stdout, deadline) {
            Ok(value) => value,
            Err(error) => return Err(Self::fail_closed(&mut slot, error)),
        };
        let response = match decode_row_response(&raw) {
            Ok(value) => value,
            Err(error) => {
                return Err(Self::fail_closed(
                    &mut slot,
                    format!("decode SSH row response: {error}"),
                ));
            }
        };
        if response.call_id != request.call_id
            || response.input_hash != request.input_hash
            || response.assignment != request.assignment
            || response.values.len() != request.assignment.rows()
        {
            return Err(Self::fail_closed(
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

fn encode_row_request(request: &RowProjectionRequest) -> Result<Vec<u8>, TensorParallelError> {
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

fn decode_row_response(raw: &[u8]) -> Result<RowProjectionResponse, TensorParallelError> {
    let mut bytes = raw;
    if pull(&mut bytes, 8)? != ROW_FRAME_MAGIC {
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
fn export_canonical_row_file(
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
}
