//! Platform I/O backend abstraction for the WAL.
//!
//! Provides an `IoBackend` trait with multiple implementations:
//! - `StdBackend`: Standard BufWriter<File> (default, works everywhere)
//! - `DirectBackend`: Uses F_NOCACHE (macOS) or O_DIRECT (Linux) to bypass
//!   kernel page cache, reducing double-buffering for write-heavy workloads.
//! - `PreallocBackend`: Pre-allocates files in 64MB chunks with a 4MB write buffer
//!   to reduce fragmentation and syscalls on write-heavy workloads.
//! - `BatchWriter<B>`: Wraps any IoBackend to coalesce multiple small writes into
//!   fewer large writes, critical for WAL entry batching.
//! - `StatsBackend<B>`: Wraps any IoBackend to track I/O statistics (bytes, calls,
//!   throughput) for benchmarking and observability.
//! - `DevNullBackend`: Tracks call counts but discards data. Useful for benchmarking
//!   execution speed without I/O overhead.

use std::fs::{File, OpenOptions};
use std::io::{self, BufWriter, Write};
use std::path::Path;
use std::time::Instant;

// ── IoBackend Trait ─────────────────────────────────────────────────────────

/// Trait for WAL I/O backends.
pub trait IoBackend: Send + 'static {
    /// Write a buffer to the backend.
    fn write_all(&mut self, buf: &[u8]) -> io::Result<()>;
    /// Flush any internal buffers.
    fn flush(&mut self) -> io::Result<()>;
    /// Sync data to persistent storage (fsync/fdatasync).
    fn sync_data(&mut self) -> io::Result<()>;
    /// Total bytes written through this backend.
    fn bytes_written(&self) -> u64;
}

// ── StdBackend ──────────────────────────────────────────────────────────────

/// Standard BufWriter-based backend. Works on all platforms.
/// Uses a 256KB buffer for batching small writes.
pub struct StdBackend {
    writer: BufWriter<File>,
    total_bytes: u64,
}

impl StdBackend {
    pub fn new(path: &Path) -> io::Result<Self> {
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)?;
        Ok(Self {
            writer: BufWriter::with_capacity(256 * 1024, file),
            total_bytes: 0,
        })
    }
}

impl IoBackend for StdBackend {
    fn write_all(&mut self, buf: &[u8]) -> io::Result<()> {
        self.writer.write_all(buf)?;
        self.total_bytes += buf.len() as u64;
        Ok(())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.writer.flush()
    }

    fn sync_data(&mut self) -> io::Result<()> {
        self.writer.flush()?;
        self.writer.get_ref().sync_data()
    }

    fn bytes_written(&self) -> u64 {
        self.total_bytes
    }
}

// ── DirectBackend ───────────────────────────────────────────────────────────

/// Direct I/O backend that bypasses the kernel page cache.
/// On macOS: uses F_NOCACHE via fcntl.
/// On Linux: uses O_DIRECT (would need aligned buffers — simplified here).
/// Falls back to StdBackend + F_NOCACHE hint on macOS.
pub struct DirectBackend {
    writer: BufWriter<File>,
    total_bytes: u64,
}

impl DirectBackend {
    pub fn new(path: &Path) -> io::Result<Self> {
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)?;

        // Platform-specific cache bypass hints
        #[cfg(target_os = "macos")]
        {
            use std::os::unix::io::AsRawFd;
            // F_NOCACHE = 48 on macOS — tells the kernel not to cache this file's data
            unsafe {
                libc::fcntl(file.as_raw_fd(), libc::F_NOCACHE, 1);
            }
        }

        #[cfg(target_os = "linux")]
        {
            use std::os::unix::io::AsRawFd;
            // On Linux, we'd ideally use O_DIRECT, but that requires page-aligned
            // buffers. Instead, use posix_fadvise DONTNEED to hint the kernel.
            unsafe {
                libc::posix_fadvise(
                    file.as_raw_fd(),
                    0,
                    0,
                    libc::POSIX_FADV_DONTNEED,
                );
            }
        }

