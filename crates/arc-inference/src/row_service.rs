//! Shared, private Unix-socket service for immutable canonical row bundles.
//! Each SSH relay carries the existing row protocol to one resident bundle;
//! connections never reload weights. This is deliberately not a public TCP
//! endpoint and does not cache projections or generation results.

use crate::cached_integer_model::{
    GGUF_INTERLEAVED_ROPE_I8_INFERENCE_PROFILE, I8Weights, matmul_i8_canonical_row_range,
};
use crate::tensor_parallel::{
    MAX_CANONICAL_ROW_FILE_BYTES, MAX_ROW_FRAME_BYTES, RowProjectionRequest, RowProjectionResponse,
    RowShard, TensorKey, decode_row_request, encode_row_response, hash_i64,
};
use arc_crypto::Hash256;
use std::io::{self, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const MAX_FILES: usize = 1024;
const MAX_CLIENTS: usize = 32;

fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}
fn timed_out() -> io::Error {
    io::Error::new(io::ErrorKind::TimedOut, "row service deadline expired")
}
fn read_u64(file: &mut impl Read) -> io::Result<u64> {
    let mut bytes = [0; 8];
    file.read_exact(&mut bytes)?;
    Ok(u64::from_le_bytes(bytes))
}
fn read_short(file: &mut impl Read) -> io::Result<Vec<u8>> {
    let mut len = [0; 2];
    file.read_exact(&mut len)?;
    let len = u16::from_le_bytes(len) as usize;
    if len == 0 || len > 256 {
        return Err(invalid("row identity length"));
    }
    let mut bytes = vec![0; len];
    file.read_exact(&mut bytes)?;
    Ok(bytes)
}

