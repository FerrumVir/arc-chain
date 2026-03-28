//! Write-Ahead Log (WAL) for ARC Chain state persistence.
//!
//! Every state mutation is journaled to an append-only file BEFORE acknowledging.
//! Sequential writes only — never seeks, never reads during execution.
//! The async writer batches entries and flushes to SSD periodically.

use arc_crypto::Hash256;
use arc_types::{Account, Address, Block, TxReceipt};
use crossbeam::channel::{self, Receiver, Sender};
use serde::{Deserialize, Serialize};
use std::fs::{self, File, OpenOptions};
use std::io::{BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::thread;

// ── WAL Types ───────────────────────────────────────────────────────────────

/// A single WAL entry recording one state mutation.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WalEntry {
    /// Block height this mutation belongs to.
    pub block_height: u64,
    /// Monotonic sequence number within the WAL.
    pub sequence: u64,
    /// The state operation.
    pub op: WalOp,
    /// CRC32 checksum of the serialized (block_height, sequence, op).
    pub checksum: u32,
}

/// State operations that the WAL records.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum WalOp {
    /// Set or update an account.
    SetAccount(Address, Account),
    /// Set a storage key-value pair for a contract.
    SetStorage(Address, Hash256, Vec<u8>),
    /// Delete a storage key for a contract.
    DeleteStorage(Address, Hash256),
    /// Store a finalized block.
    SetBlock(u64, Block),
    /// Store a transaction receipt.
    SetReceipt(Hash256, TxReceipt),
    /// Store agent info (agent_address, name, endpoint, capabilities).
    SetAgent(Address, String, String, Vec<u8>),
    /// Store contract WASM bytecode.
    SetContract(Address, Vec<u8>),
    /// Checkpoint: marks a consistent state root at this point.
    /// Used for crash recovery — replay starts from the last checkpoint.
    Checkpoint(Hash256),
}

/// Internal command for the WAL background thread.
enum WalCommand {
    /// Append an entry to the WAL.
    Append(WalEntry),
    /// Flush all pending writes and fsync.
    Sync(channel::Sender<()>),
    /// Rotate: close the current segment, open a new one.
    Rotate(channel::Sender<()>),
    /// Shutdown the writer thread.
    Shutdown,
}

// ── WAL Writer ──────────────────────────────────────────────────────────────

/// Non-blocking WAL writer. Sends entries to a background thread that batches
/// and flushes writes. Execution threads are never blocked by I/O.
pub struct WalWriter {
    sender: Sender<WalCommand>,
    sequence: AtomicU64,
    handle: Option<thread::JoinHandle<()>>,
    shutdown: Arc<AtomicBool>,
    /// Directory containing WAL segment files.
    wal_dir: PathBuf,
}

impl WalWriter {
    /// Create a new WAL writer that writes to the given file path.
    /// Spawns a background thread for async I/O.
    pub fn new(path: impl AsRef<Path>) -> std::io::Result<Self> {
        let path = path.as_ref().to_path_buf();
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)?;
        let mut writer = BufWriter::with_capacity(256 * 1024, file); // 256KB buffer

        let (sender, receiver): (Sender<WalCommand>, Receiver<WalCommand>) = channel::unbounded();
        let shutdown = Arc::new(AtomicBool::new(false));
        let shutdown_clone = shutdown.clone();

        // The WAL directory is the parent of the WAL file path.
        let wal_dir = path.parent().unwrap_or_else(|| Path::new(".")).to_path_buf();

        // Determine starting sequence by reading existing entries (before path is moved)
        let seq = Self::count_entries(&path);

        let writer_path = path.clone();
        let handle = thread::Builder::new()
            .name("wal-writer".into())
            .spawn(move || {
                Self::writer_loop(&mut writer, &receiver, &shutdown_clone, &writer_path, 0, 0);
            })?;