        // Use a larger buffer (512KB) since we're bypassing page cache
        Ok(Self {
            writer: BufWriter::with_capacity(512 * 1024, file),
            total_bytes: 0,
        })
    }
}

impl IoBackend for DirectBackend {
    fn write_all(&mut self, buf: &[u8]) -> io::Result<()> {
        self.writer.write_all(buf)?;
        self.total_bytes += buf.len() as u64;
        Ok(())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.writer.flush()
    }

    fn sync_data(&mut self) -> io::Result<()> {
        self.writer.flush()?;
        self.writer.get_ref().sync_data()
    }

    fn bytes_written(&self) -> u64 {
        self.total_bytes
    }
}

// ── PreallocBackend ─────────────────────────────────────────────────────────

/// Default pre-allocation chunk size: 64 MB.
const PREALLOC_CHUNK: u64 = 64 * 1024 * 1024;
/// Default write buffer capacity: 4 MB.
const PREALLOC_BUFFER_CAP: usize = 4 * 1024 * 1024;

/// Pre-allocated buffered I/O backend.
///
/// On creation, pre-extends the file to 64 MB (using `ftruncate` on macOS,
/// `fallocate` on Linux) to reduce filesystem fragmentation. Maintains a
/// 4 MB userspace write buffer and flushes on sync or when the buffer is full.
/// When the file's logical write position approaches the pre-allocated boundary,
/// the file is extended by another 64 MB chunk.
pub struct PreallocBackend {
    file: File,
    buffer: Vec<u8>,
    buffer_capacity: usize,
    /// Logical size of the file (actual data written, not the pre-allocated extent).
    file_size: u64,
    /// Total pre-allocated extent of the file on disk.
    preallocated_size: u64,
    /// Cumulative bytes written through this backend (for stats).
    total_bytes_written: u64,
    /// Number of sync_data calls (for benchmarking).
    syncs: u64,
}

impl PreallocBackend {
    /// Create a new PreallocBackend for the file at `path`.
    ///
    /// * Pre-allocates the file to 64 MB if it is smaller.
    /// * Opens the file for writing (create if missing, does NOT use append mode
    ///   because we seek to the logical end ourselves to support pre-allocation).
    pub fn new(path: &Path) -> io::Result<Self> {
        Self::with_capacity(path, PREALLOC_BUFFER_CAP, PREALLOC_CHUNK)
    }

    /// Create with custom buffer capacity and pre-allocation chunk size.
    pub fn with_capacity(
        path: &Path,
        buffer_capacity: usize,
        prealloc_chunk: u64,
    ) -> io::Result<Self> {
        let file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .open(path)?;

        // Determine the current logical file size (actual data).
        let meta = file.metadata()?;
        let file_size = meta.len();

        // Pre-allocate if the file is smaller than one chunk.
        let preallocated_size = if file_size < prealloc_chunk {
            Self::preallocate_file(&file, prealloc_chunk)?;
            prealloc_chunk
        } else {
            // Round up to next chunk boundary.
            let chunks = (file_size + prealloc_chunk - 1) / prealloc_chunk;
            let target = chunks * prealloc_chunk;
            if target > file_size {
                Self::preallocate_file(&file, target)?;
            }
            target
        };

        // Seek to the logical end so writes append at the right position.
        use std::io::Seek;
        let mut f = &file;
        f.seek(io::SeekFrom::Start(file_size))?;

        Ok(Self {
            file,
            buffer: Vec::with_capacity(buffer_capacity),
            buffer_capacity,
            file_size,
            preallocated_size,
            total_bytes_written: 0,
            syncs: 0,
        })
    }

    /// Return the number of sync_data calls made on this backend.
    pub fn sync_count(&self) -> u64 {
        self.syncs
    }