/// Exactly one loaded bundle. All connections borrow these same row heaps.
pub struct SharedRowBundle {
    artifact: Hash256,
    worker_id: String,
    shards: Vec<RowShard>,
    serialized_bytes: usize,
    largest_file: usize,
    // An exclusive nonblocking directory lock prevents a second daemon
    // from preparing another heap for this same bundle, even at a different socket.
    _directory_lease: std::fs::File,
}
impl SharedRowBundle {
    /// Preflight the complete aggregate before allocating any row weights.
    /// Files must be canonical ARCROW01 exports for the expected artifact.
    pub fn load(directory: &Path, expected: Hash256) -> io::Result<Self> {
        let lease = std::fs::File::open(directory)?;
        if !lease.metadata()?.is_dir() {
            return Err(invalid("row bundle path is not a directory"));
        }
        // SAFETY: the descriptor remains owned by `lease`; flock neither
        // dereferences pointers nor changes the underlying artifact files.
        if unsafe { libc::flock(lease.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            return Err(io::Error::last_os_error());
        }
        let mut files = Vec::new();
        let mut total = 0usize;
        let mut largest = 0usize;
        for entry in std::fs::read_dir(directory)? {
            let entry = entry?;
            if !entry.file_type()?.is_file() {
                return Err(invalid("row bundle contains a non-regular file"));
            }
            if files.len() >= MAX_FILES {
                return Err(invalid("too many row files"));
            }
            let len = usize::try_from(entry.metadata()?.len())
                .map_err(|_| invalid("row file length overflow"))?;
            total = total
                .checked_add(len)
                .ok_or_else(|| invalid("row bundle length overflow"))?;
            if total > MAX_CANONICAL_ROW_FILE_BYTES {
                return Err(invalid("row bundle exceeds 1 GiB"));
            }
            largest = largest.max(len);
            files.push((entry.path(), len));
        }
        if files.is_empty() {
            return Err(invalid("empty row bundle"));
        }
        files.sort_by(|a, b| a.0.cmp(&b.0));
        let mut shards = Vec::with_capacity(files.len());
        let mut worker_id = None;
        for (path, len) in files {
            let (shard, id) = Self::load_file(&path, len, expected)?;
            if worker_id.as_ref().is_some_and(|held| held != &id) {
                return Err(invalid("row bundle worker ids are inconsistent"));
            }
            worker_id = Some(id);
            shards.push(shard);
        }
        for (i, a) in shards.iter().enumerate() {
            if shards[..i].iter().any(|b| {
                a.layer == b.layer
                    && a.tensor == b.tensor
                    && a.row_start < b.row_end
                    && b.row_start < a.row_end
            }) {
                return Err(invalid("row bundle contains overlapping tensor ranges"));
            }
        }
        Ok(Self {
            artifact: expected,
            worker_id: worker_id.ok_or_else(|| invalid("empty row bundle"))?,
            shards,
            serialized_bytes: total,
            largest_file: largest,
            _directory_lease: lease,
        })
    }
    fn load_file(
        path: &Path,
        expected_len: usize,
        expected: Hash256,
    ) -> io::Result<(RowShard, String)> {
        let mut file = std::fs::File::open(path)?;
        if file.metadata()?.len() != expected_len as u64 {
            return Err(invalid("row file changed after size preflight"));
        }
        let mut magic = [0; 8];
        file.read_exact(&mut magic)?;
        if &magic != b"ARCROW01" {
            return Err(invalid("row file magic"));
        }
        let mut artifact = [0; 32];
        file.read_exact(&mut artifact)?;
        if Hash256(artifact) != expected {
            return Err(invalid("row file artifact differs from pinned artifact"));
        }
        let profile = String::from_utf8(read_short(&mut file)?)
            .map_err(|_| invalid("row profile encoding"))?;
        if profile != GGUF_INTERLEAVED_ROPE_I8_INFERENCE_PROFILE {
            return Err(invalid(
                "row file is not the canonical interleaved-RoPE profile",
            ));
        }
        let layer = read_u64(&mut file)? as i64;
        let layer = match layer {
            -1 => None,
            n if n >= 0 => Some(usize::try_from(n).map_err(|_| invalid("layer bound"))?),
            _ => return Err(invalid("layer identity")),
        };
        let mut tag = [0];
        file.read_exact(&mut tag)?;
        let tensor = match tag[0] {
            0 => TensorKey::Wq,
            1 => TensorKey::Wk,
            2 => TensorKey::Wv,
            3 => TensorKey::Wo,
            4 => TensorKey::WGate,
            5 => TensorKey::WUp,
            6 => TensorKey::WDown,
            7 => TensorKey::LmHead,
            _ => return Err(invalid("tensor identity")),
        };
        if (tensor == TensorKey::LmHead) != layer.is_none() {
            return Err(invalid("layer/tensor identity mismatch"));
        }
        let start = usize::try_from(read_u64(&mut file)?).map_err(|_| invalid("row bound"))?;
        let end = usize::try_from(read_u64(&mut file)?).map_err(|_| invalid("row bound"))?;
        let worker = String::from_utf8(read_short(&mut file)?)
            .map_err(|_| invalid("row worker id encoding"))?;
        let cols = usize::try_from(read_u64(&mut file)?).map_err(|_| invalid("column bound"))?;
        let rows = usize::try_from(read_u64(&mut file)?).map_err(|_| invalid("row bound"))?;
        if start >= end || end - start != rows || rows > 131072 || cols == 0 || cols > 131072 {
            return Err(invalid("row shape"));
        }
        let data_len = rows
            .checked_mul(cols)
            .ok_or_else(|| invalid("row size overflow"))?;
        let length = data_len
            .checked_add(
                rows.checked_mul(8)
                    .ok_or_else(|| invalid("row size overflow"))?,
            )
            .and_then(|n| n.checked_add(85 + profile.len() + worker.len()))
            .ok_or_else(|| invalid("row size overflow"))?;
        if length != expected_len {
            return Err(invalid("row shape differs from preflight file length"));
        }
        let mut scales = Vec::with_capacity(rows);
        for _ in 0..rows {
            scales.push(read_u64(&mut file)? as i64);
        }
        let mut raw = vec![0u8; data_len];
        file.read_exact(&mut raw)?;
        let mut tail = [0];
        if file.read(&mut tail)? != 0 {
            return Err(invalid("trailing row bytes"));
        }
        let data = raw.into_iter().map(|v| v as i8).collect();
        Ok((
            RowShard {
                artifact: expected,
                profile,
                layer,
                tensor,
                row_start: start,
                row_end: end,
                weights: I8Weights {
                    data,
                    scales,
                    n_rows: rows,
                    n_cols: cols,
                },
            },
            worker,
        ))
    }
    fn project(
        &self,
        request: &RowProjectionRequest,
        deadline: Instant,
        shutdown: &AtomicBool,
    ) -> io::Result<Vec<i64>> {
        let a = &request.assignment;
        if a.artifact_id != self.artifact
            || a.worker_id != self.worker_id
            || a.execution_profile != GGUF_INTERLEAVED_ROPE_I8_INFERENCE_PROFILE
            || hash_i64(&request.input) != request.input_hash
            || a.row_start >= a.row_end
        {
            return Err(invalid("request identity or input commitment"));
        }
        let shard = self
            .shards
            .iter()
            .find(|s| {
                s.layer == a.layer
                    && s.tensor == a.tensor
                    && s.row_start <= a.row_start
                    && a.row_end <= s.row_end
            })
            .ok_or_else(|| invalid("requested rows are not resident"))?;
        if request.input.len() != shard.weights.n_cols {
            return Err(invalid("projection input shape"));
        }
        let mut values = vec![0; a.row_end - a.row_start];
        // A deadline is checked between bounded row chunks, so an expired
        // client cannot monopolize the single compute slot indefinitely.
        for (chunk, out) in values.chunks_mut(32).enumerate() {
            if shutdown.load(Ordering::Relaxed) || Instant::now() >= deadline {
                return Err(timed_out());
            }
            let start = a.row_start - shard.row_start + chunk * 32;
            matmul_i8_canonical_row_range(
                &shard.weights,
                start,
                start + out.len(),
                &request.input,
                out,
            )
            .map_err(invalid)?;
        }
        if Instant::now() >= deadline {
            return Err(timed_out());
        }
        Ok(values)
    }
}

#[derive(Clone, Copy, Debug)]
pub struct RowServiceLimits {
    pub max_clients: usize,
    pub idle_timeout: Duration,
    pub frame_timeout: Duration,
    pub call_timeout: Duration,
}
impl Default for RowServiceLimits {
    fn default() -> Self {
        Self {
            max_clients: 8,
            idle_timeout: Duration::from_secs(600),
            frame_timeout: Duration::from_secs(10),
            call_timeout: Duration::from_secs(120),
        }
    }
}
impl RowServiceLimits {
    pub fn validate(&self) -> io::Result<()> {
        if self.max_clients == 0
            || self.max_clients > MAX_CLIENTS
            || self.idle_timeout.is_zero()
            || self.idle_timeout > Duration::from_secs(3600)
            || self.frame_timeout.is_zero()
            || self.frame_timeout > Duration::from_secs(60)
            || self.call_timeout.is_zero()
            || self.call_timeout > Duration::from_secs(600)
        {
            return Err(invalid("row service limits outside supported bounds"));
        }
        Ok(())
    }
}

#[derive(Debug, serde::Serialize)]
pub struct RowServiceStats {
    pub worker_id: String,
    pub resident_bundle_copies: usize,
    pub resident_row_files: usize,
    pub resident_row_bytes: usize,
    pub serialized_row_bytes: usize,
    pub startup_payload_peak_upper_bound_bytes: usize,
    pub active_clients: usize,
    pub total_clients: u64,
    pub rejected_clients: u64,
    pub completed_calls: u64,
    pub refused_calls: u64,
    pub max_clients: usize,
}

pub struct SharedRowService {
    bundle: Arc<SharedRowBundle>,
    limits: RowServiceLimits,
    compute: Mutex<()>,
    active: AtomicUsize,
    total: AtomicU64,
    rejected: AtomicU64,
    completed: AtomicU64,
    refused: AtomicU64,
}
impl SharedRowService {
    pub fn new(bundle: SharedRowBundle, limits: RowServiceLimits) -> io::Result<Arc<Self>> {
        limits.validate()?;
        Ok(Arc::new(Self {
            bundle: Arc::new(bundle),
            limits,
            compute: Mutex::new(()),
            active: AtomicUsize::new(0),
            total: AtomicU64::new(0),
            rejected: AtomicU64::new(0),
            completed: AtomicU64::new(0),
            refused: AtomicU64::new(0),
        }))
    }
    pub fn stats(&self) -> RowServiceStats {
        RowServiceStats {
            worker_id: self.bundle.worker_id.clone(),
            resident_bundle_copies: 1,
            resident_row_files: self.bundle.shards.len(),
            resident_row_bytes: self
                .bundle
                .shards
                .iter()
                .map(|s| s.weights.data.len() + s.weights.scales.len() * 8)
                .sum(),
            serialized_row_bytes: self.bundle.serialized_bytes,
            startup_payload_peak_upper_bound_bytes: self.bundle.serialized_bytes
                + self.bundle.largest_file,
            active_clients: self.active.load(Ordering::Relaxed),
            total_clients: self.total.load(Ordering::Relaxed),
            rejected_clients: self.rejected.load(Ordering::Relaxed),
            completed_calls: self.completed.load(Ordering::Relaxed),
            refused_calls: self.refused.load(Ordering::Relaxed),
            max_clients: self.limits.max_clients,
        }
    }
    /// Nonblocking accept is solely for bounded graceful shutdown. There are
    /// at most max_clients connection tasks and queued requests; each client
    /// has at most one decoded frame waiting for the one compute slot.
    pub fn serve(
        self: &Arc<Self>,
        listener: UnixListener,
        shutdown: Arc<AtomicBool>,
    ) -> io::Result<()> {
        listener.set_nonblocking(true)?;
        let mut threads: Vec<std::thread::JoinHandle<()>> = Vec::new();
        while !shutdown.load(Ordering::Relaxed) {
            threads.retain(|thread| !thread.is_finished());
            match listener.accept() {
                Ok((stream, _)) => {
                    if require_same_uid(&stream).is_err() {
                        self.rejected.fetch_add(1, Ordering::Relaxed);
                        continue;
                    }
                    if self
                        .active
                        .fetch_update(Ordering::AcqRel, Ordering::Relaxed, |n| {
                            (n < self.limits.max_clients).then_some(n + 1)
                        })
                        .is_err()
                    {
                        self.rejected.fetch_add(1, Ordering::Relaxed);
                        drop(stream);
                        continue;
                    }
                    self.total.fetch_add(1, Ordering::Relaxed);
                    let guard = ClientGuard(self.clone());
                    let cancel = shutdown.clone();
                    let thread = std::thread::Builder::new()
                        .name("arc-row-client".into())
                        .spawn(move || {
                            let service = &guard.0;
                            if service.client(stream, &cancel).is_err() {
                                service.refused.fetch_add(1, Ordering::Relaxed);
                            }
                        });
                    match thread {
                        Ok(thread) => threads.push(thread),
                        Err(_) => {
                            self.rejected.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(5))
                }
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e),
            }
        }
        // Active streams check cancellation at most 100 ms apart, including
        // idle clients. Row kernels check it every <=32 rows.
        for thread in threads {
            let _ = thread.join();
        }
        Ok(())
    }
    fn client(&self, mut stream: UnixStream, shutdown: &AtomicBool) -> io::Result<()> {
        loop {
            if shutdown.load(Ordering::Relaxed) {
                return Ok(());
            }
            let mut length = [0; 4];
            let idle = Instant::now() + self.limits.idle_timeout;
            if !read_exact_until(&mut stream, &mut length[..1], idle, shutdown, true)? {
                return Ok(());
            }
            let frame_deadline = Instant::now() + self.limits.frame_timeout;
            read_exact_until(
                &mut stream,
                &mut length[1..],
                frame_deadline,
                shutdown,
                false,
            )?;
            let count = u32::from_le_bytes(length) as usize;
            if count == 0 || count > MAX_ROW_FRAME_BYTES {
                return Err(invalid("row frame size"));
            }
            let mut frame = vec![0; count];
            read_exact_until(&mut stream, &mut frame, frame_deadline, shutdown, false)?;
            let request = decode_row_request(&frame).map_err(|e| invalid(e.to_string()))?;
            drop(frame);
            let deadline = Instant::now() + self.limits.call_timeout;
            let slot = loop {
                if shutdown.load(Ordering::Relaxed) || Instant::now() >= deadline {
                    return Err(timed_out());
                }
                match self.compute.try_lock() {
                    Ok(slot) => break slot,
                    Err(std::sync::TryLockError::WouldBlock) => {
                        std::thread::sleep(Duration::from_millis(1))
                    }
                    Err(_) => return Err(invalid("row compute slot poisoned")),
                }
            };
            let values = self.bundle.project(&request, deadline, shutdown)?;
            drop(slot);
            let response = RowProjectionResponse {
                call_id: request.call_id,
                input_hash: request.input_hash,
                assignment: request.assignment,
                values,
            };
            let bytes = encode_row_response(&response).map_err(|e| invalid(e.to_string()))?;
            write_all_until(
                &mut stream,
                &(bytes.len() as u32).to_le_bytes(),
                deadline,
                shutdown,
            )?;
            write_all_until(&mut stream, &bytes, deadline, shutdown)?;
            self.completed.fetch_add(1, Ordering::Relaxed);
        }
    }
}
struct ClientGuard(Arc<SharedRowService>);
impl Drop for ClientGuard {
    fn drop(&mut self) {
        self.0.active.fetch_sub(1, Ordering::AcqRel);
    }
}

fn remaining(deadline: Instant, shutdown: &AtomicBool) -> io::Result<Duration> {
    if shutdown.load(Ordering::Relaxed) {
        return Err(timed_out());
    }
    deadline
        .checked_duration_since(Instant::now())
        .filter(|d| !d.is_zero())
        .map(|d| d.min(Duration::from_millis(100)))
        .ok_or_else(timed_out)
}
fn read_exact_until(
    stream: &mut UnixStream,
    mut out: &mut [u8],
    deadline: Instant,
    shutdown: &AtomicBool,
    eof_ok: bool,
) -> io::Result<bool> {
    let mut any = false;
    while !out.is_empty() {
        stream.set_read_timeout(Some(remaining(deadline, shutdown)?))?;
        match stream.read(out) {
            Ok(0) if !any && eof_ok => return Ok(false),
            Ok(0) => {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "partial row frame",
                ));
            }
            Ok(n) => {
                any = true;
                out = &mut out[n..];
            }
            Err(e)
                if matches!(
                    e.kind(),
                    io::ErrorKind::Interrupted
                        | io::ErrorKind::WouldBlock
                        | io::ErrorKind::TimedOut
                ) =>
            {
                continue;
            }
            Err(e) => return Err(e),
        }
    }
    Ok(true)
}
fn write_all_until(
    stream: &mut UnixStream,
    mut bytes: &[u8],
    deadline: Instant,
    shutdown: &AtomicBool,
) -> io::Result<()> {
    while !bytes.is_empty() {
        stream.set_write_timeout(Some(remaining(deadline, shutdown)?))?;
        match stream.write(bytes) {
            Ok(0) => {
                return Err(io::Error::new(
                    io::ErrorKind::WriteZero,
                    "row response write stopped",
                ));
            }
            Ok(n) => bytes = &bytes[n..],
            Err(e)
                if matches!(
                    e.kind(),
                    io::ErrorKind::Interrupted
                        | io::ErrorKind::WouldBlock
                        | io::ErrorKind::TimedOut
                ) =>
            {
                continue;
            }
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

/// Bind only beneath a directory owned by this uid with mode 0700 (or more
/// restrictive), and set the socket itself to 0600. Existing paths are never
/// unlinked by startup. The operator explicitly handles a stale old socket.
pub struct PrivateRowSocket {
    path: PathBuf,
    inode: u64,
}
impl PrivateRowSocket {
    pub fn bind(path: &Path) -> io::Result<(Self, UnixListener)> {
        if !path.is_absolute() {
            return Err(invalid("row socket must be absolute"));
        }
        let parent = path
            .parent()
            .ok_or_else(|| invalid("row socket has no parent"))?;
        let metadata = std::fs::symlink_metadata(parent)?;
        // SAFETY: geteuid reads this process's uid and has no pointer inputs.
        if !metadata.is_dir()
            || metadata.uid() != unsafe { libc::geteuid() }
            || metadata.permissions().mode() & 0o077 != 0
        {
            return Err(invalid(
                "row socket parent must be a private directory owned by the service uid",
            ));
        }
        let listener = UnixListener::bind(path)?;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
        let inode = std::fs::symlink_metadata(path)?.ino();
        Ok((
            Self {
                path: path.into(),
                inode,
            },
            listener,
        ))
    }
}
impl Drop for PrivateRowSocket {
    fn drop(&mut self) {
        if std::fs::symlink_metadata(&self.path).is_ok_and(|m| m.ino() == self.inode) {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

// The private directory/socket mode is the access boundary. Also bind both
// ends to this uid, so replacing an ancestor path cannot redirect a relay's
// activations to another local user.
fn require_same_uid(stream: &UnixStream) -> io::Result<()> {
    #[cfg(any(target_os = "linux", target_os = "android"))]
    {
        let mut credentials = libc::ucred {
            pid: 0,
            uid: 0,
            gid: 0,
        };
        let mut size = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
        // SAFETY: both output buffers have the exact sizes supplied, the
        // connected descriptor is live, and getsockopt retains no pointer.
        let status = unsafe {
            libc::getsockopt(
                stream.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_PEERCRED,
                (&mut credentials as *mut libc::ucred).cast(),
                &mut size,
            )
        };
        if status != 0 {
            return Err(io::Error::last_os_error());
        }
        if size as usize != std::mem::size_of::<libc::ucred>()
            || credentials.uid != unsafe { libc::geteuid() }
        {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "row socket peer uid differs",
            ));
        }
        Ok(())
    }
    #[cfg(any(
        target_os = "macos",
        target_os = "freebsd",
        target_os = "openbsd",
        target_os = "netbsd",
        target_os = "dragonfly"
    ))]
    {
        let mut uid: libc::uid_t = 0;
        let mut gid: libc::gid_t = 0;
        // SAFETY: live stream and valid uid/gid output pointers.
        if unsafe { libc::getpeereid(stream.as_raw_fd(), &mut uid, &mut gid) } != 0 {
            return Err(io::Error::last_os_error());
        }
        if uid != unsafe { libc::geteuid() } {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "row socket peer uid differs",
            ));
        }
        Ok(())
    }
    #[cfg(not(any(
        target_os = "linux",
        target_os = "android",
        target_os = "macos",
        target_os = "freebsd",
        target_os = "openbsd",
        target_os = "netbsd",
        target_os = "dragonfly"
    )))]
    {
        let _ = stream;
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "row socket peer credentials unavailable",
        ))
    }
}