        Ok(Self {
            sender,
            sequence: AtomicU64::new(seq),
            handle: Some(handle),
            shutdown,
            wal_dir,
        })
    }

    /// Create a new WAL writer that writes segmented files in a directory.
    /// Segment naming: `wal-{segment_number:08}.bin`
    /// Spawns a background thread for async I/O.
    /// Automatically rotates when segment exceeds `max_segment_size`.
    pub fn with_segments(
        wal_dir: impl AsRef<Path>,
        max_segment_size: u64,
    ) -> std::io::Result<Self> {
        let wal_dir = wal_dir.as_ref().to_path_buf();
        fs::create_dir_all(&wal_dir)?;

        // Find the latest segment or create segment 0
        let (segment_number, seg_path) = Self::find_latest_segment(&wal_dir);
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&seg_path)?;
        let mut writer = BufWriter::with_capacity(256 * 1024, file);

        let (sender, receiver): (Sender<WalCommand>, Receiver<WalCommand>) = channel::unbounded();
        let shutdown = Arc::new(AtomicBool::new(false));
        let shutdown_clone = shutdown.clone();

        let dir_clone = wal_dir.clone();
        let handle = thread::Builder::new()
            .name("wal-writer".into())
            .spawn(move || {
                Self::writer_loop(
                    &mut writer,
                    &receiver,
                    &shutdown_clone,
                    &dir_clone,
                    segment_number,
                    max_segment_size,
                );
            })?;

        // Count entries across all segments for sequence recovery
        let seq = Self::count_entries_in_dir(&wal_dir);

        Ok(Self {
            sender,
            sequence: AtomicU64::new(seq),
            handle: Some(handle),
            shutdown,
            wal_dir,
        })
    }

    /// Create a "null" WAL writer that discards all entries.
    /// Used for benchmarks and tests that don't need persistence.
    pub fn null() -> Self {
        let (sender, _receiver) = channel::unbounded();
        Self {
            sender,
            sequence: AtomicU64::new(0),
            handle: None,
            shutdown: Arc::new(AtomicBool::new(false)),
            wal_dir: PathBuf::new(),
        }
    }

    /// Returns true if this WAL writer is active (not null).
    #[inline]
    /// Current WAL sequence number (monotonically increasing entry counter).
    pub fn sequence(&self) -> u64 {
        self.sequence.load(std::sync::atomic::Ordering::Relaxed)
    }

    pub fn is_active(&self) -> bool {
        self.handle.is_some()
    }

    /// Non-blocking. Sends an entry to the background writer.
    pub fn append(&self, op: WalOp, block_height: u64) {
        // Null WAL: no writer thread, no handle → skip serialize/send entirely
        if self.handle.is_none() {
            return;
        }

        let seq = self.sequence.fetch_add(1, Ordering::Relaxed);
        let payload = bincode::serialize(&(&block_height, &seq, &op)).unwrap_or_default();
        let checksum = crc32fast::hash(&payload);

        let entry = WalEntry {
            block_height,
            sequence: seq,
            op,
            checksum,
        };

        // Best effort — if the channel is full or disconnected, we log and continue.
        // In production, this should never happen (writer is faster than execution).
        if self.sender.send(WalCommand::Append(entry)).is_err() {
            tracing::error!("WAL writer channel disconnected");
        }
    }

    /// Blocks until all pending entries are fsynced to disk.
    /// Call at block boundaries for durability guarantees.
    pub fn sync(&self) {
        let (done_tx, done_rx) = channel::bounded(1);
        if self.sender.send(WalCommand::Sync(done_tx)).is_ok() {
            let _ = done_rx.recv();
        }
    }

    /// Request the writer thread to rotate: close the current segment file
    /// and open a new one. Blocks until rotation is complete.
    pub fn rotate(&self) {
        if self.handle.is_none() {
            return;
        }
        let (done_tx, done_rx) = channel::bounded(1);
        if self.sender.send(WalCommand::Rotate(done_tx)).is_ok() {
            let _ = done_rx.recv();
        }
    }

    /// Delete WAL segment files whose entries are all before the given sequence number.
    /// Keeps at least `min_retain` segments for safety (defaults to 2 if 0 is given).
    pub fn delete_segments_before(&self, wal_sequence: u64) -> std::io::Result<u32> {
        Self::delete_segments_before_in_dir(&self.wal_dir, wal_sequence, 2)
    }

    /// Static version: scan `wal_dir` for segment files, read each segment's
    /// last entry sequence, and delete segments whose entries are all before
    /// `wal_sequence`. Keeps at least `min_retain` segments.
    pub fn delete_segments_before_in_dir(
        wal_dir: &Path,
        wal_sequence: u64,
        min_retain: usize,
    ) -> std::io::Result<u32> {
        let min_retain = if min_retain < 2 { 2 } else { min_retain };
        let mut segments = Self::list_segments(wal_dir);
        segments.sort(); // sort by name (ascending segment number)

        if segments.len() <= min_retain {
            return Ok(0);
        }

        // For each segment, find the last entry's sequence number.
        // A segment is deletable if its last entry sequence < wal_sequence.
        let mut deletable: Vec<PathBuf> = Vec::new();
        for seg_path in &segments {
            let entries = read_wal(seg_path);
            if let Some(last) = entries.last() {
                if last.sequence < wal_sequence {
                    deletable.push(seg_path.clone());
                }
            }
            // Empty segments are also candidates for deletion
            else {
                deletable.push(seg_path.clone());
            }
        }

        // Never delete so many that fewer than min_retain segments remain.
        let max_deletable = segments.len().saturating_sub(min_retain);
        let to_delete = deletable.len().min(max_deletable);

        let mut deleted = 0u32;
        for path in deletable.into_iter().take(to_delete) {
            if fs::remove_file(&path).is_ok() {
                deleted += 1;
            }
        }
        Ok(deleted)
    }

    /// Shut down the WAL writer, flushing all remaining entries.
    pub fn shutdown(&mut self) {
        self.shutdown.store(true, Ordering::Release);
        let _ = self.sender.send(WalCommand::Shutdown);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }

    /// The background writer loop. Receives entries, writes them, periodically flushes.
    /// Handles Rotate commands by closing the current file and opening a new segment.
    /// When `max_segment_size > 0`, automatically rotates after the segment exceeds that size.
    fn writer_loop(
        writer: &mut BufWriter<File>,
        receiver: &Receiver<WalCommand>,
        _shutdown: &AtomicBool,
        wal_path: &Path,
        initial_segment: u64,
        max_segment_size: u64,
    ) {
        let mut segment_number = initial_segment;
        // Determine if we are in segmented mode (wal_path is a directory)
        // or legacy mode (wal_path is a file).
        let is_dir = wal_path.is_dir();

        // Track bytes written to the current segment for auto-rotation.
        let mut bytes_written: u64 = writer
            .get_ref()
            .metadata()
            .map(|m| m.len())
            .unwrap_or(0);

        /// Helper: check if auto-rotation is needed and perform it.
        fn maybe_auto_rotate(
            writer: &mut BufWriter<File>,
            wal_path: &Path,
            is_dir: bool,
            segment_number: &mut u64,
            bytes_written: &mut u64,
            max_segment_size: u64,
        ) {
            if max_segment_size > 0 && *bytes_written >= max_segment_size {
                WalWriter::do_rotate(writer, wal_path, is_dir, segment_number);
                *bytes_written = 0;
            }
        }

        loop {
            match receiver.recv() {
                Ok(WalCommand::Append(entry)) => {
                    // Serialize entry with a length prefix (4 bytes LE)
                    if let Ok(data) = bincode::serialize(&entry) {
                        let entry_size = 4 + data.len() as u64;
                        let len = (data.len() as u32).to_le_bytes();
                        let _ = writer.write_all(&len);
                        let _ = writer.write_all(&data);
                        bytes_written += entry_size;
                    }

                    // Drain any buffered entries without blocking
                    while let Ok(cmd) = receiver.try_recv() {
                        match cmd {
                            WalCommand::Append(entry) => {
                                if let Ok(data) = bincode::serialize(&entry) {
                                    let entry_size = 4 + data.len() as u64;
                                    let len = (data.len() as u32).to_le_bytes();
                                    let _ = writer.write_all(&len);
                                    let _ = writer.write_all(&data);
                                    bytes_written += entry_size;
                                }
                            }
                            WalCommand::Sync(done) => {
                                let _ = writer.flush();
                                let _ = writer.get_ref().sync_data();
                                let _ = done.send(());
                            }
                            WalCommand::Rotate(done) => {
                                Self::do_rotate(writer, wal_path, is_dir, &mut segment_number);
                                bytes_written = 0;
                                let _ = done.send(());
                            }
                            WalCommand::Shutdown => {
                                let _ = writer.flush();
                                let _ = writer.get_ref().sync_data();
                                return;
                            }
                        }
                    }
                    // Flush after draining batch
                    let _ = writer.flush();

                    // Auto-rotate if segment size exceeded
                    maybe_auto_rotate(
                        writer,
                        wal_path,
                        is_dir,
                        &mut segment_number,
                        &mut bytes_written,
                        max_segment_size,
                    );
                }
                Ok(WalCommand::Sync(done)) => {
                    let _ = writer.flush();
                    let _ = writer.get_ref().sync_data();
                    let _ = done.send(());
                }
                Ok(WalCommand::Rotate(done)) => {
                    Self::do_rotate(writer, wal_path, is_dir, &mut segment_number);
                    bytes_written = 0;
                    let _ = done.send(());
                }
                Ok(WalCommand::Shutdown) | Err(_) => {
                    let _ = writer.flush();
                    let _ = writer.get_ref().sync_data();
                    return;
                }
            }
        }
    }

    /// Perform a segment rotation: flush + fsync the current writer, then
    /// replace it with a new segment file.
    fn do_rotate(
        writer: &mut BufWriter<File>,
        wal_path: &Path,
        is_dir: bool,
        segment_number: &mut u64,
    ) {
        // Flush and sync current file
        let _ = writer.flush();
        let _ = writer.get_ref().sync_data();

        // Determine the directory and new segment number
        *segment_number += 1;
        let new_path = if is_dir {
            wal_path.join(format!("wal-{:08}.bin", segment_number))
        } else {
            let dir = wal_path.parent().unwrap_or_else(|| Path::new("."));
            dir.join(format!("wal-{:08}.bin", segment_number))
        };

        // Open new segment file
        if let Ok(file) = OpenOptions::new().create(true).append(true).open(&new_path) {
            *writer = BufWriter::with_capacity(256 * 1024, file);
        } else {
            tracing::error!("Failed to open new WAL segment: {:?}", new_path);
        }
    }

    /// Count existing entries in a WAL file (for sequence recovery).
    fn count_entries(path: &Path) -> u64 {
        let file = match File::open(path) {
            Ok(f) => f,
            Err(_) => return 0,
        };
        let mut reader = BufReader::new(file);
        let mut count = 0u64;
        let mut len_buf = [0u8; 4];

        loop {
            if reader.read_exact(&mut len_buf).is_err() {
                break;
            }
            let len = u32::from_le_bytes(len_buf) as usize;
            let mut data = vec![0u8; len];
            if reader.read_exact(&mut data).is_err() {
                break;
            }
            count += 1;
        }
        count
    }

    /// Count entries across all segment files in a directory.
    fn count_entries_in_dir(dir: &Path) -> u64 {
        let mut total = 0u64;
        for seg_path in Self::list_segments(dir) {
            total += Self::count_entries(&seg_path);
        }
        total
    }

    /// Find the latest segment file in a directory, returning (segment_number, path).
    /// If no segments exist, returns (0, dir/wal-00000000.bin).
    fn find_latest_segment(dir: &Path) -> (u64, PathBuf) {
        let segments = Self::list_segments(dir);
        if let Some(last) = segments.last() {
            if let Some(num) = Self::parse_segment_number(last) {
                return (num, last.clone());
            }
        }
        (0, dir.join("wal-00000000.bin"))
    }

    /// List all WAL segment files in a directory, sorted by name.
    fn list_segments(dir: &Path) -> Vec<PathBuf> {
        let mut segments = Vec::new();
        if let Ok(entries) = fs::read_dir(dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                    if name.starts_with("wal-") && name.ends_with(".bin") {
                        segments.push(path);
                    }
                }
            }
        }
        segments.sort();
        segments
    }

    /// Parse the segment number from a segment file path like `wal-00000003.bin`.
    fn parse_segment_number(path: &Path) -> Option<u64> {
        let name = path.file_name()?.to_str()?;
        let stripped = name.strip_prefix("wal-")?.strip_suffix(".bin")?;
        stripped.parse::<u64>().ok()
    }
}