    /// Return the current logical file size (actual data, not the pre-allocated extent).
    pub fn logical_file_size(&self) -> u64 {
        self.file_size
    }

    /// Return the current pre-allocated extent on disk.
    pub fn preallocated_size(&self) -> u64 {
        self.preallocated_size
    }

    // ── internal helpers ────────────────────────────────────────────────────

    /// Pre-allocate (extend) the file to `size` bytes.
    /// Uses `fallocate` on Linux, `ftruncate` on macOS/others.
    fn preallocate_file(file: &File, size: u64) -> io::Result<()> {
        #[cfg(target_os = "linux")]
        {
            use std::os::unix::io::AsRawFd;
            let ret = unsafe {
                libc::fallocate(file.as_raw_fd(), 0, 0, size as libc::off_t)
            };
            if ret != 0 {
                return Err(io::Error::last_os_error());
            }
            return Ok(());
        }

        #[cfg(not(target_os = "linux"))]
        {
            // macOS and others: use ftruncate to extend the file.
            file.set_len(size)?;
            Ok(())
        }
    }

    /// Flush the userspace buffer to the kernel.
    fn flush_buffer(&mut self) -> io::Result<()> {
        if self.buffer.is_empty() {
            return Ok(());
        }
        self.file.write_all(&self.buffer)?;
        self.file_size += self.buffer.len() as u64;
        self.buffer.clear();

        // Extend pre-allocation if we're within one buffer-capacity of the boundary.
        if self.file_size + self.buffer_capacity as u64 >= self.preallocated_size {
            let new_size = self.preallocated_size + PREALLOC_CHUNK;
            Self::preallocate_file(&self.file, new_size)?;
            self.preallocated_size = new_size;
        }
        Ok(())
    }
}

impl IoBackend for PreallocBackend {
    fn write_all(&mut self, buf: &[u8]) -> io::Result<()> {
        self.total_bytes_written += buf.len() as u64;

        // If the incoming data alone exceeds the buffer capacity, bypass the buffer
        // entirely to avoid a double-copy.
        if buf.len() >= self.buffer_capacity {
            self.flush_buffer()?;
            self.file.write_all(buf)?;
            self.file_size += buf.len() as u64;

            // Extend pre-allocation if needed.
            if self.file_size + self.buffer_capacity as u64 >= self.preallocated_size {
                let new_size = self.preallocated_size + PREALLOC_CHUNK;
                Self::preallocate_file(&self.file, new_size)?;
                self.preallocated_size = new_size;
            }
            return Ok(());
        }

        self.buffer.extend_from_slice(buf);

        // Auto-flush when the buffer is full.
        if self.buffer.len() >= self.buffer_capacity {
            self.flush_buffer()?;
        }
        Ok(())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.flush_buffer()
    }

    fn sync_data(&mut self) -> io::Result<()> {
        self.flush_buffer()?;
        self.file.sync_data()?;

        // Truncate the file back to logical size so readers don't see zero-fill.
        // (Only needed if preallocated_size > file_size, which is the common case.)
        // NOTE: We intentionally do NOT truncate here — the zeros beyond file_size
        // are only visible if something reads past the logical data. The WAL reader
        // already handles short reads / zero-length entries gracefully.
        self.syncs += 1;
        Ok(())
    }

    fn bytes_written(&self) -> u64 {
        self.total_bytes_written
    }
}

impl Drop for PreallocBackend {
    fn drop(&mut self) {
        // Best-effort: flush remaining buffer and truncate to logical size.
        let _ = self.flush_buffer();
        let _ = self.file.set_len(self.file_size);
        let _ = self.file.sync_all();
    }
}

// ── BatchWriter ─────────────────────────────────────────────────────────────

/// Default batch capacity: 1 MB.
const BATCH_DEFAULT_CAPACITY: usize = 1024 * 1024;
/// Default auto-flush threshold: 512 KB.
const BATCH_DEFAULT_FLUSH_THRESHOLD: usize = 512 * 1024;

