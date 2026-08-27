//! Write-Ahead Log (WAL) for ARC Chain state persistence.
//!
//! Every state mutation is journaled to an append-only file BEFORE acknowledging.
//! Sequential writes only - never seeks, never reads during execution.
//! The async writer batches entries and flushes to SSD periodically.

use arc_crypto::Hash256;
use arc_types::{Account, Address, Block, EventLog, Identity, Transaction, TxReceipt};
use crossbeam::channel::{self, Receiver, Sender};
use serde::{Deserialize, Serialize};
use std::fs::{self, File, OpenOptions};
use std::io::{BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::thread;

// ── WAL Types ───────────────────────────────────────────────────────────────

/// Contract storage as carried in a snapshot: `(contract_address, [(key, value)])`.
///
/// A named alias for the shape used by `Snapshot::storage` and by the
/// `StateDB` snapshot exporters, so the same nested tuple is spelled once.
pub type ContractStorage = Vec<(Address, Vec<(Hash256, Vec<u8>)>)>;

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
    /// Used for crash recovery - replay starts from the last checkpoint.
    Checkpoint(Hash256),
    /// Store a DAG consensus block (hash → serialized DagBlock).
    /// Enables consensus state recovery after restart.
    SetDagBlock(Hash256, Vec<u8>),
    /// Record a DAG round advancement (current round number).
    SetDagRound(u64),
    /// Record a DAG block commit (hash of committed block).
    CommitDagBlock(Hash256),
    /// Persist a full transaction body for restart-safe receipt/explorer state.
    SetFullTransaction(Hash256, Transaction),
    /// Persist EVM event logs associated with one canonical block.
    SetEventLogs(u64, Vec<EventLog>),
    /// Persist one identity-registry update.
    SetIdentity(Address, Identity),
    /// Persist the complete active validator map and staking pool atomically at
    /// a block boundary. Appended at the end to retain legacy enum indexes.
    SetValidatorState(Vec<(Address, u64)>, u64),
}

/// Internal command for the WAL background thread.
enum WalCommand {
    /// Append an entry to the WAL. Boxed: a `WalEntry` dwarfs the other
    /// variants, and every message through the channel would otherwise be
    /// sized for it. `Box<WalEntry>` serialises identically to `WalEntry`,
    /// so the on-disk format is unchanged.
    Append(Box<WalEntry>),
    /// Flush all pending writes and fsync.
    Sync(channel::Sender<Result<(), WalError>>),
    /// Rotate: close the current segment, open a new one.
    Rotate(channel::Sender<Result<(), WalError>>),
    /// Shutdown the writer thread.
    Shutdown(channel::Sender<Result<(), WalError>>),
    #[cfg(test)]
    Disconnect,
}

/// A fatal WAL writer failure. The first failure is retained for the lifetime
/// of the writer so callers cannot accidentally resume after durability was
/// lost.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WalError {
    operation: &'static str,
    kind: std::io::ErrorKind,
    message: String,
}

impl WalError {
    fn io(operation: &'static str, error: &std::io::Error) -> Self {
        Self {
            operation,
            kind: error.kind(),
            message: error.to_string(),
        }
    }

    fn message(
        operation: &'static str,
        kind: std::io::ErrorKind,
        message: impl Into<String>,
    ) -> Self {
        Self {
            operation,
            kind,
            message: message.into(),
        }
    }

    /// The operation that first made the WAL unhealthy.
    pub fn operation(&self) -> &'static str {
        self.operation
    }

    /// The underlying I/O error category.
    pub fn kind(&self) -> std::io::ErrorKind {
        self.kind
    }
}

impl std::fmt::Display for WalError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "WAL {} failed ({:?}): {}",
            self.operation, self.kind, self.message
        )
    }
}

impl std::error::Error for WalError {}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub(crate) enum WalFaultPoint {
    ChecksumSerialization = 1,
    EntrySerialization = 2,
    Write = 3,
    Flush = 4,
    Fsync = 5,
    Rotation = 6,
}

#[cfg(test)]
#[derive(Default)]
struct WalFaultInjector {
    next: std::sync::atomic::AtomicU8,
}

#[cfg(not(test))]
#[derive(Default)]
struct WalFaultInjector;

impl WalFaultInjector {
    fn new() -> Self {
        #[cfg(test)]
        {
            Self::default()
        }
        #[cfg(not(test))]
        {
            Self
        }
    }

