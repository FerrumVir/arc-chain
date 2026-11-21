use std::fs::{File, OpenOptions};
use std::io::{self, BufWriter, Write};
use std::path::Path;
use std::time::Instant;

pub trait IoBackend: Send + 'static {
    fn write_all(&mut self, buf: &[u8]) -> io::Result<()>;
    fn flush(&mut self) -> io::Result<()>;
    fn sync_data(&mut self) -> io::Result<()>;
    fn bytes_written(&self) -> u64;
}

pub struct StdBackend {
    writer: BufWriter<File>,
    total_bytes: u64,
}

impl StdBackend {
    pub fn new(path: &Path) -> io::Result<Self> {
        let file = OpenOptions::new().create(true).append(true).open(path)?;
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

pub struct DirectBackend {
    writer: BufWriter<File>,
    total_bytes: u64,
}

impl DirectBackend {
    pub fn new(path: &Path) -> io::Result<Self> {
        let file = OpenOptions::new().create(true).append(true).open(path)?;

        #[cfg(target_os = "macos")]
        {
            use std::os::unix::io::AsRawFd;
            unsafe {
                libc::fcntl(file.as_raw_fd(), libc::F_NOCACHE, 1);
            }
        }

        #[cfg(target_os = "linux")]
        {
            use std::os::unix::io::AsRawFd;
            unsafe {
                libc::posix_fadvise(file.as_raw_fd(), 0, 0, libc::POSIX_FADV_DONTNEED);
            }
        }

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

const PREALLOC_CHUNK: u64 = 64 * 1024 * 1024;
const PREALLOC_BUFFER_CAP: usize = 4 * 1024 * 1024;

pub struct PreallocBackend {
    file: File,
    buffer: Vec<u8>,
    buffer_capacity: usize,
    file_size: u64,
    preallocated_size: u64,
    total_bytes_written: u64,
    syncs: u64,
}

impl PreallocBackend {
    pub fn new(path: &Path) -> io::Result<Self> {
        Self::with_capacity(path, PREALLOC_BUFFER_CAP, PREALLOC_CHUNK)
    }

    pub fn with_capacity(
        path: &Path,
        buffer_capacity: usize,
        prealloc_chunk: u64,
    ) -> io::Result<Self> {
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(path)?;

        let meta = file.metadata()?;
        let file_size = meta.len();

        let preallocated_size = if file_size < prealloc_chunk {
            Self::preallocate_file(&file, prealloc_chunk)?;
            prealloc_chunk
        } else {
            let chunks = file_size.div_ceil(prealloc_chunk);
            let target = chunks * prealloc_chunk;
            if target > file_size {
                Self::preallocate_file(&file, target)?;
            }
            target
        };

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

    pub fn sync_count(&self) -> u64 {
        self.syncs
    }

    pub fn logical_file_size(&self) -> u64 {
        self.file_size
    }

    pub fn preallocated_size(&self) -> u64 {
        self.preallocated_size
    }

    fn preallocate_file(file: &File, size: u64) -> io::Result<()> {
        #[cfg(target_os = "linux")]
        {
            use std::os::unix::io::AsRawFd;
            let ret = unsafe { libc::fallocate(file.as_raw_fd(), 0, 0, size as libc::off_t) };
            if ret != 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        }

        #[cfg(not(target_os = "linux"))]
        {
            file.set_len(size)?;
            Ok(())
        }
    }

    fn flush_buffer(&mut self) -> io::Result<()> {
        if self.buffer.is_empty() {
            return Ok(());
        }
        self.file.write_all(&self.buffer)?;
        self.file_size += self.buffer.len() as u64;
        self.buffer.clear();

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

        if buf.len() >= self.buffer_capacity {
            self.flush_buffer()?;
            self.file.write_all(buf)?;
            self.file_size += buf.len() as u64;

            if self.file_size + self.buffer_capacity as u64 >= self.preallocated_size {
                let new_size = self.preallocated_size + PREALLOC_CHUNK;
                Self::preallocate_file(&self.file, new_size)?;
                self.preallocated_size = new_size;
            }
            return Ok(());
        }

        self.buffer.extend_from_slice(buf);

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

        self.syncs += 1;
        Ok(())
    }

    fn bytes_written(&self) -> u64 {
        self.total_bytes_written
    }
}

impl Drop for PreallocBackend {
    fn drop(&mut self) {
        let _ = self.flush_buffer();
        let _ = self.file.set_len(self.file_size);
        let _ = self.file.sync_all();
    }
}

const BATCH_DEFAULT_CAPACITY: usize = 1024 * 1024;
const BATCH_DEFAULT_FLUSH_THRESHOLD: usize = 512 * 1024;

pub struct BatchWriter<B: IoBackend> {
    inner: B,
    batch: Vec<u8>,
    batch_capacity: usize,
    auto_flush_threshold: usize,
    total_batched: u64,
    total_flushes: u64,
    write_calls: u64,
}

impl<B: IoBackend> BatchWriter<B> {
    pub fn new(inner: B) -> Self {
        Self::with_capacity(inner, BATCH_DEFAULT_CAPACITY, BATCH_DEFAULT_FLUSH_THRESHOLD)
    }

    pub fn with_capacity(inner: B, batch_capacity: usize, auto_flush_threshold: usize) -> Self {
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

    pub fn write_calls(&self) -> u64 {
        self.write_calls
    }

    pub fn flush_count(&self) -> u64 {
        self.total_flushes
    }

    pub fn total_batched(&self) -> u64 {
        self.total_batched
    }

    pub fn pending(&self) -> usize {
        self.batch.len()
    }

    pub fn into_inner(mut self) -> B {
        let _ = self.flush_batch();
        self.inner
    }

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

#[derive(Debug, Clone)]
pub struct IoStats {
    pub bytes_written: u64,
    pub bytes_read: u64,
    pub write_calls: u64,
    pub read_calls: u64,
    pub sync_calls: u64,
    pub flush_calls: u64,
    pub avg_write_size: f64,
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
    fn recalculate(&mut self) {
        self.avg_write_size = if self.write_calls > 0 {
            self.bytes_written as f64 / self.write_calls as f64
        } else {
            0.0
        };
    }
}

pub struct StatsBackend<B: IoBackend> {
    inner: B,
    stats: IoStats,
    last_write_time: Instant,
}

impl<B: IoBackend> StatsBackend<B> {
    pub fn new(inner: B) -> Self {
        Self {
            inner,
            stats: IoStats::default(),
            last_write_time: Instant::now(),
        }
    }

    pub fn stats(&self) -> IoStats {
        self.stats.clone()
    }

    pub fn reset_stats(&mut self) {
        self.stats = IoStats::default();
        self.last_write_time = Instant::now();
    }

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

    pub fn write_calls(&self) -> u64 {
        self.write_calls
    }

    pub fn flush_calls(&self) -> u64 {
        self.flush_calls
    }

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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackendType {
    Null,
    Standard,
    Direct,
    Prealloc,
    Batched,
}

pub fn create_backend(path: &Path, use_direct: bool) -> io::Result<Box<dyn IoBackend>> {
    if use_direct {
        Ok(Box::new(DirectBackend::new(path)?))
    } else {
        Ok(Box::new(StdBackend::new(path)?))
    }
}

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

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn tmp_path(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join("arc-io-backend-tests");
        let _ = fs::create_dir_all(&dir);
        dir.join(name)
    }

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
        }

        let data = fs::read(&path).unwrap();
        assert_eq!(&data[..], payload);
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn test_prealloc_backend_auto_extends() {
        let path = tmp_path("prealloc_extend.bin");
        let _ = fs::remove_file(&path);

        let chunk: u64 = 1024;
        let buf_cap: usize = 256;

        {
            let mut backend = PreallocBackend::with_capacity(&path, buf_cap, chunk).unwrap();

            let initial_prealloc = backend.preallocated_size();
            assert_eq!(initial_prealloc, chunk);

            let block = vec![0xABu8; 512];
            for _ in 0..4 {
                backend.write_all(&block).unwrap();
            }
            backend.flush().unwrap();

            assert!(
                backend.preallocated_size() > initial_prealloc,
                "preallocated_size ({}) should have grown past initial ({})",
                backend.preallocated_size(),
                initial_prealloc,
            );
        }

        let data = fs::read(&path).unwrap();
        assert_eq!(data.len(), 512 * 4);
        assert!(data.iter().all(|&b| b == 0xAB));
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn test_batch_writer_coalesces_writes() {
        let inner = DevNullBackend::new();
        let mut batch = BatchWriter::with_capacity(inner, 1024, 512);

        for _ in 0..10 {
            batch.write_all(&[0u8; 50]).unwrap();
        }
        assert_eq!(batch.flush_count(), 0);
        assert_eq!(batch.write_calls(), 10);
        assert_eq!(batch.pending(), 500);

        batch.write_all(&[0u8; 50]).unwrap();
        assert_eq!(batch.flush_count(), 1);
        assert_eq!(batch.pending(), 0);

        assert_eq!(batch.bytes_written(), 550);
    }

    #[test]
    fn test_batch_writer_auto_flush() {
        let path = tmp_path("batch_autoflush.bin");
        let _ = fs::remove_file(&path);

        let inner = StdBackend::new(&path).unwrap();
        let mut batch = BatchWriter::with_capacity(inner, 256, 128);

        batch.write_all(&[b'A'; 64]).unwrap();
        assert_eq!(batch.flush_count(), 0);

        batch.write_all(&[b'B'; 128]).unwrap();
        assert_eq!(batch.flush_count(), 1);

        batch.sync_data().unwrap();

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
        assert!((s.avg_write_size - 133.333).abs() < 1.0);
        assert_eq!(stats_be.bytes_written(), 400);
    }

    #[test]
    fn test_backend_factory() {
        let path = tmp_path("factory_typed.bin");
        let _ = fs::remove_file(&path);

        {
            let mut be = create_backend_typed(BackendType::Null, &path).unwrap();
            be.write_all(b"gone").unwrap();
            assert_eq!(be.bytes_written(), 4);
        }

        {
            let _ = fs::remove_file(&path);
            let mut be = create_backend_typed(BackendType::Standard, &path).unwrap();
            be.write_all(b"std").unwrap();
            be.sync_data().unwrap();
            assert_eq!(fs::read(&path).unwrap(), b"std");
        }

        {
            let _ = fs::remove_file(&path);
            let mut be = create_backend_typed(BackendType::Direct, &path).unwrap();
            be.write_all(b"direct").unwrap();
            be.sync_data().unwrap();
            assert_eq!(fs::read(&path).unwrap(), b"direct");
        }

        {
            let _ = fs::remove_file(&path);
            let mut be = create_backend_typed(BackendType::Prealloc, &path).unwrap();
            be.write_all(b"prealloc").unwrap();
            be.sync_data().unwrap();
            drop(be);
            assert_eq!(fs::read(&path).unwrap(), b"prealloc");
        }

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

        assert_eq!(be.bytes_written(), 41);
        assert_eq!(be.write_calls(), 2);
        assert_eq!(be.flush_calls(), 1);
        assert_eq!(be.sync_calls(), 1);
    }
}