impl Drop for WalWriter {
    fn drop(&mut self) {
        self.shutdown();
    }
}

// ── WAL Reader (for crash recovery) ─────────────────────────────────────────

/// Read all entries from a WAL file. Used during crash recovery.
pub fn read_wal(path: impl AsRef<Path>) -> Vec<WalEntry> {
    let file = match File::open(path.as_ref()) {
        Ok(f) => f,
        Err(_) => return Vec::new(),
    };
    let mut reader = BufReader::new(file);
    let mut entries = Vec::new();
    let mut len_buf = [0u8; 4];

    loop {
        if reader.read_exact(&mut len_buf).is_err() {
            break;
        }
        let len = u32::from_le_bytes(len_buf) as usize;
        let mut data = vec![0u8; len];
        if reader.read_exact(&mut data).is_err() {
            break; // Truncated entry — stop here (crash mid-write)
        }

        match bincode::deserialize::<WalEntry>(&data) {
            Ok(entry) => {
                // Verify checksum
                let payload =
                    bincode::serialize(&(&entry.block_height, &entry.sequence, &entry.op))
                        .unwrap_or_default();
                let expected_crc = crc32fast::hash(&payload);
                if entry.checksum == expected_crc {
                    entries.push(entry);
                } else {
                    tracing::warn!(
                        "WAL entry {} has invalid checksum, stopping replay",
                        entries.len()
                    );
                    break; // Corrupted entry — stop here
                }
            }
            Err(_) => {
                tracing::warn!("Failed to deserialize WAL entry, stopping replay");
                break;
            }
        }
    }
    entries
}