    #[inline]
    fn check(&self, point: WalFaultPoint) -> std::io::Result<()> {
        #[cfg(test)]
        if self
            .next
            .compare_exchange(point as u8, 0, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
        {
            return Err(std::io::Error::other(format!(
                "injected WAL {point:?} failure"
            )));
        }

        #[cfg(not(test))]
        let _ = point;

        Ok(())
    }

    #[cfg(test)]
    fn inject(&self, point: WalFaultPoint) {
        self.next.store(point as u8, Ordering::Release);
    }
}

// ── WAL Writer ──────────────────────────────────────────────────────────────

/// Non-blocking WAL writer. Sends entries to a background thread that batches
/// and flushes writes. Execution threads are never blocked by I/O.
pub struct WalWriter {
    sender: Sender<WalCommand>,
    sequence: AtomicU64,
    handle: Option<thread::JoinHandle<()>>,
    shutdown: Arc<AtomicBool>,
    failure: Arc<OnceLock<WalError>>,
    faults: Arc<WalFaultInjector>,
    is_null: bool,
    /// Directory containing WAL segment files.
    wal_dir: PathBuf,
}

impl WalWriter {
    /// Create a new WAL writer that writes to the given file path.
    /// Spawns a background thread for async I/O.
    pub fn new(path: impl AsRef<Path>) -> std::io::Result<Self> {
        let path = path.as_ref().to_path_buf();
        let existed = path.exists();
        let file = OpenOptions::new().create(true).append(true).open(&path)?;
        if !existed {
            Self::sync_parent_directory(&path)?;
        }
        let mut writer = BufWriter::with_capacity(256 * 1024, file); // 256KB buffer

        let (sender, receiver): (Sender<WalCommand>, Receiver<WalCommand>) = channel::unbounded();
        let shutdown = Arc::new(AtomicBool::new(false));
        let failure = Arc::new(OnceLock::new());
        let failure_clone = failure.clone();
        let faults = Arc::new(WalFaultInjector::new());
        let faults_clone = faults.clone();

        // The WAL directory is the parent of the WAL file path.
        let wal_dir = path
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .to_path_buf();

        // Determine starting sequence by reading existing entries (before path is moved)
        let seq = Self::count_entries(&path);

        let writer_path = path.clone();
        let handle = thread::Builder::new()
            .name("wal-writer".into())
            .spawn(move || {
                Self::writer_loop(
                    &mut writer,
                    &receiver,
                    &failure_clone,
                    &faults_clone,
                    &writer_path,
                    0,
                    0,
                );
            })?;

        Ok(Self {
            sender,
            sequence: AtomicU64::new(seq),
            handle: Some(handle),
            shutdown,
            failure,
            faults,
            is_null: false,
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
        let segment_existed = seg_path.exists();
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&seg_path)?;
        if !segment_existed {
            Self::sync_parent_directory(&seg_path)?;
        }
        let mut writer = BufWriter::with_capacity(256 * 1024, file);

        let (sender, receiver): (Sender<WalCommand>, Receiver<WalCommand>) = channel::unbounded();
        let shutdown = Arc::new(AtomicBool::new(false));
        let failure = Arc::new(OnceLock::new());
        let failure_clone = failure.clone();
        let faults = Arc::new(WalFaultInjector::new());
        let faults_clone = faults.clone();

        let dir_clone = wal_dir.clone();
        let handle = thread::Builder::new()
            .name("wal-writer".into())
            .spawn(move || {
                Self::writer_loop(
                    &mut writer,
                    &receiver,
                    &failure_clone,
                    &faults_clone,
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
            failure,
            faults,
            is_null: false,
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
            failure: Arc::new(OnceLock::new()),
            faults: Arc::new(WalFaultInjector::new()),
            is_null: true,
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

    /// Return the first fatal writer failure, if persistence is unhealthy.
    pub fn failure(&self) -> Option<WalError> {
        self.failure.get().cloned()
    }

    /// Fail if the writer has lost its durability guarantee.
    pub fn check_health(&self) -> Result<(), WalError> {
        if self.is_null {
            return Ok(());
        }
        if let Some(error) = self.failure() {
            return Err(error);
        }
        if self.shutdown.load(Ordering::Acquire) {
            return Err(WalError::message(
                "health check",
                std::io::ErrorKind::BrokenPipe,
                "writer is shut down",
            ));
        }
        Ok(())
    }

    /// Non-blocking. Sends an entry to the background writer.
    pub fn append(&self, op: WalOp, block_height: u64) {
        // Null WAL: no writer thread, no handle → skip serialize/send entirely
        if self.is_null {
            return;
        }

        if self.check_health().is_err() {
            return;
        }

        let seq = self.sequence.fetch_add(1, Ordering::Relaxed);
        if let Err(error) = self.faults.check(WalFaultPoint::ChecksumSerialization) {
            self.latch(WalError::io("checksum serialization", &error));
            return;
        }
        let payload = match bincode::serialize(&(&block_height, &seq, &op)) {
            Ok(payload) => payload,
            Err(error) => {
                self.latch(WalError::message(
                    "checksum serialization",
                    std::io::ErrorKind::InvalidData,
                    error.to_string(),
                ));
                return;
            }
        };
        let checksum = crc32fast::hash(&payload);

        let entry = WalEntry {
            block_height,
            sequence: seq,
            op,
            checksum,
        };

        if self
            .sender
            .send(WalCommand::Append(Box::new(entry)))
            .is_err()
        {
            self.latch(WalError::message(
                "append channel send",
                std::io::ErrorKind::BrokenPipe,
                "writer channel disconnected",
            ));
        }
    }

    /// Blocks until all pending entries are fsynced to disk.
    /// Call at block boundaries for durability guarantees.
    pub fn sync(&self) -> Result<(), WalError> {
        if self.is_null {
            return Ok(());
        }
        self.check_health()?;
        let (done_tx, done_rx) = channel::bounded(1);
        self.sender.send(WalCommand::Sync(done_tx)).map_err(|_| {
            self.latch(WalError::message(
                "sync channel send",
                std::io::ErrorKind::BrokenPipe,
                "writer channel disconnected",
            ))
        })?;
        done_rx.recv().map_err(|_| {
            self.failure().unwrap_or_else(|| {
                self.latch(WalError::message(
                    "sync acknowledgement",
                    std::io::ErrorKind::BrokenPipe,
                    "writer exited before acknowledging fsync",
                ))
            })
        })?
    }

    /// Request the writer thread to rotate: close the current segment file
    /// and open a new one. Blocks until rotation is complete.
    pub fn rotate(&self) -> Result<(), WalError> {
        if self.is_null {
            return Ok(());
        }
        self.check_health()?;
        let (done_tx, done_rx) = channel::bounded(1);
        self.sender.send(WalCommand::Rotate(done_tx)).map_err(|_| {
            self.latch(WalError::message(
                "rotation channel send",
                std::io::ErrorKind::BrokenPipe,
                "writer channel disconnected",
            ))
        })?;
        done_rx.recv().map_err(|_| {
            self.failure().unwrap_or_else(|| {
                self.latch(WalError::message(
                    "rotation acknowledgement",
                    std::io::ErrorKind::BrokenPipe,
                    "writer exited before acknowledging rotation",
                ))
            })
        })?
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

    /// Shut down the WAL writer, flushing and fsyncing all remaining entries.
    pub fn shutdown(&mut self) -> Result<(), WalError> {
        if self.is_null || self.handle.is_none() {
            return self.failure().map_or(Ok(()), Err);
        }

        self.shutdown.store(true, Ordering::Release);
        let (done_tx, done_rx) = channel::bounded(1);
        let send_result = self.sender.send(WalCommand::Shutdown(done_tx));
        let acknowledgement = if send_result.is_ok() {
            done_rx.recv().ok()
        } else {
            None
        };

        let join_result = self.handle.take().expect("WAL handle checked above").join();

        if let Some(error) = self.failure() {
            return Err(error);
        }
        if send_result.is_err() {
            return Err(self.latch(WalError::message(
                "shutdown channel send",
                std::io::ErrorKind::BrokenPipe,
                "writer channel disconnected",
            )));
        }
        if join_result.is_err() {
            return Err(self.latch(WalError::message(
                "writer thread join",
                std::io::ErrorKind::Other,
                "writer thread panicked",
            )));
        }
        acknowledgement.unwrap_or_else(|| {
            Err(self.latch(WalError::message(
                "shutdown acknowledgement",
                std::io::ErrorKind::BrokenPipe,
                "writer exited before acknowledging shutdown",
            )))
        })
    }

    /// The background writer loop. Receives entries, writes them, and flushes
    /// each drained batch. Any failure is fatal and permanently latched.
    /// Handles Rotate commands by closing the current file and opening a new segment.
    /// When `max_segment_size > 0`, automatically rotates after the segment exceeds that size.
    fn writer_loop(
        writer: &mut BufWriter<File>,
        receiver: &Receiver<WalCommand>,
        failure: &OnceLock<WalError>,
        faults: &WalFaultInjector,
        wal_path: &Path,
        initial_segment: u64,
        max_segment_size: u64,
    ) {
        let mut segment_number = initial_segment;
        // Determine if we are in segmented mode (wal_path is a directory)
        // or legacy mode (wal_path is a file).
        let is_dir = wal_path.is_dir();

        // Track bytes written to the current segment for auto-rotation.
        let mut bytes_written: u64 = writer.get_ref().metadata().map(|m| m.len()).unwrap_or(0);

        let mut deferred = None;
        loop {
            let command = match deferred.take() {
                Some(command) => command,
                None => match receiver.recv() {
                    Ok(command) => command,
                    Err(_) => {
                        if let Err(error) = Self::flush_and_sync(writer, faults) {
                            Self::latch_shared(failure, error);
                        }
                        return;
                    }
                },
            };

            match command {
                WalCommand::Append(entry) => {
                    match Self::write_entry(writer, &entry, faults) {
                        Ok(entry_size) => bytes_written += entry_size,
                        Err(error) => {
                            Self::latch_shared(failure, error);
                            return;
                        }
                    }

                    // Drain append commands into one buffered write batch, but
                    // preserve command ordering by deferring the first barrier.
                    loop {
                        match receiver.try_recv() {
                            Ok(WalCommand::Append(entry)) => {
                                match Self::write_entry(writer, &entry, faults) {
                                    Ok(entry_size) => bytes_written += entry_size,
                                    Err(error) => {
                                        Self::latch_shared(failure, error);
                                        return;
                                    }
                                }
                            }
                            Ok(command) => {
                                deferred = Some(command);
                                break;
                            }
                            Err(channel::TryRecvError::Empty) => break,
                            Err(channel::TryRecvError::Disconnected) => break,
                        }
                    }

                    if let Err(error) = Self::flush_buffer(writer, faults) {
                        Self::latch_shared(failure, error);
                        return;
                    }

                    if max_segment_size > 0 && bytes_written >= max_segment_size {
                        if let Err(error) =
                            Self::do_rotate(writer, wal_path, is_dir, &mut segment_number, faults)
                        {
                            Self::latch_shared(failure, error);
                            return;
                        }
                        bytes_written = 0;
                    }
                }
                WalCommand::Sync(done) => {
                    let result = Self::flush_and_sync(writer, faults)
                        .map_err(|error| Self::latch_shared(failure, error));
                    let failed = result.is_err();
                    if done.send(result).is_err() {
                        Self::latch_shared(
                            failure,
                            WalError::message(
                                "sync acknowledgement",
                                std::io::ErrorKind::BrokenPipe,
                                "sync caller disconnected",
                            ),
                        );
                        return;
                    }
                    if failed {
                        return;
                    }
                }
                WalCommand::Rotate(done) => {
                    let result =
                        Self::do_rotate(writer, wal_path, is_dir, &mut segment_number, faults)
                            .map_err(|error| Self::latch_shared(failure, error));
                    let failed = result.is_err();
                    if !failed {
                        bytes_written = 0;
                    }
                    if done.send(result).is_err() {
                        Self::latch_shared(
                            failure,
                            WalError::message(
                                "rotation acknowledgement",
                                std::io::ErrorKind::BrokenPipe,
                                "rotation caller disconnected",
                            ),
                        );
                        return;
                    }
                    if failed {
                        return;
                    }
                }
                WalCommand::Shutdown(done) => {
                    let result = Self::flush_and_sync(writer, faults)
                        .map_err(|error| Self::latch_shared(failure, error));
                    let _ = done.send(result);
                    return;
                }
                #[cfg(test)]
                WalCommand::Disconnect => return,
            }
        }
    }

    fn write_entry(
        writer: &mut BufWriter<File>,
        entry: &WalEntry,
        faults: &WalFaultInjector,
    ) -> Result<u64, WalError> {
        faults
            .check(WalFaultPoint::EntrySerialization)
            .map_err(|error| WalError::io("entry serialization", &error))?;
        let data = bincode::serialize(entry).map_err(|error| {
            WalError::message(
                "entry serialization",
                std::io::ErrorKind::InvalidData,
                error.to_string(),
            )
        })?;
        let length = u32::try_from(data.len()).map_err(|_| {
            WalError::message(
                "entry framing",
                std::io::ErrorKind::InvalidData,
                format!("serialized entry is too large: {} bytes", data.len()),
            )
        })?;
        faults
            .check(WalFaultPoint::Write)
            .map_err(|error| WalError::io("write", &error))?;
        writer
            .write_all(&length.to_le_bytes())
            .and_then(|_| writer.write_all(&data))
            .map_err(|error| WalError::io("write", &error))?;
        Ok(4 + data.len() as u64)
    }

    fn flush_buffer(
        writer: &mut BufWriter<File>,
        faults: &WalFaultInjector,
    ) -> Result<(), WalError> {
        faults
            .check(WalFaultPoint::Flush)
            .map_err(|error| WalError::io("flush", &error))?;
        writer
            .flush()
            .map_err(|error| WalError::io("flush", &error))
    }

    fn flush_and_sync(
        writer: &mut BufWriter<File>,
        faults: &WalFaultInjector,
    ) -> Result<(), WalError> {
        Self::flush_buffer(writer, faults)?;
        faults
            .check(WalFaultPoint::Fsync)
            .map_err(|error| WalError::io("fsync", &error))?;
        writer
            .get_ref()
            .sync_data()
            .map_err(|error| WalError::io("fsync", &error))
    }

    /// Perform a segment rotation: flush + fsync the current writer, then
    /// replace it with a new segment file.
    fn do_rotate(
        writer: &mut BufWriter<File>,
        wal_path: &Path,
        is_dir: bool,
        segment_number: &mut u64,
        faults: &WalFaultInjector,
    ) -> Result<(), WalError> {
        Self::flush_and_sync(writer, faults)?;
        faults
            .check(WalFaultPoint::Rotation)
            .map_err(|error| WalError::io("rotation", &error))?;

        // Determine the directory and new segment number
        let next_segment = segment_number.checked_add(1).ok_or_else(|| {
            WalError::message(
                "rotation",
                std::io::ErrorKind::InvalidInput,
                "segment number overflow",
            )
        })?;
        let new_path = if is_dir {
            wal_path.join(format!("wal-{next_segment:08}.bin"))
        } else {
            let dir = wal_path.parent().unwrap_or_else(|| Path::new("."));
            dir.join(format!("wal-{next_segment:08}.bin"))
        };

        // A new segment must never silently reuse a stale file. Persist its
        // directory entry before allowing the rotation barrier to succeed.
        let file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&new_path)
            .map_err(|error| WalError::io("rotation open", &error))?;
        Self::sync_parent_directory(&new_path)
            .map_err(|error| WalError::io("rotation directory fsync", &error))?;

        *writer = BufWriter::with_capacity(256 * 1024, file);
        *segment_number = next_segment;
        Ok(())
    }

    fn sync_parent_directory(path: &Path) -> std::io::Result<()> {
        #[cfg(unix)]
        {
            let parent = path.parent().unwrap_or_else(|| Path::new("."));
            File::open(parent)?.sync_all()
        }
        #[cfg(not(unix))]
        {
            let _ = path;
            Ok(())
        }
    }

    fn latch(&self, error: WalError) -> WalError {
        Self::latch_shared(&self.failure, error)
    }

    fn latch_shared(failure: &OnceLock<WalError>, error: WalError) -> WalError {
        let first = failure.get_or_init(|| {
            tracing::error!(operation = error.operation(), error = %error, "WAL durability failure");
            error
        });
        first.clone()
    }

    #[cfg(test)]
    pub(crate) fn inject_failure(&self, point: WalFaultPoint) {
        self.faults.inject(point);
    }

    #[cfg(test)]
    fn disconnect_writer(&mut self) {
        let _ = self.sender.send(WalCommand::Disconnect);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
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
        if let Some(last) = segments.last()
            && let Some(num) = Self::parse_segment_number(last)
        {
            return (num, last.clone());
        }
        (0, dir.join("wal-00000000.bin"))
    }

    /// List all WAL segment files in a directory, sorted by name.
    fn list_segments(dir: &Path) -> Vec<PathBuf> {
        let mut segments = Vec::new();
        if let Ok(entries) = fs::read_dir(dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if let Some(name) = path.file_name().and_then(|n| n.to_str())
                    && name.starts_with("wal-")
                    && name.ends_with(".bin")
                {
                    segments.push(path);
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
        if let Err(error) = self.shutdown() {
            tracing::error!(error = %error, "WAL shutdown did not complete durably");
        }
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
            break; // Truncated entry - stop here (crash mid-write)
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
                    break; // Corrupted entry - stop here
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

/// Strict WAL reader for recovery-mode startup.
///
/// Unlike [`read_wal`], this never treats corruption or a torn final frame as
/// a successful prefix. ARCCHKPT nodes fail closed so a restart cannot expose
/// partially replayed consensus state as canonical.
pub fn read_wal_strict(path: impl AsRef<Path>) -> std::io::Result<Vec<WalEntry>> {
    let mut expected_sequence = 0u64;
    read_wal_strict_segment(path.as_ref(), &mut expected_sequence)
}

fn read_wal_strict_segment(
    path: &Path,
    expected_sequence: &mut u64,
) -> std::io::Result<Vec<WalEntry>> {
    const MAX_ENTRY_BYTES: usize = 1024 * 1024 * 1024;
    let file = File::open(path)?;
    let mut reader = BufReader::new(file);
    let mut entries = Vec::new();

    loop {
        let mut len_buf = [0u8; 4];
        let first = reader.read(&mut len_buf[..1])?;
        if first == 0 {
            break;
        }
        reader.read_exact(&mut len_buf[1..]).map_err(|error| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("truncated WAL frame length: {error}"),
            )
        })?;
        let len = u32::from_le_bytes(len_buf) as usize;
        if len == 0 || len > MAX_ENTRY_BYTES {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("invalid WAL frame length {len}"),
            ));
        }
        let mut data = vec![0u8; len];
        reader.read_exact(&mut data).map_err(|error| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("truncated WAL frame payload: {error}"),
            )
        })?;
        let entry: WalEntry = bincode::deserialize(&data).map_err(|error| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("invalid WAL entry encoding: {error}"),
            )
        })?;
        let payload = bincode::serialize(&(&entry.block_height, &entry.sequence, &entry.op))
            .map_err(std::io::Error::other)?;
        if entry.checksum != crc32fast::hash(&payload) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("WAL checksum mismatch at sequence {}", entry.sequence),
            ));
        }
        if entry.sequence != *expected_sequence {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!(
                    "WAL sequence gap: expected {}, got {}",
                    *expected_sequence, entry.sequence
                ),
            ));
        }
        *expected_sequence = expected_sequence.checked_add(1).ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::InvalidData, "WAL sequence overflow")
        })?;
        entries.push(entry);
    }
    Ok(entries)
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

