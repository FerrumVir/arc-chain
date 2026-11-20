use crate::recovery::RecoveryContext;
use arc_crypto::Hash256;
use arc_types::inference_contract::{
    InferenceCertificate, InferenceRequest, SettlementCredit, ValidatorMember,
};
use arc_types::{Account, Address, Block, EventLog, Identity, Transaction, TxReceipt};
use crossbeam::channel::{self, Receiver, Sender};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use std::fs::{self, File, OpenOptions};
use std::io::{BufReader, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use std::thread;

#[cfg(windows)]
fn move_file_create_only_write_through(source: &Path, destination: &Path) -> std::io::Result<()> {
    arc_crypto::secret_file::windows_move_path_write_through(source, destination, false)
}

fn internal_wal_namespace_target(name: &str) -> bool {
    if name == "state.wal" {
        return true;
    }
    if name
        .strip_prefix("state.wal.quarantine-")
        .and_then(|value| value.strip_suffix(".bin"))
        .is_some_and(|value| {
            value.len() == 64
                && value
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        })
    {
        return true;
    }
    if name
        .strip_prefix(".state.wal.genesis-")
        .and_then(|value| value.strip_suffix(".tmp"))
        .is_some_and(|value| uuid::Uuid::parse_str(value).is_ok())
    {
        return true;
    }
    let Some(segment) = name
        .strip_prefix("wal-")
        .and_then(|value| value.strip_suffix(".bin"))
    else {
        return false;
    };
    segment.len() == 8 && segment.bytes().all(|byte| byte.is_ascii_digit())
}

fn wal_namespace_rebarrier_target(name: &str) -> Option<&str> {
    let target = name
        .strip_prefix('.')?
        .strip_suffix(".namespace-rebarrier")?;
    internal_wal_namespace_target(target).then_some(target)
}

fn wal_namespace_rebarrier_path(path: &Path) -> std::io::Result<PathBuf> {
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("WAL path has no UTF-8 file name: {path:?}"),
            )
        })?;
    Ok(path.with_file_name(format!(".{file_name}.namespace-rebarrier")))
}

pub(crate) fn select_read_only_wal_path(path: &Path) -> std::io::Result<PathBuf> {
    fn validate_candidate(path: &Path) -> std::io::Result<bool> {
        match fs::symlink_metadata(path) {
            Ok(metadata) => {
                if metadata.file_type().is_symlink() || !metadata.is_file() {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        format!("legacy WAL namespace is not a regular non-symlink file: {path:?}"),
                    ));
                }
                drop(arc_crypto::secret_file::open_owned_nofollow_read(path)?);
                Ok(true)
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(error) => Err(error),
        }
    }

    let staging = wal_namespace_rebarrier_path(path)?;
    let live_exists = validate_candidate(path)?;
    let staging_exists = validate_candidate(&staging)?;
    match (live_exists, staging_exists) {
        (true, false) => Ok(path.to_path_buf()),
        (false, true) => Ok(staging),
        (true, true) => Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!(
                "legacy WAL exists in both live and interrupted namespace-rebarrier locations: {path:?}"
            ),
        )),
        (false, false) => Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!("legacy recovery source has no state WAL at {path:?}"),
        )),
    }
}

fn restore_wal_namespace_rebarrier(staging: &Path, target: &Path) -> std::io::Result<()> {
    let metadata = fs::symlink_metadata(staging)?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("WAL namespace rebarrier is not a regular file: {staging:?}"),
        ));
    }
    match fs::symlink_metadata(target) {
        Ok(_) => {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("WAL exists in both live and namespace-rebarrier locations: {target:?}"),
            ));
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    #[cfg(windows)]
    move_file_create_only_write_through(staging, target)?;
    #[cfg(not(windows))]
    {
        fs::rename(staging, target)?;
        WalWriter::sync_parent_directory(target)?;
    }
    Ok(())
}

#[cfg(windows)]
fn restore_exact_wal_namespace_rebarrier(path: &Path) -> std::io::Result<()> {
    let staging = wal_namespace_rebarrier_path(path)?;
    match fs::symlink_metadata(&staging) {
        Ok(_) => restore_wal_namespace_rebarrier(&staging, path),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

pub fn restore_interrupted_wal_namespace_rebarriers(directory: &Path) -> std::io::Result<()> {
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        let Some(target_name) = wal_namespace_rebarrier_target(&name) else {
            continue;
        };
        restore_wal_namespace_rebarrier(&entry.path(), &directory.join(target_name))?;
    }
    Ok(())
}

fn create_new_append_file_durably(path: &Path) -> std::io::Result<File> {
    #[cfg(windows)]
    {
        let parent = path.parent().unwrap_or_else(|| Path::new("."));
        let file_name = path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("wal");
        let staging = parent.join(format!(".{file_name}.create-{}.tmp", uuid::Uuid::new_v4()));
        let staging_file = arc_crypto::secret_file::create_new_private(&staging)?;
        if let Err(error) = staging_file.sync_all() {
            drop(staging_file);
            let _ = fs::remove_file(&staging);
            return Err(error);
        }
        drop(staging_file);
        if let Err(error) = move_file_create_only_write_through(&staging, path) {
            let _ = fs::remove_file(&staging);
            return Err(error);
        }
        arc_crypto::secret_file::open_private_append_owned_migration(path)
    }
    #[cfg(not(windows))]
    {
        let file = arc_crypto::secret_file::create_new_private(path)?;
        file.sync_all()?;
        WalWriter::sync_parent_directory(path)?;
        drop(file);
        arc_crypto::secret_file::open_private_append_owned_migration(path)
    }
}

fn wal_create_staging_target(name: &str) -> Option<&str> {
    let value = name.strip_prefix('.')?;
    let (target, identifier) = value.rsplit_once(".create-")?;
    let identifier = identifier.strip_suffix(".tmp")?;
    uuid::Uuid::parse_str(identifier).ok()?;
    (!target.is_empty()).then_some(target)
}

fn cleanup_create_staging_for_target(path: &Path) -> std::io::Result<()> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let target = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("WAL path has no UTF-8 file name: {path:?}"),
            )
        })?;
    for entry in fs::read_dir(parent)? {
        let entry = entry?;
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        if wal_create_staging_target(&name) != Some(target) {
            continue;
        }
        let staging = entry.path();
        let metadata = fs::symlink_metadata(&staging)?;
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("recognized WAL create staging is not a regular file: {staging:?}"),
            ));
        }
        if let Err(error) = fs::remove_file(&staging)
            && error.kind() != std::io::ErrorKind::NotFound
        {
            tracing::warn!(
                path = %staging.display(),
                %error,
                "deferring exact WAL create-staging cleanup"
            );
        }
    }
    Ok(())
}

#[cfg(windows)]
fn rebarrier_existing_append_file_durably(path: &Path) -> std::io::Result<File> {
    let staging = wal_namespace_rebarrier_path(path)?;
    restore_exact_wal_namespace_rebarrier(path)?;
    let file = arc_crypto::secret_file::open_private_append_owned_migration(path)?;
    file.sync_all()?;
    drop(file);
    move_file_create_only_write_through(path, &staging)?;
    move_file_create_only_write_through(&staging, path)?;
    let file = arc_crypto::secret_file::open_private_append_owned_migration(path)?;
    file.sync_all()?;
    Ok(file)
}

fn open_existing_append_file_durably(path: &Path) -> std::io::Result<File> {
    #[cfg(windows)]
    {
        rebarrier_existing_append_file_durably(path)
    }
    #[cfg(not(windows))]
    {
        let file = arc_crypto::secret_file::open_private_append_owned_migration(path)?;
        file.sync_all()?;
        WalWriter::sync_parent_directory(path)?;
        Ok(file)
    }
}

fn open_or_create_append_file_durably(path: &Path) -> std::io::Result<File> {
    cleanup_create_staging_for_target(path)?;
    match open_existing_append_file_durably(path) {
        Ok(file) => Ok(file),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            match create_new_append_file_durably(path) {
                Ok(file) => Ok(file),
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                    open_existing_append_file_durably(path)
                }
                Err(error) => Err(error),
            }
        }
        Err(error) => Err(error),
    }
}

fn publish_wal_quarantine_create_only(temporary: &Path, destination: &Path) -> std::io::Result<()> {
    #[cfg(windows)]
    {
        move_file_create_only_write_through(temporary, destination)
    }
    #[cfg(not(windows))]
    {
        fs::hard_link(temporary, destination)?;
        WalWriter::sync_parent_directory(destination)?;
        fs::remove_file(temporary)
    }
}

fn is_removed_wal_tombstone(name: &str) -> bool {
    let Some(rest) = name.strip_prefix(".wal-") else {
        return false;
    };
    let Some((segment, removal)) = rest.split_once(".bin.removed-") else {
        return false;
    };
    let Some(identifier) = removal.strip_suffix(".tmp") else {
        return false;
    };
    segment.len() == 8
        && segment.bytes().all(|byte| byte.is_ascii_digit())
        && uuid::Uuid::parse_str(identifier).is_ok()
}

fn remove_wal_tombstone_best_effort(path: &Path) {
    if let Err(error) = fs::remove_file(path)
        && error.kind() != std::io::ErrorKind::NotFound
    {
        tracing::warn!(
            path = %path.display(),
            %error,
            "deferring cleanup of retired WAL tombstone"
        );
    }
}

fn cleanup_removed_wal_tombstones(directory: &Path) -> std::io::Result<()> {
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        if !is_removed_wal_tombstone(&name) {
            continue;
        }
        let path = entry.path();
        let metadata = fs::symlink_metadata(&path)?;
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("WAL removal tombstone is not a regular file: {path:?}"),
            ));
        }
        remove_wal_tombstone_best_effort(&path);
    }
    Ok(())
}

#[cfg(windows)]
const WINDOWS_WAL_RETIREMENT_MOVE_ATTEMPTS: usize = 400;
#[cfg(windows)]
const WINDOWS_WAL_RETIREMENT_MOVE_RETRY_DELAY: std::time::Duration =
    std::time::Duration::from_millis(5);

#[cfg(windows)]
fn retry_windows_wal_retirement_move<Move, Sleep>(
    mut move_once: Move,
    mut sleep: Sleep,
) -> std::io::Result<()>
where
    Move: FnMut() -> std::io::Result<()>,
    Sleep: FnMut(std::time::Duration),
{
    use windows_sys::Win32::Foundation::{ERROR_LOCK_VIOLATION, ERROR_SHARING_VIOLATION};

    let mut last_transient = None;
    for attempt in 0..WINDOWS_WAL_RETIREMENT_MOVE_ATTEMPTS {
        match move_once() {
            Ok(()) => return Ok(()),
            Err(error)
                if matches!(
                    error.raw_os_error(),
                    Some(code)
                        if code == ERROR_SHARING_VIOLATION as i32
                            || code == ERROR_LOCK_VIOLATION as i32
                ) =>
            {
                last_transient = Some(error);
                if attempt + 1 < WINDOWS_WAL_RETIREMENT_MOVE_ATTEMPTS {
                    sleep(WINDOWS_WAL_RETIREMENT_MOVE_RETRY_DELAY);
                }
            }
            Err(error) => return Err(error),
        }
    }
    Err(last_transient.expect("a bounded WAL retirement retry always records its transient error"))
}

#[cfg(windows)]
fn move_wal_segment_to_tombstone_create_only_with_retry(
    source: &Path,
    destination: &Path,
) -> std::io::Result<()> {
    retry_windows_wal_retirement_move(
        || move_file_create_only_write_through(source, destination),
        std::thread::sleep,
    )
}

fn durably_remove_wal_segment(path: &Path) -> std::io::Result<()> {
    #[cfg(windows)]
    {
        let parent = path.parent().unwrap_or_else(|| Path::new("."));
        let file_name = path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("wal");
        let tombstone = parent.join(format!(".{file_name}.removed-{}.tmp", uuid::Uuid::new_v4()));
        move_wal_segment_to_tombstone_create_only_with_retry(path, &tombstone)?;
        remove_wal_tombstone_best_effort(&tombstone);
        Ok(())
    }
    #[cfg(not(windows))]
    {
        fs::remove_file(path)?;
        WalWriter::sync_parent_directory(path)
    }
}

pub type ContractStorage = Vec<(Address, Vec<(Hash256, Vec<u8>)>)>;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WalEntry {
    pub block_height: u64,
    pub sequence: u64,
    pub op: WalOp,
    pub checksum: u32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RepairableWalTail {
    TruncatedFrameLength,
    TruncatedFramePayload,
}

impl RepairableWalTail {
    pub(crate) fn stable_reason(self) -> &'static str {
        match self {
            Self::TruncatedFrameLength => "truncated_wal_frame_length",
            Self::TruncatedFramePayload => "truncated_wal_frame_payload",
        }
    }
}