/// Batched write coalescing wrapper.
///
/// Wraps any `IoBackend` and coalesces multiple small writes into fewer large
/// writes. This is critical for WAL workloads where individual entries are small
/// (tens to hundreds of bytes) but disk I/O is most efficient with large
/// sequential writes.
pub struct BatchWriter<B: IoBackend> {
    inner: B,
    batch: Vec<u8>,
    batch_capacity: usize,
    auto_flush_threshold: usize,
    /// Cumulative bytes that have entered the batch (including those already flushed).
    total_batched: u64,
    /// Number of times the batch has been flushed to the inner backend.
    total_flushes: u64,
    /// Number of individual write_all calls received.
    write_calls: u64,
}

impl<B: IoBackend> BatchWriter<B> {
    /// Create a new BatchWriter wrapping `inner` with default settings
    /// (1 MB capacity, 512 KB auto-flush threshold).
    pub fn new(inner: B) -> Self {
        Self::with_capacity(inner, BATCH_DEFAULT_CAPACITY, BATCH_DEFAULT_FLUSH_THRESHOLD)
    }

    /// Create a BatchWriter with custom capacity and flush threshold.
    pub fn with_capacity(
        inner: B,
        batch_capacity: usize,
        auto_flush_threshold: usize,
    ) -> Self {
        Self {
            inner,
            batch: Vec::with_capacity(batch_capacity),
            batch_capacity,
            auto_flush_threshold,
            total_batched: 0,
            total_flushes: 0,
            write_calls: 0,
        }
    }

    /// Number of individual write_all calls received by this BatchWriter.
    pub fn write_calls(&self) -> u64 {
        self.write_calls
    }

    /// Number of flush-to-inner events.
    pub fn flush_count(&self) -> u64 {
        self.total_flushes
    }

    /// Total bytes that have entered the batch.
    pub fn total_batched(&self) -> u64 {
        self.total_batched
    }

    /// Current pending batch size in bytes.
    pub fn pending(&self) -> usize {
        self.batch.len()
    }

    /// Consume the BatchWriter and return the inner backend.
    pub fn into_inner(mut self) -> B {
        // Best-effort flush before handing back the inner backend.
        let _ = self.flush_batch();
        self.inner
    }

    // ── internal ────────────────────────────────────────────────────────────

    fn flush_batch(&mut self) -> io::Result<()> {
        if self.batch.is_empty() {
            return Ok(());
        }
        self.inner.write_all(&self.batch)?;
        self.batch.clear();
        self.total_flushes += 1;
        Ok(())
    }
}

impl<B: IoBackend> IoBackend for BatchWriter<B> {
    fn write_all(&mut self, buf: &[u8]) -> io::Result<()> {
        self.write_calls += 1;
        self.total_batched += buf.len() as u64;

        // If a single write exceeds the batch capacity, flush existing batch
        // then write directly through to the inner backend.
        if buf.len() >= self.batch_capacity {
            self.flush_batch()?;
            self.inner.write_all(buf)?;
            self.total_flushes += 1;
            return Ok(());
        }

        self.batch.extend_from_slice(buf);

        if self.batch.len() >= self.auto_flush_threshold {
            self.flush_batch()?;
        }
        Ok(())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.flush_batch()?;
        self.inner.flush()
    }

    fn sync_data(&mut self) -> io::Result<()> {
        self.flush_batch()?;
        self.inner.sync_data()
    }

    fn bytes_written(&self) -> u64 {
        self.total_batched
    }
}

// ── IoStats + StatsBackend ──────────────────────────────────────────────────