/// Strictly read every segmented WAL entry with one continuous sequence.
///
/// Recovery-domain DAG startup uses this instead of [`read_wal_dir`] so a
/// corrupt/torn frame, a missing segment, or a cross-segment sequence gap
/// aborts startup rather than silently accepting a valid-looking prefix.
pub fn read_wal_dir_strict(dir: impl AsRef<Path>) -> std::io::Result<Vec<WalEntry>> {
    let segments = WalWriter::list_segments(dir.as_ref());
    let mut expected_sequence = 0u64;
    let mut all_entries = Vec::new();
    for segment in segments {
        all_entries.extend(read_wal_strict_segment(&segment, &mut expected_sequence)?);
    }
    Ok(all_entries)
}

/// Find the highest `block_height` recorded in any WAL segment under `dir`.
///
/// Reads only the most recent segment (segments are append-only, sorted by
/// segment number, and rotation only happens when the previous segment is
/// full — so the highest block_height is always in the last one). This is
/// the dag-wal recovery primitive: at boot, find where we left off, hand
/// that round number to consensus.set_initial_round, resume mid-stream.
///
/// Returns 0 if `dir` is empty, missing, or contains no parseable entries.
/// Bounded memory: reads at most one segment (default 64 MB).
pub fn latest_block_height_in_wal_dir(dir: impl AsRef<Path>) -> u64 {
    // Inline the same segment listing logic as WalWriter::list_segments —
    // we can't call the private helper from this free function.
    let mut segments: Vec<PathBuf> = Vec::new();
    if let Ok(entries) = fs::read_dir(dir.as_ref()) {
        for entry in entries.flatten() {
            let path = entry.path();
            if let Some(name) = path.file_name().and_then(|n| n.to_str())
                && name.starts_with("wal-")
                && name.ends_with(".bin")
            {
                segments.push(path);
            }
        }
    }
    segments.sort();

    // Walk segments newest-to-oldest and return the first non-zero max we
    // find. Rotation creates a new empty segment after the previous one
    // fills, so the very last segment can legitimately be empty — without
    // this fallback we'd return 0 and miss the recovery.
    //
    // Bound the scan to the last 3 segments (≤192 MB at default 64 MB
    // segment size) so we never read the full multi-GB history just to
    // find a round number.
    const MAX_SEGMENTS_TO_SCAN: usize = 3;
    let scan_count = segments.len().min(MAX_SEGMENTS_TO_SCAN);
    for seg in segments.iter().rev().take(scan_count) {
        let max = read_wal(seg).into_iter().map(|e| e.block_height).max();
        if let Some(h) = max {
            return h;
        }
    }
    0
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
    pub storage: ContractStorage,
    /// Contract bytecode cache: (address, wasm_bytes)
    pub contracts: Vec<(Address, Vec<u8>)>,
}