#[derive(Debug)]
pub(crate) struct RepairableWalRead {
    pub(crate) entries: Vec<WalEntry>,
    pub(crate) frame_end_offsets: Vec<u64>,
    pub(crate) original_bytes: u64,
    pub(crate) original_hash: blake3::Hash,
    pub(crate) torn_tail: Option<RepairableWalTail>,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct RepairableWalSummary {
    pub(crate) entry_count: u64,
    pub(crate) original_bytes: u64,
    pub(crate) original_hash: blake3::Hash,
    pub(crate) torn_tail: Option<RepairableWalTail>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum WalOp {
    SetAccount(Address, Account),
    SetStorage(Address, Hash256, Vec<u8>),
    DeleteStorage(Address, Hash256),
    SetBlock(u64, Block),
    SetReceipt(Hash256, TxReceipt),
    SetAgent(Address, String, String, Vec<u8>),
    SetContract(Address, Vec<u8>),
    Checkpoint(Hash256),
    SetDagBlock(Hash256, Vec<u8>),
    SetDagRound(u64),
    CommitDagBlock(Hash256),
    SetFullTransaction(Hash256, Box<Transaction>),
    SetEventLogs(u64, Vec<EventLog>),
    SetIdentity(Address, Identity),
    SetValidatorState(Vec<(Address, u64)>, u64),
    SetRecoveryContext(RecoveryContext, Option<u64>),
    InferenceTransition(Box<InferenceTransitionRecord>),
    Rebase(Box<RebaseRecord>),
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RebaseRecord {
    pub height: u64,
    pub tip: Block,
    pub state: crate::snapshot::SnapshotPayload,
    pub anchor_hash: Hash256,
    pub anchor_round: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct InferenceTransitionRecord {
    pub context_commitment: Hash256,
    pub request_id: Hash256,
    pub escrow: Address,
    pub admission_height: u64,
    pub members: Vec<ValidatorMember>,
    pub metadata: InferenceMetadata,
    pub account_updates: Vec<(Address, Account)>,
    pub storage_updates: Vec<(Address, Hash256, Vec<u8>)>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct InferenceMetadata {
    pub context_commitment: Hash256,
    pub request: InferenceRequest,
    pub input_blob: Vec<u8>,
    pub status: InferenceTransitionStatus,
    pub output: Vec<u8>,
    pub output_hash: Option<Hash256>,
    pub certificate: Option<InferenceCertificate>,
    pub credits: Vec<SettlementCredit>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum InferenceTransitionStatus {
    Pending,
    Finalized,
    Refunded,
}

struct PendingWalEntry {
    block_height: u64,
    sequence: u64,
    op: WalOp,
}

enum WalCommand {
    Append(Box<PendingWalEntry>),
    Sync(channel::Sender<Result<(), WalError>>),
    Rotate(channel::Sender<Result<(), WalError>>),
    Shutdown(channel::Sender<Result<(), WalError>>),
    #[cfg(test)]
    Disconnect,
}

struct WalAdmission {
    next_sequence: u64,
    accepting: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct WalFileIdentity {
    len: u64,
    #[cfg(unix)]
    device: u64,
    #[cfg(unix)]
    inode: u64,
    #[cfg(unix)]
    modified_seconds: i64,
    #[cfg(unix)]
    modified_nanoseconds: i64,
    #[cfg(unix)]
    changed_seconds: i64,
    #[cfg(unix)]
    changed_nanoseconds: i64,
    #[cfg(windows)]
    creation_time: u64,
    #[cfg(windows)]
    last_write_time: u64,
    #[cfg(not(any(unix, windows)))]
    modified: Option<std::time::SystemTime>,
    #[cfg(not(any(unix, windows)))]
    created: Option<std::time::SystemTime>,
}

impl WalFileIdentity {
    fn from_metadata(metadata: &fs::Metadata) -> Self {
        #[cfg(unix)]
        use std::os::unix::fs::MetadataExt as _;
        #[cfg(windows)]
        use std::os::windows::fs::MetadataExt as _;

        Self {
            len: metadata.len(),
            #[cfg(unix)]
            device: metadata.dev(),
            #[cfg(unix)]
            inode: metadata.ino(),
            #[cfg(unix)]
            modified_seconds: metadata.mtime(),
            #[cfg(unix)]
            modified_nanoseconds: metadata.mtime_nsec(),
            #[cfg(unix)]
            changed_seconds: metadata.ctime(),
            #[cfg(unix)]
            changed_nanoseconds: metadata.ctime_nsec(),
            #[cfg(windows)]
            creation_time: metadata.creation_time(),
            #[cfg(windows)]
            last_write_time: metadata.last_write_time(),
            #[cfg(not(any(unix, windows)))]
            modified: metadata.modified().ok(),
            #[cfg(not(any(unix, windows)))]
            created: metadata.created().ok(),
        }
    }
}

#[cfg(windows)]
fn windows_wal_file_identity(file: &File) -> std::io::Result<(u32, u64)> {
    use std::os::windows::io::AsRawHandle as _;
    use windows_sys::Win32::Storage::FileSystem::{
        BY_HANDLE_FILE_INFORMATION, GetFileInformationByHandle,
    };

    let mut information = BY_HANDLE_FILE_INFORMATION::default();
    if unsafe { GetFileInformationByHandle(file.as_raw_handle().cast(), &mut information) } == 0 {
        return Err(std::io::Error::last_os_error());
    }
    let file_index =
        (u64::from(information.nFileIndexHigh) << 32) | u64::from(information.nFileIndexLow);
    Ok((information.dwVolumeSerialNumber, file_index))
}

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

    pub fn operation(&self) -> &'static str {
        self.operation
    }

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
            .compare_exchange(
                point as u8,
                0,
                std::sync::atomic::Ordering::AcqRel,
                std::sync::atomic::Ordering::Acquire,
            )
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
        self.next
            .store(point as u8, std::sync::atomic::Ordering::Release);
    }
}

pub struct WalWriter {
    sender: Sender<WalCommand>,
    admission: Mutex<WalAdmission>,
    handle: Option<thread::JoinHandle<()>>,
    failure: Arc<OnceLock<WalError>>,
    #[cfg(test)]
    faults: Arc<WalFaultInjector>,
    is_null: bool,
    wal_dir: PathBuf,
}

impl WalWriter {
    pub fn new(path: impl AsRef<Path>) -> std::io::Result<Self> {
        let path = path.as_ref().to_path_buf();
        let file = open_or_create_append_file_durably(&path)?;
        let mut expected_sequence = Some(0u64);
        Self::validate_existing_segment_handle(&path, &file, &mut expected_sequence)?;
        let seq = expected_sequence.unwrap_or(0);
        let mut writer = BufWriter::with_capacity(256 * 1024, file);

        let (sender, receiver): (Sender<WalCommand>, Receiver<WalCommand>) = channel::unbounded();
        let failure = Arc::new(OnceLock::new());
        let failure_clone = failure.clone();
        let faults = Arc::new(WalFaultInjector::new());
        let faults_clone = faults.clone();

        let wal_dir = path
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .to_path_buf();

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
            admission: Mutex::new(WalAdmission {
                next_sequence: seq,
                accepting: true,
            }),
            handle: Some(handle),
            failure,
            #[cfg(test)]
            faults,
            is_null: false,
            wal_dir,
        })
    }

    pub fn with_segments(
        wal_dir: impl AsRef<Path>,
        max_segment_size: u64,
    ) -> std::io::Result<Self> {
        let requested_wal_dir = wal_dir.as_ref();
        let parent = requested_wal_dir
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        match fs::symlink_metadata(parent) {
            Ok(_) if parent.is_dir() => {}
            Ok(_) => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    format!(
                        "segmented WAL parent is not a directory: {}",
                        parent.display()
                    ),
                ));
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                arc_crypto::secret_file::create_private_directory_tree(parent)?;
            }
            Err(error) => return Err(error),
        }
        let namespace_lock =
            arc_crypto::secret_file::acquire_private_directory_namespace_lock(requested_wal_dir)?;
        namespace_lock.restore_interrupted()?;
        let wal_dir = namespace_lock.target().to_path_buf();
        if wal_dir.exists() {
            let metadata = fs::symlink_metadata(&wal_dir)?;
            if metadata.file_type().is_symlink() || !metadata.is_dir() {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    format!(
                        "segmented WAL path is not a real directory: {}",
                        wal_dir.display()
                    ),
                ));
            }
        }
        arc_crypto::secret_file::create_private_directory_tree(&wal_dir)?;
        namespace_lock.rebarrier_existing()?;
        restore_interrupted_wal_namespace_rebarriers(&wal_dir)?;
        cleanup_removed_wal_tombstones(&wal_dir)?;

        let (segment_number, seg_path) = Self::find_latest_segment(&wal_dir)?;
        let file = open_or_create_append_file_durably(&seg_path)?;
        let seq = Self::count_entries_in_dir_with_append_handle(&wal_dir, &seg_path, &file)?;
        let mut writer = BufWriter::with_capacity(256 * 1024, file);

        let (sender, receiver): (Sender<WalCommand>, Receiver<WalCommand>) = channel::unbounded();
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

        Ok(Self {
            sender,
            admission: Mutex::new(WalAdmission {
                next_sequence: seq,
                accepting: true,
            }),
            handle: Some(handle),
            failure,
            #[cfg(test)]
            faults,
            is_null: false,
            wal_dir,
        })
    }

    pub fn null() -> Self {
        let (sender, _receiver) = channel::unbounded();
        Self {
            sender,
            admission: Mutex::new(WalAdmission {
                next_sequence: 0,
                accepting: false,
            }),
            handle: None,
            failure: Arc::new(OnceLock::new()),
            #[cfg(test)]
            faults: Arc::new(WalFaultInjector::new()),
            is_null: true,
            wal_dir: PathBuf::new(),
        }
    }

    pub fn sequence(&self) -> u64 {
        self.admission.lock().next_sequence
    }

    #[inline]
    pub fn is_active(&self) -> bool {
        self.handle.is_some()
    }

    pub fn directory(&self) -> Option<&Path> {
        (!self.is_null).then_some(self.wal_dir.as_path())
    }

    pub fn failure(&self) -> Option<WalError> {
        self.failure.get().cloned()
    }

    pub fn check_health(&self) -> Result<(), WalError> {
        if self.is_null {
            return Ok(());
        }
        let admission = self.admission.lock();
        self.check_admission(&admission)
    }

    fn check_admission(&self, admission: &WalAdmission) -> Result<(), WalError> {
        if let Some(error) = self.failure() {
            return Err(error);
        }
        if !admission.accepting {
            return Err(WalError::message(
                "health check",
                std::io::ErrorKind::BrokenPipe,
                "writer is shut down",
            ));
        }
        Ok(())
    }

    pub fn append(&self, op: WalOp, block_height: u64) {
        self.append_with_admission_hook(op, block_height, || {});
    }

    fn append_with_admission_hook<F>(&self, op: WalOp, block_height: u64, admitted: F)
    where
        F: FnOnce(),
    {
        if self.is_null {
            return;
        }

        let mut admission = self.admission.lock();
        if self.check_admission(&admission).is_err() {
            return;
        }

        let sequence = admission.next_sequence;
        let Some(next_sequence) = sequence.checked_add(1) else {
            self.latch(WalError::message(
                "sequence assignment",
                std::io::ErrorKind::InvalidData,
                "WAL sequence overflow",
            ));
            return;
        };
        admitted();
        let pending = PendingWalEntry {
            block_height,
            sequence,
            op,
        };

        if self
            .sender
            .send(WalCommand::Append(Box::new(pending)))
            .is_err()
        {
            self.latch(WalError::message(
                "append channel send",
                std::io::ErrorKind::BrokenPipe,
                "writer channel disconnected",
            ));
        } else {
            admission.next_sequence = next_sequence;
        }
    }

    pub fn sync(&self) -> Result<(), WalError> {
        if self.is_null {
            return Ok(());
        }
        let (done_tx, done_rx) = channel::bounded(1);
        let send_result = {
            let admission = self.admission.lock();
            self.check_admission(&admission)?;
            self.sender.send(WalCommand::Sync(done_tx))
        };
        send_result.map_err(|_| {
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

    pub fn rotate(&self) -> Result<(), WalError> {
        if self.is_null {
            return Ok(());
        }
        let (done_tx, done_rx) = channel::bounded(1);
        let send_result = {
            let admission = self.admission.lock();
            self.check_admission(&admission)?;
            self.sender.send(WalCommand::Rotate(done_tx))
        };
        send_result.map_err(|_| {
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

    pub fn delete_segments_before(&self, wal_sequence: u64) -> std::io::Result<u32> {
        self.delete_segments_before_with_admission_hook(wal_sequence, || {})
    }

    fn delete_segments_before_with_admission_hook<F>(
        &self,
        wal_sequence: u64,
        synchronized: F,
    ) -> std::io::Result<u32>
    where
        F: FnOnce(),
    {
        if self.is_null {
            return Ok(0);
        }

        let admission = self.admission.lock();
        self.check_admission(&admission)
            .map_err(|error| std::io::Error::new(error.kind(), error))?;
        let (done_tx, done_rx) = channel::bounded(1);
        self.sender.send(WalCommand::Sync(done_tx)).map_err(|_| {
            let error = self.latch(WalError::message(
                "pruning sync channel send",
                std::io::ErrorKind::BrokenPipe,
                "writer channel disconnected",
            ));
            std::io::Error::new(error.kind(), error)
        })?;
        let sync_result = done_rx.recv().map_err(|_| {
            let error = self.failure().unwrap_or_else(|| {
                self.latch(WalError::message(
                    "pruning sync acknowledgement",
                    std::io::ErrorKind::BrokenPipe,
                    "writer exited before acknowledging pruning fsync",
                ))
            });
            std::io::Error::new(error.kind(), error)
        })?;
        sync_result.map_err(|error| std::io::Error::new(error.kind(), error))?;

        synchronized();
        Self::delete_segments_before_live_writer_fenced(&self.wal_dir, wal_sequence, 2)
    }

    pub fn delete_segments_before_in_dir(
        wal_dir: &Path,
        wal_sequence: u64,
        min_retain: usize,
    ) -> std::io::Result<u32> {
        Self::delete_segments_before_in_dir_inner(wal_dir, wal_sequence, min_retain, false)
    }

    fn delete_segments_before_live_writer_fenced(
        wal_dir: &Path,
        wal_sequence: u64,
        min_retain: usize,
    ) -> std::io::Result<u32> {
        Self::delete_segments_before_in_dir_inner(wal_dir, wal_sequence, min_retain, true)
    }

    fn delete_segments_before_in_dir_inner(
        wal_dir: &Path,
        wal_sequence: u64,
        min_retain: usize,
        live_writer_fenced: bool,
    ) -> std::io::Result<u32> {
        cleanup_removed_wal_tombstones(wal_dir)?;
        let min_retain = if min_retain < 2 { 2 } else { min_retain };
        let segments = Self::list_segments_strict(wal_dir)?;
        Self::validate_segment_namespace(&segments)?;

        if segments.len() <= min_retain {
            return Ok(0);
        }

        let mut deletable: Vec<PathBuf> = Vec::new();
        let mut last_nonempty_segment = None;
        let first_segment = segments
            .first()
            .and_then(|segment| Self::parse_segment_number(segment))
            .unwrap_or(0);
        let mut expected_sequence = (first_segment == 0).then_some(0);
        let mut within_deletable_prefix = true;
        for (segment_index, seg_path) in segments.iter().enumerate() {
            #[cfg(windows)]
            let entries = if live_writer_fenced && segment_index + 1 == segments.len() {
                read_wal_strict_live_writer_segment(seg_path, &mut expected_sequence)?
            } else {
                read_wal_strict_segment(seg_path, &mut expected_sequence)?
            };
            #[cfg(not(windows))]
            let entries = read_wal_strict_segment(seg_path, &mut expected_sequence)?;
            #[cfg(not(windows))]
            let _ = live_writer_fenced;
            if !entries.is_empty() {
                last_nonempty_segment = Some(segment_index);
            }
            let segment_is_before_cutoff = entries
                .last()
                .is_none_or(|last| last.sequence < wal_sequence);
            if within_deletable_prefix && segment_is_before_cutoff {
                deletable.push(seg_path.clone());
            } else {
                within_deletable_prefix = false;
            }
        }

        if let Some(last_nonempty_segment) = last_nonempty_segment
            && last_nonempty_segment < deletable.len()
        {
            deletable.truncate(last_nonempty_segment);
        }

        let max_deletable = segments.len().saturating_sub(min_retain);
        let to_delete = deletable.len().min(max_deletable);

        let mut deleted = 0u32;
        for path in deletable.into_iter().take(to_delete) {
            durably_remove_wal_segment(&path)?;
            deleted += 1;
        }
        Ok(deleted)
    }

    pub fn shutdown(&mut self) -> Result<(), WalError> {
        if self.is_null || self.handle.is_none() {
            return self.failure().map_or(Ok(()), Err);
        }

        let (done_tx, done_rx) = channel::bounded(1);
        let send_result = {
            let mut admission = self.admission.lock();
            admission.accepting = false;
            self.sender.send(WalCommand::Shutdown(done_tx))
        };
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
        let is_dir = wal_path.is_dir();

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
                WalCommand::Append(pending) => {
                    match Self::write_pending_entry(writer, *pending, faults) {
                        Ok(entry_size) => bytes_written += entry_size,
                        Err(error) => {
                            Self::latch_shared(failure, error);
                            return;
                        }
                    }

                    loop {
                        match receiver.try_recv() {
                            Ok(WalCommand::Append(pending)) => {
                                match Self::write_pending_entry(writer, *pending, faults) {
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

    fn write_pending_entry(
        writer: &mut BufWriter<File>,
        pending: PendingWalEntry,
        faults: &WalFaultInjector,
    ) -> Result<u64, WalError> {
        faults
            .check(WalFaultPoint::ChecksumSerialization)
            .map_err(|error| WalError::io("checksum serialization", &error))?;
        let payload = bincode::serialize(&(&pending.block_height, &pending.sequence, &pending.op))
            .map_err(|error| {
                WalError::message(
                    "checksum serialization",
                    std::io::ErrorKind::InvalidData,
                    error.to_string(),
                )
            })?;
        let checksum = crc32fast::hash(&payload);
        drop(payload);
        let entry = WalEntry {
            block_height: pending.block_height,
            sequence: pending.sequence,
            op: pending.op,
            checksum,
        };
        Self::write_entry(writer, &entry, faults)
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
        if data.len() > MAX_WAL_ENTRY_BYTES {
            return Err(WalError::message(
                "entry framing",
                std::io::ErrorKind::InvalidData,
                format!(
                    "serialized entry is too large: {} bytes (maximum {MAX_WAL_ENTRY_BYTES})",
                    data.len()
                ),
            ));
        }
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

        let next_segment = segment_number.checked_add(1).ok_or_else(|| {
            WalError::message(
                "rotation",
                std::io::ErrorKind::InvalidInput,
                "segment number overflow",
            )
        })?;
        if next_segment > MAX_WAL_SEGMENT_NUMBER {
            return Err(WalError::message(
                "rotation",
                std::io::ErrorKind::InvalidInput,
                "WAL exhausted its canonical eight-digit segment namespace",
            ));
        }
        let new_path = if is_dir {
            wal_path.join(format!("wal-{next_segment:08}.bin"))
        } else {
            let dir = wal_path.parent().unwrap_or_else(|| Path::new("."));
            dir.join(format!("wal-{next_segment:08}.bin"))
        };

        let file = create_new_append_file_durably(&new_path)
            .map_err(|error| WalError::io("rotation open", &error))?;

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

    #[cfg(all(test, windows))]
    fn count_entries_in_dir(dir: &Path) -> std::io::Result<u64> {
        Self::count_entries_in_dir_inner(dir, None)
    }

    fn count_entries_in_dir_with_append_handle(
        dir: &Path,
        append_path: &Path,
        append_file: &File,
    ) -> std::io::Result<u64> {
        Self::count_entries_in_dir_inner(dir, Some((append_path, append_file)))
    }

    fn count_entries_in_dir_inner(
        dir: &Path,
        append: Option<(&Path, &File)>,
    ) -> std::io::Result<u64> {
        let segments = Self::list_segments_strict(dir)?;
        let first_segment = Self::validate_segment_namespace(&segments)?;
        if let Some((append_path, _)) = append
            && segments.last().map(PathBuf::as_path) != Some(append_path)
        {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("WAL append target is not the final canonical segment: {append_path:?}"),
            ));
        }
        let mut expected_sequence = (first_segment == 0).then_some(0);
        for segment in segments {
            if let Some((append_path, append_file)) = append
                && segment == append_path
            {
                Self::validate_existing_segment_handle(
                    &segment,
                    append_file,
                    &mut expected_sequence,
                )?;
            } else {
                Self::validate_existing_segment(&segment, &mut expected_sequence)?;
            }
        }
        Ok(expected_sequence.unwrap_or(0))
    }

    fn validate_segment_namespace(segments: &[PathBuf]) -> std::io::Result<u64> {
        let first_segment = match segments.first() {
            Some(segment) => Self::parse_segment_number(segment).ok_or_else(|| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!("invalid WAL segment name: {}", segment.display()),
                )
            })?,
            None => return Ok(0),
        };
        let mut expected_segment = first_segment;
        for segment in segments {
            let actual_segment = Self::parse_segment_number(segment).ok_or_else(|| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!("invalid WAL segment name: {}", segment.display()),
                )
            })?;
            if actual_segment != expected_segment {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!(
                        "WAL segment gap: expected {expected_segment:08}, got {actual_segment:08}"
                    ),
                ));
            }
            expected_segment = expected_segment.checked_add(1).ok_or_else(|| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "WAL segment number overflow",
                )
            })?;
        }
        Ok(first_segment)
    }

    fn validate_existing_segment(
        path: &Path,
        expected_sequence: &mut Option<u64>,
    ) -> std::io::Result<()> {
        let file = arc_crypto::secret_file::open_owned_nofollow_read(path)?;
        Self::validate_existing_segment_handle(path, &file, expected_sequence)
    }

    fn validate_existing_segment_handle(
        path: &Path,
        file: &File,
        expected_sequence: &mut Option<u64>,
    ) -> std::io::Result<()> {
        let path_metadata = fs::symlink_metadata(path)?;
        if path_metadata.file_type().is_symlink() || !path_metadata.is_file() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("WAL segment is not a regular non-symlink file: {path:?}"),
            ));
        }
        let original_identity = WalFileIdentity::from_metadata(&file.metadata()?);
        #[cfg(windows)]
        let original_file_identity = windows_wal_file_identity(file)?;
        if WalFileIdentity::from_metadata(&path_metadata) != original_identity {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("WAL segment changed while being opened: {path:?}"),
            ));
        }
        let original_bytes = original_identity.len;
        let mut reader = BufReader::new(file.try_clone()?);
        reader.seek(SeekFrom::Start(0))?;
        let mut offset = 0u64;

        while offset < original_bytes {
            let remaining = original_bytes.checked_sub(offset).ok_or_else(|| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "WAL byte offset exceeds file size",
                )
            })?;
            if remaining < 4 {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!("truncated WAL frame length at byte {offset}"),
                ));
            }

            let mut len_buf = [0u8; 4];
            reader.read_exact(&mut len_buf)?;
            let len = u32::from_le_bytes(len_buf) as usize;
            if len == 0 || len > MAX_WAL_ENTRY_BYTES {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!("invalid WAL frame length {len} at byte {offset}"),
                ));
            }
            let frame_end = offset
                .checked_add(4)
                .and_then(|value| value.checked_add(len as u64))
                .ok_or_else(|| {
                    std::io::Error::new(std::io::ErrorKind::InvalidData, "WAL byte offset overflow")
                })?;
            if frame_end > original_bytes {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!("truncated WAL frame payload at byte {offset}"),
                ));
            }

            let mut data = allocate_wal_frame(len)?;
            reader.read_exact(&mut data)?;

            let entry = bincode::deserialize_limited_exact::<WalEntry, MAX_WAL_ENTRY_BYTES>(&data)
                .map_err(|error| {
                    std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        format!("invalid WAL entry encoding at byte {offset}: {error}"),
                    )
                })?;
            drop(data);
            let payload = bincode::serialize(&(&entry.block_height, &entry.sequence, &entry.op))
                .map_err(std::io::Error::other)?;
            if entry.checksum != crc32fast::hash(&payload) {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!("WAL checksum mismatch at sequence {}", entry.sequence),
                ));
            }
            let expected = expected_sequence.get_or_insert(entry.sequence);
            if entry.sequence != *expected {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!(
                        "WAL sequence gap: expected {}, got {}",
                        *expected, entry.sequence
                    ),
                ));
            }
            *expected = expected.checked_add(1).ok_or_else(|| {
                std::io::Error::new(std::io::ErrorKind::InvalidData, "WAL sequence overflow")
            })?;
            offset = frame_end;
        }

        let final_handle_identity = WalFileIdentity::from_metadata(&reader.get_ref().metadata()?);
        let retained_handle_identity = WalFileIdentity::from_metadata(&file.metadata()?);
        let final_path_metadata = fs::symlink_metadata(path)?;
        if final_path_metadata.file_type().is_symlink() || !final_path_metadata.is_file() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("WAL segment changed type while validating: {path:?}"),
            ));
        }
        let final_path_identity = WalFileIdentity::from_metadata(&final_path_metadata);
        if final_handle_identity != original_identity
            || retained_handle_identity != original_identity
            || final_path_identity != original_identity
        {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("WAL segment changed while validating: {path:?}"),
            ));
        }
        #[cfg(windows)]
        {
            let final_path_file =
                arc_crypto::secret_file::open_owned_nofollow_identity_probe(path)?;
            if windows_wal_file_identity(file)? != original_file_identity
                || windows_wal_file_identity(reader.get_ref())? != original_file_identity
                || windows_wal_file_identity(&final_path_file)? != original_file_identity
            {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!("WAL segment identity changed while validating: {path:?}"),
                ));
            }
        }
        Ok(())
    }

    fn find_latest_segment(dir: &Path) -> std::io::Result<(u64, PathBuf)> {
        let segments = Self::list_segments_strict(dir)?;
        Self::validate_segment_namespace(&segments)?;
        if let Some(last) = segments.last()
            && let Some(num) = Self::parse_segment_number(last)
        {
            return Ok((num, last.clone()));
        }
        Ok((0, dir.join("wal-00000000.bin")))
    }

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

    fn list_segments_strict(dir: &Path) -> std::io::Result<Vec<PathBuf>> {
        let mut segments = Vec::new();
        for entry in fs::read_dir(dir)? {
            let entry = entry?;
            let file_name = entry.file_name();
            let name = file_name.to_str().ok_or_else(|| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!(
                        "WAL directory contains a non-UTF-8 entry: {}",
                        dir.display()
                    ),
                )
            })?;
            if name.starts_with("wal-") && name.ends_with(".bin") {
                segments.push(entry.path());
            }
        }
        segments.sort();
        Ok(segments)
    }

    fn parse_segment_number(path: &Path) -> Option<u64> {
        let name = path.file_name()?.to_str()?;
        let stripped = name.strip_prefix("wal-")?.strip_suffix(".bin")?;
        if stripped.len() != 8 || !stripped.bytes().all(|byte| byte.is_ascii_digit()) {
            return None;
        }
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

const MAX_WAL_ENTRY_BYTES: usize = 256 * 1024 * 1024;
const MAX_WAL_SEGMENT_NUMBER: u64 = 99_999_999;

fn allocate_wal_frame(len: usize) -> std::io::Result<Vec<u8>> {
    if len == 0 || len > MAX_WAL_ENTRY_BYTES {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("invalid WAL frame length {len}"),
        ));
    }
    let mut data = Vec::new();
    data.try_reserve_exact(len).map_err(|error| {
        std::io::Error::new(
            std::io::ErrorKind::OutOfMemory,
            format!("cannot allocate bounded WAL frame of {len} bytes: {error}"),
        )
    })?;
    data.resize(len, 0);
    Ok(data)
}