/// I/O statistics tracker.
#[derive(Debug, Clone)]
pub struct IoStats {
    pub bytes_written: u64,
    pub bytes_read: u64,
    pub write_calls: u64,
    pub read_calls: u64,
    pub sync_calls: u64,
    pub flush_calls: u64,
    /// Computed: `bytes_written / write_calls` (0.0 if no writes).
    pub avg_write_size: f64,
    /// Peak observed instantaneous write rate in bytes/second.
    pub peak_write_rate_bps: f64,
}

impl Default for IoStats {
    fn default() -> Self {
        Self {
            bytes_written: 0,
            bytes_read: 0,
            write_calls: 0,
            read_calls: 0,
            sync_calls: 0,
            flush_calls: 0,
            avg_write_size: 0.0,
            peak_write_rate_bps: 0.0,
        }
    }
}

impl IoStats {
    /// Recalculate derived fields.
    fn recalculate(&mut self) {
        self.avg_write_size = if self.write_calls > 0 {
            self.bytes_written as f64 / self.write_calls as f64
        } else {
            0.0
        };
    }
}

/// Wrapper that adds I/O statistics tracking to any backend.
pub struct StatsBackend<B: IoBackend> {
    inner: B,
    stats: IoStats,
    /// Instant of the last write call (for rate calculation).
    last_write_time: Instant,
}

impl<B: IoBackend> StatsBackend<B> {
    /// Wrap an existing backend with statistics tracking.
    pub fn new(inner: B) -> Self {
        Self {
            inner,
            stats: IoStats::default(),
            last_write_time: Instant::now(),
        }
    }

    /// Return a snapshot of the current I/O statistics.
    pub fn stats(&self) -> IoStats {
        self.stats.clone()
    }

    /// Reset all statistics counters to zero.
    pub fn reset_stats(&mut self) {
        self.stats = IoStats::default();
        self.last_write_time = Instant::now();
    }

    /// Consume the StatsBackend and return the inner backend.
    pub fn into_inner(self) -> B {
        self.inner
    }
}

impl<B: IoBackend> IoBackend for StatsBackend<B> {
    fn write_all(&mut self, buf: &[u8]) -> io::Result<()> {
        let now = Instant::now();
        let elapsed = now.duration_since(self.last_write_time);

        self.inner.write_all(buf)?;

        self.stats.bytes_written += buf.len() as u64;
        self.stats.write_calls += 1;
        self.stats.recalculate();

        // Update peak write rate (bytes / elapsed seconds since last write).
        let elapsed_secs = elapsed.as_secs_f64();
        if elapsed_secs > 0.0 {
            let rate = buf.len() as f64 / elapsed_secs;
            if rate > self.stats.peak_write_rate_bps {
                self.stats.peak_write_rate_bps = rate;
            }
        }
        self.last_write_time = now;

        Ok(())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.stats.flush_calls += 1;
        self.inner.flush()
    }

    fn sync_data(&mut self) -> io::Result<()> {
        self.stats.sync_calls += 1;
        self.inner.sync_data()
    }

    fn bytes_written(&self) -> u64 {
        self.stats.bytes_written
    }
}

// ── DevNullBackend ──────────────────────────────────────────────────────────

/// A /dev/null backend that tracks call counts but discards all data.
///
/// Useful for benchmarking execution speed without any I/O overhead. Unlike
/// a completely no-op backend, this one increments counters so you can verify
/// that the expected number of writes/syncs occurred.
pub struct DevNullBackend {
    total_bytes: u64,
    write_calls: u64,
    flush_calls: u64,
    sync_calls: u64,
}

impl DevNullBackend {
    pub fn new() -> Self {
        Self {
            total_bytes: 0,
            write_calls: 0,
            flush_calls: 0,
            sync_calls: 0,
        }
    }

    /// Number of write_all calls.
    pub fn write_calls(&self) -> u64 {
        self.write_calls
    }

    /// Number of flush calls.
    pub fn flush_calls(&self) -> u64 {
        self.flush_calls
    }