impl Snapshot {
    /// Write snapshot to disk as LZ4-compressed bincode.
    pub fn write_to(&self, path: impl AsRef<Path>) -> std::io::Result<()> {
        let data = bincode::serialize(self).map_err(std::io::Error::other)?;
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
                WalOp::SetAccount(test_addr(1), Account::new(test_addr(1), 1000)),
                1,
            );
            writer.append(
                WalOp::SetAccount(test_addr(2), Account::new(test_addr(2), 2000)),
                1,
            );
            writer.append(WalOp::Checkpoint(hash_bytes(b"root1")), 1);
            writer.sync().unwrap();
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
    fn strict_reader_rejects_torn_tail_instead_of_accepting_prefix() {
        let path = tmp_path("wal_strict_torn_tail.bin");
        let _ = fs::remove_file(&path);
        {
            let writer = WalWriter::new(&path).expect("create wal");
            writer.append(WalOp::Checkpoint(hash_bytes(b"complete")), 1);
            writer.sync().unwrap();
        }
        OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all(&8u32.to_le_bytes())
            .unwrap();

        assert_eq!(read_wal(&path).len(), 1, "legacy reader accepts prefix");
        let error = read_wal_strict(&path).unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
        assert!(error.to_string().contains("truncated WAL frame payload"));
        fs::remove_file(path).unwrap();
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
            writer.sync().unwrap();
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
            writer.sync().unwrap();
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
            writer.sync().unwrap();
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
        writer.sync().unwrap();
    }