fn finish_repairable_visit(
    reader: &BufReader<File>,
    path: &Path,
    entry_count: u64,
    original_bytes: u64,
    torn_tail: Option<RepairableWalTail>,
) -> std::io::Result<RepairableWalSummary> {
    let final_bytes = reader.get_ref().metadata()?.len();
    if final_bytes != original_bytes {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!(
                "WAL changed size while being inspected: path={path:?}, before={original_bytes}, after={final_bytes}"
            ),
        ));
    }
    let mut identity = reader.get_ref().try_clone()?;
    let original_hash = hash_file_range(&mut identity, 0, original_bytes)?;
    if identity.metadata()?.len() != original_bytes {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("WAL changed while being hashed: {path:?}"),
        ));
    }
    Ok(RepairableWalSummary {
        entry_count,
        original_bytes,
        original_hash,
        torn_tail,
    })
}

pub(crate) fn read_repairable_wal_prefix(
    path: impl AsRef<Path>,
) -> std::io::Result<RepairableWalRead> {
    let mut entries = Vec::new();
    let mut frame_end_offsets = Vec::new();
    let summary = visit_repairable_wal_prefix(path, |entry, frame_end| {
        entries.push(entry);
        frame_end_offsets.push(frame_end);
        Ok(())
    })?;
    Ok(RepairableWalRead {
        entries,
        frame_end_offsets,
        original_bytes: summary.original_bytes,
        original_hash: summary.original_hash,
        torn_tail: summary.torn_tail,
    })
}