/// SSH invokes this relay, which copies only protocol bytes. It creates no
/// resident rows. A detached stdin task lets a daemon disconnect terminate
/// the relay even if the SSH input pipe remains open; process exit reaps it.
pub fn relay(socket: &Path) -> io::Result<()> {
    let mut stream = UnixStream::connect(socket)?;
    require_same_uid(&stream)?;
    let mut input_stream = stream.try_clone()?;
    std::thread::Builder::new()
        .name("arc-row-relay-input".into())
        .spawn(move || {
            let _ = io::copy(&mut io::stdin().lock(), &mut input_stream);
            let _ = input_stream.shutdown(std::net::Shutdown::Write);
        })?;
    io::copy(&mut stream, &mut io::stdout().lock())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tensor_parallel::{
        RowAssignment, decode_row_response, encode_row_request, export_canonical_row_file,
    };
    static NEXT: AtomicU64 = AtomicU64::new(0);
    struct Running {
        directory: PathBuf,
        socket: PathBuf,
        file: PathBuf,
        service: Arc<SharedRowService>,
        shutdown: Arc<AtomicBool>,
        handle: Option<std::thread::JoinHandle<io::Result<()>>>,
        _socket_guard: PrivateRowSocket,
    }
    impl Running {
        fn start(limits: RowServiceLimits) -> Self {
            let directory = PathBuf::from(format!(
                "/tmp/arc-row-shared-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir(&directory).unwrap();
            std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o700)).unwrap();
            let rows = directory.join("rows");
            std::fs::create_dir(&rows).unwrap();
            let file = rows.join("q.arcrow");
            let assignment = assignment("fixture", Hash256([1; 32]));
            let weights = I8Weights {
                data: (0..32).map(|i| (i % 7) as i8 - 3).collect(),
                scales: vec![65536; 8],
                n_rows: 8,
                n_cols: 4,
            };
            export_canonical_row_file(&file, &assignment, &weights).unwrap();
            let bundle = SharedRowBundle::load(&rows, Hash256([7; 32])).unwrap();
            // A second instance cannot prepare duplicate resident heaps for
            // this directory, even if it would choose a different socket.
            assert!(SharedRowBundle::load(&rows, Hash256([7; 32])).is_err());
            let service = SharedRowService::new(bundle, limits).unwrap();
            let socket = directory.join("s");
            let (guard, listener) = PrivateRowSocket::bind(&socket).unwrap();
            let shutdown = Arc::new(AtomicBool::new(false));
            let (serving, cancel) = (service.clone(), shutdown.clone());
            let handle = std::thread::spawn(move || serving.serve(listener, cancel));
            Self {
                directory,
                socket,
                file,
                service,
                shutdown,
                handle: Some(handle),
                _socket_guard: guard,
            }
        }
        fn connect(&self) -> UnixStream {
            let stream = UnixStream::connect(&self.socket).unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(3)))
                .unwrap();
            stream
                .set_write_timeout(Some(Duration::from_secs(3)))
                .unwrap();
            stream
        }
    }
    impl Drop for Running {
        fn drop(&mut self) {
            self.shutdown.store(true, Ordering::Relaxed);
            self.handle.take().unwrap().join().unwrap().unwrap();
            let _ = std::fs::remove_dir_all(&self.directory);
        }
    }
    fn assignment(worker: &str, _call: Hash256) -> RowAssignment {
        RowAssignment {
            artifact_id: Hash256([7; 32]),
            execution_profile: GGUF_INTERLEAVED_ROPE_I8_INFERENCE_PROFILE.into(),
            layer: Some(0),
            tensor: TensorKey::Wq,
            row_start: 0,
            row_end: 8,
            worker_id: worker.into(),
        }
    }
    fn request(worker: &str, call: Hash256) -> RowProjectionRequest {
        let input = vec![1234, -5678, 9012, 3456];
        RowProjectionRequest {
            call_id: call,
            input_hash: hash_i64(&input),
            assignment: assignment(worker, call),
            input,
        }
    }
    fn send(stream: &mut UnixStream, request: &RowProjectionRequest) {
        let bytes = encode_row_request(request).unwrap();
        stream
            .write_all(&(bytes.len() as u32).to_le_bytes())
            .unwrap();
        stream.write_all(&bytes).unwrap();
    }
    fn answer(stream: &mut UnixStream) -> RowProjectionResponse {
        let mut length = [0; 4];
        stream.read_exact(&mut length).unwrap();
        let length = u32::from_le_bytes(length) as usize;
        assert!(length > 0 && length <= MAX_ROW_FRAME_BYTES);
        let mut bytes = vec![0; length];
        stream.read_exact(&mut bytes).unwrap();
        decode_row_response(&bytes).unwrap()
    }
    fn until(mut condition: impl FnMut() -> bool) {
        let end = Instant::now() + Duration::from_secs(3);
        while !condition() {
            assert!(Instant::now() < end, "row service test condition timed out");
            std::thread::sleep(Duration::from_millis(5));
        }
    }
    #[test]
    fn independent_clients_share_one_loaded_bundle_and_keep_exact_response_identities() {
        let running = Running::start(RowServiceLimits::default());
        let mut first = running.connect();
        let mut second = running.connect();
        let a = request("fixture", Hash256([1; 32]));
        let b = request("fixture", Hash256([2; 32]));
        send(&mut first, &a);
        let first = answer(&mut first);
        // Connections must not reload. The second client still uses the same
        // verified in-memory rows after the source file is no longer usable.
        std::fs::write(&running.file, b"not a row file").unwrap();
        send(&mut second, &b);
        let second = answer(&mut second);
        assert_eq!(first.values, second.values);
        assert_eq!(first.assignment, a.assignment);
        assert_eq!(second.assignment, b.assignment);
        assert_eq!(first.call_id, a.call_id);
        assert_eq!(second.call_id, b.call_id);
        until(|| running.service.stats().completed_calls == 2);
        let stats = running.service.stats();
        assert_eq!(stats.resident_bundle_copies, 1);
        assert_eq!(stats.resident_row_files, 1);
        assert_eq!(stats.total_clients, 2);
        assert_eq!(
            Arc::strong_count(&running.service.bundle),
            1,
            "connections cloned a resident bundle"
        );
    }
    #[test]
    fn malformed_and_disconnected_clients_do_not_kill_the_service() {
        let running = Running::start(RowServiceLimits::default());
        let mut malformed = running.connect();
        malformed
            .write_all(&((MAX_ROW_FRAME_BYTES + 1) as u32).to_le_bytes())
            .unwrap();
        assert_eq!(malformed.read(&mut [0]).unwrap(), 0);
        let mut partial = running.connect();
        partial.write_all(&[1, 0]).unwrap();
        drop(partial);
        until(|| running.service.stats().refused_calls >= 2);
        let mut good = running.connect();
        let expected = request("fixture", Hash256([3; 32]));
        send(&mut good, &expected);
        assert_eq!(answer(&mut good).call_id, expected.call_id);
        let mut wrong = request("fixture", Hash256([4; 32]));
        wrong.assignment.artifact_id = Hash256([9; 32]);
        send(&mut good, &wrong);
        assert_eq!(good.read(&mut [0]).unwrap(), 0);
        let mut alias = running.connect();
        let mut aliased = expected.clone();
        aliased.assignment.worker_id = "a-second-name-for-the-same-bundle".into();
        send(&mut alias, &aliased);
        assert_eq!(alias.read(&mut [0]).unwrap(), 0);
        let mut later = running.connect();
        send(&mut later, &expected);
        assert_eq!(answer(&mut later).assignment, expected.assignment);
    }
    #[test]
    fn client_limit_frame_deadline_and_queue_deadline_are_bounded() {
        let running = Running::start(RowServiceLimits {
            max_clients: 1,
            frame_timeout: Duration::from_millis(40),
            call_timeout: Duration::from_millis(40),
            ..Default::default()
        });
        let mut first = running.connect();
        until(|| running.service.stats().active_clients == 1);
        let mut excess = running.connect();
        assert_eq!(excess.read(&mut [0]).unwrap(), 0);
        assert_eq!(running.service.stats().rejected_clients, 1);
        // A length prefix without its payload expires rather than keeping a
        // queue slot alive with an incomplete frame.
        first.write_all(&100u32.to_le_bytes()).unwrap();
        assert_eq!(first.read(&mut [0]).unwrap(), 0);
        drop(first);
        until(|| running.service.stats().active_clients == 0);
        let slot = running.service.compute.lock().unwrap();
        let mut queued = running.connect();
        send(&mut queued, &request("fixture", Hash256([5; 32])));
        assert_eq!(queued.read(&mut [0]).unwrap(), 0);
        drop(slot);
        assert_eq!(running.service.stats().completed_calls, 0);
        until(|| running.service.stats().active_clients == 0);
        let mut healthy = running.connect();
        send(&mut healthy, &request("fixture", Hash256([6; 32])));
        assert_eq!(answer(&mut healthy).values.len(), 8);
    }
    #[test]
    fn private_socket_requires_owned_private_parent_and_cleans_only_its_own_path() {
        let dir = PathBuf::from(format!(
            "/tmp/arc-row-mode-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir(&dir).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
        let path = dir.join("s");
        assert!(PrivateRowSocket::bind(&path).is_err());
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        let (guard, listener) = PrivateRowSocket::bind(&path).unwrap();
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert!(PrivateRowSocket::bind(&path).is_err());
        drop(listener);
        drop(guard);
        assert!(!path.exists());
        std::fs::remove_dir(dir).unwrap();
    }
}