    #[test]
    fn checksum_serialization_failure_is_latched() {
        let path = tmp_path("wal_checksum_serialization_failure.bin");
        let _ = fs::remove_file(&path);
        let writer = WalWriter::new(&path).unwrap();
        writer.inject_failure(WalFaultPoint::ChecksumSerialization);

        writer.append(WalOp::Checkpoint(Hash256::ZERO), 1);
        let error = writer.sync().unwrap_err();
        assert_eq!(error.operation(), "checksum serialization");
        assert_eq!(writer.failure(), Some(error));

        let _ = fs::remove_file(path);
    }

    #[test]
    fn entry_serialization_failure_is_latched_by_sync_barrier() {
        let path = tmp_path("wal_entry_serialization_failure.bin");
        let _ = fs::remove_file(&path);
        let writer = WalWriter::new(&path).unwrap();
        writer.inject_failure(WalFaultPoint::EntrySerialization);

        writer.append(WalOp::Checkpoint(Hash256::ZERO), 1);
        let error = writer.sync().unwrap_err();
        assert_eq!(error.operation(), "entry serialization");
        assert_eq!(writer.failure(), Some(error));

        let _ = fs::remove_file(path);
    }

    #[test]
    fn write_failure_is_latched_by_sync_barrier() {
        let path = tmp_path("wal_write_failure.bin");
        let _ = fs::remove_file(&path);
        let writer = WalWriter::new(&path).unwrap();
        writer.inject_failure(WalFaultPoint::Write);

        writer.append(WalOp::Checkpoint(Hash256::ZERO), 1);
        let error = writer.sync().unwrap_err();
        assert_eq!(error.operation(), "write");
        assert_eq!(writer.failure(), Some(error.clone()));
        assert_eq!(writer.sync().unwrap_err(), error, "first error is sticky");

        let _ = fs::remove_file(path);
    }