pub(crate) fn visit_repairable_wal_prefix(
    path: impl AsRef<Path>,
    mut visitor: impl FnMut(WalEntry, u64) -> std::io::Result<()>,
) -> std::io::Result<RepairableWalSummary> {
    let path = path.as_ref();
    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() || !metadata.file_type().is_file() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("state WAL must be a regular non-symlink file: {path:?}"),
        ));
    }
    let original_bytes = metadata.len();
    let mut reader = BufReader::new(File::open(path)?);
    let mut expected_sequence = 0u64;
    let mut entry_count = 0u64;
    let mut offset = 0u64;

    loop {
        let mut len_buf = [0u8; 4];
        let first = reader.read(&mut len_buf[..1])?;
        if first == 0 {
            if offset != original_bytes {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!(
                        "WAL parsed byte count differs from file size: parsed={offset}, file={original_bytes}"
                    ),
                ));
            }
            return finish_repairable_visit(&reader, path, entry_count, original_bytes, None);
        }
        if let Err(error) = reader.read_exact(&mut len_buf[1..]) {
            if error.kind() != std::io::ErrorKind::UnexpectedEof {
                return Err(error);
            }
            return finish_repairable_visit(
                &reader,
                path,
                entry_count,
                original_bytes,
                Some(RepairableWalTail::TruncatedFrameLength),
            );
        }

        let len = u32::from_le_bytes(len_buf) as usize;
        if len == 0 || len > MAX_WAL_ENTRY_BYTES {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("invalid WAL frame length {len}"),
            ));
        }
        let declared_frame_end = offset
            .checked_add(4)
            .and_then(|value| value.checked_add(len as u64))
            .ok_or_else(|| {
                std::io::Error::new(std::io::ErrorKind::InvalidData, "WAL byte offset overflow")
            })?;
        if declared_frame_end > original_bytes {
            return finish_repairable_visit(
                &reader,
                path,
                entry_count,
                original_bytes,
                Some(RepairableWalTail::TruncatedFramePayload),
            );
        }
        let mut data = allocate_wal_frame(len)?;
        if let Err(error) = reader.read_exact(&mut data) {
            if error.kind() != std::io::ErrorKind::UnexpectedEof {
                return Err(error);
            }
            return finish_repairable_visit(
                &reader,
                path,
                entry_count,
                original_bytes,
                Some(RepairableWalTail::TruncatedFramePayload),
            );
        }

        let entry: WalEntry = bincode::deserialize_limited_exact::<WalEntry, MAX_WAL_ENTRY_BYTES>(
            &data,
        )
        .map_err(|error| {
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
        if entry.sequence != expected_sequence {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!(
                    "WAL sequence gap: expected {expected_sequence}, got {}",
                    entry.sequence
                ),
            ));
        }

        offset = declared_frame_end;
        expected_sequence = expected_sequence.checked_add(1).ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::InvalidData, "WAL sequence overflow")
        })?;
        entry_count = entry_count.checked_add(1).ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::InvalidData, "WAL entry count overflow")
        })?;
        visitor(entry, offset)?;
    }
}

fn hash_file_range(file: &mut File, start: u64, length: u64) -> std::io::Result<blake3::Hash> {
    file.seek(SeekFrom::Start(start))?;
    let mut remaining = length;
    let mut buffer = [0u8; 64 * 1024];
    let mut hasher = blake3::Hasher::new();
    while remaining != 0 {
        let limit = usize::try_from(remaining.min(buffer.len() as u64)).unwrap_or(buffer.len());
        let read = file.read(&mut buffer[..limit])?;
        if read == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "state WAL changed while hashing its rejected tail",
            ));
        }
        hasher.update(&buffer[..read]);
        remaining -= read as u64;
    }
    Ok(hasher.finalize())
}

fn copy_file_range(
    source: &mut File,
    destination: &mut File,
    start: u64,
    length: u64,
) -> std::io::Result<()> {
    source.seek(SeekFrom::Start(start))?;
    let mut remaining = length;
    let mut buffer = [0u8; 64 * 1024];
    while remaining != 0 {
        let limit = usize::try_from(remaining.min(buffer.len() as u64)).unwrap_or(buffer.len());
        let read = source.read(&mut buffer[..limit])?;
        if read == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "state WAL changed while copying its rejected tail",
            ));
        }
        destination.write_all(&buffer[..read])?;
        remaining -= read as u64;
    }
    Ok(())
}

fn verify_quarantine_file(
    path: &Path,
    expected_length: u64,
    expected_hash: blake3::Hash,
) -> std::io::Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink()
        || !metadata.file_type().is_file()
        || metadata.len() != expected_length
    {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("existing WAL quarantine is not the expected regular file: {path:?}"),
        ));
    }
    let mut file = File::open(path)?;
    let actual_hash = hash_file_range(&mut file, 0, expected_length)?;
    if actual_hash != expected_hash {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("existing WAL quarantine content hash mismatch: {path:?}"),
        ));
    }
    Ok(())
}

pub(crate) fn verify_wal_file_identity(
    path: impl AsRef<Path>,
    expected_bytes: u64,
    expected_hash: blake3::Hash,
) -> std::io::Result<()> {
    let path = path.as_ref();
    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink()
        || !metadata.file_type().is_file()
        || metadata.len() != expected_bytes
    {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("state WAL identity changed before replay: {path:?}"),
        ));
    }
    let mut file = File::open(path)?;
    let actual_hash = hash_file_range(&mut file, 0, expected_bytes)?;
    if actual_hash != expected_hash || file.metadata()?.len() != expected_bytes {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("state WAL content changed before replay: {path:?}"),
        ));
    }
    Ok(())
}

pub(crate) fn hash_wal_identity_and_prefix(
    path: impl AsRef<Path>,
    expected_bytes: u64,
    expected_hash: blake3::Hash,
    prefix_bytes: u64,
) -> std::io::Result<blake3::Hash> {
    let path = path.as_ref();
    if prefix_bytes > expected_bytes {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("WAL prefix length {prefix_bytes} exceeds expected size {expected_bytes}"),
        ));
    }
    let before_path = fs::symlink_metadata(path)?;
    if before_path.file_type().is_symlink() || !before_path.is_file() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("state WAL must be a regular non-symlink file: {path:?}"),
        ));
    }
    let before = WalFileIdentity::from_metadata(&before_path);
    if before.len != expected_bytes {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("state WAL size changed before hashing: {path:?}"),
        ));
    }
    let mut file = File::open(path)?;
    if WalFileIdentity::from_metadata(&file.metadata()?) != before {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("state WAL identity changed while opening: {path:?}"),
        ));
    }

    let mut full_hasher = blake3::Hasher::new();
    let mut prefix_hasher = blake3::Hasher::new();
    let mut full_remaining = expected_bytes;
    let mut prefix_remaining = prefix_bytes;
    let mut buffer = [0u8; 64 * 1024];
    while full_remaining != 0 {
        let limit =
            usize::try_from(full_remaining.min(buffer.len() as u64)).unwrap_or(buffer.len());
        let read = file.read(&mut buffer[..limit])?;
        if read == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "state WAL was truncated while hashing identity and prefix",
            ));
        }
        full_hasher.update(&buffer[..read]);
        let prefix_read = usize::try_from(prefix_remaining.min(read as u64)).unwrap_or(read);
        prefix_hasher.update(&buffer[..prefix_read]);
        prefix_remaining -= prefix_read as u64;
        full_remaining -= read as u64;
    }
    let after_handle = WalFileIdentity::from_metadata(&file.metadata()?);
    let after_path = fs::symlink_metadata(path)?;
    if after_path.file_type().is_symlink()
        || !after_path.is_file()
        || after_handle != before
        || WalFileIdentity::from_metadata(&after_path) != before
    {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("state WAL identity changed while hashing: {path:?}"),
        ));
    }
    if full_hasher.finalize() != expected_hash {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("state WAL content changed while hashing: {path:?}"),
        ));
    }
    Ok(prefix_hasher.finalize())
}

pub(crate) fn quarantine_and_truncate_wal_tail(
    path: impl AsRef<Path>,
    accepted_prefix_bytes: u64,
    original_bytes: u64,
    expected_original_hash: blake3::Hash,
) -> std::io::Result<PathBuf> {
    let path = path.as_ref();
    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() || !metadata.file_type().is_file() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("state WAL must be a regular non-symlink file: {path:?}"),
        ));
    }
    if metadata.len() != original_bytes || accepted_prefix_bytes >= original_bytes {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!(
                "state WAL repair boundary is stale or empty: accepted={accepted_prefix_bytes}, expected_original={original_bytes}, actual_original={}",
                metadata.len()
            ),
        ));
    }

    let tail_bytes = original_bytes - accepted_prefix_bytes;
    let mut source = OpenOptions::new().read(true).write(true).open(path)?;
    if hash_file_range(&mut source, 0, original_bytes)? != expected_original_hash
        || source.metadata()?.len() != original_bytes
    {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "state WAL changed after checkpoint validation and before quarantine",
        ));
    }
    let tail_hash = hash_file_range(&mut source, accepted_prefix_bytes, tail_bytes)?;
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("state.wal");
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let quarantine_path = parent.join(format!("{file_name}.quarantine-{}.bin", tail_hash.to_hex()));

    #[cfg(windows)]
    restore_exact_wal_namespace_rebarrier(&quarantine_path)?;

    if quarantine_path.exists() {
        verify_quarantine_file(&quarantine_path, tail_bytes, tail_hash)?;
        drop(arc_crypto::secret_file::open_private_append_owned_migration(&quarantine_path)?);
        let quarantine_file = arc_crypto::secret_file::open_private_read_write(&quarantine_path)?;
        quarantine_file.sync_all()?;
        drop(quarantine_file);
        #[cfg(unix)]
        {
            WalWriter::sync_parent_directory(&quarantine_path)?;
        }
        #[cfg(windows)]
        {
            let staging = wal_namespace_rebarrier_path(&quarantine_path)?;
            move_file_create_only_write_through(&quarantine_path, &staging)?;
            move_file_create_only_write_through(&staging, &quarantine_path)?;
            verify_quarantine_file(&quarantine_path, tail_bytes, tail_hash)?;
        }
    } else {
        let mut temporary = None;
        for serial in 0..100u32 {
            let candidate = parent.join(format!(
                ".{file_name}.quarantine-tmp-{}-{serial}",
                std::process::id()
            ));
            match arc_crypto::secret_file::create_new_private(&candidate) {
                Ok(file) => {
                    temporary = Some((candidate, file));
                    break;
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(error) => return Err(error),
            }
        }
        let Some((temporary_path, mut temporary_file)) = temporary else {
            return Err(std::io::Error::new(
                std::io::ErrorKind::AlreadyExists,
                "could not allocate a unique WAL quarantine temporary file",
            ));
        };
        if let Err(error) = copy_file_range(
            &mut source,
            &mut temporary_file,
            accepted_prefix_bytes,
            tail_bytes,
        )
        .and_then(|()| temporary_file.sync_all())
        {
            let _ = fs::remove_file(&temporary_path);
            return Err(error);
        }
        drop(temporary_file);

        match publish_wal_quarantine_create_only(&temporary_path, &quarantine_path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                let _ = fs::remove_file(&temporary_path);
                verify_quarantine_file(&quarantine_path, tail_bytes, tail_hash)?;
            }
            Err(error) => {
                let _ = fs::remove_file(&temporary_path);
                return Err(error);
            }
        }
        verify_quarantine_file(&quarantine_path, tail_bytes, tail_hash)?;
    }

    if source.metadata()?.len() != original_bytes
        || hash_file_range(&mut source, accepted_prefix_bytes, tail_bytes)? != tail_hash
    {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "state WAL changed after its rejected tail was quarantined",
        ));
    }
    source.set_len(accepted_prefix_bytes)?;
    source.sync_all()?;
    WalWriter::sync_parent_directory(path)?;
    Ok(quarantine_path)
}