/// Read all entries from all WAL segment files in a directory, in order.
pub fn read_wal_dir(dir: impl AsRef<Path>) -> Vec<WalEntry> {
    let segments = WalWriter::list_segments(dir.as_ref());
    let mut all_entries = Vec::new();
    for seg_path in segments {
        all_entries.extend(read_wal(&seg_path));
    }
    all_entries
}

/// Read WAL entries starting from a given sequence number (for replay after snapshot).
pub fn read_wal_from(path: impl AsRef<Path>, from_sequence: u64) -> Vec<WalEntry> {
    read_wal(path)
        .into_iter()
        .filter(|e| e.sequence >= from_sequence)
        .collect()
}

/// Find the last checkpoint in a WAL file.
/// Returns (sequence, state_root) of the last checkpoint.
pub fn find_last_checkpoint(path: impl AsRef<Path>) -> Option<(u64, Hash256)> {
    let entries = read_wal(path);
    entries.iter().rev().find_map(|e| match &e.op {
        WalOp::Checkpoint(root) => Some((e.sequence, *root)),
        _ => None,
    })
}

// ── Snapshot ────────────────────────────────────────────────────────────────

/// Full state snapshot for fast node bootstrap and crash recovery.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Snapshot {
    /// Block height at which this snapshot was taken.
    pub block_height: u64,
    /// State root hash at this snapshot.
    pub state_root: Hash256,
    /// WAL sequence number at the time of snapshot.
    pub wal_sequence: u64,
    /// All accounts sorted by address.
    pub accounts: Vec<(Address, Account)>,
    /// Contract storage: (contract_address, [(key, value)])
    pub storage: Vec<(Address, Vec<(Hash256, Vec<u8>)>)>,
    /// Contract bytecode cache: (address, wasm_bytes)
    pub contracts: Vec<(Address, Vec<u8>)>,
}