    #[test]
    fn flush_and_fsync_failures_are_observable() {
        for (name, point, operation) in [
            ("flush", WalFaultPoint::Flush, "flush"),
            ("fsync", WalFaultPoint::Fsync, "fsync"),
        ] {
            let path = tmp_path(&format!("wal_{name}_failure.bin"));
            let _ = fs::remove_file(&path);
            let writer = WalWriter::new(&path).unwrap();
            writer.inject_failure(point);

            if point == WalFaultPoint::Flush {
                writer.append(WalOp::Checkpoint(Hash256::ZERO), 1);
            }
            let error = writer.sync().unwrap_err();
            assert_eq!(error.operation(), operation);
            assert_eq!(writer.failure(), Some(error));

            let _ = fs::remove_file(path);
        }
    }

    #[test]
    fn rotation_failure_is_latched_and_does_not_advance_segment() {
        let dir = tmp_dir("wal_rotation_failure");
        let writer = WalWriter::with_segments(&dir, u64::MAX).unwrap();
        writer.inject_failure(WalFaultPoint::Rotation);

        let error = writer.rotate().unwrap_err();
        assert_eq!(error.operation(), "rotation");
        assert_eq!(writer.sync().unwrap_err(), error);
        assert!(!dir.join("wal-00000001.bin").exists());

        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn disconnected_writer_channel_is_latched() {
        let path = tmp_path("wal_channel_disconnect.bin");
        let _ = fs::remove_file(&path);
        let mut writer = WalWriter::new(&path).unwrap();
        writer.disconnect_writer();

        writer.append(WalOp::Checkpoint(Hash256::ZERO), 1);
        let error = writer.sync().unwrap_err();
        assert_eq!(error.operation(), "append channel send");
        assert_eq!(error.kind(), std::io::ErrorKind::BrokenPipe);
        assert_eq!(writer.failure(), Some(error));

        let _ = fs::remove_file(path);
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
            writer.sync().unwrap();
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
            writer.sync().unwrap();

            // Rotate to segment 1
            writer.rotate().unwrap();

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
            writer.sync().unwrap();

            // Rotate again to segment 2
            writer.rotate().unwrap();

            // Write to segment 2
            writer.append(WalOp::Checkpoint(hash_bytes(b"root")), 2);
            writer.sync().unwrap();
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
            writer.sync().unwrap();
            writer.rotate().unwrap();

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
            writer.sync().unwrap();
            writer.rotate().unwrap();

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
            writer.sync().unwrap();
            writer.rotate().unwrap();

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
            writer.sync().unwrap();
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
        assert!(
            segments_after[0]
                .file_name()
                .unwrap()
                .to_str()
                .unwrap()
                .contains("00000002")
        );
        assert!(
            segments_after[1]
                .file_name()
                .unwrap()
                .to_str()
                .unwrap()
                .contains("00000003")
        );

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
            writer.sync().unwrap();
            writer.rotate().unwrap();

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
            writer.sync().unwrap();
            writer.rotate().unwrap();

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
            writer.sync().unwrap();
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

    // ── v0.7.0 dag-wal recovery helper tests ───────────────────────────

    #[test]
    fn latest_block_height_returns_zero_for_missing_dir() {
        let dir = std::env::temp_dir().join("arc-wal-tests-missing-xyz");
        let _ = fs::remove_dir_all(&dir);
        assert_eq!(latest_block_height_in_wal_dir(&dir), 0);
    }

    #[test]
    fn latest_block_height_returns_zero_for_empty_dir() {
        let dir = tmp_dir("latest_empty");
        assert_eq!(latest_block_height_in_wal_dir(&dir), 0);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn latest_block_height_returns_max_in_latest_segment() {
        let dir = tmp_dir("latest_height");
        // Force tiny segments so we span multiple files. Append entries with
        // increasing block_height and ensure latest_block_height_in_wal_dir
        // returns the max.
        {
            let writer = WalWriter::with_segments(&dir, 1024).expect("create");
            for h in 0u64..200 {
                writer.append(WalOp::Checkpoint(Hash256::ZERO), h);
            }
            writer.sync().unwrap();
            // Drop the writer so the bg thread flushes before we read.
            drop(writer);
        }
        // Latest segment has the largest block_height. With tiny segments
        // and rotation-on-overflow, the auto-rotated final segment may be
        // empty (rotation happens AFTER the final write that filled the
        // segment lands in the previous one). So we read the latest
        // NON-EMPTY segment by walking backward — which the helper does
        // by virtue of taking max() over read_wal of the latest file (or
        // returning 0 and falling back is acceptable too). Either way the
        // helper should produce the actual max we wrote.
        //
        // For this test we relax the assertion: the helper must return
        // SOME value > 0 (proving it read something), and that value must
        // be ≤ 199 (the max we ever wrote). Detecting "exactly 199" relies
        // on rotation timing which is implementation-defined.
        let latest = latest_block_height_in_wal_dir(&dir);
        assert_eq!(
            latest, 199,
            "helper must find max round even when newest segment is the empty post-rotation file"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn latest_block_height_handles_unsorted_entries_within_segment() {
        // Write entries in non-monotonic order; helper still finds the max.
        let dir = tmp_dir("latest_unsorted");
        {
            let writer = WalWriter::with_segments(&dir, 64 * 1024 * 1024).expect("create");
            writer.append(WalOp::Checkpoint(Hash256::ZERO), 100);
            writer.append(WalOp::Checkpoint(Hash256::ZERO), 50);
            writer.append(WalOp::Checkpoint(Hash256::ZERO), 999);
            writer.append(WalOp::Checkpoint(Hash256::ZERO), 12);
            writer.sync().unwrap();
        }
        assert_eq!(latest_block_height_in_wal_dir(&dir), 999);
        let _ = fs::remove_dir_all(&dir);
    }
}