pub fn read_wal(path: impl AsRef<Path>) -> Vec<WalEntry> {
    let file = match File::open(path.as_ref()) {
        Ok(f) => f,
        Err(_) => return Vec::new(),
    };
    let original_bytes = match file.metadata() {
        Ok(metadata) => metadata.len(),
        Err(_) => return Vec::new(),
    };
    let mut reader = BufReader::new(file);
    let mut entries = Vec::new();
    let mut offset = 0u64;

    while offset < original_bytes {
        let remaining = original_bytes.saturating_sub(offset);
        if remaining < 4 {
            tracing::warn!(offset, "Truncated WAL frame length, stopping replay");
            break;
        }
        let mut len_buf = [0u8; 4];
        if reader.read_exact(&mut len_buf).is_err() {
            break;
        }
        let len = u32::from_le_bytes(len_buf) as usize;
        if len == 0 || len > MAX_WAL_ENTRY_BYTES {
            tracing::warn!(len, offset, "Invalid WAL frame length, stopping replay");
            break;
        }
        let Some(frame_end) = offset
            .checked_add(4)
            .and_then(|value| value.checked_add(len as u64))
        else {
            tracing::warn!(offset, "WAL frame offset overflow, stopping replay");
            break;
        };
        if frame_end > original_bytes {
            tracing::warn!(offset, len, "Truncated WAL frame payload, stopping replay");
            break;
        }
        let mut data = match allocate_wal_frame(len) {
            Ok(data) => data,
            Err(error) => {
                tracing::warn!(%error, offset, "Cannot allocate WAL frame, stopping replay");
                break;
            }
        };
        if reader.read_exact(&mut data).is_err() {
            break;
        }
        offset = frame_end;

        match bincode::deserialize_limited_exact::<WalEntry, MAX_WAL_ENTRY_BYTES>(&data) {
            Ok(entry) => {
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
                    break;
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

pub fn read_wal_strict(path: impl AsRef<Path>) -> std::io::Result<Vec<WalEntry>> {
    let mut expected_sequence = Some(0u64);
    read_wal_strict_segment(path.as_ref(), &mut expected_sequence)
}

fn read_wal_strict_segment(
    path: &Path,
    expected_sequence: &mut Option<u64>,
) -> std::io::Result<Vec<WalEntry>> {
    let file = arc_crypto::secret_file::open_owned_nofollow_read(path)?;
    read_wal_strict_segment_from_file(file, expected_sequence)
}

#[cfg(windows)]
fn read_wal_strict_live_writer_segment(
    path: &Path,
    expected_sequence: &mut Option<u64>,
) -> std::io::Result<Vec<WalEntry>> {
    let file = arc_crypto::secret_file::open_owned_nofollow_shared_read(path)?;
    read_wal_strict_segment_from_file(file, expected_sequence)
}

fn read_wal_strict_segment_from_file(
    file: File,
    expected_sequence: &mut Option<u64>,
) -> std::io::Result<Vec<WalEntry>> {
    let original_bytes = file.metadata()?.len();
    let mut reader = BufReader::new(file);
    let mut entries = Vec::new();
    let mut offset = 0u64;

    while offset < original_bytes {
        let remaining = original_bytes.checked_sub(offset).ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "WAL byte offset exceeds file size",
            )
        })?;
        if remaining < 4 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("truncated WAL frame length at byte {offset}"),
            ));
        }
        let mut len_buf = [0u8; 4];
        reader.read_exact(&mut len_buf)?;
        let len = u32::from_le_bytes(len_buf) as usize;
        if len == 0 || len > MAX_WAL_ENTRY_BYTES {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("invalid WAL frame length {len}"),
            ));
        }
        let frame_end = offset
            .checked_add(4)
            .and_then(|value| value.checked_add(len as u64))
            .ok_or_else(|| {
                std::io::Error::new(std::io::ErrorKind::InvalidData, "WAL byte offset overflow")
            })?;
        if frame_end > original_bytes {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("truncated WAL frame payload at byte {offset}"),
            ));
        }
        let mut data = allocate_wal_frame(len)?;
        reader.read_exact(&mut data).map_err(|error| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("truncated WAL frame payload: {error}"),
            )
        })?;
        let entry: WalEntry = bincode::deserialize_limited_exact::<WalEntry, MAX_WAL_ENTRY_BYTES>(
            &data,
        )
        .map_err(|error| {
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
        let expected = expected_sequence.get_or_insert(entry.sequence);
        if entry.sequence != *expected {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!(
                    "WAL sequence gap: expected {}, got {}",
                    *expected, entry.sequence
                ),
            ));
        }
        *expected = expected.checked_add(1).ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::InvalidData, "WAL sequence overflow")
        })?;
        entries.push(entry);
        offset = frame_end;
    }
    Ok(entries)
}

pub fn read_wal_dir(dir: impl AsRef<Path>) -> Vec<WalEntry> {
    let segments = WalWriter::list_segments(dir.as_ref());
    let mut all_entries = Vec::new();
    for seg_path in segments {
        all_entries.extend(read_wal(&seg_path));
    }
    all_entries
}

pub fn read_wal_dir_strict(dir: impl AsRef<Path>) -> std::io::Result<Vec<WalEntry>> {
    let segments = WalWriter::list_segments_strict(dir.as_ref())?;
    WalWriter::validate_segment_namespace(&segments)?;
    let first_segment = segments
        .first()
        .and_then(|segment| WalWriter::parse_segment_number(segment))
        .unwrap_or(0);
    let mut expected_segment = first_segment;
    let mut expected_sequence = (first_segment == 0).then_some(0);
    let mut all_entries = Vec::new();
    for segment in segments {
        let actual_segment = WalWriter::parse_segment_number(&segment).ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("invalid WAL segment name: {}", segment.display()),
            )
        })?;
        if actual_segment != expected_segment {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("WAL segment gap: expected {expected_segment:08}, got {actual_segment:08}"),
            ));
        }
        expected_segment = expected_segment.checked_add(1).ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "WAL segment number overflow",
            )
        })?;
        all_entries.extend(read_wal_strict_segment(&segment, &mut expected_sequence)?);
    }
    Ok(all_entries)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct StrictWalTailInspection {
    pub first_segment: u64,
    pub last_segment: u64,
    pub segment_count: u64,
    pub inspected_first_segment: u64,
    pub inspected_segment_count: u64,
    pub inspected_entry_count: u64,
    pub source_consensus_round: u64,
}

pub fn inspect_wal_dir_tail_strict(
    dir: impl AsRef<Path>,
) -> std::io::Result<StrictWalTailInspection> {
    const MAX_SEGMENTS_TO_SCAN: usize = 3;

    let segments = WalWriter::list_segments_strict(dir.as_ref())?;
    let first_segment = WalWriter::validate_segment_namespace(&segments)?;
    let segment_count = u64::try_from(segments.len()).map_err(|_| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "WAL segment count exceeds u64",
        )
    })?;
    if segments.is_empty() {
        return Ok(StrictWalTailInspection {
            first_segment: 0,
            last_segment: 0,
            segment_count: 0,
            inspected_first_segment: 0,
            inspected_segment_count: 0,
            inspected_entry_count: 0,
            source_consensus_round: 0,
        });
    }

    let start = segments.len().saturating_sub(MAX_SEGMENTS_TO_SCAN);
    let inspected = &segments[start..];
    let inspected_first_segment =
        WalWriter::parse_segment_number(&inspected[0]).ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("invalid WAL segment name: {}", inspected[0].display()),
            )
        })?;
    let last_segment = WalWriter::parse_segment_number(segments.last().expect("non-empty"))
        .ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "final WAL segment name is malformed",
            )
        })?;
    let mut expected_sequence = (inspected_first_segment == 0).then_some(0);
    let mut inspected_entry_count = 0u64;
    let mut source_consensus_round = 0u64;
    for segment in inspected {
        let entries = read_wal_strict_segment(segment, &mut expected_sequence)?;
        inspected_entry_count = inspected_entry_count
            .checked_add(u64::try_from(entries.len()).map_err(|_| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "WAL tail entry count exceeds u64",
                )
            })?)
            .ok_or_else(|| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "WAL tail entry count overflows u64",
                )
            })?;
        source_consensus_round = entries
            .iter()
            .map(|entry| entry.block_height)
            .max()
            .unwrap_or(source_consensus_round)
            .max(source_consensus_round);
    }
    if segment_count > MAX_SEGMENTS_TO_SCAN as u64 && inspected_entry_count == 0 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "latest three WAL segments are all empty",
        ));
    }

    Ok(StrictWalTailInspection {
        first_segment,
        last_segment,
        segment_count,
        inspected_first_segment,
        inspected_segment_count: u64::try_from(inspected.len()).map_err(|_| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "inspected WAL segment count exceeds u64",
            )
        })?,
        inspected_entry_count,
        source_consensus_round,
    })
}