impl Snapshot {
    /// Write snapshot to disk as LZ4-compressed bincode.
    pub fn write_to(&self, path: impl AsRef<Path>) -> std::io::Result<()> {
        let data = bincode::serialize(self)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;
        let compressed = lz4_flex::compress_prepend_size(&data);

        let mut file = File::create(path)?;
        file.write_all(&compressed)?;
        file.sync_all()?;
        Ok(())
    }

    /// Read snapshot from an LZ4-compressed bincode file.
    pub fn read_from(path: impl AsRef<Path>) -> std::io::Result<Self> {
        let mut file = File::open(path)?;
        let mut compressed = Vec::new();
        file.read_to_end(&mut compressed)?;

        let data = lz4_flex::decompress_size_prepended(&compressed)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;

        bincode::deserialize(&data)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
    }
}

// ── Snapshot Config ─────────────────────────────────────────────────────────

/// Configuration for snapshot frequency and state rent.
pub struct PersistenceConfig {
    /// Take a snapshot every N blocks (default: 10,000).
    pub snapshot_interval: u64,
    /// WAL file path.
    pub wal_path: PathBuf,
    /// Snapshot directory path.
    pub snapshot_dir: PathBuf,
    /// Maximum WAL segment file size in bytes before rotation (default: 256MB).
    pub max_wal_segment_size: u64,
    /// Minimum number of WAL segments to retain after cleanup (default: 2).
    pub wal_retention_segments: u32,
}