    /// Number of sync_data calls.
    pub fn sync_calls(&self) -> u64 {
        self.sync_calls
    }
}

impl Default for DevNullBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl IoBackend for DevNullBackend {
    fn write_all(&mut self, buf: &[u8]) -> io::Result<()> {
        self.total_bytes += buf.len() as u64;
        self.write_calls += 1;
        Ok(())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.flush_calls += 1;
        Ok(())
    }

    fn sync_data(&mut self) -> io::Result<()> {
        self.sync_calls += 1;
        Ok(())
    }

    fn bytes_written(&self) -> u64 {
        self.total_bytes
    }
}

// ── BackendType + Factory ───────────────────────────────────────────────────

/// Selects which I/O backend to instantiate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackendType {
    /// DevNullBackend — discards data, tracks counts.
    Null,
    /// StdBackend — BufWriter<File> (256 KB buffer).
    Standard,
    /// DirectBackend — cache-bypass hints + BufWriter (512 KB buffer).
    Direct,
    /// PreallocBackend — 64 MB pre-allocation + 4 MB write buffer.
    Prealloc,
    /// BatchWriter wrapping StdBackend — coalesces small writes.
    Batched,
}

/// Create the best available I/O backend for the current platform.
///
/// This is the original two-argument factory preserved for backward compatibility.
pub fn create_backend(path: &Path, use_direct: bool) -> io::Result<Box<dyn IoBackend>> {
    if use_direct {
        Ok(Box::new(DirectBackend::new(path)?))
    } else {
        Ok(Box::new(StdBackend::new(path)?))
    }
}

/// Create an I/O backend by type.
///
/// For the `Null` variant, `path` is ignored (no file is opened).
/// For all other variants, `path` is the file to open/create.
pub fn create_backend_typed(
    backend_type: BackendType,
    path: &Path,
) -> io::Result<Box<dyn IoBackend>> {
    match backend_type {
        BackendType::Null => Ok(Box::new(DevNullBackend::new())),
        BackendType::Standard => Ok(Box::new(StdBackend::new(path)?)),
        BackendType::Direct => Ok(Box::new(DirectBackend::new(path)?)),
        BackendType::Prealloc => Ok(Box::new(PreallocBackend::new(path)?)),
        BackendType::Batched => {
            let std = StdBackend::new(path)?;
            Ok(Box::new(BatchWriter::new(std)))
        }
    }
}

// ── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn tmp_path(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join("arc-io-backend-tests");
        let _ = fs::create_dir_all(&dir);
        dir.join(name)
    }

    // ── Existing tests (preserved) ──────────────────────────────────────────

    #[test]
    fn std_backend_write_and_sync() {
        let path = tmp_path("std_backend.bin");
        let _ = fs::remove_file(&path);

        {
            let mut backend = StdBackend::new(&path).unwrap();
            backend.write_all(b"hello world").unwrap();
            backend.sync_data().unwrap();
        }

        let data = fs::read(&path).unwrap();
        assert_eq!(data, b"hello world");
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn direct_backend_write_and_sync() {
        let path = tmp_path("direct_backend.bin");
        let _ = fs::remove_file(&path);

        {
            let mut backend = DirectBackend::new(&path).unwrap();
            backend.write_all(b"direct io test").unwrap();
            backend.sync_data().unwrap();
        }

        let data = fs::read(&path).unwrap();
        assert_eq!(data, b"direct io test");
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn create_backend_factory() {
        let path = tmp_path("factory_test.bin");
        let _ = fs::remove_file(&path);

        {
            let mut backend = create_backend(&path, false).unwrap();
            backend.write_all(b"factory").unwrap();
            backend.sync_data().unwrap();
        }

        let data = fs::read(&path).unwrap();
        assert_eq!(data, b"factory");
        let _ = fs::remove_file(&path);
    }

    // ── New tests ───────────────────────────────────────────────────────────

    #[test]
    fn test_prealloc_backend_write_and_read() {
        let path = tmp_path("prealloc_wr.bin");
        let _ = fs::remove_file(&path);

        let payload = b"prealloc write test data 1234567890";
        {
            let mut backend = PreallocBackend::new(&path).unwrap();
            backend.write_all(payload).unwrap();
            backend.sync_data().unwrap();
            assert_eq!(backend.bytes_written(), payload.len() as u64);
            // Drop triggers truncate-to-logical-size + sync.
        }

        // After drop, file should be truncated to logical size.
        let data = fs::read(&path).unwrap();
        assert_eq!(&data[..], payload);
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn test_prealloc_backend_auto_extends() {
        let path = tmp_path("prealloc_extend.bin");
        let _ = fs::remove_file(&path);

        // Use a tiny pre-allocation chunk (1 KB) and tiny buffer (256 bytes)
        // so we can test auto-extension without writing 64 MB of data.
        let chunk: u64 = 1024;
        let buf_cap: usize = 256;

        {
            let mut backend =
                PreallocBackend::with_capacity(&path, buf_cap, chunk).unwrap();

            let initial_prealloc = backend.preallocated_size();
            assert_eq!(initial_prealloc, chunk);

            // Write more than one chunk worth of data.
            let block = vec![0xABu8; 512];
            for _ in 0..4 {
                backend.write_all(&block).unwrap();
            }
            backend.flush().unwrap();

            // Pre-allocated size should have grown beyond the initial chunk.
            assert!(
                backend.preallocated_size() > initial_prealloc,
                "preallocated_size ({}) should have grown past initial ({})",
                backend.preallocated_size(),
                initial_prealloc,
            );
        }

        // Verify the logical data is intact after drop.
        let data = fs::read(&path).unwrap();
        assert_eq!(data.len(), 512 * 4);
        assert!(data.iter().all(|&b| b == 0xAB));
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn test_batch_writer_coalesces_writes() {
        // Use DevNullBackend as the inner so we can count actual flush-throughs.
        let inner = DevNullBackend::new();
        // 1 KB capacity, 512 byte flush threshold.
        let mut batch = BatchWriter::with_capacity(inner, 1024, 512);

        // Write 10 small chunks (50 bytes each = 500 bytes, under threshold).
        for _ in 0..10 {
            batch.write_all(&[0u8; 50]).unwrap();
        }
        // Nothing should have been flushed yet (500 < 512 threshold).
        assert_eq!(batch.flush_count(), 0);
        assert_eq!(batch.write_calls(), 10);
        assert_eq!(batch.pending(), 500);

        // One more write pushes us to 550 bytes, over the 512 threshold.
        batch.write_all(&[0u8; 50]).unwrap();
        // The batch should have auto-flushed.
        assert_eq!(batch.flush_count(), 1);
        assert_eq!(batch.pending(), 0);

        // The inner backend should have received exactly ONE write_all call
        // (the coalesced batch), not 11.
        // We can check via bytes_written: 550 bytes total.
        assert_eq!(batch.bytes_written(), 550);
    }

    #[test]
    fn test_batch_writer_auto_flush() {
        let path = tmp_path("batch_autoflush.bin");
        let _ = fs::remove_file(&path);

        let inner = StdBackend::new(&path).unwrap();
        // 256 byte capacity, 128 byte threshold.
        let mut batch = BatchWriter::with_capacity(inner, 256, 128);

        // Write 64 bytes — under threshold, should stay in batch.
        batch.write_all(&[b'A'; 64]).unwrap();
        assert_eq!(batch.flush_count(), 0);

        // Write another 128 bytes — total 192, over 128 threshold => auto-flush.
        batch.write_all(&[b'B'; 128]).unwrap();
        assert_eq!(batch.flush_count(), 1);

        // Explicit sync flushes any remaining + syncs inner.
        batch.sync_data().unwrap();

        // Verify file contents.
        let data = fs::read(&path).unwrap();
        assert_eq!(data.len(), 192);
        assert_eq!(&data[..64], &[b'A'; 64]);
        assert_eq!(&data[64..], &[b'B'; 128]);

        let _ = fs::remove_file(&path);
    }

    #[test]
    fn test_stats_backend_tracks_writes() {
        let inner = DevNullBackend::new();
        let mut stats_be = StatsBackend::new(inner);

        stats_be.write_all(b"one").unwrap();
        stats_be.write_all(b"two").unwrap();
        stats_be.write_all(b"three").unwrap();
        stats_be.flush().unwrap();
        stats_be.sync_data().unwrap();

        let s = stats_be.stats();
        assert_eq!(s.write_calls, 3);
        assert_eq!(s.flush_calls, 1);
        assert_eq!(s.sync_calls, 1);
    }

    #[test]
    fn test_stats_backend_bytes_accurate() {
        let inner = DevNullBackend::new();
        let mut stats_be = StatsBackend::new(inner);

        stats_be.write_all(&[0u8; 100]).unwrap();
        stats_be.write_all(&[0u8; 250]).unwrap();
        stats_be.write_all(&[0u8; 50]).unwrap();

        let s = stats_be.stats();
        assert_eq!(s.bytes_written, 400);
        assert_eq!(s.write_calls, 3);
        // avg_write_size should be 400 / 3 ≈ 133.33
        assert!((s.avg_write_size - 133.333).abs() < 1.0);
        assert_eq!(stats_be.bytes_written(), 400);
    }

    #[test]
    fn test_backend_factory() {
        let path = tmp_path("factory_typed.bin");
        let _ = fs::remove_file(&path);

        // Null backend — no file needed.
        {
            let mut be = create_backend_typed(BackendType::Null, &path).unwrap();
            be.write_all(b"gone").unwrap();
            assert_eq!(be.bytes_written(), 4);
        }

        // Standard backend.
        {
            let _ = fs::remove_file(&path);
            let mut be = create_backend_typed(BackendType::Standard, &path).unwrap();
            be.write_all(b"std").unwrap();
            be.sync_data().unwrap();
            assert_eq!(fs::read(&path).unwrap(), b"std");
        }

        // Direct backend.
        {
            let _ = fs::remove_file(&path);
            let mut be = create_backend_typed(BackendType::Direct, &path).unwrap();
            be.write_all(b"direct").unwrap();
            be.sync_data().unwrap();
            assert_eq!(fs::read(&path).unwrap(), b"direct");
        }

        // Prealloc backend.
        {
            let _ = fs::remove_file(&path);
            let mut be = create_backend_typed(BackendType::Prealloc, &path).unwrap();
            be.write_all(b"prealloc").unwrap();
            be.sync_data().unwrap();
            drop(be); // drop truncates to logical size
            assert_eq!(fs::read(&path).unwrap(), b"prealloc");
        }

        // Batched backend.
        {
            let _ = fs::remove_file(&path);
            let mut be = create_backend_typed(BackendType::Batched, &path).unwrap();
            be.write_all(b"batched").unwrap();
            be.sync_data().unwrap();
            assert_eq!(fs::read(&path).unwrap(), b"batched");
        }

        let _ = fs::remove_file(&path);
    }

    #[test]
    fn test_null_backend_discards_data() {
        let mut be = DevNullBackend::new();

        be.write_all(b"this data goes nowhere").unwrap();
        be.write_all(b"more discarded data").unwrap();
        be.flush().unwrap();
        be.sync_data().unwrap();

        // Bytes are tracked even though data is discarded.
        assert_eq!(be.bytes_written(), 41); // 22 + 19
        assert_eq!(be.write_calls(), 2);
        assert_eq!(be.flush_calls(), 1);
        assert_eq!(be.sync_calls(), 1);

        // But there is no file to read — all data was discarded.
    }
}