pub fn latest_block_height_in_wal_dir(dir: impl AsRef<Path>) -> u64 {
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

pub fn read_wal_from(path: impl AsRef<Path>, from_sequence: u64) -> Vec<WalEntry> {
    read_wal(path)
        .into_iter()
        .filter(|e| e.sequence >= from_sequence)
        .collect()
}

pub fn find_last_checkpoint(path: impl AsRef<Path>) -> Option<(u64, Hash256)> {
    let entries = read_wal(path);
    entries.iter().rev().find_map(|e| match &e.op {
        WalOp::Checkpoint(root) => Some((e.sequence, *root)),
        _ => None,
    })
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Snapshot {
    pub block_height: u64,
    pub state_root: Hash256,
    pub wal_sequence: u64,
    pub accounts: Vec<(Address, Account)>,
    pub storage: ContractStorage,
    pub contracts: Vec<(Address, Vec<u8>)>,
}

impl Snapshot {
    pub fn write_to(&self, path: impl AsRef<Path>) -> std::io::Result<()> {
        let data = bincode::serialize(self).map_err(std::io::Error::other)?;
        let compressed = lz4_flex::compress_prepend_size(&data);

        let mut file = File::create(path)?;
        file.write_all(&compressed)?;
        file.sync_all()?;
        Ok(())
    }

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

pub struct PersistenceConfig {
    pub snapshot_interval: u64,
    pub wal_path: PathBuf,
    pub snapshot_dir: PathBuf,
    pub max_wal_segment_size: u64,
    pub wal_retention_segments: u32,
}

impl Default for PersistenceConfig {
    fn default() -> Self {
        Self {
            snapshot_interval: 10_000,
            wal_path: PathBuf::from("data/wal.bin"),
            snapshot_dir: PathBuf::from("data/snapshots"),
            max_wal_segment_size: 268_435_456,
            wal_retention_segments: 2,
        }
    }
}

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

    fn checkpoint_entry(sequence: u64) -> WalEntry {
        let block_height = sequence;
        let op = WalOp::Checkpoint(hash_bytes(&sequence.to_le_bytes()));
        let payload = bincode::serialize(&(&block_height, &sequence, &op)).unwrap();
        WalEntry {
            block_height,
            sequence,
            op,
            checksum: crc32fast::hash(&payload),
        }
    }

    fn append_encoded_entry(path: &Path, entry: &WalEntry) {
        let data = bincode::serialize(entry).unwrap();
        let length = u32::try_from(data.len()).unwrap();
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .unwrap();
        file.write_all(&length.to_le_bytes()).unwrap();
        file.write_all(&data).unwrap();
    }

    #[test]
    fn wal_identity_helper_hashes_requested_prefix_only_after_full_match() {
        let path = tmp_path("identity-prefix-hash");
        let bytes = (0..200_000)
            .map(|index| (index % 251) as u8)
            .collect::<Vec<_>>();
        fs::write(&path, &bytes).unwrap();
        let full_hash = blake3::hash(&bytes);
        let prefix =
            hash_wal_identity_and_prefix(&path, bytes.len() as u64, full_hash, 65_537).unwrap();
        assert_eq!(prefix, blake3::hash(&bytes[..65_537]));
        assert_eq!(
            hash_wal_identity_and_prefix(&path, bytes.len() as u64, blake3::hash(b"wrong"), 0)
                .unwrap_err()
                .kind(),
            std::io::ErrorKind::InvalidData
        );
        assert_eq!(
            hash_wal_identity_and_prefix(
                &path,
                bytes.len() as u64,
                full_hash,
                bytes.len() as u64 + 1
            )
            .unwrap_err()
            .kind(),
            std::io::ErrorKind::InvalidInput
        );
    }

    #[test]
    fn streaming_repairable_reader_matches_collecting_reader() {
        let path = tmp_path("streaming-repairable-reader");
        let _ = fs::remove_file(&path);
        append_encoded_entry(&path, &checkpoint_entry(0));
        append_encoded_entry(&path, &checkpoint_entry(1));
        OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all(&[1])
            .unwrap();

        let collected = read_repairable_wal_prefix(&path).unwrap();
        let mut visited = Vec::new();
        let summary = visit_repairable_wal_prefix(&path, |entry, end| {
            visited.push((bincode::serialize(&entry).unwrap(), end));
            Ok(())
        })
        .unwrap();
        assert_eq!(summary.entry_count, collected.entries.len() as u64);
        assert_eq!(summary.original_bytes, collected.original_bytes);
        assert_eq!(summary.original_hash, collected.original_hash);
        assert_eq!(summary.torn_tail, collected.torn_tail);
        assert_eq!(
            visited,
            collected
                .entries
                .iter()
                .zip(&collected.frame_end_offsets)
                .map(|(entry, end)| (bincode::serialize(entry).unwrap(), *end))
                .collect::<Vec<_>>()
        );

        let malformed = tmp_path("streaming-repairable-malformed");
        let _ = fs::remove_file(&malformed);
        append_encoded_entry(&malformed, &checkpoint_entry(0));
        let mut bytes = fs::read(&malformed).unwrap();
        *bytes.last_mut().unwrap() ^= 1;
        fs::write(&malformed, bytes).unwrap();
        let mut callbacks = 0;
        assert!(read_repairable_wal_prefix(&malformed).is_err());
        assert!(
            visit_repairable_wal_prefix(&malformed, |_, _| {
                callbacks += 1;
                Ok(())
            })
            .is_err()
        );
        assert_eq!(callbacks, 0, "malformed frame must not reach the visitor");
    }

    fn assert_new_rejects_without_changing_bytes(path: &Path, expected: &str) {
        let before = fs::read(path).unwrap();
        let error = match WalWriter::new(path) {
            Ok(_) => panic!("corrupt WAL unexpectedly opened"),
            Err(error) => error,
        };
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
        assert!(
            error.to_string().contains(expected),
            "expected {expected:?} in {error}"
        );
        assert_eq!(fs::read(path).unwrap(), before);
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
    fn concurrent_append_sequences_match_physical_order() {
        const THREADS: usize = 12;
        const ENTRIES_PER_THREAD: usize = 128;

        let path = tmp_path("wal_concurrent_sequence_order.bin");
        let _ = fs::remove_file(&path);
        let writer = Arc::new(WalWriter::new(&path).unwrap());
        let start = Arc::new(std::sync::Barrier::new(THREADS + 1));
        let mut handles = Vec::new();
        for worker in 0..THREADS {
            let writer = Arc::clone(&writer);
            let start = Arc::clone(&start);
            handles.push(thread::spawn(move || {
                start.wait();
                for item in 0..ENTRIES_PER_THREAD {
                    let mut identity = Vec::with_capacity(16);
                    identity.extend_from_slice(&(worker as u64).to_le_bytes());
                    identity.extend_from_slice(&(item as u64).to_le_bytes());
                    writer.append(WalOp::Checkpoint(hash_bytes(&identity)), item as u64);
                }
            }));
        }
        start.wait();
        for handle in handles {
            handle.join().unwrap();
        }
        writer.sync().unwrap();
        assert_eq!(writer.sequence(), (THREADS * ENTRIES_PER_THREAD) as u64);
        drop(writer);

        let entries = read_wal_strict(&path).unwrap();
        assert_eq!(entries.len(), THREADS * ENTRIES_PER_THREAD);
        for (physical_index, entry) in entries.iter().enumerate() {
            assert_eq!(entry.sequence, physical_index as u64);
        }
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn sync_cannot_overtake_an_admitted_append() {
        let path = tmp_path("wal_sync_admission_barrier.bin");
        let _ = fs::remove_file(&path);
        let writer = Arc::new(WalWriter::new(&path).unwrap());
        let (admitted_tx, admitted_rx) = channel::bounded(1);
        let (release_tx, release_rx) = channel::bounded(1);

        let append_writer = Arc::clone(&writer);
        let append = thread::spawn(move || {
            append_writer.append_with_admission_hook(
                WalOp::Checkpoint(hash_bytes(b"admitted-before-sync")),
                1,
                || {
                    admitted_tx.send(()).unwrap();
                    release_rx.recv().unwrap();
                },
            );
        });
        admitted_rx.recv().unwrap();

        let (sync_started_tx, sync_started_rx) = channel::bounded(1);
        let (sync_done_tx, sync_done_rx) = channel::bounded(1);
        let sync_writer = Arc::clone(&writer);
        let sync = thread::spawn(move || {
            sync_started_tx.send(()).unwrap();
            sync_done_tx.send(sync_writer.sync()).unwrap();
        });
        sync_started_rx.recv().unwrap();
        assert!(
            sync_done_rx
                .recv_timeout(std::time::Duration::from_millis(50))
                .is_err(),
            "sync passed an append holding the admission gate"
        );

        release_tx.send(()).unwrap();
        append.join().unwrap();
        sync_done_rx.recv().unwrap().unwrap();
        sync.join().unwrap();
        drop(writer);

        let entries = read_wal_strict(&path).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].sequence, 0);
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn reopen_continues_at_last_valid_sequence() {
        let path = tmp_path("wal_reopen_sequence.bin");
        let _ = fs::remove_file(&path);
        {
            let writer = WalWriter::new(&path).unwrap();
            writer.append(WalOp::Checkpoint(hash_bytes(b"zero")), 0);
            writer.append(WalOp::Checkpoint(hash_bytes(b"one")), 1);
            writer.sync().unwrap();
        }
        {
            let writer = WalWriter::new(&path).unwrap();
            assert_eq!(writer.sequence(), 2);
            writer.append(WalOp::Checkpoint(hash_bytes(b"two")), 2);
            writer.sync().unwrap();
        }

        let entries = read_wal_strict(&path).unwrap();
        assert_eq!(
            entries
                .iter()
                .map(|entry| entry.sequence)
                .collect::<Vec<_>>(),
            vec![0, 1, 2]
        );
        fs::remove_file(path).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn retained_append_handle_rejects_a_same_owner_path_replacement() {
        let dir = tmp_dir("wal_append_handle_path_replacement");
        let path = dir.join("state.wal");
        append_encoded_entry(&path, &checkpoint_entry(0));
        let append_file = open_or_create_append_file_durably(&path).unwrap();

        let displaced = dir.join("displaced.wal");
        fs::rename(&path, &displaced).unwrap();
        append_encoded_entry(&path, &checkpoint_entry(0));

        let mut expected_sequence = Some(0);
        let error = WalWriter::validate_existing_segment_handle(
            &path,
            &append_file,
            &mut expected_sequence,
        )
        .unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
        assert!(error.to_string().contains("changed while being opened"));
        drop(append_file);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn reopen_rejects_zero_length_frame_without_mutating_wal() {
        let path = tmp_path("wal_reopen_zero_length.bin");
        let _ = fs::remove_file(&path);
        append_encoded_entry(&path, &checkpoint_entry(0));
        OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all(&0u32.to_le_bytes())
            .unwrap();

        assert_eq!(read_wal(&path).len(), 1);
        assert_new_rejects_without_changing_bytes(&path, "invalid WAL frame length 0");
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn reopen_rejects_oversized_length_before_payload_allocation() {
        let path = tmp_path("wal_reopen_oversized_length.bin");
        let _ = fs::remove_file(&path);
        fs::write(&path, u32::MAX.to_le_bytes()).unwrap();

        assert!(read_wal(&path).is_empty());
        assert_new_rejects_without_changing_bytes(&path, "invalid WAL frame length");
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn reopen_rejects_truncated_payload_before_declared_allocation() {
        let path = tmp_path("wal_reopen_truncated_payload.bin");
        let _ = fs::remove_file(&path);
        let mut bytes = 16_000_000u32.to_le_bytes().to_vec();
        bytes.push(0xaa);
        fs::write(&path, bytes).unwrap();

        assert!(read_wal(&path).is_empty());
        assert_new_rejects_without_changing_bytes(&path, "truncated WAL frame payload");
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn reopen_rejects_checksum_corruption_without_mutating_wal() {
        let path = tmp_path("wal_reopen_bad_checksum.bin");
        let _ = fs::remove_file(&path);
        append_encoded_entry(&path, &checkpoint_entry(0));
        let mut bytes = fs::read(&path).unwrap();
        let final_byte = bytes.last_mut().unwrap();
        *final_byte ^= 0x80;
        fs::write(&path, bytes).unwrap();

        assert_new_rejects_without_changing_bytes(&path, "WAL checksum mismatch");
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn reopen_rejects_swapped_sequences_without_mutating_wal() {
        let path = tmp_path("wal_reopen_swapped_sequences.bin");
        let _ = fs::remove_file(&path);
        append_encoded_entry(&path, &checkpoint_entry(0));
        append_encoded_entry(&path, &checkpoint_entry(2));
        append_encoded_entry(&path, &checkpoint_entry(1));

        assert_new_rejects_without_changing_bytes(&path, "expected 1, got 2");
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn segmented_reopen_rejects_a_missing_segment() {
        let dir = tmp_dir("wal_reopen_missing_segment");
        append_encoded_entry(&dir.join("wal-00000000.bin"), &checkpoint_entry(0));
        append_encoded_entry(&dir.join("wal-00000002.bin"), &checkpoint_entry(1));
        let before_zero = fs::read(dir.join("wal-00000000.bin")).unwrap();
        let before_two = fs::read(dir.join("wal-00000002.bin")).unwrap();

        let error = match WalWriter::with_segments(&dir, u64::MAX) {
            Ok(_) => panic!("segmented WAL gap unexpectedly opened"),
            Err(error) => error,
        };
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
        assert!(error.to_string().contains("WAL segment gap"));
        let read_error = read_wal_dir_strict(&dir).unwrap_err();
        assert_eq!(read_error.kind(), std::io::ErrorKind::InvalidData);
        assert!(read_error.to_string().contains("WAL segment gap"));
        assert_eq!(fs::read(dir.join("wal-00000000.bin")).unwrap(), before_zero);
        assert_eq!(fs::read(dir.join("wal-00000002.bin")).unwrap(), before_two);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn segment_zero_cannot_claim_a_prefix_pruned_sequence() {
        let dir = tmp_dir("wal_segment_zero_nonzero_sequence");
        let segment = dir.join("wal-00000000.bin");
        append_encoded_entry(&segment, &checkpoint_entry(10));
        let before = fs::read(&segment).unwrap();

        let read_error = read_wal_dir_strict(&dir).unwrap_err();
        assert_eq!(read_error.kind(), std::io::ErrorKind::InvalidData);
        assert!(read_error.to_string().contains("expected 0, got 10"));

        let open_error = match WalWriter::with_segments(&dir, u64::MAX) {
            Ok(_) => panic!("segment zero with nonzero first sequence unexpectedly opened"),
            Err(error) => error,
        };
        assert_eq!(open_error.kind(), std::io::ErrorKind::InvalidData);
        assert!(open_error.to_string().contains("expected 0, got 10"));
        assert_eq!(fs::read(&segment).unwrap(), before);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn segmented_reopen_rejects_noncanonical_segment_names() {
        for (case, name) in ["wal-1.bin", "wal-100000000.bin"].into_iter().enumerate() {
            let dir = tmp_dir(&format!("wal_noncanonical_segment_name_{case}"));
            let segment = dir.join(name);
            append_encoded_entry(&segment, &checkpoint_entry(0));

            let read_error = read_wal_dir_strict(&dir).unwrap_err();
            assert_eq!(read_error.kind(), std::io::ErrorKind::InvalidData);
            assert!(read_error.to_string().contains("invalid WAL segment name"));

            let open_error = match WalWriter::with_segments(&dir, u64::MAX) {
                Ok(_) => panic!("noncanonical WAL segment name unexpectedly opened"),
                Err(error) => error,
            };
            assert_eq!(open_error.kind(), std::io::ErrorKind::InvalidData);
            assert!(open_error.to_string().contains("invalid WAL segment name"));
            assert_eq!(WalWriter::list_segments(&dir), vec![segment]);
            fs::remove_dir_all(dir).unwrap();
        }
    }

    #[test]
    fn strict_segment_discovery_propagates_directory_open_errors() {
        let not_a_directory = tmp_path("wal_segment_listing_not_a_directory");
        fs::write(&not_a_directory, b"not a WAL directory").unwrap();

        let error = read_wal_dir_strict(&not_a_directory).unwrap_err();
        assert!(matches!(
            error.kind(),
            std::io::ErrorKind::NotADirectory | std::io::ErrorKind::Other
        ));

        fs::remove_file(not_a_directory).unwrap();
    }

    #[test]
    fn rotate_cannot_overtake_an_admitted_append() {
        let dir = tmp_dir("wal_rotate_admission_barrier");
        let writer = Arc::new(WalWriter::with_segments(&dir, u64::MAX).unwrap());
        let (admitted_tx, admitted_rx) = channel::bounded(1);
        let (release_tx, release_rx) = channel::bounded(1);

        let append_writer = Arc::clone(&writer);
        let append = thread::spawn(move || {
            append_writer.append_with_admission_hook(
                WalOp::Checkpoint(hash_bytes(b"before-rotation")),
                1,
                || {
                    admitted_tx.send(()).unwrap();
                    release_rx.recv().unwrap();
                },
            );
        });
        admitted_rx.recv().unwrap();

        let (rotate_started_tx, rotate_started_rx) = channel::bounded(1);
        let (rotate_done_tx, rotate_done_rx) = channel::bounded(1);
        let rotate_writer = Arc::clone(&writer);
        let rotate = thread::spawn(move || {
            rotate_started_tx.send(()).unwrap();
            rotate_done_tx.send(rotate_writer.rotate()).unwrap();
        });
        rotate_started_rx.recv().unwrap();
        assert!(
            rotate_done_rx
                .recv_timeout(std::time::Duration::from_millis(50))
                .is_err(),
            "rotation passed an append holding the admission gate"
        );

        release_tx.send(()).unwrap();
        append.join().unwrap();
        rotate_done_rx.recv().unwrap().unwrap();
        rotate.join().unwrap();
        writer.append(WalOp::Checkpoint(hash_bytes(b"after-rotation")), 2);
        writer.sync().unwrap();
        drop(writer);

        let first = read_wal(dir.join("wal-00000000.bin"));
        let second = read_wal(dir.join("wal-00000001.bin"));
        assert_eq!(first.len(), 1);
        assert_eq!(first[0].sequence, 0);
        assert_eq!(second.len(), 1);
        assert_eq!(second[0].sequence, 1);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn live_segment_pruning_fences_append_and_rotation() {
        let dir = tmp_dir("wal_live_pruning_admission_barrier");
        let writer = Arc::new(WalWriter::with_segments(&dir, u64::MAX).unwrap());
        for segment in 0..4u64 {
            for item in 0..5u64 {
                let sequence = segment * 5 + item;
                writer.append(
                    WalOp::Checkpoint(hash_bytes(&sequence.to_le_bytes())),
                    segment,
                );
            }
            writer.sync().unwrap();
            if segment != 3 {
                writer.rotate().unwrap();
            }
        }

        let (prune_synchronized_tx, prune_synchronized_rx) = channel::bounded(1);
        let (release_prune_tx, release_prune_rx) = channel::bounded(1);
        let prune_writer = Arc::clone(&writer);
        let prune = thread::spawn(move || {
            prune_writer.delete_segments_before_with_admission_hook(10, || {
                prune_synchronized_tx.send(()).unwrap();
                release_prune_rx.recv().unwrap();
            })
        });
        prune_synchronized_rx.recv().unwrap();

        let (append_started_tx, append_started_rx) = channel::bounded(1);
        let (append_done_tx, append_done_rx) = channel::bounded(1);
        let append_writer = Arc::clone(&writer);
        let append = thread::spawn(move || {
            append_started_tx.send(()).unwrap();
            append_writer.append(WalOp::Checkpoint(hash_bytes(b"after-pruning")), 4);
            append_done_tx.send(()).unwrap();
        });

        let (rotate_started_tx, rotate_started_rx) = channel::bounded(1);
        let (rotate_done_tx, rotate_done_rx) = channel::bounded(1);
        let rotate_writer = Arc::clone(&writer);
        let rotate = thread::spawn(move || {
            rotate_started_tx.send(()).unwrap();
            rotate_done_tx.send(rotate_writer.rotate()).unwrap();
        });

        append_started_rx.recv().unwrap();
        rotate_started_rx.recv().unwrap();
        assert!(
            append_done_rx
                .recv_timeout(std::time::Duration::from_millis(50))
                .is_err(),
            "append passed live pruning while it held the admission gate"
        );
        assert!(
            rotate_done_rx
                .recv_timeout(std::time::Duration::from_millis(50))
                .is_err(),
            "rotation passed live pruning while it held the admission gate"
        );

        release_prune_tx.send(()).unwrap();
        assert_eq!(prune.join().unwrap().unwrap(), 2);
        append_done_rx.recv().unwrap();
        rotate_done_rx.recv().unwrap().unwrap();
        append.join().unwrap();
        rotate.join().unwrap();
        writer.sync().unwrap();
        drop(writer);

        let entries = read_wal_dir_strict(&dir).unwrap();
        assert_eq!(
            entries
                .iter()
                .map(|entry| entry.sequence)
                .collect::<Vec<_>>(),
            (10..=20).collect::<Vec<_>>()
        );
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn shutdown_drains_admitted_entries_and_closes_admission() {
        let path = tmp_path("wal_shutdown_admission.bin");
        let _ = fs::remove_file(&path);
        let mut writer = WalWriter::new(&path).unwrap();
        for sequence in 0..128u64 {
            writer.append(
                WalOp::Checkpoint(hash_bytes(&sequence.to_le_bytes())),
                sequence,
            );
        }
        writer.shutdown().unwrap();
        let sequence_after_shutdown = writer.sequence();
        let bytes_after_shutdown = fs::read(&path).unwrap();

        writer.append(WalOp::Checkpoint(hash_bytes(b"must-not-append")), 129);
        assert_eq!(writer.sequence(), sequence_after_shutdown);
        assert_eq!(fs::read(&path).unwrap(), bytes_after_shutdown);
        assert!(writer.check_health().is_err());

        let entries = read_wal_strict(&path).unwrap();
        assert_eq!(entries.len(), 128);
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn segmented_reopen_preserves_sequence_after_prefix_pruning() {
        let dir = tmp_dir("wal_reopen_after_prefix_pruning");
        {
            let writer = WalWriter::with_segments(&dir, u64::MAX).unwrap();
            for segment in 0..4u64 {
                for item in 0..5u64 {
                    let sequence = segment * 5 + item;
                    writer.append(
                        WalOp::Checkpoint(hash_bytes(&sequence.to_le_bytes())),
                        segment,
                    );
                }
                writer.sync().unwrap();
                if segment != 3 {
                    writer.rotate().unwrap();
                }
            }
        }
        assert_eq!(
            WalWriter::delete_segments_before_in_dir(&dir, 10, 2).unwrap(),
            2
        );
        assert!(!dir.join("wal-00000000.bin").exists());
        assert!(!dir.join("wal-00000001.bin").exists());

        {
            let writer = WalWriter::with_segments(&dir, u64::MAX).unwrap();
            assert_eq!(writer.sequence(), 20);
            writer.append(WalOp::Checkpoint(hash_bytes(b"continued")), 4);
            writer.sync().unwrap();
        }
        let entries = read_wal_dir_strict(&dir).unwrap();
        assert_eq!(entries.len(), 11);
        assert_eq!(entries.first().unwrap().sequence, 10);
        assert_eq!(entries.last().unwrap().sequence, 20);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn existing_late_visible_wal_is_tightened_rebarriered_and_appended() {
        let dir = tmp_dir("existing_late_visible_wal");
        let path = dir.join("state.wal");
        let file = arc_crypto::secret_file::create_new_private(&path).unwrap();
        file.sync_all().unwrap();
        drop(file);

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        }

        let writer = WalWriter::new(&path).unwrap();
        writer.append(WalOp::Checkpoint(hash_bytes(b"retry")), 1);
        writer.sync().unwrap();
        drop(writer);
        assert_eq!(read_wal_strict(&path).unwrap().len(), 1);

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            assert_eq!(
                fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        fs::remove_dir_all(dir).unwrap();
    }

    #[cfg(windows)]
    #[test]
    fn live_wal_shared_read_is_narrow_and_keeps_writer_exclusive() {
        use windows_sys::Win32::Foundation::ERROR_SHARING_VIOLATION;

        let dir = tmp_dir("windows_live_shared_read");
        let path = dir.join("state.wal");

        let writer = WalWriter::new(&path).unwrap();
        writer.append(WalOp::Checkpoint(hash_bytes(b"first")), 1);
        writer.sync().unwrap();

        let exclusive_error = read_wal_strict(&path).unwrap_err();
        assert_eq!(
            exclusive_error.raw_os_error(),
            Some(ERROR_SHARING_VIOLATION as i32)
        );

        let mut expected_sequence = Some(0);
        let live_entries =
            read_wal_strict_live_writer_segment(&path, &mut expected_sequence).unwrap();
        assert_eq!(live_entries.len(), 1);
        assert_eq!(live_entries[0].sequence, 0);

        let second = WalWriter::new(&path);
        assert!(
            second.is_err(),
            "the compatible shared reader must not admit a second writer"
        );
        drop(writer);

        let reopened = WalWriter::new(&path).unwrap();
        assert_eq!(reopened.sequence(), 1);
        reopened.append(WalOp::Checkpoint(hash_bytes(b"second")), 2);
        reopened.sync().unwrap();
        drop(reopened);

        let entries = read_wal_strict(&path).unwrap();
        assert_eq!(
            entries
                .iter()
                .map(|entry| entry.sequence)
                .collect::<Vec<_>>(),
            vec![0, 1]
        );
        fs::remove_dir_all(dir).unwrap();
    }

    #[cfg(windows)]
    #[test]
    fn segmented_wal_identity_probe_preserves_strict_sequence_on_reopen() {
        let dir = tmp_dir("windows_segmented_identity_probe");

        let writer = WalWriter::with_segments(&dir, u64::MAX).unwrap();
        writer.append(WalOp::Checkpoint(hash_bytes(b"first")), 1);
        writer.sync().unwrap();
        writer.rotate().unwrap();
        writer.append(WalOp::Checkpoint(hash_bytes(b"second")), 2);
        writer.sync().unwrap();
        assert!(
            WalWriter::with_segments(&dir, u64::MAX).is_err(),
            "the compatible identity probe must not admit a second segmented writer"
        );
        drop(writer);

        let reopened = WalWriter::with_segments(&dir, u64::MAX).unwrap();
        assert_eq!(reopened.sequence(), 2);
        reopened.append(WalOp::Checkpoint(hash_bytes(b"third")), 3);
        reopened.sync().unwrap();
        drop(reopened);

        let entries = read_wal_dir_strict(&dir).unwrap();
        assert_eq!(
            entries
                .iter()
                .map(|entry| entry.sequence)
                .collect::<Vec<_>>(),
            vec![0, 1, 2]
        );
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn startup_restores_only_exact_interrupted_wal_namespace_rebarriers() {
        let dir = tmp_dir("wal-namespace-rebarrier-restore");
        let staging = dir.join(".state.wal.namespace-rebarrier");
        fs::write(&staging, b"preserved WAL bytes").unwrap();
        let unrelated = dir.join(".operator.namespace-rebarrier");
        fs::write(&unrelated, b"operator bytes").unwrap();

        restore_interrupted_wal_namespace_rebarriers(&dir).unwrap();
        assert!(!staging.exists());
        assert_eq!(
            fs::read(dir.join("state.wal")).unwrap(),
            b"preserved WAL bytes"
        );
        assert_eq!(fs::read(&unrelated).unwrap(), b"operator bytes");

        fs::write(&staging, b"ambiguous WAL bytes").unwrap();
        assert!(restore_interrupted_wal_namespace_rebarriers(&dir).is_err());
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn existing_quarantine_retry_uses_a_write_capable_private_barrier() {
        let dir = tmp_dir("existing-quarantine-retry");
        let path = dir.join("state.wal");
        let accepted = b"accepted-prefix";
        let rejected = b"rejected-tail";
        let mut original = accepted.to_vec();
        original.extend_from_slice(rejected);
        fs::write(&path, &original).unwrap();

        let tail_hash = blake3::hash(rejected);
        let quarantine = dir.join(format!("state.wal.quarantine-{}.bin", tail_hash.to_hex()));
        fs::write(&quarantine, rejected).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            fs::set_permissions(&quarantine, fs::Permissions::from_mode(0o644)).unwrap();
        }

        let recovered = quarantine_and_truncate_wal_tail(
            &path,
            accepted.len() as u64,
            original.len() as u64,
            blake3::hash(&original),
        )
        .unwrap();
        assert_eq!(recovered, quarantine);
        assert_eq!(fs::read(&path).unwrap(), accepted);
        assert_eq!(fs::read(&quarantine).unwrap(), rejected);
        drop(arc_crypto::secret_file::open_private(&quarantine).unwrap());

        fs::remove_dir_all(dir).unwrap();
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
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].sequence, 3);
        assert_eq!(entries[1].sequence, 4);

        let _ = fs::remove_file(&path);
    }

    #[test]
    fn wal_null_writer() {
        let writer = WalWriter::null();
        assert_eq!(writer.directory(), None);
        writer.append(WalOp::Checkpoint(Hash256::ZERO), 0);
        writer.sync().unwrap();
    }

    #[test]
    fn wal_writer_reports_its_persistence_directory() {
        let wal_dir = tmp_dir("persistence-directory");
        let writer = WalWriter::new(wal_dir.join("state.wal")).unwrap();
        assert_eq!(writer.directory(), Some(wal_dir.as_path()));
        drop(writer);
        std::fs::remove_dir_all(wal_dir).unwrap();
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

    #[test]
    fn wal_rotation_creates_new_segments() {
        let dir = tmp_dir("wal_rotation_segments");

        {
            let writer = WalWriter::with_segments(&dir, 1024 * 1024).expect("create segmented wal");

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

            writer.append(WalOp::Checkpoint(hash_bytes(b"root")), 2);
            writer.sync().unwrap();
        }

        let segments = WalWriter::list_segments(&dir);
        assert!(
            segments.len() >= 3,
            "expected at least 3 segments, got {}",
            segments.len()
        );

        let seg0 = dir.join("wal-00000000.bin");
        let entries0 = read_wal(&seg0);
        assert_eq!(entries0.len(), 5);

        let seg1 = dir.join("wal-00000001.bin");
        let entries1 = read_wal(&seg1);
        assert_eq!(entries1.len(), 5);

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

        let segments_before = WalWriter::list_segments(&dir);
        assert_eq!(segments_before.len(), 4);

        let deleted = WalWriter::delete_segments_before_in_dir(&dir, 10, 2).unwrap();
        assert_eq!(deleted, 2);

        let segments_after = WalWriter::list_segments(&dir);
        assert_eq!(segments_after.len(), 2);

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
    fn segment_pruning_rejects_corruption_before_deleting_any_file() {
        let dir = tmp_dir("wal_delete_rejects_corruption");
        for sequence in 0..4 {
            append_encoded_entry(
                &dir.join(format!("wal-{sequence:08}.bin")),
                &checkpoint_entry(sequence),
            );
        }
        let corrupt = dir.join("wal-00000001.bin");
        let mut corrupt_bytes = fs::read(&corrupt).unwrap();
        *corrupt_bytes.last_mut().unwrap() ^= 0x80;
        fs::write(&corrupt, corrupt_bytes).unwrap();
        let before = WalWriter::list_segments(&dir);

        let error = WalWriter::delete_segments_before_in_dir(&dir, 4, 2).unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
        assert!(error.to_string().contains("checksum mismatch"));
        assert_eq!(WalWriter::list_segments(&dir), before);

        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn segment_pruning_never_deletes_an_empty_segment_after_retained_data() {
        let dir = tmp_dir("wal_delete_keeps_contiguous_namespace");
        append_encoded_entry(&dir.join("wal-00000000.bin"), &checkpoint_entry(0));
        File::create(dir.join("wal-00000001.bin")).unwrap();
        append_encoded_entry(&dir.join("wal-00000002.bin"), &checkpoint_entry(1));

        assert_eq!(
            WalWriter::delete_segments_before_in_dir(&dir, 0, 2).unwrap(),
            0
        );
        assert_eq!(WalWriter::list_segments_strict(&dir).unwrap().len(), 3);
        assert_eq!(read_wal_dir_strict(&dir).unwrap().len(), 2);

        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn segment_pruning_retains_a_sequence_watermark_before_empty_tail_segments() {
        let dir = tmp_dir("wal_delete_retains_sequence_watermark");
        append_encoded_entry(&dir.join("wal-00000000.bin"), &checkpoint_entry(0));
        append_encoded_entry(&dir.join("wal-00000001.bin"), &checkpoint_entry(1));
        File::create(dir.join("wal-00000002.bin")).unwrap();

        assert_eq!(
            WalWriter::delete_segments_before_in_dir(&dir, 2, 2).unwrap(),
            1
        );
        assert!(!dir.join("wal-00000000.bin").exists());
        assert!(dir.join("wal-00000001.bin").exists());
        assert!(dir.join("wal-00000002.bin").exists());

        {
            let writer = WalWriter::with_segments(&dir, u64::MAX).unwrap();
            assert_eq!(writer.sequence(), 2);
            writer.append(WalOp::Checkpoint(hash_bytes(b"after-empty-tail")), 2);
            writer.sync().unwrap();
        }
        let entries = read_wal_dir_strict(&dir).unwrap();
        assert_eq!(
            entries
                .iter()
                .map(|entry| entry.sequence)
                .collect::<Vec<_>>(),
            vec![1, 2]
        );

        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn segmented_wal_startup_reclaims_only_exact_removal_tombstones() {
        let dir = tmp_dir("wal_stale_removal_tombstone");
        let tombstone = dir.join(format!(
            ".wal-00000000.bin.removed-{}.tmp",
            uuid::Uuid::new_v4()
        ));
        let unrelated = dir.join(".wal-not-a-segment.removed-not-a-uuid.tmp");
        fs::write(&tombstone, b"retired segment").unwrap();
        fs::write(&unrelated, b"unrelated").unwrap();

        let writer = WalWriter::with_segments(&dir, 1024 * 1024).unwrap();
        assert!(!tombstone.exists());
        assert!(unrelated.exists());
        drop(writer);

        let _ = fs::remove_dir_all(dir);
    }

    #[cfg(windows)]
    #[test]
    fn windows_wal_retirement_retry_accepts_only_sharing_and_lock_violations() {
        use std::collections::VecDeque;

        let mut outcomes = VecDeque::from([
            Err(std::io::Error::from_raw_os_error(32)),
            Err(std::io::Error::from_raw_os_error(33)),
            Ok(()),
        ]);
        let mut sleeps = Vec::new();
        retry_windows_wal_retirement_move(
            || outcomes.pop_front().expect("one configured move outcome"),
            |duration| sleeps.push(duration),
        )
        .unwrap();
        assert!(outcomes.is_empty());
        assert_eq!(
            sleeps,
            vec![
                WINDOWS_WAL_RETIREMENT_MOVE_RETRY_DELAY,
                WINDOWS_WAL_RETIREMENT_MOVE_RETRY_DELAY,
            ]
        );

        let mut move_attempts = 0;
        let mut sleep_attempts = 0;
        let error = retry_windows_wal_retirement_move(
            || {
                move_attempts += 1;
                Err(std::io::Error::from_raw_os_error(5))
            },
            |_| sleep_attempts += 1,
        )
        .unwrap_err();
        assert_eq!(error.raw_os_error(), Some(5));
        assert_eq!(move_attempts, 1);
        assert_eq!(sleep_attempts, 0);
    }

    #[cfg(windows)]
    #[test]
    fn windows_wal_retirement_retry_is_bounded() {
        let mut move_attempts = 0;
        let mut sleep_attempts = 0;
        let error = retry_windows_wal_retirement_move(
            || {
                move_attempts += 1;
                Err(std::io::Error::from_raw_os_error(32))
            },
            |_| sleep_attempts += 1,
        )
        .unwrap_err();
        assert_eq!(error.raw_os_error(), Some(32));
        assert_eq!(move_attempts, WINDOWS_WAL_RETIREMENT_MOVE_ATTEMPTS);
        assert_eq!(sleep_attempts, WINDOWS_WAL_RETIREMENT_MOVE_ATTEMPTS - 1);
    }

    #[cfg(windows)]
    #[test]
    fn windows_wal_retirement_move_retries_a_real_sharing_violation() {
        use std::os::windows::fs::OpenOptionsExt as _;
        use windows_sys::Win32::Storage::FileSystem::{FILE_SHARE_READ, FILE_SHARE_WRITE};

        let dir = tmp_dir("wal_create_only_move_sharing_violation");
        let source = dir.join("wal-00000000.bin");
        let destination = dir.join(".wal-00000000.bin.removed-test.tmp");
        fs::write(&source, b"immutable WAL segment").unwrap();

        let mut blocker = Some(
            OpenOptions::new()
                .read(true)
                .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
                .open(&source)
                .unwrap(),
        );
        let mut sleep_attempts = 0;

        retry_windows_wal_retirement_move(
            || move_file_create_only_write_through(&source, &destination),
            |_| {
                sleep_attempts += 1;
                drop(blocker.take());
            },
        )
        .unwrap();
        assert_eq!(sleep_attempts, 1);
        assert!(!source.exists());
        assert_eq!(fs::read(&destination).unwrap(), b"immutable WAL segment");
        fs::remove_dir_all(dir).unwrap();
    }

    #[cfg(windows)]
    #[test]
    fn windows_wal_retirement_move_never_replaces_a_destination() {
        let dir = tmp_dir("wal_create_only_move_existing_destination");
        let source = dir.join("wal-00000000.bin");
        let destination = dir.join(".wal-00000000.bin.removed-existing.tmp");
        fs::write(&source, b"live WAL segment").unwrap();
        fs::write(&destination, b"existing tombstone").unwrap();

        let error = move_wal_segment_to_tombstone_create_only_with_retry(&source, &destination)
            .unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::AlreadyExists);
        assert_ne!(error.raw_os_error(), Some(32));
        assert_ne!(error.raw_os_error(), Some(33));
        assert_eq!(fs::read(&source).unwrap(), b"live WAL segment");
        assert_eq!(fs::read(&destination).unwrap(), b"existing tombstone");
        fs::remove_dir_all(dir).unwrap();
    }

    #[cfg(windows)]
    #[test]
    fn segmented_wal_restores_a_staged_root_before_discovery() {
        let parent =
            std::env::temp_dir().join(format!("arc-wal-staged-root-{}", uuid::Uuid::new_v4()));
        arc_crypto::secret_file::create_private_directory_tree(&parent).unwrap();
        let dir = parent.join("dag-wal");

        let writer = WalWriter::with_segments(&dir, u64::MAX).unwrap();
        writer.append(WalOp::Checkpoint(hash_bytes(b"preserved")), 7);
        writer.sync().unwrap();
        drop(writer);

        let digest = arc_crypto::secret_file::namespace_path_digest(&dir).unwrap();
        let staged = parent.join(format!(
            ".arc-private-directory-namespace-{digest}.rebarrier"
        ));
        arc_crypto::secret_file::windows_move_path_write_through(&dir, &staged, false).unwrap();
        assert!(!dir.exists());

        let writer = WalWriter::with_segments(&dir, u64::MAX).unwrap();
        assert_eq!(writer.sequence(), 1);
        assert!(!staged.exists());
        writer.append(WalOp::Checkpoint(hash_bytes(b"continued")), 8);
        writer.sync().unwrap();
        drop(writer);

        assert_eq!(WalWriter::count_entries_in_dir(&dir).unwrap(), 2);
        fs::remove_dir_all(parent).unwrap();
    }

    #[test]
    fn wal_read_across_segment_boundaries() {
        let dir = tmp_dir("wal_cross_segment");

        {
            let writer = WalWriter::with_segments(&dir, 1024 * 1024).expect("create segmented wal");

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

        let all_entries = read_wal_dir(&dir);
        assert_eq!(all_entries.len(), 9);

        for (i, entry) in all_entries.iter().enumerate() {
            assert_eq!(
                entry.sequence, i as u64,
                "expected sequence {}, got {}",
                i, entry.sequence
            );
        }

        let _ = fs::remove_dir_all(&dir);
    }

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
        {
            let writer = WalWriter::with_segments(&dir, 1024).expect("create");
            for h in 0u64..200 {
                writer.append(WalOp::Checkpoint(Hash256::ZERO), h);
            }
            writer.sync().unwrap();
            drop(writer);
        }
        let latest = latest_block_height_in_wal_dir(&dir);
        assert_eq!(
            latest, 199,
            "helper must find max round even when newest segment is the empty post-rotation file"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn strict_tail_inspection_returns_the_exact_durable_round() {
        let dir = tmp_dir("strict_tail_round");
        {
            let writer = WalWriter::with_segments(&dir, 1024).expect("create");
            for round in 0u64..200 {
                writer.append(WalOp::Checkpoint(Hash256::ZERO), round);
            }
            writer.sync().unwrap();
            drop(writer);
        }

        let inspection = inspect_wal_dir_tail_strict(&dir).unwrap();
        assert_eq!(inspection.source_consensus_round, 199);
        assert!(inspection.segment_count >= inspection.inspected_segment_count);
        assert!((1..=3).contains(&inspection.inspected_segment_count));
        assert!(inspection.inspected_entry_count > 0);
        assert_eq!(inspection.first_segment, 0);
        assert!(inspection.last_segment >= inspection.inspected_first_segment);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn strict_tail_inspection_rejects_a_corrupt_final_segment() {
        let dir = tmp_dir("strict_tail_corrupt");
        {
            let writer = WalWriter::with_segments(&dir, u64::MAX).expect("create");
            writer.append(WalOp::Checkpoint(Hash256::ZERO), 77);
            writer.sync().unwrap();
            drop(writer);
        }
        let segment = dir.join("wal-00000000.bin");
        let mut bytes = fs::read(&segment).unwrap();
        *bytes.last_mut().expect("encoded WAL entry") ^= 0x80;
        fs::write(&segment, bytes).unwrap();

        let error = inspect_wal_dir_tail_strict(&dir).unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
        assert!(error.to_string().contains("checksum"));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn latest_block_height_handles_unsorted_entries_within_segment() {
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