impl Default for PersistenceConfig {
    fn default() -> Self {
        Self {
            snapshot_interval: 10_000,
            wal_path: PathBuf::from("data/wal.bin"),
            snapshot_dir: PathBuf::from("data/snapshots"),
            max_wal_segment_size: 268_435_456, // 256 MB
            wal_retention_segments: 2,
        }
    }
}

// ── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use arc_crypto::hash_bytes;
    use std::fs;

    fn tmp_path(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join("arc-wal-tests");
        let _ = fs::create_dir_all(&dir);
        dir.join(name)
    }

    fn tmp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join("arc-wal-tests").join(name);
        let _ = fs::remove_dir_all(&dir);
        let _ = fs::create_dir_all(&dir);
        dir
    }

    fn test_addr(n: u8) -> Address {
        hash_bytes(&[n])
    }

    #[test]
    fn wal_write_and_read() {
        let path = tmp_path("wal_rw.bin");
        let _ = fs::remove_file(&path);

        {
            let writer = WalWriter::new(&path).expect("create wal");
            writer.append(
                WalOp::SetAccount(
                    test_addr(1),
                    Account::new(test_addr(1), 1000),
                ),
                1,
            );
            writer.append(
                WalOp::SetAccount(
                    test_addr(2),
                    Account::new(test_addr(2), 2000),
                ),
                1,
            );
            writer.append(WalOp::Checkpoint(hash_bytes(b"root1")), 1);
            writer.sync();
        }

        let entries = read_wal(&path);
        assert_eq!(entries.len(), 3);
        assert!(matches!(entries[0].op, WalOp::SetAccount(_, _)));
        assert!(matches!(entries[2].op, WalOp::Checkpoint(_)));
        assert_eq!(entries[0].sequence, 0);
        assert_eq!(entries[1].sequence, 1);
        assert_eq!(entries[2].sequence, 2);

        let _ = fs::remove_file(&path);
    }

    #[test]
    fn wal_checksum_verification() {
        let path = tmp_path("wal_crc.bin");
        let _ = fs::remove_file(&path);

        {
            let writer = WalWriter::new(&path).expect("create wal");
            writer.append(
                WalOp::SetAccount(test_addr(1), Account::new(test_addr(1), 500)),
                1,
            );
            writer.sync();
        }

        let entries = read_wal(&path);
        assert_eq!(entries.len(), 1);

        // Verify the checksum is valid
        let entry = &entries[0];
        let payload =
            bincode::serialize(&(&entry.block_height, &entry.sequence, &entry.op)).unwrap();
        assert_eq!(entry.checksum, crc32fast::hash(&payload));

        let _ = fs::remove_file(&path);
    }

    #[test]
    fn wal_find_last_checkpoint() {
        let path = tmp_path("wal_ckpt.bin");
        let _ = fs::remove_file(&path);

        {
            let writer = WalWriter::new(&path).expect("create wal");
            writer.append(WalOp::Checkpoint(hash_bytes(b"root1")), 1);
            writer.append(
                WalOp::SetAccount(test_addr(1), Account::new(test_addr(1), 100)),
                2,
            );
            writer.append(WalOp::Checkpoint(hash_bytes(b"root2")), 2);
            writer.sync();
        }

        let (seq, root) = find_last_checkpoint(&path).expect("should find checkpoint");
        assert_eq!(seq, 2);
        assert_eq!(root, hash_bytes(b"root2"));

        let _ = fs::remove_file(&path);
    }

    #[test]
    fn wal_read_from_sequence() {
        let path = tmp_path("wal_seq.bin");
        let _ = fs::remove_file(&path);

        {
            let writer = WalWriter::new(&path).expect("create wal");
            for i in 0..5 {
                writer.append(
                    WalOp::SetAccount(
                        test_addr(i as u8),
                        Account::new(test_addr(i as u8), i * 100),
                    ),
                    1,
                );
            }
            writer.sync();
        }

        let entries = read_wal_from(&path, 3);
        assert_eq!(entries.len(), 2); // sequences 3 and 4
        assert_eq!(entries[0].sequence, 3);
        assert_eq!(entries[1].sequence, 4);

        let _ = fs::remove_file(&path);
    }

    #[test]
    fn wal_null_writer() {
        let writer = WalWriter::null();
        // Should not panic or error
        writer.append(WalOp::Checkpoint(Hash256::ZERO), 0);
        // Sync on null writer is a no-op
        writer.sync();
    }

    #[test]
    fn snapshot_write_and_read() {
        let path = tmp_path("snapshot_test.snap");
        let _ = fs::remove_file(&path);

        let snapshot = Snapshot {
            block_height: 42,
            state_root: hash_bytes(b"state-root"),
            wal_sequence: 100,
            accounts: vec![
                (test_addr(1), Account::new(test_addr(1), 1000)),
                (test_addr(2), Account::new(test_addr(2), 2000)),
            ],
            storage: vec![(
                test_addr(10),
                vec![(hash_bytes(b"key1"), b"value1".to_vec())],
            )],
            contracts: vec![(test_addr(20), vec![0x00, 0x61, 0x73, 0x6d])],
        };

        snapshot.write_to(&path).expect("write snapshot");
        let loaded = Snapshot::read_from(&path).expect("read snapshot");

        assert_eq!(loaded.block_height, 42);
        assert_eq!(loaded.state_root, hash_bytes(b"state-root"));
        assert_eq!(loaded.wal_sequence, 100);
        assert_eq!(loaded.accounts.len(), 2);
        assert_eq!(loaded.accounts[0].1.balance, 1000);
        assert_eq!(loaded.storage.len(), 1);
        assert_eq!(loaded.contracts.len(), 1);

        let _ = fs::remove_file(&path);
    }

    #[test]
    fn snapshot_compression_ratio() {
        let path = tmp_path("snapshot_compress.snap");
        let _ = fs::remove_file(&path);

        // Create a snapshot with 1000 accounts
        let accounts: Vec<(Address, Account)> = (0..1000u32)
            .map(|i| {
                let addr = hash_bytes(&i.to_le_bytes());
                (addr, Account::new(addr, (i as u64) * 1000))
            })
            .collect();

        let snapshot = Snapshot {
            block_height: 1000,
            state_root: hash_bytes(b"big-state"),
            wal_sequence: 5000,
            accounts,
            storage: Vec::new(),
            contracts: Vec::new(),
        };

        let raw_size = bincode::serialize(&snapshot).unwrap().len();
        snapshot.write_to(&path).expect("write");
        let compressed_size = fs::metadata(&path).unwrap().len() as usize;

        // LZ4 should compress account data well (repetitive structure)
        assert!(
            compressed_size < raw_size,
            "compressed {} should be < raw {}",
            compressed_size,
            raw_size
        );

        let _ = fs::remove_file(&path);
    }

    #[test]
    fn wal_many_entries() {
        let path = tmp_path("wal_many.bin");
        let _ = fs::remove_file(&path);

        {
            let writer = WalWriter::new(&path).expect("create wal");
            for i in 0..100u64 {
                writer.append(
                    WalOp::SetAccount(
                        test_addr((i % 256) as u8),
                        Account::new(test_addr((i % 256) as u8), i * 10),
                    ),
                    i / 10,
                );
            }
            writer.sync();
        }

        let entries = read_wal(&path);
        assert_eq!(entries.len(), 100);
        assert_eq!(entries[99].sequence, 99);

        let _ = fs::remove_file(&path);
    }

    // ── New tests for WAL rotation ──────────────────────────────────────────

    #[test]
    fn wal_rotation_creates_new_segments() {
        let dir = tmp_dir("wal_rotation_segments");

        {
            let writer = WalWriter::with_segments(&dir, 1024 * 1024).expect("create segmented wal");

            // Write some entries to segment 0
            for i in 0..5u64 {
                writer.append(
                    WalOp::SetAccount(
                        test_addr(i as u8),
                        Account::new(test_addr(i as u8), i * 100),
                    ),
                    1,
                );
            }
            writer.sync();

            // Rotate to segment 1
            writer.rotate();

            // Write more entries to segment 1
            for i in 5..10u64 {
                writer.append(
                    WalOp::SetAccount(
                        test_addr(i as u8),
                        Account::new(test_addr(i as u8), i * 100),
                    ),
                    2,
                );
            }
            writer.sync();

            // Rotate again to segment 2
            writer.rotate();

            // Write to segment 2
            writer.append(WalOp::Checkpoint(hash_bytes(b"root")), 2);
            writer.sync();
        }

        // Verify segment files exist
        let segments = WalWriter::list_segments(&dir);
        assert!(
            segments.len() >= 3,
            "expected at least 3 segments, got {}",
            segments.len()
        );

        // Verify we can read entries from segment 0
        let seg0 = dir.join("wal-00000000.bin");
        let entries0 = read_wal(&seg0);
        assert_eq!(entries0.len(), 5);

        // Verify we can read entries from segment 1
        let seg1 = dir.join("wal-00000001.bin");
        let entries1 = read_wal(&seg1);
        assert_eq!(entries1.len(), 5);

        // Verify segment 2 has the checkpoint
        let seg2 = dir.join("wal-00000002.bin");
        let entries2 = read_wal(&seg2);
        assert_eq!(entries2.len(), 1);
        assert!(matches!(entries2[0].op, WalOp::Checkpoint(_)));

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn wal_delete_old_segments_after_snapshot() {
        let dir = tmp_dir("wal_delete_segments");

        {
            let writer = WalWriter::with_segments(&dir, 1024 * 1024).expect("create segmented wal");

            // Segment 0: entries 0..4
            for i in 0..5u64 {
                writer.append(
                    WalOp::SetAccount(
                        test_addr(i as u8),
                        Account::new(test_addr(i as u8), i * 100),
                    ),
                    1,
                );
            }
            writer.sync();
            writer.rotate();

            // Segment 1: entries 5..9
            for i in 5..10u64 {
                writer.append(
                    WalOp::SetAccount(
                        test_addr(i as u8),
                        Account::new(test_addr(i as u8), i * 100),
                    ),
                    2,
                );
            }
            writer.sync();
            writer.rotate();

            // Segment 2: entries 10..14
            for i in 10..15u64 {
                writer.append(
                    WalOp::SetAccount(
                        test_addr(i as u8),
                        Account::new(test_addr(i as u8), i * 100),
                    ),
                    3,
                );
            }
            writer.sync();
            writer.rotate();

            // Segment 3: entries 15..19
            for i in 15..20u64 {
                writer.append(
                    WalOp::SetAccount(
                        test_addr(i as u8),
                        Account::new(test_addr(i as u8), i * 100),
                    ),
                    4,
                );
            }
            writer.sync();
        }

        // Before deletion: 4 segments (0, 1, 2, 3)
        let segments_before = WalWriter::list_segments(&dir);
        assert_eq!(segments_before.len(), 4);

        // Delete segments with entries all before sequence 10.
        // Segment 0 (last seq = 4) and segment 1 (last seq = 9) qualify.
        // But we keep at least 2, so we can delete at most 2 (4 - 2 = 2).
        let deleted = WalWriter::delete_segments_before_in_dir(&dir, 10, 2).unwrap();
        assert_eq!(deleted, 2);

        let segments_after = WalWriter::list_segments(&dir);
        assert_eq!(segments_after.len(), 2);

        // Remaining segments should be 2 and 3
        assert!(segments_after[0].file_name().unwrap().to_str().unwrap().contains("00000002"));
        assert!(segments_after[1].file_name().unwrap().to_str().unwrap().contains("00000003"));

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn wal_read_across_segment_boundaries() {
        let dir = tmp_dir("wal_cross_segment");

        {
            let writer = WalWriter::with_segments(&dir, 1024 * 1024).expect("create segmented wal");

            // Write entries 0..2 in segment 0
            for i in 0..3u64 {
                writer.append(
                    WalOp::SetAccount(
                        test_addr(i as u8),
                        Account::new(test_addr(i as u8), i * 100),
                    ),
                    1,
                );
            }
            writer.sync();
            writer.rotate();

            // Write entries 3..5 in segment 1
            for i in 3..6u64 {
                writer.append(
                    WalOp::SetAccount(
                        test_addr(i as u8),
                        Account::new(test_addr(i as u8), i * 100),
                    ),
                    2,
                );
            }
            writer.sync();
            writer.rotate();

            // Write entries 6..8 in segment 2
            for i in 6..9u64 {
                writer.append(
                    WalOp::SetAccount(
                        test_addr(i as u8),
                        Account::new(test_addr(i as u8), i * 100),
                    ),
                    3,
                );
            }
            writer.sync();
        }

        // Read all entries across segments
        let all_entries = read_wal_dir(&dir);
        assert_eq!(all_entries.len(), 9);

        // Verify sequences are continuous across segments
        for (i, entry) in all_entries.iter().enumerate() {
            assert_eq!(
                entry.sequence, i as u64,
                "expected sequence {}, got {}",
                i, entry.sequence
            );
        }

        let _ = fs::remove_dir_all(&dir);
    }
}
