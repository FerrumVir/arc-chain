//! Write-Ahead Log (WAL) for ARC Chain state persistence.
//!
//! Every state mutation is journaled to an append-only file BEFORE acknowledging.
//! Sequential writes only — never seeks, never reads during execution.
//! The async writer batches entries and flushes to SSD periodically.

use arc_crypto::Hash256;
use arc_types::{Account, Address, Block, TxReceipt};
use crossbeam::channel::{self, Receiver, Sender};
use serde::{Deserialize, Serialize};
use std::fs::{File, OpenOptions};
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

        let handle = thread::Builder::new()
            .name("wal-writer".into())
            .spawn(move || {
                Self::writer_loop(&mut writer, &receiver, &shutdown_clone);
            })?;

        // Determine starting sequence by reading existing entries
        let seq = Self::count_entries(&path);

        Ok(Self {
            sender,
            sequence: AtomicU64::new(seq),
            handle: Some(handle),
            shutdown,
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
        }
    }

    /// Non-blocking. Sends an entry to the background writer.
    pub fn append(&self, op: WalOp, block_height: u64) {
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

    /// Shut down the WAL writer, flushing all remaining entries.
    pub fn shutdown(&mut self) {
        self.shutdown.store(true, Ordering::Release);
        let _ = self.sender.send(WalCommand::Shutdown);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }

    /// The background writer loop. Receives entries, writes them, periodically flushes.
    fn writer_loop(
        writer: &mut BufWriter<File>,
        receiver: &Receiver<WalCommand>,
        _shutdown: &AtomicBool,
    ) {
        loop {
            match receiver.recv() {
                Ok(WalCommand::Append(entry)) => {
                    // Serialize entry with a length prefix (4 bytes LE)
                    if let Ok(data) = bincode::serialize(&entry) {
                        let len = (data.len() as u32).to_le_bytes();
                        let _ = writer.write_all(&len);
                        let _ = writer.write_all(&data);
                    }

                    // Drain any buffered entries without blocking
                    while let Ok(cmd) = receiver.try_recv() {
                        match cmd {
                            WalCommand::Append(entry) => {
                                if let Ok(data) = bincode::serialize(&entry) {
                                    let len = (data.len() as u32).to_le_bytes();
                                    let _ = writer.write_all(&len);
                                    let _ = writer.write_all(&data);
                                }
                            }
                            WalCommand::Sync(done) => {
                                let _ = writer.flush();
                                let _ = writer.get_ref().sync_data();
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
                }
                Ok(WalCommand::Sync(done)) => {
                    let _ = writer.flush();
                    let _ = writer.get_ref().sync_data();
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
}

impl Default for PersistenceConfig {
    fn default() -> Self {
        Self {
            snapshot_interval: 10_000,
            wal_path: PathBuf::from("data/wal.bin"),
            snapshot_dir: PathBuf::from("data/snapshots"),
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
}
