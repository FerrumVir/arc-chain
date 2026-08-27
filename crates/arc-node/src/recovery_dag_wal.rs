//! Atomic, bounded generations for the post-recovery consensus DAG WAL.
//!
//! A generation is immutable and content addressed. Its manifest binds the
//! recovery checkpoint/domain, the validator set, the canonical state baseline,
//! the DAG cursors, and the exact retained record log. Publishing is two phase:
//! all generation files and directories are fsynced first, then `CURRENT` is
//! atomically replaced and the store directory is fsynced. Previous generations
//! are never removed by this module.
//!
//! Every immutable generation owns `active-<generation-hash>.bin`. `CURRENT`
//! selects both by the same hash. Active writes are whole checksummed batch
//! frames under an OS advisory lock: a crash can leave only a valid prefix plus
//! one classifiable torn final batch. The final bytes can be quarantined before
//! append resumes; complete-frame corruption always fails closed.
//!
//! `CURRENT` is intentionally not treated as a cryptographic rollback oracle.
//! Callers should persist [`GenerationPin`] in an independent recovery marker
//! and pass it to [`GenerationStore::load_current`]. [`GenerationStore::audit`]
//! additionally detects a pointer moved behind a preserved descendant or a
//! fork/swap among the generations still present on disk.

use arc_crypto::Hash256;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufReader, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use thiserror::Error;

const MANIFEST_SCHEMA: &str = "arc.recovery.dag-wal-generation.v1";
const POINTER_SCHEMA: &str = "arc.recovery.dag-wal-current.v2";
const RECORD_MAGIC: &[u8; 8] = b"ARCDAGW1";
const RECORD_SCHEMA: u8 = 1;
const ACTIVE_MAGIC: &[u8; 8] = b"ARCACTW1";
const ACTIVE_SCHEMA: u8 = 1;
const ACTIVE_BATCH_SCHEMA: u8 = 1;
const MANIFEST_FILE: &str = "manifest.json";
const RECORDS_FILE: &str = "records.bin";
const CURRENT_FILE: &str = "CURRENT";
const WRITE_LOCK_FILE: &str = ".WRITE.lock";
const MAX_MANIFEST_BYTES: u64 = 64 * 1024;
const MAX_POINTER_BYTES: u64 = 4 * 1024;
const FRAME_FIXED_BODY_BYTES: u64 = 1 + 1 + 8 + 32 + 4;
const FRAME_OVERHEAD_BYTES: u64 = 4 + FRAME_FIXED_BODY_BYTES + 32;
const ACTIVE_HEADER_BYTES: u64 = 8 + 1 + 32 + 8 + 32 + 32 + 32 + 8 + 8;
const ACTIVE_BATCH_FIXED_BODY_BYTES: u64 = 1 + 8 + 4;
const ACTIVE_BATCH_FRAME_OVERHEAD_BYTES: u64 = 4 + ACTIVE_BATCH_FIXED_BODY_BYTES + 32;
const MAX_GENERATIONS_TO_AUDIT: usize = 10_000;

/// Absolute fail-closed caps. A manifest may select lower limits, never higher.
pub const HARD_MAX_RETAINED_RECORDS: u64 = 100_000;
pub const HARD_MAX_RETAINED_PAYLOAD_BYTES: u64 = 256 * 1024 * 1024;
pub const HARD_MAX_SINGLE_RECORD_PAYLOAD_BYTES: u64 = 64 * 1024 * 1024;
pub const HARD_MAX_ACTIVE_BATCH_PAYLOAD_BYTES: u64 = 64 * 1024 * 1024;
pub const HARD_MAX_RETENTION_ROUND_SPAN: u64 = 4_096;

#[derive(Debug, Error)]
pub enum GenerationError {
    #[error("{operation} {path}: {source}")]
    Io {
        operation: &'static str,
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("invalid recovery DAG generation: {0}")]
    Invalid(String),
    #[error("recovery DAG generation store is locked at {0}")]
    Locked(PathBuf),
    #[error("recovery DAG generation store has no CURRENT pointer at {0}")]
    NoCurrent(PathBuf),
    #[error(
        "CURRENT pin mismatch: expected sequence {expected_sequence} hash {expected_hash}, found sequence {actual_sequence} hash {actual_hash}"
    )]
    PinMismatch {
        expected_sequence: u64,
        expected_hash: Hash256,
        actual_sequence: u64,
        actual_hash: Hash256,
    },
    #[error("published record log contains a torn final suffix: {0:?}")]
    TornPublishedRecordLog(TornSuffix),
    #[error("injected generation publish failure after {0:?}")]
    InjectedFailure(PublishPoint),
    #[error("active DAG delta writer is poisoned: {0}")]
    ActiveWriterPoisoned(String),
}

pub type Result<T> = std::result::Result<T, GenerationError>;

fn io_error(operation: &'static str, path: &Path, source: io::Error) -> GenerationError {
    GenerationError::Io {
        operation,
        path: path.to_path_buf(),
        source,
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecoveryDagBinding {
    pub recovery_manifest_hash: Hash256,
    pub recovery_domain: Hash256,
    pub validator_set_commitment: Hash256,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BaselineState {
    pub height: u64,
    pub block_hash: Hash256,
    pub state_root: Hash256,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DagCursor {
    pub committed_block_count: u64,
    pub next_dag_round: u64,
    pub current_round: u64,
    pub retention_floor_round: u64,
    pub retention_ceiling_round: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RetentionLimits {
    pub max_records: u64,
    pub max_payload_bytes: u64,
}

impl Default for RetentionLimits {
    fn default() -> Self {
        Self {
            max_records: HARD_MAX_RETAINED_RECORDS,
            max_payload_bytes: HARD_MAX_RETAINED_PAYLOAD_BYTES,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum RetainedRecordKind {
    TransactionBody = 1,
    DagBlock = 2,
    RoundCursor = 3,
    Commit = 4,
}

impl TryFrom<u8> for RetainedRecordKind {
    type Error = GenerationError;

    fn try_from(value: u8) -> Result<Self> {
        match value {
            1 => Ok(Self::TransactionBody),
            2 => Ok(Self::DagBlock),
            3 => Ok(Self::RoundCursor),
            4 => Ok(Self::Commit),
            _ => Err(GenerationError::Invalid(format!(
                "unknown retained record kind {value}"
            ))),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RetainedDagRecord {
    pub kind: RetainedRecordKind,
    pub round: u64,
    pub object_hash: Hash256,
    pub payload: Vec<u8>,
}

impl RetainedDagRecord {
    pub fn transaction(round: u64, transaction_hash: Hash256, bytes: Vec<u8>) -> Self {
        Self {
            kind: RetainedRecordKind::TransactionBody,
            round,
            object_hash: transaction_hash,
            payload: bytes,
        }
    }

    pub fn dag_block(round: u64, block_hash: Hash256, bytes: Vec<u8>) -> Self {
        Self {
            kind: RetainedRecordKind::DagBlock,
            round,
            object_hash: block_hash,
            payload: bytes,
        }
    }

    pub fn round_cursor(round: u64) -> Self {
        Self {
            kind: RetainedRecordKind::RoundCursor,
            round,
            object_hash: Hash256::ZERO,
            payload: Vec::new(),
        }
    }

    pub fn commit(round: u64, block_hash: Hash256) -> Self {
        Self {
            kind: RetainedRecordKind::Commit,
            round,
            object_hash: block_hash,
            payload: Vec::new(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RetainedRecordSet {
    pub limits: RetentionLimits,
    pub record_count: u64,
    pub payload_bytes: u64,
    pub file_bytes: u64,
    /// Minimum round present; physical record order is preserved separately.
    pub first_round: Option<u64>,
    /// Maximum round present; physical record order is preserved separately.
    pub last_round: Option<u64>,
    pub records_file_hash: Hash256,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GenerationManifest {
    pub schema: String,
    pub sequence: u64,
    pub previous_generation: Option<Hash256>,
    pub binding: RecoveryDagBinding,
    pub baseline_state: BaselineState,
    pub dag_cursor: DagCursor,
    pub retained_records: RetainedRecordSet,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GenerationInput {
    pub binding: RecoveryDagBinding,
    pub baseline_state: BaselineState,
    pub dag_cursor: DagCursor,
    pub retention_limits: RetentionLimits,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GenerationPin {
    pub sequence: u64,
    pub hash: Hash256,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VerifiedGeneration {
    pub pin: GenerationPin,
    pub manifest: GenerationManifest,
    pub directory: PathBuf,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum TornSuffix {
    Clean,
    TruncatedHeader {
        present_bytes: u64,
        expected_bytes: u64,
    },
    PartialLength {
        present_bytes: u64,
        expected_bytes: u64,
    },
    PartialPayload {
        present_bytes: u64,
        expected_bytes: u64,
    },
    PartialChecksum {
        present_bytes: u64,
        expected_bytes: u64,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecordLogInspection {
    pub record_count: u64,
    pub payload_bytes: u64,
    pub valid_prefix_bytes: u64,
    pub total_file_bytes: u64,
    /// Minimum round in the valid prefix, not the first physical record.
    pub first_round: Option<u64>,
    /// Maximum round in the valid prefix, not the last physical record.
    pub last_round: Option<u64>,
    pub valid_prefix_hash: Hash256,
    pub complete_file_hash: Hash256,
    pub suffix: TornSuffix,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ActiveLogInspection {
    pub generation_pin: GenerationPin,
    pub batch_count: u64,
    pub record_count: u64,
    pub payload_bytes: u64,
    pub valid_prefix_bytes: u64,
    pub total_file_bytes: u64,
    /// Minimum round in complete active batches, independent of append order.
    pub first_round: Option<u64>,
    /// Maximum round in complete active batches, independent of append order.
    pub last_round: Option<u64>,
    pub valid_prefix_hash: Hash256,
    pub complete_file_hash: Hash256,
    pub suffix: TornSuffix,
}

impl ActiveLogInspection {
    pub fn pin(&self) -> ActiveLogPin {
        ActiveLogPin {
            generation_pin: self.generation_pin,
            complete_file_hash: self.complete_file_hash,
            file_bytes: self.total_file_bytes,
            batch_count: self.batch_count,
            record_count: self.record_count,
            payload_bytes: self.payload_bytes,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ActiveLogPin {
    pub generation_pin: GenerationPin,
    pub complete_file_hash: Hash256,
    pub file_bytes: u64,
    pub batch_count: u64,
    pub record_count: u64,
    pub payload_bytes: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ActiveDurability {
    Buffered,
    Fsync,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ActiveAppendReceipt {
    pub generation_pin: GenerationPin,
    pub batch_sequence: Option<u64>,
    pub requested_records: u64,
    pub appended_records: u64,
    pub idempotently_omitted_records: u64,
    pub total_active_records: u64,
    pub total_active_payload_bytes: u64,
    pub valid_prefix_bytes: u64,
    pub durable: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CurrentStreamSummary {
    pub generation_pin: GenerationPin,
    pub base_record_count: u64,
    pub active_batch_count: u64,
    pub active_record_count: u64,
    pub active_valid_prefix_bytes: u64,
    pub active_total_file_bytes: u64,
    pub active_complete_file_hash: Hash256,
    pub active_pin: ActiveLogPin,
    pub active_suffix: TornSuffix,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ActiveSuffixQuarantine {
    pub generation_pin: GenerationPin,
    pub active_log_path: PathBuf,
    pub quarantine_path: PathBuf,
    pub original_file_hash: Hash256,
    pub valid_prefix_hash: Hash256,
    pub quarantined_suffix_hash: Hash256,
    pub valid_prefix_bytes: u64,
    pub quarantined_suffix_bytes: u64,
    pub classification: TornSuffix,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StoreAuditStatus {
    Clean,
    PointerBehind { heads: Vec<GenerationPin> },
    Forked { heads: Vec<GenerationPin> },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StoreAudit {
    pub current: VerifiedGeneration,
    pub generation_count: usize,
    pub status: StoreAuditStatus,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PublishPoint {
    RecordsSynced,
    ManifestSynced,
    GenerationDirectorySynced,
    GenerationPublished,
    RootAfterGenerationSynced,
    ActiveLogSynced,
    RootAfterActiveLogSynced,
    PointerFileSynced,
    PointerRenamed,
    RootAfterPointerSynced,
}

trait PublishObserver {
    fn reached(&mut self, _point: PublishPoint) -> Result<()> {
        Ok(())
    }
}

struct NoopObserver;
impl PublishObserver for NoopObserver {}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CurrentPointer {
    schema: String,
    generation_hash: Hash256,
    active_log_generation_hash: Hash256,
    sequence: u64,
    previous_generation: Option<Hash256>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct ActiveLogHeader {
    generation_pin: GenerationPin,
    binding: RecoveryDagBinding,
    limits: RetentionLimits,
}

/// Exclusive append handle for the active delta selected by `CURRENT`.
/// Holding this value also holds the store write lock, so compaction cannot
/// switch generations concurrently with a batch append.
pub struct ActiveLogWriter {
    generation: VerifiedGeneration,
    path: PathBuf,
    file: File,
    inspection: ActiveLogInspection,
    seen: HashMap<(RetainedRecordKind, u64, [u8; 32]), Hash256>,
    active_hasher: blake3::Hasher,
    poisoned: Option<String>,
    _store_lock: StoreLock,
}

impl std::fmt::Debug for ActiveLogWriter {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ActiveLogWriter")
            .field("generation", &self.generation.pin)
            .field("path", &self.path)
            .field("inspection", &self.inspection)
            .field("poisoned", &self.poisoned)
            .finish_non_exhaustive()
    }
}

impl ActiveLogWriter {
    pub fn generation_pin(&self) -> GenerationPin {
        self.generation.pin
    }

    pub fn inspection(&self) -> &ActiveLogInspection {
        &self.inspection
    }

    /// Append one checksum-atomic batch. Every record and the combined
    /// immutable-generation + active-delta limits are preflighted before the
    /// first byte is written. A short/erroring write poisons this handle; after
    /// restart the incomplete final batch is classified and excluded.
    pub fn append_batch(
        &mut self,
        records: &[RetainedDagRecord],
        durability: ActiveDurability,
    ) -> Result<ActiveAppendReceipt> {
        if let Some(reason) = &self.poisoned {
            return Err(GenerationError::ActiveWriterPoisoned(reason.clone()));
        }
        if records.is_empty() {
            return Err(GenerationError::Invalid(
                "active delta batch must contain at least one record".into(),
            ));
        }
        let limits = self.generation.manifest.retained_records.limits;
        let cursor = &self.generation.manifest.dag_cursor;
        let mut batch_payload_bytes = 0u64;
        let mut batch_seen = HashMap::new();
        let mut appendable = Vec::with_capacity(records.len());
        for record in records {
            validate_record(record)?;
            let identity = record_identity(record);
            let fingerprint = record_payload_fingerprint(record);
            let existing = batch_seen
                .get(&identity)
                .or_else(|| self.seen.get(&identity));
            if let Some(existing) = existing {
                if *existing != fingerprint {
                    let reason = format!(
                        "record key {:?}/round {}/{} was reused with different payload",
                        record.kind, record.round, record.object_hash
                    );
                    self.poisoned = Some(reason.clone());
                    return Err(GenerationError::ActiveWriterPoisoned(reason));
                }
                continue;
            }
            if record.round < cursor.retention_floor_round
                || record.round > cursor.retention_ceiling_round
            {
                return Err(GenerationError::Invalid(format!(
                    "active record round {} is outside {}..={}",
                    record.round, cursor.retention_floor_round, cursor.retention_ceiling_round
                )));
            }
            batch_seen.insert(identity, fingerprint);
            appendable.push(record.clone());
            batch_payload_bytes = batch_payload_bytes
                .checked_add(record.payload.len() as u64)
                .ok_or_else(|| GenerationError::Invalid("batch payload size overflow".into()))?;
        }
        if batch_payload_bytes > HARD_MAX_ACTIVE_BATCH_PAYLOAD_BYTES {
            return Err(GenerationError::Invalid(format!(
                "active batch payload exceeds {HARD_MAX_ACTIVE_BATCH_PAYLOAD_BYTES} bytes"
            )));
        }
        let requested_records = records.len() as u64;
        let batch_record_count = appendable.len() as u64;
        let omitted_records = requested_records - batch_record_count;
        if appendable.is_empty() {
            let durable = durability == ActiveDurability::Fsync;
            if durable && let Err(error) = self.file.sync_all() {
                self.poisoned = Some(format!("idempotent retry fsync failed: {error}"));
                return Err(io_error("fsync active delta", &self.path, error));
            }
            return Ok(ActiveAppendReceipt {
                generation_pin: self.generation.pin,
                batch_sequence: None,
                requested_records,
                appended_records: 0,
                idempotently_omitted_records: omitted_records,
                total_active_records: self.inspection.record_count,
                total_active_payload_bytes: self.inspection.payload_bytes,
                valid_prefix_bytes: self.inspection.valid_prefix_bytes,
                durable,
            });
        }
        let combined_records = self
            .generation
            .manifest
            .retained_records
            .record_count
            .checked_add(self.inspection.record_count)
            .and_then(|count| count.checked_add(batch_record_count))
            .ok_or_else(|| GenerationError::Invalid("combined record count overflow".into()))?;
        if combined_records > limits.max_records {
            return Err(GenerationError::Invalid(
                "active batch would exceed the combined retained-record cap".into(),
            ));
        }
        let combined_payload = self
            .generation
            .manifest
            .retained_records
            .payload_bytes
            .checked_add(self.inspection.payload_bytes)
            .and_then(|bytes| bytes.checked_add(batch_payload_bytes))
            .ok_or_else(|| GenerationError::Invalid("combined payload size overflow".into()))?;
        if combined_payload > limits.max_payload_bytes {
            return Err(GenerationError::Invalid(
                "active batch would exceed the combined retained-payload cap".into(),
            ));
        }

        let batch_sequence = self.inspection.batch_count;
        let body = encode_active_batch(batch_sequence, &appendable)?;
        let body_length = u32::try_from(body.len())
            .map_err(|_| GenerationError::Invalid("active batch frame is too large".into()))?;
        let checksum = domain_hash("ARC recovery DAG active batch frame v1", &body);
        let mut frame = Vec::with_capacity(4 + body.len() + 32);
        frame.extend_from_slice(&body_length.to_be_bytes());
        frame.extend_from_slice(&body);
        frame.extend_from_slice(checksum.as_ref());

        if let Err(error) = self.file.write_all(&frame) {
            self.poisoned = Some(format!("short/erroring batch append: {error}"));
            return Err(io_error("append batch to", &self.path, error));
        }
        self.active_hasher.update(&frame);
        self.inspection.batch_count = self
            .inspection
            .batch_count
            .checked_add(1)
            .ok_or_else(|| GenerationError::Invalid("active batch count overflow".into()))?;
        self.inspection.record_count = self
            .inspection
            .record_count
            .checked_add(batch_record_count)
            .ok_or_else(|| GenerationError::Invalid("active record count overflow".into()))?;
        self.inspection.payload_bytes = self
            .inspection
            .payload_bytes
            .checked_add(batch_payload_bytes)
            .ok_or_else(|| GenerationError::Invalid("active payload size overflow".into()))?;
        self.inspection.valid_prefix_bytes = self
            .inspection
            .valid_prefix_bytes
            .checked_add(frame.len() as u64)
            .ok_or_else(|| GenerationError::Invalid("active prefix size overflow".into()))?;
        self.inspection.total_file_bytes = self.inspection.valid_prefix_bytes;
        let batch_min_round = appendable
            .iter()
            .map(|record| record.round)
            .min()
            .expect("non-empty appendable batch");
        let batch_max_round = appendable
            .iter()
            .map(|record| record.round)
            .max()
            .expect("non-empty appendable batch");
        self.inspection.first_round = Some(
            self.inspection
                .first_round
                .map_or(batch_min_round, |round| round.min(batch_min_round)),
        );
        self.inspection.last_round = Some(
            self.inspection
                .last_round
                .map_or(batch_max_round, |round| round.max(batch_max_round)),
        );
        let current_hash = Hash256(*self.active_hasher.clone().finalize().as_bytes());
        self.inspection.valid_prefix_hash = current_hash;
        self.inspection.complete_file_hash = current_hash;
        self.inspection.suffix = TornSuffix::Clean;
        for record in &appendable {
            self.seen
                .insert(record_identity(record), record_payload_fingerprint(record));
        }

        let durable = durability == ActiveDurability::Fsync;
        if durable && let Err(error) = self.file.sync_all() {
            self.poisoned = Some(format!("batch fsync failed: {error}"));
            return Err(io_error("fsync active delta", &self.path, error));
        }
        Ok(ActiveAppendReceipt {
            generation_pin: self.generation.pin,
            batch_sequence: Some(batch_sequence),
            requested_records,
            appended_records: batch_record_count,
            idempotently_omitted_records: omitted_records,
            total_active_records: self.inspection.record_count,
            total_active_payload_bytes: self.inspection.payload_bytes,
            valid_prefix_bytes: self.inspection.valid_prefix_bytes,
            durable,
        })
    }

    pub fn sync(&mut self) -> Result<()> {
        if let Some(reason) = &self.poisoned {
            return Err(GenerationError::ActiveWriterPoisoned(reason.clone()));
        }
        if let Err(error) = self.file.sync_all() {
            self.poisoned = Some(format!("active delta fsync failed: {error}"));
            return Err(io_error("fsync active delta", &self.path, error));
        }
        Ok(())
    }
}

#[derive(Debug)]
pub struct GenerationStore {
    root: PathBuf,
}

impl GenerationStore {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn create_initial<I>(
        &self,
        input: GenerationInput,
        records: I,
    ) -> Result<VerifiedGeneration>
    where
        I: IntoIterator<Item = RetainedDagRecord>,
    {
        self.ensure_root()?;
        let lock = StoreLock::acquire(&self.root)?;
        if self.root.join(CURRENT_FILE).exists() {
            return Err(GenerationError::Invalid(
                "cannot create an initial generation when CURRENT already exists".into(),
            ));
        }
        if self.generation_directory_count()? != 0 {
            return Err(GenerationError::Invalid(
                "cannot create an initial generation alongside existing generation directories"
                    .into(),
            ));
        }
        let mut observer = NoopObserver;
        let generation = self.publish_generation(None, input, records, &mut observer)?;
        lock.release()?;
        Ok(generation)
    }

    pub fn append<I>(
        &self,
        expected_current: GenerationPin,
        input: GenerationInput,
        records: I,
    ) -> Result<VerifiedGeneration>
    where
        I: IntoIterator<Item = RetainedDagRecord>,
    {
        self.ensure_root()?;
        let lock = StoreLock::acquire(&self.root)?;
        let current = self.load_current(&input.binding, Some(expected_current))?;
        let active = inspect_active_log(&self.active_log_path(current.pin), &current)?;
        require_empty_active_for_direct_append(&active)?;
        let mut observer = NoopObserver;
        let generation = self.publish_generation(Some(&current), input, records, &mut observer)?;
        lock.release()?;
        Ok(generation)
    }

    /// Publish a generation compacted from an exact active-delta snapshot.
    /// The external active pin prevents any append that occurred after the
    /// caller streamed/prepared `records` from being silently omitted.
    pub fn append_compacted<I>(
        &self,
        expected_current: GenerationPin,
        expected_active: ActiveLogPin,
        input: GenerationInput,
        records: I,
    ) -> Result<VerifiedGeneration>
    where
        I: IntoIterator<Item = RetainedDagRecord>,
    {
        self.ensure_root()?;
        let lock = StoreLock::acquire(&self.root)?;
        let current = self.load_current(&input.binding, Some(expected_current))?;
        let active = inspect_active_log(&self.active_log_path(current.pin), &current)?;
        if active.suffix != TornSuffix::Clean || active.pin() != expected_active {
            return Err(GenerationError::Invalid(
                "active delta changed after the compaction snapshot was selected".into(),
            ));
        }
        let mut observer = NoopObserver;
        let generation = self.publish_generation(Some(&current), input, records, &mut observer)?;
        lock.release()?;
        Ok(generation)
    }

    /// Verify `CURRENT`, its exact files, its binding, and an optional external
    /// rollback pin. This performs no directory scan and is therefore bounded
    /// by the manifest's retained-record limits.
    pub fn load_current(
        &self,
        expected_binding: &RecoveryDagBinding,
        expected_pin: Option<GenerationPin>,
    ) -> Result<VerifiedGeneration> {
        let pointer = self.read_current_pointer()?;
        if let Some(expected) = expected_pin {
            let actual = GenerationPin {
                sequence: pointer.sequence,
                hash: pointer.generation_hash,
            };
            if actual != expected {
                return Err(GenerationError::PinMismatch {
                    expected_sequence: expected.sequence,
                    expected_hash: expected.hash,
                    actual_sequence: actual.sequence,
                    actual_hash: actual.hash,
                });
            }
        }
        let generation = self.verify_generation(pointer.generation_hash, expected_binding)?;
        if generation.pin.sequence != pointer.sequence
            || generation.manifest.previous_generation != pointer.previous_generation
        {
            return Err(GenerationError::Invalid(
                "CURRENT metadata differs from its content-addressed manifest".into(),
            ));
        }
        inspect_active_log(&self.active_log_path(generation.pin), &generation)?;
        Ok(generation)
    }

    pub fn verify_generation(
        &self,
        generation_hash: Hash256,
        expected_binding: &RecoveryDagBinding,
    ) -> Result<VerifiedGeneration> {
        let directory = self.generation_path(generation_hash);
        ensure_real_directory(&directory)?;
        let manifest_path = directory.join(MANIFEST_FILE);
        let manifest_bytes = read_small_regular_file(&manifest_path, MAX_MANIFEST_BYTES)?;
        let actual_hash = domain_hash("ARC recovery DAG generation manifest v1", &manifest_bytes);
        if actual_hash != generation_hash {
            return Err(GenerationError::Invalid(format!(
                "manifest hash {actual_hash} differs from generation directory {generation_hash}"
            )));
        }
        let manifest: GenerationManifest =
            serde_json::from_slice(&manifest_bytes).map_err(|error| {
                GenerationError::Invalid(format!("manifest JSON is invalid: {error}"))
            })?;
        require_canonical_json(&manifest_bytes, &manifest, "manifest")?;
        validate_manifest(&manifest)?;
        if &manifest.binding != expected_binding {
            return Err(GenerationError::Invalid(
                "generation recovery/domain/validator binding differs from the expected binding"
                    .into(),
            ));
        }
        let records_path = directory.join(RECORDS_FILE);
        let inspection = inspect_record_log(&records_path, manifest.retained_records.limits)?;
        if inspection.suffix != TornSuffix::Clean {
            return Err(GenerationError::TornPublishedRecordLog(inspection.suffix));
        }
        require_inspection_matches_manifest(&inspection, &manifest.retained_records)?;
        validate_inspection_window(&inspection, &manifest.dag_cursor)?;
        Ok(VerifiedGeneration {
            pin: GenerationPin {
                sequence: manifest.sequence,
                hash: generation_hash,
            },
            manifest,
            directory,
        })
    }

    /// Stream a previously verified generation without materializing its
    /// retained payloads together in memory. The generation is reverified
    /// before callbacks run and the streamed aggregate is checked again before
    /// success is returned. A visitor must stage reversible in-memory work and
    /// finalize it only after this method returns `Ok(())`.
    pub fn for_each_record<F>(&self, generation: &VerifiedGeneration, visitor: F) -> Result<()>
    where
        F: FnMut(RetainedDagRecord) -> Result<()>,
    {
        let reverified =
            self.verify_generation(generation.pin.hash, &generation.manifest.binding)?;
        if reverified.pin != generation.pin || reverified.manifest != generation.manifest {
            return Err(GenerationError::Invalid(
                "generation changed between verification and record streaming".into(),
            ));
        }
        let inspection = scan_record_log(
            &generation.directory.join(RECORDS_FILE),
            generation.manifest.retained_records.limits,
            visitor,
        )?;
        if inspection.suffix != TornSuffix::Clean {
            return Err(GenerationError::TornPublishedRecordLog(inspection.suffix));
        }
        require_inspection_matches_manifest(&inspection, &generation.manifest.retained_records)?;
        Ok(())
    }

    /// Open the active delta selected by the exact externally pinned CURRENT.
    /// The returned writer holds the store's OS advisory lock until drop.
    pub fn open_current_active_writer(
        &self,
        expected_binding: &RecoveryDagBinding,
        expected_pin: GenerationPin,
    ) -> Result<ActiveLogWriter> {
        self.ensure_root()?;
        let store_lock = StoreLock::acquire(&self.root)?;
        let generation = self.load_current(expected_binding, Some(expected_pin))?;
        let mut seen = HashMap::new();
        let base_inspection = scan_record_log(
            &generation.directory.join(RECORDS_FILE),
            generation.manifest.retained_records.limits,
            |record| {
                seen.insert(
                    record_identity(&record),
                    record_payload_fingerprint(&record),
                );
                Ok(())
            },
        )?;
        require_inspection_matches_manifest(
            &base_inspection,
            &generation.manifest.retained_records,
        )?;
        let path = self.active_log_path(generation.pin);
        let active_inspection = scan_active_log(&path, &generation, |record| {
            let identity = record_identity(&record);
            let fingerprint = record_payload_fingerprint(&record);
            if let Some(existing) = seen.get(&identity) {
                if *existing != fingerprint {
                    return Err(GenerationError::Invalid(
                        "active delta reuses a retained record key with different payload".into(),
                    ));
                }
            } else {
                seen.insert(identity, fingerprint);
            }
            Ok(())
        })?;
        if active_inspection.suffix != TornSuffix::Clean {
            return Err(GenerationError::Invalid(format!(
                "active delta has a torn final batch at byte {}; inspect/recover its valid prefix before reopening for append",
                active_inspection.valid_prefix_bytes
            )));
        }
        let mut options = OpenOptions::new();
        options.read(true).append(true);
        let file = options
            .open(&path)
            .map_err(|error| io_error("open active delta for append", &path, error))?;
        if file
            .metadata()
            .map_err(|error| io_error("inspect active delta", &path, error))?
            .len()
            != active_inspection.total_file_bytes
        {
            return Err(GenerationError::Invalid(
                "active delta changed between inspection and writer open".into(),
            ));
        }
        let active_hasher = hash_file_into_hasher(
            &path,
            "ARC recovery DAG active delta v1",
            active_inspection.total_file_bytes,
        )?;
        if Hash256(*active_hasher.clone().finalize().as_bytes())
            != active_inspection.complete_file_hash
        {
            return Err(GenerationError::Invalid(
                "active delta changed between validation and writer open".into(),
            ));
        }
        Ok(ActiveLogWriter {
            generation,
            path,
            file,
            inspection: active_inspection,
            seen,
            active_hasher,
            poisoned: None,
            _store_lock: store_lock,
        })
    }

    /// Stream the immutable generation followed by every complete active batch
    /// selected by the same pinned CURRENT. The OS advisory lock prevents an
    /// append or compaction switch during the snapshot. Visitors must stage
    /// reversible effects until this method returns successfully.
    pub fn stream_current_generation_and_active<F>(
        &self,
        expected_binding: &RecoveryDagBinding,
        expected_pin: GenerationPin,
        mut visitor: F,
    ) -> Result<CurrentStreamSummary>
    where
        F: FnMut(RetainedDagRecord) -> Result<()>,
    {
        self.ensure_root()?;
        let lock = StoreLock::acquire(&self.root)?;
        let generation = self.load_current(expected_binding, Some(expected_pin))?;
        let mut seen = HashMap::new();
        let base_inspection = scan_record_log(
            &generation.directory.join(RECORDS_FILE),
            generation.manifest.retained_records.limits,
            |record| {
                seen.insert(
                    record_identity(&record),
                    record_payload_fingerprint(&record),
                );
                visitor(record)
            },
        )?;
        require_inspection_matches_manifest(
            &base_inspection,
            &generation.manifest.retained_records,
        )?;
        let active = scan_active_log(
            &self.active_log_path(generation.pin),
            &generation,
            |record| {
                let identity = record_identity(&record);
                let fingerprint = record_payload_fingerprint(&record);
                if let Some(existing) = seen.get(&identity) {
                    if *existing != fingerprint {
                        return Err(GenerationError::Invalid(
                            "active delta reuses a generation record key with different payload"
                                .into(),
                        ));
                    }
                    return Ok(());
                }
                seen.insert(identity, fingerprint);
                visitor(record)
            },
        )?;
        lock.release()?;
        Ok(CurrentStreamSummary {
            generation_pin: generation.pin,
            base_record_count: base_inspection.record_count,
            active_batch_count: active.batch_count,
            active_record_count: active.record_count,
            active_valid_prefix_bytes: active.valid_prefix_bytes,
            active_total_file_bytes: active.total_file_bytes,
            active_complete_file_hash: active.complete_file_hash,
            active_pin: active.pin(),
            active_suffix: active.suffix,
        })
    }

    /// Preserve and remove exactly one previously inspected torn final active
    /// batch. The active log is re-inspected while holding the exclusive OS
    /// advisory lock; any changed prefix, clean log, full-frame corruption, or
    /// different tear boundary fails closed.
    pub fn quarantine_current_active_suffix(
        &self,
        expected_binding: &RecoveryDagBinding,
        expected_pin: GenerationPin,
        expected_valid_prefix_bytes: u64,
    ) -> Result<ActiveSuffixQuarantine> {
        self.ensure_root()?;
        let lock = StoreLock::acquire(&self.root)?;
        let generation = self.load_current(expected_binding, Some(expected_pin))?;
        let active_path = self.active_log_path(generation.pin);
        let inspection = inspect_active_log(&active_path, &generation)?;
        if inspection.valid_prefix_bytes != expected_valid_prefix_bytes {
            return Err(GenerationError::Invalid(format!(
                "active tear boundary changed: expected {expected_valid_prefix_bytes}, found {}",
                inspection.valid_prefix_bytes
            )));
        }
        if inspection.valid_prefix_bytes < ACTIVE_HEADER_BYTES
            || inspection.valid_prefix_bytes >= inspection.total_file_bytes
            || matches!(
                inspection.suffix,
                TornSuffix::Clean | TornSuffix::TruncatedHeader { .. }
            )
        {
            return Err(GenerationError::Invalid(
                "active quarantine requires one non-empty, structurally torn final batch".into(),
            ));
        }
        let full_hasher = hash_file_into_hasher(
            &active_path,
            "ARC recovery DAG active delta v1",
            inspection.total_file_bytes,
        )?;
        let observed_full_hash = Hash256(*full_hasher.finalize().as_bytes());
        if observed_full_hash != inspection.complete_file_hash {
            return Err(GenerationError::Invalid(
                "active delta changed after its torn suffix was inspected".into(),
            ));
        }
        let suffix_bytes = read_regular_file_range(
            &active_path,
            inspection.valid_prefix_bytes,
            inspection.total_file_bytes,
        )?;
        let suffix_hash = domain_hash(
            "ARC recovery DAG quarantined active suffix v1",
            &suffix_bytes,
        );
        let quarantine_path = self.root.join(format!(
            "active-{}.torn-{}.bin",
            generation.pin.hash.to_hex(),
            inspection.complete_file_hash.to_hex()
        ));
        persist_exact_quarantine(&self.root, &quarantine_path, &suffix_bytes, suffix_hash)?;

        let active = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&active_path)
            .map_err(|error| {
                io_error("open active delta for prefix recovery", &active_path, error)
            })?;
        if active
            .metadata()
            .map_err(|error| io_error("inspect active delta", &active_path, error))?
            .len()
            != inspection.total_file_bytes
        {
            return Err(GenerationError::Invalid(
                "active delta changed before exact-prefix truncation".into(),
            ));
        }
        active
            .set_len(inspection.valid_prefix_bytes)
            .map_err(|error| {
                io_error("truncate active delta to valid prefix", &active_path, error)
            })?;
        active
            .sync_all()
            .map_err(|error| io_error("fsync recovered active delta", &active_path, error))?;
        fsync_directory(&self.root)?;
        let recovered = inspect_active_log(&active_path, &generation)?;
        if recovered.suffix != TornSuffix::Clean
            || recovered.total_file_bytes != inspection.valid_prefix_bytes
            || recovered.complete_file_hash != inspection.valid_prefix_hash
        {
            return Err(GenerationError::Invalid(
                "active delta did not verify as the exact pre-quarantine valid prefix".into(),
            ));
        }
        lock.release()?;
        Ok(ActiveSuffixQuarantine {
            generation_pin: generation.pin,
            active_log_path: active_path,
            quarantine_path,
            original_file_hash: inspection.complete_file_hash,
            valid_prefix_hash: inspection.valid_prefix_hash,
            quarantined_suffix_hash: suffix_hash,
            valid_prefix_bytes: inspection.valid_prefix_bytes,
            quarantined_suffix_bytes: suffix_bytes.len() as u64,
            classification: inspection.suffix,
        })
    }

    /// Audit all immutable manifest heads still present in this store. This is
    /// deliberately separate from the bounded hot-path load. A stale pointer is
    /// recoverable with [`Self::activate_existing_successor`]; it is never
    /// silently advanced.
    pub fn audit(&self, expected_binding: &RecoveryDagBinding) -> Result<StoreAudit> {
        let current = self.load_current(expected_binding, None)?;
        let manifests = self.read_all_manifests(expected_binding)?;
        let generation_count = manifests.len();
        let mut referenced = HashSet::new();
        for (hash, manifest) in &manifests {
            if let Some(parent) = manifest.previous_generation {
                let parent_manifest = manifests.get(&parent).ok_or_else(|| {
                    GenerationError::Invalid(format!(
                        "generation {hash} references missing parent {parent}"
                    ))
                })?;
                if parent_manifest.sequence.checked_add(1) != Some(manifest.sequence) {
                    return Err(GenerationError::Invalid(format!(
                        "generation {hash} sequence does not follow parent {parent}"
                    )));
                }
                referenced.insert(parent);
            } else if manifest.sequence != 0 {
                return Err(GenerationError::Invalid(format!(
                    "generation {hash} has no parent at nonzero sequence {}",
                    manifest.sequence
                )));
            }
        }
        let mut heads: Vec<GenerationPin> = manifests
            .iter()
            .filter(|(hash, _)| !referenced.contains(hash))
            .map(|(hash, manifest)| GenerationPin {
                sequence: manifest.sequence,
                hash: *hash,
            })
            .collect();
        heads.sort_by_key(|pin| (pin.sequence, pin.hash.0));
        let status = if heads.len() == 1 && heads[0] == current.pin {
            StoreAuditStatus::Clean
        } else if heads.len() == 1 {
            StoreAuditStatus::PointerBehind { heads }
        } else {
            StoreAuditStatus::Forked { heads }
        };
        Ok(StoreAudit {
            current,
            generation_count,
            status,
        })
    }

    /// Complete only the pointer phase for an already fsynced direct successor.
    /// This is the explicit recovery path for a crash between generation publish
    /// and `CURRENT` replacement.
    pub fn activate_existing_successor(
        &self,
        expected_current: GenerationPin,
        successor_hash: Hash256,
    ) -> Result<VerifiedGeneration> {
        self.ensure_root()?;
        let lock = StoreLock::acquire(&self.root)?;
        let current_pointer = self.read_current_pointer()?;
        let actual = GenerationPin {
            sequence: current_pointer.sequence,
            hash: current_pointer.generation_hash,
        };
        if actual != expected_current {
            return Err(GenerationError::PinMismatch {
                expected_sequence: expected_current.sequence,
                expected_hash: expected_current.hash,
                actual_sequence: actual.sequence,
                actual_hash: actual.hash,
            });
        }
        let current_manifest = self.read_manifest(actual.hash)?;
        let current = self.verify_generation(actual.hash, &current_manifest.binding)?;
        let successor = self.verify_generation(successor_hash, &current.manifest.binding)?;
        validate_successor(&current, &successor.manifest)?;
        self.ensure_empty_active_log(&successor)?;
        fsync_directory(&self.root)?;
        let mut observer = NoopObserver;
        self.publish_pointer(&successor, &mut observer)?;
        lock.release()?;
        Ok(successor)
    }

    fn publish_generation<I, O>(
        &self,
        previous: Option<&VerifiedGeneration>,
        input: GenerationInput,
        records: I,
        observer: &mut O,
    ) -> Result<VerifiedGeneration>
    where
        I: IntoIterator<Item = RetainedDagRecord>,
        O: PublishObserver,
    {
        validate_input(&input)?;
        if let Some(old) = previous
            && old.manifest.binding != input.binding
        {
            return Err(GenerationError::Invalid(
                "successor binding differs from current generation".into(),
            ));
        }
        let sequence =
            match previous {
                Some(old) => old.pin.sequence.checked_add(1).ok_or_else(|| {
                    GenerationError::Invalid("generation sequence overflow".into())
                })?,
                None => 0,
            };
        let previous_generation = previous.map(|old| old.pin.hash);
        let pending = self.root.join(format!(".pending-{}", uuid::Uuid::new_v4()));
        create_private_directory(&pending)?;

        let retained_records = write_record_log(
            &pending.join(RECORDS_FILE),
            input.retention_limits,
            &input.dag_cursor,
            records,
        )?;
        observer.reached(PublishPoint::RecordsSynced)?;
        let manifest = GenerationManifest {
            schema: MANIFEST_SCHEMA.to_owned(),
            sequence,
            previous_generation,
            binding: input.binding,
            baseline_state: input.baseline_state,
            dag_cursor: input.dag_cursor,
            retained_records,
        };
        validate_manifest(&manifest)?;
        if let Some(old) = previous {
            validate_successor(old, &manifest)?;
        }
        let manifest_bytes = canonical_json(&manifest, "manifest")?;
        let generation_hash =
            domain_hash("ARC recovery DAG generation manifest v1", &manifest_bytes);
        let manifest_path = pending.join(MANIFEST_FILE);
        write_new_synced_file(&manifest_path, &manifest_bytes)?;
        observer.reached(PublishPoint::ManifestSynced)?;
        fsync_directory(&pending)?;
        observer.reached(PublishPoint::GenerationDirectorySynced)?;

        let final_directory = self.generation_path(generation_hash);
        if final_directory.exists() {
            return Err(GenerationError::Invalid(format!(
                "content-addressed generation {generation_hash} already exists"
            )));
        }
        fs::rename(&pending, &final_directory)
            .map_err(|error| io_error("rename generation into", &final_directory, error))?;
        observer.reached(PublishPoint::GenerationPublished)?;
        fsync_directory(&self.root)?;
        observer.reached(PublishPoint::RootAfterGenerationSynced)?;

        let generation = self.verify_generation(generation_hash, &manifest.binding)?;
        self.create_empty_active_log(&generation)?;
        observer.reached(PublishPoint::ActiveLogSynced)?;
        fsync_directory(&self.root)?;
        observer.reached(PublishPoint::RootAfterActiveLogSynced)?;
        self.publish_pointer(&generation, observer)?;
        Ok(generation)
    }

    fn publish_pointer<O: PublishObserver>(
        &self,
        generation: &VerifiedGeneration,
        observer: &mut O,
    ) -> Result<()> {
        let pointer = CurrentPointer {
            schema: POINTER_SCHEMA.to_owned(),
            generation_hash: generation.pin.hash,
            active_log_generation_hash: generation.pin.hash,
            sequence: generation.pin.sequence,
            previous_generation: generation.manifest.previous_generation,
        };
        let pointer_bytes = canonical_json(&pointer, "CURRENT pointer")?;
        let temporary = self
            .root
            .join(format!(".CURRENT-{}.tmp", uuid::Uuid::new_v4()));
        write_new_synced_file(&temporary, &pointer_bytes)?;
        observer.reached(PublishPoint::PointerFileSynced)?;
        let current = self.root.join(CURRENT_FILE);
        fs::rename(&temporary, &current)
            .map_err(|error| io_error("atomically replace", &current, error))?;
        observer.reached(PublishPoint::PointerRenamed)?;
        fsync_directory(&self.root)?;
        observer.reached(PublishPoint::RootAfterPointerSynced)?;
        Ok(())
    }

    fn read_current_pointer(&self) -> Result<CurrentPointer> {
        let path = self.root.join(CURRENT_FILE);
        if !path.exists() {
            return Err(GenerationError::NoCurrent(path));
        }
        let bytes = read_small_regular_file(&path, MAX_POINTER_BYTES)?;
        let pointer: CurrentPointer = serde_json::from_slice(&bytes).map_err(|error| {
            GenerationError::Invalid(format!("CURRENT JSON is invalid: {error}"))
        })?;
        require_canonical_json(&bytes, &pointer, "CURRENT pointer")?;
        if pointer.schema != POINTER_SCHEMA {
            return Err(GenerationError::Invalid(format!(
                "unsupported CURRENT schema {}",
                pointer.schema
            )));
        }
        if pointer.active_log_generation_hash != pointer.generation_hash {
            return Err(GenerationError::Invalid(
                "CURRENT generation and active-log selector differ".into(),
            ));
        }
        if pointer.sequence == 0 && pointer.previous_generation.is_some()
            || pointer.sequence > 0 && pointer.previous_generation.is_none()
        {
            return Err(GenerationError::Invalid(
                "CURRENT sequence/previous-generation relationship is invalid".into(),
            ));
        }
        Ok(pointer)
    }

    fn read_manifest(&self, hash: Hash256) -> Result<GenerationManifest> {
        let directory = self.generation_path(hash);
        ensure_real_directory(&directory)?;
        let bytes = read_small_regular_file(&directory.join(MANIFEST_FILE), MAX_MANIFEST_BYTES)?;
        let actual = domain_hash("ARC recovery DAG generation manifest v1", &bytes);
        if actual != hash {
            return Err(GenerationError::Invalid(format!(
                "manifest hash {actual} differs from generation directory {hash}"
            )));
        }
        let manifest: GenerationManifest = serde_json::from_slice(&bytes).map_err(|error| {
            GenerationError::Invalid(format!("manifest JSON is invalid: {error}"))
        })?;
        require_canonical_json(&bytes, &manifest, "manifest")?;
        validate_manifest(&manifest)?;
        Ok(manifest)
    }

    fn read_all_manifests(
        &self,
        expected_binding: &RecoveryDagBinding,
    ) -> Result<HashMap<Hash256, GenerationManifest>> {
        let mut manifests = HashMap::new();
        let entries = fs::read_dir(&self.root)
            .map_err(|error| io_error("read directory", &self.root, error))?;
        for entry in entries {
            let entry = entry.map_err(|error| io_error("read entry in", &self.root, error))?;
            let name = entry.file_name();
            let name = name.to_string_lossy();
            let Some(hex_hash) = name.strip_prefix("gen-") else {
                continue;
            };
            if manifests.len() >= MAX_GENERATIONS_TO_AUDIT {
                return Err(GenerationError::Invalid(format!(
                    "generation audit exceeds hard cap {MAX_GENERATIONS_TO_AUDIT}"
                )));
            }
            let hash = Hash256::from_hex(hex_hash).map_err(|_| {
                GenerationError::Invalid(format!("malformed generation directory {name}"))
            })?;
            let manifest = self.read_manifest(hash)?;
            if &manifest.binding != expected_binding {
                return Err(GenerationError::Invalid(format!(
                    "generation {hash} has a foreign recovery/domain/validator binding"
                )));
            }
            if manifests.insert(hash, manifest).is_some() {
                return Err(GenerationError::Invalid(format!(
                    "duplicate generation hash {hash}"
                )));
            }
        }
        Ok(manifests)
    }

    fn generation_directory_count(&self) -> Result<usize> {
        let entries = fs::read_dir(&self.root)
            .map_err(|error| io_error("read directory", &self.root, error))?;
        let mut count = 0usize;
        for entry in entries {
            let entry = entry.map_err(|error| io_error("read entry in", &self.root, error))?;
            if entry.file_name().to_string_lossy().starts_with("gen-") {
                count = count.saturating_add(1);
            }
        }
        Ok(count)
    }

    fn generation_path(&self, hash: Hash256) -> PathBuf {
        self.root.join(format!("gen-{}", hash.to_hex()))
    }

    pub fn active_log_path(&self, pin: GenerationPin) -> PathBuf {
        self.root.join(format!("active-{}.bin", pin.hash.to_hex()))
    }

    fn create_empty_active_log(&self, generation: &VerifiedGeneration) -> Result<()> {
        let path = self.active_log_path(generation.pin);
        let header = ActiveLogHeader {
            generation_pin: generation.pin,
            binding: generation.manifest.binding.clone(),
            limits: generation.manifest.retained_records.limits,
        };
        let bytes = encode_active_header(&header);
        write_new_synced_file(&path, &bytes)
    }

    fn ensure_empty_active_log(&self, generation: &VerifiedGeneration) -> Result<()> {
        let path = self.active_log_path(generation.pin);
        if !path.exists() {
            return self.create_empty_active_log(generation);
        }
        let inspection = inspect_active_log(&path, generation)?;
        if inspection.suffix != TornSuffix::Clean
            || inspection.batch_count != 0
            || inspection.record_count != 0
            || inspection.total_file_bytes != ACTIVE_HEADER_BYTES
        {
            return Err(GenerationError::Invalid(
                "unselected successor has a non-empty or torn active log".into(),
            ));
        }
        Ok(())
    }

    fn ensure_root(&self) -> Result<()> {
        if self.root.exists() {
            ensure_real_directory(&self.root)?;
            return Ok(());
        }
        let parent = self.root.parent().ok_or_else(|| {
            GenerationError::Invalid("generation store path has no parent".into())
        })?;
        ensure_real_directory(parent)?;
        create_private_directory(&self.root)?;
        fsync_directory(parent)?;
        Ok(())
    }

    #[cfg(test)]
    fn append_with_observer<I, O>(
        &self,
        expected_current: GenerationPin,
        input: GenerationInput,
        records: I,
        observer: &mut O,
    ) -> Result<VerifiedGeneration>
    where
        I: IntoIterator<Item = RetainedDagRecord>,
        O: PublishObserver,
    {
        self.ensure_root()?;
        let lock = StoreLock::acquire(&self.root)?;
        let current = self.load_current(&input.binding, Some(expected_current))?;
        let active = inspect_active_log(&self.active_log_path(current.pin), &current)?;
        require_empty_active_for_direct_append(&active)?;
        let result = self.publish_generation(Some(&current), input, records, observer);
        lock.release()?;
        result
    }
}

struct StoreLock {
    path: PathBuf,
    file: File,
    released: bool,
}

impl StoreLock {
    fn acquire(root: &Path) -> Result<Self> {
        let path = root.join(WRITE_LOCK_FILE);
        if path.exists() {
            regular_file_metadata(&path)?;
        }
        let mut options = OpenOptions::new();
        options.read(true).write(true).create(true);
        set_private_file_mode(&mut options);
        let mut file = options
            .open(&path)
            .map_err(|error| io_error("open/create", &path, error))?;
        if !file
            .metadata()
            .map_err(|error| io_error("inspect open", &path, error))?
            .is_file()
        {
            return Err(GenerationError::Invalid(format!(
                "{} is not a regular lock file",
                path.display()
            )));
        }
        match file.try_lock() {
            Ok(()) => {}
            Err(std::fs::TryLockError::WouldBlock) => {
                return Err(GenerationError::Locked(path));
            }
            Err(std::fs::TryLockError::Error(error)) => {
                return Err(io_error("acquire advisory lock on", &path, error));
            }
        }
        file.set_len(0)
            .map_err(|error| io_error("truncate", &path, error))?;
        file.seek(SeekFrom::Start(0))
            .map_err(|error| io_error("seek", &path, error))?;
        let value = format!(
            "schema=arc.recovery.dag-wal-lock.v2\npid={}\nnonce={}\n",
            std::process::id(),
            uuid::Uuid::new_v4()
        );
        file.write_all(value.as_bytes())
            .map_err(|error| io_error("write", &path, error))?;
        file.sync_all()
            .map_err(|error| io_error("fsync", &path, error))?;
        fsync_directory(root)?;
        Ok(Self {
            path,
            file,
            released: false,
        })
    }

    fn release(mut self) -> Result<()> {
        self.file
            .unlock()
            .map_err(|error| io_error("release advisory lock on", &self.path, error))?;
        self.released = true;
        Ok(())
    }
}

impl Drop for StoreLock {
    fn drop(&mut self) {
        if !self.released {
            let _ = self.file.unlock();
        }
    }
}

fn validate_input(input: &GenerationInput) -> Result<()> {
    validate_binding(&input.binding)?;
    validate_baseline(&input.baseline_state)?;
    validate_cursor(&input.dag_cursor)?;
    validate_limits(input.retention_limits)
}

fn require_empty_active_for_direct_append(active: &ActiveLogInspection) -> Result<()> {
    if active.suffix != TornSuffix::Clean
        || active.batch_count != 0
        || active.record_count != 0
        || active.total_file_bytes != ACTIVE_HEADER_BYTES
    {
        return Err(GenerationError::Invalid(
            "direct generation append requires an empty clean active delta; use append_compacted with the exact active pin"
                .into(),
        ));
    }
    Ok(())
}

fn validate_manifest(manifest: &GenerationManifest) -> Result<()> {
    if manifest.schema != MANIFEST_SCHEMA {
        return Err(GenerationError::Invalid(format!(
            "unsupported manifest schema {}",
            manifest.schema
        )));
    }
    if manifest.sequence == 0 && manifest.previous_generation.is_some()
        || manifest.sequence > 0 && manifest.previous_generation.is_none()
    {
        return Err(GenerationError::Invalid(
            "manifest sequence/previous-generation relationship is invalid".into(),
        ));
    }
    validate_binding(&manifest.binding)?;
    validate_baseline(&manifest.baseline_state)?;
    validate_cursor(&manifest.dag_cursor)?;
    validate_limits(manifest.retained_records.limits)?;
    let records = &manifest.retained_records;
    if records.record_count > records.limits.max_records {
        return Err(GenerationError::Invalid(
            "manifest record count exceeds its bound".into(),
        ));
    }
    if records.payload_bytes > records.limits.max_payload_bytes {
        return Err(GenerationError::Invalid(
            "manifest payload bytes exceed their bound".into(),
        ));
    }
    let maximum_file_bytes = maximum_record_file_bytes(records.limits)?;
    if records.file_bytes < RECORD_MAGIC.len() as u64 || records.file_bytes > maximum_file_bytes {
        return Err(GenerationError::Invalid(
            "manifest record file size is outside its hard bounds".into(),
        ));
    }
    if records.record_count == 0 {
        if records.first_round.is_some() || records.last_round.is_some() {
            return Err(GenerationError::Invalid(
                "empty record set must not claim round bounds".into(),
            ));
        }
    } else if records.first_round.is_none() || records.last_round.is_none() {
        return Err(GenerationError::Invalid(
            "non-empty record set must commit to both round bounds".into(),
        ));
    } else if records.first_round > records.last_round {
        return Err(GenerationError::Invalid(
            "record-set minimum round exceeds its maximum round".into(),
        ));
    }
    Ok(())
}

fn validate_binding(binding: &RecoveryDagBinding) -> Result<()> {
    if binding.recovery_manifest_hash == Hash256::ZERO
        || binding.recovery_domain == Hash256::ZERO
        || binding.validator_set_commitment == Hash256::ZERO
    {
        return Err(GenerationError::Invalid(
            "recovery manifest, domain, and validator-set commitments must be nonzero".into(),
        ));
    }
    Ok(())
}

fn validate_baseline(baseline: &BaselineState) -> Result<()> {
    if baseline.block_hash == Hash256::ZERO || baseline.state_root == Hash256::ZERO {
        return Err(GenerationError::Invalid(
            "baseline block hash and state root must be nonzero".into(),
        ));
    }
    Ok(())
}

fn validate_cursor(cursor: &DagCursor) -> Result<()> {
    if cursor.retention_floor_round > cursor.next_dag_round
        || cursor.next_dag_round > cursor.current_round
        || cursor.current_round > cursor.retention_ceiling_round
    {
        return Err(GenerationError::Invalid(
            "DAG cursor is outside its declared retention window".into(),
        ));
    }
    let span = cursor
        .retention_ceiling_round
        .checked_sub(cursor.retention_floor_round)
        .ok_or_else(|| GenerationError::Invalid("retention round span underflow".into()))?;
    if span > HARD_MAX_RETENTION_ROUND_SPAN {
        return Err(GenerationError::Invalid(format!(
            "retention round span {span} exceeds hard cap {HARD_MAX_RETENTION_ROUND_SPAN}"
        )));
    }
    Ok(())
}

fn validate_limits(limits: RetentionLimits) -> Result<()> {
    if limits.max_records == 0 || limits.max_records > HARD_MAX_RETAINED_RECORDS {
        return Err(GenerationError::Invalid(format!(
            "record limit must be in 1..={HARD_MAX_RETAINED_RECORDS}"
        )));
    }
    if limits.max_payload_bytes == 0 || limits.max_payload_bytes > HARD_MAX_RETAINED_PAYLOAD_BYTES {
        return Err(GenerationError::Invalid(format!(
            "payload limit must be in 1..={HARD_MAX_RETAINED_PAYLOAD_BYTES}"
        )));
    }
    Ok(())
}

fn validate_successor(previous: &VerifiedGeneration, successor: &GenerationManifest) -> Result<()> {
    let expected_sequence = previous
        .pin
        .sequence
        .checked_add(1)
        .ok_or_else(|| GenerationError::Invalid("generation sequence overflow".into()))?;
    if successor.previous_generation != Some(previous.pin.hash)
        || successor.sequence != expected_sequence
    {
        return Err(GenerationError::Invalid(
            "successor does not directly extend the current generation".into(),
        ));
    }
    if successor.binding != previous.manifest.binding {
        return Err(GenerationError::Invalid(
            "successor changes the recovery/domain/validator binding".into(),
        ));
    }
    let old_baseline = &previous.manifest.baseline_state;
    let new_baseline = &successor.baseline_state;
    if new_baseline.height < old_baseline.height {
        return Err(GenerationError::Invalid(
            "successor baseline height moves backwards".into(),
        ));
    }
    if new_baseline.height == old_baseline.height && new_baseline != old_baseline {
        return Err(GenerationError::Invalid(
            "successor changes block hash/root at the same baseline height".into(),
        ));
    }
    let old_cursor = &previous.manifest.dag_cursor;
    let new_cursor = &successor.dag_cursor;
    if new_cursor.committed_block_count < old_cursor.committed_block_count
        || new_cursor.next_dag_round < old_cursor.next_dag_round
        || new_cursor.current_round < old_cursor.current_round
    {
        return Err(GenerationError::Invalid(
            "successor moves a committed-count or DAG round cursor backwards".into(),
        ));
    }
    Ok(())
}

fn validate_inspection_window(inspection: &RecordLogInspection, cursor: &DagCursor) -> Result<()> {
    if let (Some(first), Some(last)) = (inspection.first_round, inspection.last_round)
        && (first < cursor.retention_floor_round || last > cursor.retention_ceiling_round)
    {
        return Err(GenerationError::Invalid(
            "record log rounds escape the manifest retention window".into(),
        ));
    }
    Ok(())
}

fn require_inspection_matches_manifest(
    inspection: &RecordLogInspection,
    expected: &RetainedRecordSet,
) -> Result<()> {
    if inspection.record_count != expected.record_count
        || inspection.payload_bytes != expected.payload_bytes
        || inspection.total_file_bytes != expected.file_bytes
        || inspection.valid_prefix_bytes != expected.file_bytes
        || inspection.first_round != expected.first_round
        || inspection.last_round != expected.last_round
        || inspection.valid_prefix_hash != expected.records_file_hash
        || inspection.complete_file_hash != expected.records_file_hash
    {
        return Err(GenerationError::Invalid(
            "record log metadata/hash differs from its generation manifest".into(),
        ));
    }
    Ok(())
}

fn validate_record(record: &RetainedDagRecord) -> Result<()> {
    if record.payload.len() as u64 > HARD_MAX_SINGLE_RECORD_PAYLOAD_BYTES {
        return Err(GenerationError::Invalid(format!(
            "single retained payload exceeds {HARD_MAX_SINGLE_RECORD_PAYLOAD_BYTES} bytes"
        )));
    }
    match record.kind {
        RetainedRecordKind::TransactionBody | RetainedRecordKind::DagBlock => {
            if record.object_hash == Hash256::ZERO || record.payload.is_empty() {
                return Err(GenerationError::Invalid(
                    "transaction/DAG-block records require nonzero identity and payload".into(),
                ));
            }
        }
        RetainedRecordKind::RoundCursor => {
            if record.object_hash != Hash256::ZERO || !record.payload.is_empty() {
                return Err(GenerationError::Invalid(
                    "round-cursor records require zero identity and empty payload".into(),
                ));
            }
        }
        RetainedRecordKind::Commit => {
            if record.object_hash == Hash256::ZERO || !record.payload.is_empty() {
                return Err(GenerationError::Invalid(
                    "commit records require a nonzero block identity and empty payload".into(),
                ));
            }
        }
    }
    Ok(())
}

fn validate_record_identity(
    record: &RetainedDagRecord,
    seen: &mut HashSet<(RetainedRecordKind, u64, [u8; 32])>,
) -> Result<()> {
    validate_record(record)?;
    if !seen.insert(record_identity(record)) {
        return Err(GenerationError::Invalid(
            "duplicate retained record identity in one generation".into(),
        ));
    }
    Ok(())
}

fn record_identity(record: &RetainedDagRecord) -> (RetainedRecordKind, u64, [u8; 32]) {
    (record.kind, record.round, record.object_hash.0)
}

fn record_payload_fingerprint(record: &RetainedDagRecord) -> Hash256 {
    domain_hash(
        "ARC recovery DAG record payload identity v1",
        &record.payload,
    )
}

fn write_record_log<I>(
    path: &Path,
    limits: RetentionLimits,
    cursor: &DagCursor,
    records: I,
) -> Result<RetainedRecordSet>
where
    I: IntoIterator<Item = RetainedDagRecord>,
{
    validate_limits(limits)?;
    validate_cursor(cursor)?;
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    set_private_file_mode(&mut options);
    let mut file = options
        .open(path)
        .map_err(|error| io_error("create", path, error))?;
    let mut hasher = domain_hasher("ARC recovery DAG retained records v1");
    file.write_all(RECORD_MAGIC)
        .map_err(|error| io_error("write", path, error))?;
    hasher.update(RECORD_MAGIC);

    let mut record_count = 0u64;
    let mut payload_bytes = 0u64;
    let mut file_bytes = RECORD_MAGIC.len() as u64;
    let mut first_round: Option<u64> = None;
    let mut last_round: Option<u64> = None;
    let mut seen = HashSet::new();
    for record in records {
        validate_record_identity(&record, &mut seen)?;
        if record.round < cursor.retention_floor_round
            || record.round > cursor.retention_ceiling_round
        {
            return Err(GenerationError::Invalid(format!(
                "retained record round {} is outside {}..={}",
                record.round, cursor.retention_floor_round, cursor.retention_ceiling_round
            )));
        }
        record_count = record_count
            .checked_add(1)
            .ok_or_else(|| GenerationError::Invalid("record count overflow".into()))?;
        if record_count > limits.max_records {
            return Err(GenerationError::Invalid(
                "retained record count exceeds configured bound".into(),
            ));
        }
        payload_bytes = payload_bytes
            .checked_add(record.payload.len() as u64)
            .ok_or_else(|| GenerationError::Invalid("payload byte count overflow".into()))?;
        if payload_bytes > limits.max_payload_bytes {
            return Err(GenerationError::Invalid(
                "retained payload bytes exceed configured bound".into(),
            ));
        }
        let encoded = encode_record(&record)?;
        let length = u32::try_from(encoded.len())
            .map_err(|_| GenerationError::Invalid("encoded record is too large".into()))?;
        let length_bytes = length.to_be_bytes();
        let checksum = domain_hash("ARC recovery DAG retained record frame v1", &encoded);
        for bytes in [
            length_bytes.as_slice(),
            encoded.as_slice(),
            checksum.as_ref(),
        ] {
            file.write_all(bytes)
                .map_err(|error| io_error("write", path, error))?;
            hasher.update(bytes);
        }
        file_bytes = file_bytes
            .checked_add(4 + encoded.len() as u64 + 32)
            .ok_or_else(|| GenerationError::Invalid("record file byte count overflow".into()))?;
        first_round = Some(first_round.map_or(record.round, |round| round.min(record.round)));
        last_round = Some(last_round.map_or(record.round, |round| round.max(record.round)));
    }
    file.sync_all()
        .map_err(|error| io_error("fsync", path, error))?;
    if file_bytes > maximum_record_file_bytes(limits)? {
        return Err(GenerationError::Invalid(
            "record file exceeds the configured hard bound".into(),
        ));
    }
    let records_file_hash = Hash256(*hasher.finalize().as_bytes());
    Ok(RetainedRecordSet {
        limits,
        record_count,
        payload_bytes,
        file_bytes,
        first_round,
        last_round,
        records_file_hash,
    })
}

/// Inspect a framed record log and classify only structurally incomplete final
/// bytes as a torn suffix. A full frame with a bad checksum or invalid record is
/// corruption and returns an error; it is never downgraded to a recoverable tear.
pub fn inspect_record_log(path: &Path, limits: RetentionLimits) -> Result<RecordLogInspection> {
    scan_record_log(path, limits, |_| Ok(()))
}

fn scan_record_log<F>(
    path: &Path,
    limits: RetentionLimits,
    mut visitor: F,
) -> Result<RecordLogInspection>
where
    F: FnMut(RetainedDagRecord) -> Result<()>,
{
    validate_limits(limits)?;
    let metadata = regular_file_metadata(path)?;
    let total_file_bytes = metadata.len();
    if total_file_bytes > maximum_record_file_bytes(limits)? {
        return Err(GenerationError::Invalid(format!(
            "record file has {total_file_bytes} bytes, above its configured hard bound"
        )));
    }
    let file = File::open(path).map_err(|error| io_error("open", path, error))?;
    let mut reader = BufReader::new(file);
    let mut full_hasher = domain_hasher("ARC recovery DAG retained records v1");
    let mut prefix_hasher = domain_hasher("ARC recovery DAG retained records v1");
    let mut header = [0u8; 8];
    let header_read =
        read_up_to(&mut reader, &mut header).map_err(|error| io_error("read", path, error))?;
    let mut total_observed_bytes = header_read as u64;
    full_hasher.update(&header[..header_read]);
    if header_read < header.len() {
        if total_observed_bytes != total_file_bytes {
            return Err(GenerationError::Invalid(
                "record file length changed while it was being inspected".into(),
            ));
        }
        return Ok(RecordLogInspection {
            record_count: 0,
            payload_bytes: 0,
            valid_prefix_bytes: 0,
            total_file_bytes,
            first_round: None,
            last_round: None,
            valid_prefix_hash: Hash256(*prefix_hasher.finalize().as_bytes()),
            complete_file_hash: Hash256(*full_hasher.finalize().as_bytes()),
            suffix: TornSuffix::TruncatedHeader {
                present_bytes: header_read as u64,
                expected_bytes: header.len() as u64,
            },
        });
    }
    if &header != RECORD_MAGIC {
        return Err(GenerationError::Invalid(
            "record log has the wrong magic".into(),
        ));
    }
    prefix_hasher.update(&header);

    let mut record_count = 0u64;
    let mut payload_bytes = 0u64;
    let mut valid_prefix_bytes = header.len() as u64;
    let mut first_round: Option<u64> = None;
    let mut last_round: Option<u64> = None;
    let mut seen = HashSet::new();
    let suffix;
    loop {
        let mut length_bytes = [0u8; 4];
        let length_read = read_up_to(&mut reader, &mut length_bytes)
            .map_err(|error| io_error("read", path, error))?;
        total_observed_bytes = total_observed_bytes
            .checked_add(length_read as u64)
            .ok_or_else(|| GenerationError::Invalid("observed byte count overflow".into()))?;
        full_hasher.update(&length_bytes[..length_read]);
        if length_read == 0 {
            suffix = TornSuffix::Clean;
            break;
        }
        if length_read < length_bytes.len() {
            suffix = TornSuffix::PartialLength {
                present_bytes: length_read as u64,
                expected_bytes: length_bytes.len() as u64,
            };
            break;
        }
        let encoded_length = u32::from_be_bytes(length_bytes) as usize;
        let maximum_encoded =
            usize::try_from(FRAME_FIXED_BODY_BYTES + HARD_MAX_SINGLE_RECORD_PAYLOAD_BYTES)
                .expect("hard record cap fits usize");
        if encoded_length < FRAME_FIXED_BODY_BYTES as usize || encoded_length > maximum_encoded {
            return Err(GenerationError::Invalid(format!(
                "record frame length {encoded_length} is outside hard bounds"
            )));
        }
        let mut encoded = vec![0u8; encoded_length];
        let encoded_read =
            read_up_to(&mut reader, &mut encoded).map_err(|error| io_error("read", path, error))?;
        total_observed_bytes = total_observed_bytes
            .checked_add(encoded_read as u64)
            .ok_or_else(|| GenerationError::Invalid("observed byte count overflow".into()))?;
        full_hasher.update(&encoded[..encoded_read]);
        if encoded_read < encoded_length {
            suffix = TornSuffix::PartialPayload {
                present_bytes: encoded_read as u64,
                expected_bytes: encoded_length as u64,
            };
            break;
        }
        let mut checksum = [0u8; 32];
        let checksum_read = read_up_to(&mut reader, &mut checksum)
            .map_err(|error| io_error("read", path, error))?;
        total_observed_bytes = total_observed_bytes
            .checked_add(checksum_read as u64)
            .ok_or_else(|| GenerationError::Invalid("observed byte count overflow".into()))?;
        full_hasher.update(&checksum[..checksum_read]);
        if checksum_read < checksum.len() {
            suffix = TornSuffix::PartialChecksum {
                present_bytes: checksum_read as u64,
                expected_bytes: checksum.len() as u64,
            };
            break;
        }
        let expected_checksum = domain_hash("ARC recovery DAG retained record frame v1", &encoded);
        if checksum != expected_checksum.0 {
            return Err(GenerationError::Invalid(format!(
                "record frame {} has a complete but invalid checksum",
                record_count
            )));
        }
        let record = decode_record(&encoded)?;
        validate_record_identity(&record, &mut seen)?;
        record_count = record_count
            .checked_add(1)
            .ok_or_else(|| GenerationError::Invalid("record count overflow".into()))?;
        if record_count > limits.max_records {
            return Err(GenerationError::Invalid(
                "record log exceeds its configured record bound".into(),
            ));
        }
        payload_bytes = payload_bytes
            .checked_add(record.payload.len() as u64)
            .ok_or_else(|| GenerationError::Invalid("payload byte count overflow".into()))?;
        if payload_bytes > limits.max_payload_bytes {
            return Err(GenerationError::Invalid(
                "record log exceeds its configured payload bound".into(),
            ));
        }
        first_round = Some(first_round.map_or(record.round, |round| round.min(record.round)));
        last_round = Some(last_round.map_or(record.round, |round| round.max(record.round)));
        visitor(record)?;
        prefix_hasher.update(&length_bytes);
        prefix_hasher.update(&encoded);
        prefix_hasher.update(&checksum);
        valid_prefix_bytes = valid_prefix_bytes
            .checked_add(4 + encoded_length as u64 + 32)
            .ok_or_else(|| GenerationError::Invalid("valid prefix size overflow".into()))?;
    }

    // Both a clean scan and every accepted torn suffix end at physical EOF.
    if total_observed_bytes != total_file_bytes {
        return Err(GenerationError::Invalid(
            "record file length changed while it was being inspected".into(),
        ));
    }
    Ok(RecordLogInspection {
        record_count,
        payload_bytes,
        valid_prefix_bytes,
        total_file_bytes,
        first_round,
        last_round,
        valid_prefix_hash: Hash256(*prefix_hasher.finalize().as_bytes()),
        complete_file_hash: Hash256(*full_hasher.finalize().as_bytes()),
        suffix,
    })
}

/// Inspect the active delta bound to `generation`. Only an incomplete final
/// batch frame is classified as torn. A complete bad checksum, malformed batch,
/// invalid record, out-of-window round, or cap violation is fatal corruption.
pub fn inspect_active_log(
    path: &Path,
    generation: &VerifiedGeneration,
) -> Result<ActiveLogInspection> {
    scan_active_log(path, generation, |_| Ok(()))
}

fn scan_active_log<F>(
    path: &Path,
    generation: &VerifiedGeneration,
    mut visitor: F,
) -> Result<ActiveLogInspection>
where
    F: FnMut(RetainedDagRecord) -> Result<()>,
{
    let limits = generation.manifest.retained_records.limits;
    validate_limits(limits)?;
    let metadata = regular_file_metadata(path)?;
    let total_file_bytes = metadata.len();
    if total_file_bytes < ACTIVE_HEADER_BYTES {
        return Err(GenerationError::Invalid(
            "active delta is missing its complete generation-bound header".into(),
        ));
    }
    if total_file_bytes > maximum_active_file_bytes(limits)? {
        return Err(GenerationError::Invalid(format!(
            "active delta has {total_file_bytes} bytes, above its configured hard bound"
        )));
    }
    let file = File::open(path).map_err(|error| io_error("open", path, error))?;
    let mut reader = BufReader::new(file);
    let mut header_bytes = vec![0u8; ACTIVE_HEADER_BYTES as usize];
    let header_read = read_up_to(&mut reader, &mut header_bytes)
        .map_err(|error| io_error("read", path, error))?;
    if header_read != header_bytes.len() {
        return Err(GenerationError::Invalid(
            "active delta header was truncated while being read".into(),
        ));
    }
    let header = decode_active_header(&header_bytes)?;
    let expected_header = ActiveLogHeader {
        generation_pin: generation.pin,
        binding: generation.manifest.binding.clone(),
        limits,
    };
    if header != expected_header {
        return Err(GenerationError::Invalid(
            "active delta header differs from its selected generation/binding/limits".into(),
        ));
    }
    let mut full_hasher = domain_hasher("ARC recovery DAG active delta v1");
    let mut prefix_hasher = domain_hasher("ARC recovery DAG active delta v1");
    full_hasher.update(&header_bytes);
    prefix_hasher.update(&header_bytes);
    let mut total_observed_bytes = ACTIVE_HEADER_BYTES;
    let mut valid_prefix_bytes = ACTIVE_HEADER_BYTES;
    let mut batch_count = 0u64;
    let mut record_count = 0u64;
    let mut payload_bytes = 0u64;
    let mut first_round: Option<u64> = None;
    let mut last_round: Option<u64> = None;
    let mut seen = HashSet::new();
    let suffix;
    loop {
        let mut length_bytes = [0u8; 4];
        let length_read = read_up_to(&mut reader, &mut length_bytes)
            .map_err(|error| io_error("read", path, error))?;
        total_observed_bytes = total_observed_bytes
            .checked_add(length_read as u64)
            .ok_or_else(|| GenerationError::Invalid("active observed size overflow".into()))?;
        full_hasher.update(&length_bytes[..length_read]);
        if length_read == 0 {
            suffix = TornSuffix::Clean;
            break;
        }
        if length_read < length_bytes.len() {
            suffix = TornSuffix::PartialLength {
                present_bytes: length_read as u64,
                expected_bytes: length_bytes.len() as u64,
            };
            break;
        }
        let body_length = u32::from_be_bytes(length_bytes) as usize;
        let maximum_body = maximum_active_batch_body_bytes(limits)? as usize;
        if body_length < ACTIVE_BATCH_FIXED_BODY_BYTES as usize || body_length > maximum_body {
            return Err(GenerationError::Invalid(format!(
                "active batch frame length {body_length} is outside hard bounds"
            )));
        }
        let mut body = vec![0u8; body_length];
        let body_read =
            read_up_to(&mut reader, &mut body).map_err(|error| io_error("read", path, error))?;
        total_observed_bytes = total_observed_bytes
            .checked_add(body_read as u64)
            .ok_or_else(|| GenerationError::Invalid("active observed size overflow".into()))?;
        full_hasher.update(&body[..body_read]);
        if body_read < body_length {
            suffix = TornSuffix::PartialPayload {
                present_bytes: body_read as u64,
                expected_bytes: body_length as u64,
            };
            break;
        }
        let mut checksum = [0u8; 32];
        let checksum_read = read_up_to(&mut reader, &mut checksum)
            .map_err(|error| io_error("read", path, error))?;
        total_observed_bytes = total_observed_bytes
            .checked_add(checksum_read as u64)
            .ok_or_else(|| GenerationError::Invalid("active observed size overflow".into()))?;
        full_hasher.update(&checksum[..checksum_read]);
        if checksum_read < checksum.len() {
            suffix = TornSuffix::PartialChecksum {
                present_bytes: checksum_read as u64,
                expected_bytes: checksum.len() as u64,
            };
            break;
        }
        let expected_checksum = domain_hash("ARC recovery DAG active batch frame v1", &body);
        if checksum != expected_checksum.0 {
            return Err(GenerationError::Invalid(format!(
                "active batch {batch_count} has a complete but invalid checksum"
            )));
        }
        let batch = decode_active_batch(&body, batch_count, limits)?;
        let mut batch_payload = 0u64;
        for record in &batch {
            validate_record_identity(record, &mut seen)?;
            if record.round < generation.manifest.dag_cursor.retention_floor_round
                || record.round > generation.manifest.dag_cursor.retention_ceiling_round
            {
                return Err(GenerationError::Invalid(format!(
                    "active record round {} is outside the generation retention window",
                    record.round
                )));
            }
            batch_payload = batch_payload
                .checked_add(record.payload.len() as u64)
                .ok_or_else(|| GenerationError::Invalid("active payload size overflow".into()))?;
            first_round = Some(first_round.map_or(record.round, |round| round.min(record.round)));
            last_round = Some(last_round.map_or(record.round, |round| round.max(record.round)));
        }
        if batch_payload > HARD_MAX_ACTIVE_BATCH_PAYLOAD_BYTES {
            return Err(GenerationError::Invalid(
                "active batch exceeds its hard payload cap".into(),
            ));
        }
        let batch_records = batch.len() as u64;
        record_count = record_count
            .checked_add(batch_records)
            .ok_or_else(|| GenerationError::Invalid("active record count overflow".into()))?;
        payload_bytes = payload_bytes
            .checked_add(batch_payload)
            .ok_or_else(|| GenerationError::Invalid("active payload size overflow".into()))?;
        let combined_records = generation
            .manifest
            .retained_records
            .record_count
            .checked_add(record_count)
            .ok_or_else(|| GenerationError::Invalid("combined record count overflow".into()))?;
        let combined_payload = generation
            .manifest
            .retained_records
            .payload_bytes
            .checked_add(payload_bytes)
            .ok_or_else(|| GenerationError::Invalid("combined payload size overflow".into()))?;
        if combined_records > limits.max_records || combined_payload > limits.max_payload_bytes {
            return Err(GenerationError::Invalid(
                "active delta exceeds the generation's combined retention caps".into(),
            ));
        }
        for record in batch {
            visitor(record)?;
        }
        batch_count = batch_count
            .checked_add(1)
            .ok_or_else(|| GenerationError::Invalid("active batch count overflow".into()))?;
        prefix_hasher.update(&length_bytes);
        prefix_hasher.update(&body);
        prefix_hasher.update(&checksum);
        valid_prefix_bytes = valid_prefix_bytes
            .checked_add(4 + body_length as u64 + 32)
            .ok_or_else(|| GenerationError::Invalid("active prefix size overflow".into()))?;
    }
    if total_observed_bytes != total_file_bytes {
        return Err(GenerationError::Invalid(
            "active delta length changed while it was being inspected".into(),
        ));
    }
    Ok(ActiveLogInspection {
        generation_pin: generation.pin,
        batch_count,
        record_count,
        payload_bytes,
        valid_prefix_bytes,
        total_file_bytes,
        first_round,
        last_round,
        valid_prefix_hash: Hash256(*prefix_hasher.finalize().as_bytes()),
        complete_file_hash: Hash256(*full_hasher.finalize().as_bytes()),
        suffix,
    })
}

fn encode_active_header(header: &ActiveLogHeader) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(ACTIVE_HEADER_BYTES as usize);
    bytes.extend_from_slice(ACTIVE_MAGIC);
    bytes.push(ACTIVE_SCHEMA);
    bytes.extend_from_slice(header.generation_pin.hash.as_ref());
    bytes.extend_from_slice(&header.generation_pin.sequence.to_be_bytes());
    bytes.extend_from_slice(header.binding.recovery_manifest_hash.as_ref());
    bytes.extend_from_slice(header.binding.recovery_domain.as_ref());
    bytes.extend_from_slice(header.binding.validator_set_commitment.as_ref());
    bytes.extend_from_slice(&header.limits.max_records.to_be_bytes());
    bytes.extend_from_slice(&header.limits.max_payload_bytes.to_be_bytes());
    debug_assert_eq!(bytes.len(), ACTIVE_HEADER_BYTES as usize);
    bytes
}

fn decode_active_header(bytes: &[u8]) -> Result<ActiveLogHeader> {
    if bytes.len() != ACTIVE_HEADER_BYTES as usize || &bytes[..8] != ACTIVE_MAGIC {
        return Err(GenerationError::Invalid(
            "active delta has an invalid header length/magic".into(),
        ));
    }
    if bytes[8] != ACTIVE_SCHEMA {
        return Err(GenerationError::Invalid(format!(
            "unsupported active delta schema {}",
            bytes[8]
        )));
    }
    let hash_at = |start: usize| {
        let mut hash = [0u8; 32];
        hash.copy_from_slice(&bytes[start..start + 32]);
        Hash256(hash)
    };
    let generation_pin = GenerationPin {
        hash: hash_at(9),
        sequence: u64::from_be_bytes(bytes[41..49].try_into().expect("fixed sequence slice")),
    };
    let binding = RecoveryDagBinding {
        recovery_manifest_hash: hash_at(49),
        recovery_domain: hash_at(81),
        validator_set_commitment: hash_at(113),
    };
    let limits = RetentionLimits {
        max_records: u64::from_be_bytes(bytes[145..153].try_into().expect("fixed limit slice")),
        max_payload_bytes: u64::from_be_bytes(
            bytes[153..161].try_into().expect("fixed limit slice"),
        ),
    };
    validate_binding(&binding)?;
    validate_limits(limits)?;
    Ok(ActiveLogHeader {
        generation_pin,
        binding,
        limits,
    })
}

fn encode_active_batch(sequence: u64, records: &[RetainedDagRecord]) -> Result<Vec<u8>> {
    if records.is_empty() {
        return Err(GenerationError::Invalid(
            "active batch must not be empty".into(),
        ));
    }
    let record_count = u32::try_from(records.len())
        .map_err(|_| GenerationError::Invalid("active batch has too many records".into()))?;
    let mut body = Vec::new();
    body.push(ACTIVE_BATCH_SCHEMA);
    body.extend_from_slice(&sequence.to_be_bytes());
    body.extend_from_slice(&record_count.to_be_bytes());
    for record in records {
        let encoded = encode_record(record)?;
        let length = u32::try_from(encoded.len())
            .map_err(|_| GenerationError::Invalid("active batch record is too large".into()))?;
        body.extend_from_slice(&length.to_be_bytes());
        body.extend_from_slice(&encoded);
    }
    Ok(body)
}

fn decode_active_batch(
    body: &[u8],
    expected_sequence: u64,
    limits: RetentionLimits,
) -> Result<Vec<RetainedDagRecord>> {
    if body.len() < ACTIVE_BATCH_FIXED_BODY_BYTES as usize {
        return Err(GenerationError::Invalid(
            "active batch body is shorter than its fixed fields".into(),
        ));
    }
    if body[0] != ACTIVE_BATCH_SCHEMA {
        return Err(GenerationError::Invalid(format!(
            "unsupported active batch schema {}",
            body[0]
        )));
    }
    let sequence = u64::from_be_bytes(body[1..9].try_into().expect("fixed batch sequence"));
    if sequence != expected_sequence {
        return Err(GenerationError::Invalid(format!(
            "active batch sequence {sequence} does not equal expected {expected_sequence}"
        )));
    }
    let record_count =
        u32::from_be_bytes(body[9..13].try_into().expect("fixed batch count")) as u64;
    if record_count == 0 || record_count > limits.max_records {
        return Err(GenerationError::Invalid(
            "active batch record count is outside configured bounds".into(),
        ));
    }
    let mut offset = ACTIVE_BATCH_FIXED_BODY_BYTES as usize;
    let mut records = Vec::with_capacity(record_count as usize);
    for _ in 0..record_count {
        let length_end = offset.checked_add(4).ok_or_else(|| {
            GenerationError::Invalid("active batch record offset overflow".into())
        })?;
        if length_end > body.len() {
            return Err(GenerationError::Invalid(
                "active batch ends before a record length".into(),
            ));
        }
        let length = u32::from_be_bytes(
            body[offset..length_end]
                .try_into()
                .expect("fixed record length"),
        ) as usize;
        let record_end = length_end.checked_add(length).ok_or_else(|| {
            GenerationError::Invalid("active batch record length overflow".into())
        })?;
        if record_end > body.len() {
            return Err(GenerationError::Invalid(
                "active batch ends inside a complete checksummed record".into(),
            ));
        }
        records.push(decode_record(&body[length_end..record_end])?);
        offset = record_end;
    }
    if offset != body.len() {
        return Err(GenerationError::Invalid(
            "active batch contains trailing uncommitted bytes".into(),
        ));
    }
    Ok(records)
}

fn maximum_active_batch_body_bytes(limits: RetentionLimits) -> Result<u64> {
    let payload_cap = limits
        .max_payload_bytes
        .min(HARD_MAX_ACTIVE_BATCH_PAYLOAD_BYTES);
    limits
        .max_records
        .checked_mul(4 + FRAME_FIXED_BODY_BYTES)
        .and_then(|records| records.checked_add(payload_cap))
        .and_then(|bytes| bytes.checked_add(ACTIVE_BATCH_FIXED_BODY_BYTES))
        .ok_or_else(|| GenerationError::Invalid("active batch size bound overflow".into()))
}

fn maximum_active_file_bytes(limits: RetentionLimits) -> Result<u64> {
    // Worst case is one record per batch, so every record pays both the batch
    // envelope and its own length/fixed fields.
    limits
        .max_records
        .checked_mul(ACTIVE_BATCH_FRAME_OVERHEAD_BYTES + 4 + FRAME_FIXED_BODY_BYTES)
        .and_then(|overhead| overhead.checked_add(limits.max_payload_bytes))
        .and_then(|bytes| bytes.checked_add(ACTIVE_HEADER_BYTES))
        .ok_or_else(|| GenerationError::Invalid("active file size bound overflow".into()))
}

fn encode_record(record: &RetainedDagRecord) -> Result<Vec<u8>> {
    validate_record(record)?;
    let payload_length = u32::try_from(record.payload.len())
        .map_err(|_| GenerationError::Invalid("record payload is too large".into()))?;
    let mut encoded = Vec::with_capacity(FRAME_FIXED_BODY_BYTES as usize + record.payload.len());
    encoded.push(RECORD_SCHEMA);
    encoded.push(record.kind as u8);
    encoded.extend_from_slice(&record.round.to_be_bytes());
    encoded.extend_from_slice(record.object_hash.as_ref());
    encoded.extend_from_slice(&payload_length.to_be_bytes());
    encoded.extend_from_slice(&record.payload);
    Ok(encoded)
}

fn decode_record(encoded: &[u8]) -> Result<RetainedDagRecord> {
    if encoded.len() < FRAME_FIXED_BODY_BYTES as usize {
        return Err(GenerationError::Invalid(
            "record frame body is shorter than its fixed fields".into(),
        ));
    }
    if encoded[0] != RECORD_SCHEMA {
        return Err(GenerationError::Invalid(format!(
            "unsupported record schema {}",
            encoded[0]
        )));
    }
    let kind = RetainedRecordKind::try_from(encoded[1])?;
    let round = u64::from_be_bytes(
        encoded[2..10]
            .try_into()
            .expect("fixed round slice has eight bytes"),
    );
    let mut object_hash = [0u8; 32];
    object_hash.copy_from_slice(&encoded[10..42]);
    let payload_length = u32::from_be_bytes(
        encoded[42..46]
            .try_into()
            .expect("fixed payload-length slice has four bytes"),
    ) as usize;
    if encoded.len() != FRAME_FIXED_BODY_BYTES as usize + payload_length {
        return Err(GenerationError::Invalid(
            "record frame payload length does not match its body".into(),
        ));
    }
    let record = RetainedDagRecord {
        kind,
        round,
        object_hash: Hash256(object_hash),
        payload: encoded[46..].to_vec(),
    };
    validate_record(&record)?;
    Ok(record)
}

fn maximum_record_file_bytes(limits: RetentionLimits) -> Result<u64> {
    limits
        .max_records
        .checked_mul(FRAME_OVERHEAD_BYTES)
        .and_then(|overhead| overhead.checked_add(limits.max_payload_bytes))
        .and_then(|total| total.checked_add(RECORD_MAGIC.len() as u64))
        .ok_or_else(|| GenerationError::Invalid("record file size bound overflow".into()))
}

fn domain_hasher(context: &'static str) -> blake3::Hasher {
    blake3::Hasher::new_derive_key(context)
}

fn domain_hash(context: &'static str, bytes: &[u8]) -> Hash256 {
    let mut hasher = domain_hasher(context);
    hasher.update(bytes);
    Hash256(*hasher.finalize().as_bytes())
}

fn hash_file_into_hasher(
    path: &Path,
    context: &'static str,
    expected_bytes: u64,
) -> Result<blake3::Hasher> {
    regular_file_metadata(path)?;
    let mut file = File::open(path).map_err(|error| io_error("open", path, error))?;
    let mut hasher = domain_hasher(context);
    let mut observed = 0u64;
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let read = file
            .read(&mut buffer)
            .map_err(|error| io_error("read", path, error))?;
        if read == 0 {
            break;
        }
        observed = observed
            .checked_add(read as u64)
            .ok_or_else(|| GenerationError::Invalid("file hash size overflow".into()))?;
        hasher.update(&buffer[..read]);
    }
    if observed != expected_bytes {
        return Err(GenerationError::Invalid(format!(
            "{} changed size while it was hashed",
            path.display()
        )));
    }
    Ok(hasher)
}

fn read_regular_file_range(path: &Path, start: u64, end: u64) -> Result<Vec<u8>> {
    let metadata = regular_file_metadata(path)?;
    if start > end || metadata.len() != end {
        return Err(GenerationError::Invalid(format!(
            "invalid or changed byte range {start}..{end} for {}",
            path.display()
        )));
    }
    let length = usize::try_from(end - start)
        .map_err(|_| GenerationError::Invalid("quarantine suffix is too large".into()))?;
    let mut file = File::open(path).map_err(|error| io_error("open", path, error))?;
    file.seek(SeekFrom::Start(start))
        .map_err(|error| io_error("seek", path, error))?;
    let mut bytes = vec![0u8; length];
    file.read_exact(&mut bytes)
        .map_err(|error| io_error("read exact suffix from", path, error))?;
    Ok(bytes)
}

fn persist_exact_quarantine(
    root: &Path,
    path: &Path,
    bytes: &[u8],
    expected_hash: Hash256,
) -> Result<()> {
    if path.exists() {
        let metadata = regular_file_metadata(path)?;
        if metadata.len() != bytes.len() as u64 {
            return Err(GenerationError::Invalid(
                "existing active quarantine has the wrong byte length".into(),
            ));
        }
        let hasher = hash_file_into_hasher(
            path,
            "ARC recovery DAG quarantined active suffix v1",
            metadata.len(),
        )?;
        if Hash256(*hasher.finalize().as_bytes()) != expected_hash {
            return Err(GenerationError::Invalid(
                "existing active quarantine differs from the exact torn suffix".into(),
            ));
        }
        return Ok(());
    }
    let temporary = root.join(format!(".active-quarantine-{}.tmp", uuid::Uuid::new_v4()));
    write_new_synced_file(&temporary, bytes)?;
    match fs::hard_link(&temporary, path) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
            let _ = fs::remove_file(&temporary);
            return persist_exact_quarantine(root, path, bytes, expected_hash);
        }
        Err(error) => return Err(io_error("publish quarantine as", path, error)),
    }
    fsync_directory(root)?;
    fs::remove_file(&temporary)
        .map_err(|error| io_error("remove quarantine temporary", &temporary, error))?;
    fsync_directory(root)?;
    let hasher = hash_file_into_hasher(
        path,
        "ARC recovery DAG quarantined active suffix v1",
        bytes.len() as u64,
    )?;
    if Hash256(*hasher.finalize().as_bytes()) != expected_hash {
        return Err(GenerationError::Invalid(
            "published active quarantine hash differs from the torn suffix".into(),
        ));
    }
    Ok(())
}

fn canonical_json<T: Serialize>(value: &T, label: &str) -> Result<Vec<u8>> {
    serde_json::to_vec(value).map_err(|error| {
        GenerationError::Invalid(format!("could not serialize canonical {label}: {error}"))
    })
}

fn require_canonical_json<T: Serialize>(bytes: &[u8], value: &T, label: &str) -> Result<()> {
    let canonical = canonical_json(value, label)?;
    if bytes != canonical {
        return Err(GenerationError::Invalid(format!(
            "{label} is not in canonical JSON encoding"
        )));
    }
    Ok(())
}

fn read_small_regular_file(path: &Path, maximum_bytes: u64) -> Result<Vec<u8>> {
    let metadata = regular_file_metadata(path)?;
    if metadata.len() > maximum_bytes {
        return Err(GenerationError::Invalid(format!(
            "{} exceeds {} bytes",
            path.display(),
            maximum_bytes
        )));
    }
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    File::open(path)
        .map_err(|error| io_error("open", path, error))?
        .read_to_end(&mut bytes)
        .map_err(|error| io_error("read", path, error))?;
    if bytes.len() as u64 != metadata.len() {
        return Err(GenerationError::Invalid(format!(
            "{} changed size while it was read",
            path.display()
        )));
    }
    Ok(bytes)
}

fn regular_file_metadata(path: &Path) -> Result<fs::Metadata> {
    let metadata = fs::symlink_metadata(path).map_err(|error| io_error("inspect", path, error))?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(GenerationError::Invalid(format!(
            "{} is not a regular non-symlink file",
            path.display()
        )));
    }
    Ok(metadata)
}

fn ensure_real_directory(path: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(path).map_err(|error| io_error("inspect", path, error))?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(GenerationError::Invalid(format!(
            "{} is not a real directory",
            path.display()
        )));
    }
    Ok(())
}

fn create_private_directory(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        let mut builder = fs::DirBuilder::new();
        builder.mode(0o700);
        builder
            .create(path)
            .map_err(|error| io_error("create directory", path, error))?;
    }
    #[cfg(not(unix))]
    fs::create_dir(path).map_err(|error| io_error("create directory", path, error))?;
    Ok(())
}

fn set_private_file_mode(options: &mut OpenOptions) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
}

fn write_new_synced_file(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    set_private_file_mode(&mut options);
    let mut file = options
        .open(path)
        .map_err(|error| io_error("create", path, error))?;
    file.write_all(bytes)
        .map_err(|error| io_error("write", path, error))?;
    file.sync_all()
        .map_err(|error| io_error("fsync", path, error))
}

fn fsync_directory(path: &Path) -> Result<()> {
    File::open(path)
        .map_err(|error| io_error("open directory", path, error))?
        .sync_all()
        .map_err(|error| io_error("fsync directory", path, error))
}

fn read_up_to<R: Read>(reader: &mut R, buffer: &mut [u8]) -> io::Result<usize> {
    let mut offset = 0usize;
    while offset < buffer.len() {
        match reader.read(&mut buffer[offset..]) {
            Ok(0) => break,
            Ok(read) => offset += read,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        }
    }
    Ok(offset)
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TestDirectory(PathBuf);

    impl TestDirectory {
        fn new(label: &str) -> Self {
            let path = std::env::temp_dir().join(format!(
                "arc-recovery-dag-wal-{label}-{}",
                uuid::Uuid::new_v4()
            ));
            fs::create_dir(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn h(label: &[u8]) -> Hash256 {
        domain_hash("ARC recovery DAG generation test v1", label)
    }

    fn binding() -> RecoveryDagBinding {
        RecoveryDagBinding {
            recovery_manifest_hash: h(b"manifest"),
            recovery_domain: h(b"domain"),
            validator_set_commitment: h(b"validators"),
        }
    }

    fn input(height: u64, committed: u64, round: u64) -> GenerationInput {
        GenerationInput {
            binding: binding(),
            baseline_state: BaselineState {
                height,
                block_hash: h(format!("block-{height}").as_bytes()),
                state_root: h(format!("root-{height}").as_bytes()),
            },
            dag_cursor: DagCursor {
                committed_block_count: committed,
                next_dag_round: round,
                current_round: round + 1,
                retention_floor_round: round,
                retention_ceiling_round: round + 3,
            },
            retention_limits: RetentionLimits {
                max_records: 32,
                max_payload_bytes: 64 * 1024,
            },
        }
    }

    fn records(round: u64) -> Vec<RetainedDagRecord> {
        vec![
            RetainedDagRecord::transaction(round, h(b"tx"), vec![1, 2, 3]),
            RetainedDagRecord::dag_block(round, h(b"dag"), vec![4, 5, 6]),
            RetainedDagRecord::round_cursor(round + 1),
        ]
    }

    fn replace_pointer(store: &GenerationStore, generation: &VerifiedGeneration) {
        let pointer = CurrentPointer {
            schema: POINTER_SCHEMA.to_owned(),
            generation_hash: generation.pin.hash,
            active_log_generation_hash: generation.pin.hash,
            sequence: generation.pin.sequence,
            previous_generation: generation.manifest.previous_generation,
        };
        let bytes = canonical_json(&pointer, "test pointer").unwrap();
        let path = store.root.join(CURRENT_FILE);
        fs::write(path, bytes).unwrap();
    }

    #[test]
    fn creates_content_addressed_hash_chained_generations() {
        let parent = TestDirectory::new("chain");
        let store = GenerationStore::new(parent.0.join("store"));
        let first = store.create_initial(input(10, 5, 20), records(20)).unwrap();
        let second = store
            .append(first.pin, input(11, 6, 21), records(21))
            .unwrap();

        assert_eq!(first.pin.sequence, 0);
        assert_eq!(second.pin.sequence, 1);
        assert_eq!(second.manifest.previous_generation, Some(first.pin.hash));
        assert!(
            first.directory.exists(),
            "prior generation must be preserved"
        );
        assert!(second.directory.exists());
        assert!(store.active_log_path(first.pin).exists());
        assert!(store.active_log_path(second.pin).exists());
        assert_eq!(
            inspect_active_log(&store.active_log_path(second.pin), &second)
                .unwrap()
                .batch_count,
            0
        );
        assert_eq!(
            store
                .load_current(&binding(), Some(second.pin))
                .unwrap()
                .pin,
            second.pin
        );
        assert_eq!(
            store.audit(&binding()).unwrap().status,
            StoreAuditStatus::Clean
        );
    }

    #[test]
    fn external_pin_and_head_audit_detect_pointer_rollback() {
        let parent = TestDirectory::new("rollback");
        let store = GenerationStore::new(parent.0.join("store"));
        let first = store.create_initial(input(10, 5, 20), records(20)).unwrap();
        let second = store
            .append(first.pin, input(11, 6, 21), records(21))
            .unwrap();
        replace_pointer(&store, &first);

        assert!(matches!(
            store.load_current(&binding(), Some(second.pin)),
            Err(GenerationError::PinMismatch { .. })
        ));
        assert_eq!(
            store.audit(&binding()).unwrap().status,
            StoreAuditStatus::PointerBehind {
                heads: vec![second.pin]
            }
        );
    }

    #[test]
    fn audit_detects_a_swapped_fork() {
        let first_parent = TestDirectory::new("fork-a");
        let second_parent = TestDirectory::new("fork-b");
        let first_store = GenerationStore::new(first_parent.0.join("store"));
        let second_store = GenerationStore::new(second_parent.0.join("store"));
        let canonical = first_store
            .create_initial(input(10, 5, 20), records(20))
            .unwrap();
        let mut fork_input = input(10, 5, 20);
        fork_input.baseline_state.block_hash = h(b"fork-block");
        fork_input.baseline_state.state_root = h(b"fork-root");
        let fork = second_store
            .create_initial(fork_input, records(20))
            .unwrap();
        let copied_fork = first_store.generation_path(fork.pin.hash);
        copy_directory(&fork.directory, &copied_fork);
        fs::copy(
            second_store.active_log_path(fork.pin),
            first_store.active_log_path(fork.pin),
        )
        .unwrap();
        replace_pointer(&first_store, &fork);

        let audit = first_store.audit(&binding()).unwrap();
        let StoreAuditStatus::Forked { heads } = audit.status else {
            panic!("swapped root must produce a fork audit")
        };
        assert_eq!(heads.len(), 2);
        assert!(heads.contains(&canonical.pin));
        assert!(heads.contains(&fork.pin));
    }

    #[test]
    fn manifest_and_record_tampering_fail_closed() {
        let parent = TestDirectory::new("tamper");
        let store = GenerationStore::new(parent.0.join("store"));
        let generation = store.create_initial(input(10, 5, 20), records(20)).unwrap();

        let record_path = generation.directory.join(RECORDS_FILE);
        let mut bytes = fs::read(&record_path).unwrap();
        bytes[RECORD_MAGIC.len() + 10] ^= 1;
        fs::write(&record_path, bytes).unwrap();
        assert!(
            store
                .verify_generation(generation.pin.hash, &binding())
                .is_err()
        );

        let other_parent = TestDirectory::new("manifest-tamper");
        let other_store = GenerationStore::new(other_parent.0.join("store"));
        let other = other_store
            .create_initial(input(10, 5, 20), records(20))
            .unwrap();
        let manifest_path = other.directory.join(MANIFEST_FILE);
        let mut manifest = fs::read(&manifest_path).unwrap();
        manifest.push(b'\n');
        fs::write(manifest_path, manifest).unwrap();
        assert!(
            other_store
                .verify_generation(other.pin.hash, &binding())
                .is_err()
        );
    }

    #[test]
    fn torn_final_suffix_is_classified_but_rejected_when_published() {
        let parent = TestDirectory::new("torn");
        let store = GenerationStore::new(parent.0.join("store"));
        let generation = store.create_initial(input(10, 5, 20), records(20)).unwrap();
        let path = generation.directory.join(RECORDS_FILE);
        let clean_len = fs::metadata(&path).unwrap().len();
        let mut file = OpenOptions::new().append(true).open(&path).unwrap();
        file.write_all(&100u32.to_be_bytes()).unwrap();
        file.write_all(&[9, 9, 9]).unwrap();
        file.sync_all().unwrap();

        let inspection = inspect_record_log(&path, generation.manifest.retained_records.limits)
            .expect("final partial payload is classifiable");
        assert_eq!(inspection.valid_prefix_bytes, clean_len);
        assert_eq!(
            inspection.suffix,
            TornSuffix::PartialPayload {
                present_bytes: 3,
                expected_bytes: 100
            }
        );
        assert!(matches!(
            store.verify_generation(generation.pin.hash, &binding()),
            Err(GenerationError::TornPublishedRecordLog(_))
        ));
    }

    #[derive(Default)]
    struct RecordingObserver {
        points: Vec<PublishPoint>,
        fail_after: Option<PublishPoint>,
    }

    impl PublishObserver for RecordingObserver {
        fn reached(&mut self, point: PublishPoint) -> Result<()> {
            self.points.push(point);
            if self.fail_after == Some(point) {
                return Err(GenerationError::InjectedFailure(point));
            }
            Ok(())
        }
    }

    #[test]
    fn publish_order_places_all_fsync_barriers_before_current() {
        let parent = TestDirectory::new("barriers");
        let store = GenerationStore::new(parent.0.join("store"));
        let first = store.create_initial(input(10, 5, 20), records(20)).unwrap();
        let mut observer = RecordingObserver::default();
        store
            .append_with_observer(first.pin, input(11, 6, 21), records(21), &mut observer)
            .unwrap();
        assert_eq!(
            observer.points,
            vec![
                PublishPoint::RecordsSynced,
                PublishPoint::ManifestSynced,
                PublishPoint::GenerationDirectorySynced,
                PublishPoint::GenerationPublished,
                PublishPoint::RootAfterGenerationSynced,
                PublishPoint::ActiveLogSynced,
                PublishPoint::RootAfterActiveLogSynced,
                PublishPoint::PointerFileSynced,
                PublishPoint::PointerRenamed,
                PublishPoint::RootAfterPointerSynced,
            ]
        );
    }

    #[test]
    fn interrupted_publish_preserves_old_head_and_can_be_explicitly_resumed() {
        let parent = TestDirectory::new("resume");
        let store = GenerationStore::new(parent.0.join("store"));
        let first = store.create_initial(input(10, 5, 20), records(20)).unwrap();
        let mut observer = RecordingObserver {
            fail_after: Some(PublishPoint::RootAfterGenerationSynced),
            ..RecordingObserver::default()
        };
        assert!(matches!(
            store.append_with_observer(first.pin, input(11, 6, 21), records(21), &mut observer,),
            Err(GenerationError::InjectedFailure(
                PublishPoint::RootAfterGenerationSynced
            ))
        ));
        assert_eq!(
            store.load_current(&binding(), Some(first.pin)).unwrap().pin,
            first.pin
        );
        let audit = store.audit(&binding()).unwrap();
        let StoreAuditStatus::PointerBehind { heads } = audit.status else {
            panic!("published successor must be reported without silent activation")
        };
        assert_eq!(heads.len(), 1);
        let resumed = store
            .activate_existing_successor(first.pin, heads[0].hash)
            .unwrap();
        assert_eq!(resumed.pin, heads[0]);
        assert!(first.directory.exists());
        assert_eq!(
            store.audit(&binding()).unwrap().status,
            StoreAuditStatus::Clean
        );
    }

    #[test]
    fn active_delta_batches_are_bounded_streamed_and_idempotent() {
        let parent = TestDirectory::new("active-batches");
        let store = GenerationStore::new(parent.0.join("store"));
        let generation = store.create_initial(input(10, 5, 20), records(20)).unwrap();
        let exact_old_transaction = RetainedDagRecord::transaction(20, h(b"tx"), vec![1, 2, 3]);
        let new_commit = RetainedDagRecord::commit(21, h(b"active-commit"));
        let requested = vec![exact_old_transaction.clone(), new_commit.clone()];

        let mut writer = store
            .open_current_active_writer(&binding(), generation.pin)
            .unwrap();
        let receipt = writer
            .append_batch(&requested, ActiveDurability::Fsync)
            .unwrap();
        assert_eq!(receipt.batch_sequence, Some(0));
        assert_eq!(receipt.requested_records, 2);
        assert_eq!(receipt.appended_records, 1);
        assert_eq!(receipt.idempotently_omitted_records, 1);
        assert!(receipt.durable);

        let retry = writer
            .append_batch(&requested, ActiveDurability::Fsync)
            .unwrap();
        assert_eq!(retry.batch_sequence, None);
        assert_eq!(retry.appended_records, 0);
        assert_eq!(retry.idempotently_omitted_records, 2);

        let conflicting = RetainedDagRecord::transaction(20, h(b"tx"), vec![9, 9, 9]);
        assert!(matches!(
            writer.append_batch(&[conflicting], ActiveDurability::Buffered),
            Err(GenerationError::ActiveWriterPoisoned(_))
        ));
        assert!(matches!(
            writer.append_batch(
                &[RetainedDagRecord::round_cursor(22)],
                ActiveDurability::Buffered
            ),
            Err(GenerationError::ActiveWriterPoisoned(_))
        ));
        drop(writer);

        let mut streamed = Vec::new();
        let summary = store
            .stream_current_generation_and_active(&binding(), generation.pin, |record| {
                streamed.push(record);
                Ok(())
            })
            .unwrap();
        assert_eq!(summary.base_record_count, 3);
        assert_eq!(summary.active_batch_count, 1);
        assert_eq!(summary.active_record_count, 1);
        assert_eq!(summary.active_suffix, TornSuffix::Clean);
        assert_eq!(streamed.len(), 4);
        assert_eq!(streamed.last(), Some(&new_commit));
        assert!(
            store
                .append(generation.pin, input(11, 6, 20), streamed.clone())
                .is_err(),
            "direct generation switch must not ignore a non-empty active delta"
        );
        let mut wrong_active_pin = summary.active_pin;
        wrong_active_pin.complete_file_hash = h(b"wrong-active-pin");
        assert!(
            store
                .append_compacted(
                    generation.pin,
                    wrong_active_pin,
                    input(11, 6, 20),
                    streamed.clone(),
                )
                .is_err()
        );
        let compacted = store
            .append_compacted(
                generation.pin,
                summary.active_pin,
                input(11, 6, 20),
                streamed,
            )
            .unwrap();
        assert_eq!(compacted.pin.sequence, 1);
        assert!(store.active_log_path(generation.pin).exists());
        assert_eq!(
            inspect_active_log(&store.active_log_path(compacted.pin), &compacted)
                .unwrap()
                .record_count,
            0
        );
    }

    #[test]
    fn consensus_append_order_allows_late_lower_round_commit_and_compaction() {
        let parent = TestDirectory::new("consensus-append-order");
        let store = GenerationStore::new(parent.0.join("store"));
        let block_100_hash = h(b"round-100-block");
        let physical_order = vec![
            RetainedDagRecord::transaction(100, h(b"round-100-transaction"), vec![1]),
            RetainedDagRecord::dag_block(100, block_100_hash, vec![2]),
            RetainedDagRecord::dag_block(101, h(b"round-101-block"), vec![3]),
            RetainedDagRecord::dag_block(102, h(b"round-102-block"), vec![4]),
        ];
        let generation = store
            .create_initial(input(10, 5, 100), physical_order.clone())
            .unwrap();
        assert_eq!(generation.manifest.retained_records.first_round, Some(100));
        assert_eq!(generation.manifest.retained_records.last_round, Some(102));

        let late_commit = RetainedDagRecord::commit(100, block_100_hash);
        let mut writer = store
            .open_current_active_writer(&binding(), generation.pin)
            .unwrap();
        writer
            .append_batch(std::slice::from_ref(&late_commit), ActiveDurability::Fsync)
            .expect("Commit(100) must remain valid after blocks through round 102");
        assert_eq!(writer.inspection().first_round, Some(100));
        assert_eq!(writer.inspection().last_round, Some(100));
        drop(writer);

        let mut streamed = Vec::new();
        let summary = store
            .stream_current_generation_and_active(&binding(), generation.pin, |record| {
                streamed.push(record);
                Ok(())
            })
            .unwrap();
        let mut expected_order = physical_order;
        expected_order.push(late_commit);
        assert_eq!(
            streamed, expected_order,
            "physical WAL order must be retained"
        );

        let compacted = store
            .append_compacted(
                generation.pin,
                summary.active_pin,
                input(11, 6, 100),
                streamed,
            )
            .unwrap();
        assert_eq!(compacted.manifest.retained_records.first_round, Some(100));
        assert_eq!(compacted.manifest.retained_records.last_round, Some(102));
        assert_eq!(
            store
                .load_current(&binding(), Some(compacted.pin))
                .unwrap()
                .pin,
            compacted.pin
        );

        let mut reloaded = Vec::new();
        store
            .stream_current_generation_and_active(&binding(), compacted.pin, |record| {
                reloaded.push(record);
                Ok(())
            })
            .unwrap();
        assert_eq!(reloaded, expected_order);
    }

    #[test]
    fn active_append_preflight_does_not_touch_disk_when_caps_fail() {
        let parent = TestDirectory::new("active-cap");
        let store = GenerationStore::new(parent.0.join("store"));
        let mut constrained = input(10, 5, 20);
        constrained.retention_limits.max_records = 4;
        let generation = store.create_initial(constrained, records(20)).unwrap();
        let path = store.active_log_path(generation.pin);
        let before = fs::read(&path).unwrap();
        let mut writer = store
            .open_current_active_writer(&binding(), generation.pin)
            .unwrap();
        assert!(
            writer
                .append_batch(
                    &[
                        RetainedDagRecord::commit(21, h(b"cap-a")),
                        RetainedDagRecord::round_cursor(22),
                    ],
                    ActiveDurability::Fsync,
                )
                .is_err()
        );
        drop(writer);
        assert_eq!(fs::read(path).unwrap(), before);
    }

    #[test]
    fn torn_active_batch_streams_only_prefix_then_quarantines_and_reopens() {
        let parent = TestDirectory::new("active-torn");
        let store = GenerationStore::new(parent.0.join("store"));
        let generation = store.create_initial(input(10, 5, 20), records(20)).unwrap();
        let committed = RetainedDagRecord::commit(21, h(b"durable-active"));
        let mut writer = store
            .open_current_active_writer(&binding(), generation.pin)
            .unwrap();
        writer
            .append_batch(std::slice::from_ref(&committed), ActiveDurability::Fsync)
            .unwrap();
        drop(writer);

        let path = store.active_log_path(generation.pin);
        let clean_bytes = fs::metadata(&path).unwrap().len();
        let torn_bytes = [100u32.to_be_bytes().as_slice(), &[7, 8, 9]].concat();
        let mut file = OpenOptions::new().append(true).open(&path).unwrap();
        file.write_all(&torn_bytes).unwrap();
        file.sync_all().unwrap();
        drop(file);

        let inspection = inspect_active_log(&path, &generation).unwrap();
        assert_eq!(inspection.valid_prefix_bytes, clean_bytes);
        assert_eq!(
            inspection.suffix,
            TornSuffix::PartialPayload {
                present_bytes: 3,
                expected_bytes: 100,
            }
        );
        let mut streamed = Vec::new();
        let summary = store
            .stream_current_generation_and_active(&binding(), generation.pin, |record| {
                streamed.push(record);
                Ok(())
            })
            .unwrap();
        assert_eq!(summary.active_record_count, 1);
        assert_eq!(summary.active_suffix, inspection.suffix);
        assert_eq!(streamed.last(), Some(&committed));
        assert!(
            store
                .open_current_active_writer(&binding(), generation.pin)
                .is_err()
        );
        assert!(
            store
                .quarantine_current_active_suffix(&binding(), generation.pin, clean_bytes + 1)
                .is_err()
        );
        assert_eq!(
            fs::metadata(&path).unwrap().len(),
            clean_bytes + torn_bytes.len() as u64
        );

        let evidence = store
            .quarantine_current_active_suffix(&binding(), generation.pin, clean_bytes)
            .unwrap();
        assert_eq!(evidence.valid_prefix_bytes, clean_bytes);
        assert_eq!(evidence.quarantined_suffix_bytes, torn_bytes.len() as u64);
        assert_eq!(fs::read(&evidence.quarantine_path).unwrap(), torn_bytes);
        assert_eq!(fs::metadata(&path).unwrap().len(), clean_bytes);
        assert_eq!(
            inspect_active_log(&path, &generation).unwrap().suffix,
            TornSuffix::Clean
        );
        store
            .open_current_active_writer(&binding(), generation.pin)
            .unwrap();
    }

    #[test]
    fn active_log_complete_checksum_corruption_is_fatal() {
        let parent = TestDirectory::new("active-checksum");
        let store = GenerationStore::new(parent.0.join("store"));
        let generation = store.create_initial(input(10, 5, 20), records(20)).unwrap();
        let mut writer = store
            .open_current_active_writer(&binding(), generation.pin)
            .unwrap();
        writer
            .append_batch(
                &[RetainedDagRecord::commit(21, h(b"checksum-active"))],
                ActiveDurability::Fsync,
            )
            .unwrap();
        drop(writer);
        let path = store.active_log_path(generation.pin);
        let mut bytes = fs::read(&path).unwrap();
        let original_len = bytes.len() as u64;
        *bytes.last_mut().unwrap() ^= 1;
        fs::write(&path, bytes).unwrap();
        let error = inspect_active_log(&path, &generation).unwrap_err();
        assert!(error.to_string().contains("invalid checksum"));
        assert!(
            store
                .quarantine_current_active_suffix(&binding(), generation.pin, ACTIVE_HEADER_BYTES)
                .is_err()
        );
        assert_eq!(fs::metadata(path).unwrap().len(), original_len);
    }

    #[test]
    fn advisory_lock_contends_and_stale_regular_file_is_harmless() {
        let parent = TestDirectory::new("advisory-lock");
        let store = GenerationStore::new(parent.0.join("store"));
        store.create_initial(input(10, 5, 20), records(20)).unwrap();
        let first = StoreLock::acquire(store.root()).unwrap();
        assert!(matches!(
            StoreLock::acquire(store.root()),
            Err(GenerationError::Locked(_))
        ));
        drop(first);

        let lock_path = store.root().join(WRITE_LOCK_FILE);
        fs::write(&lock_path, b"stale owner from a dead process\n").unwrap();
        let recovered = StoreLock::acquire(store.root()).unwrap();
        let evidence = fs::read_to_string(&lock_path).unwrap();
        assert!(evidence.contains("arc.recovery.dag-wal-lock.v2"));
        recovered.release().unwrap();
        assert!(
            lock_path.is_file(),
            "advisory lock inode is deliberately retained"
        );
    }

    #[test]
    fn crash_after_active_fsync_keeps_old_current_and_resumes_exact_pair() {
        let parent = TestDirectory::new("active-publish-crash");
        let store = GenerationStore::new(parent.0.join("store"));
        let first = store.create_initial(input(10, 5, 20), records(20)).unwrap();
        let mut observer = RecordingObserver {
            fail_after: Some(PublishPoint::RootAfterActiveLogSynced),
            ..RecordingObserver::default()
        };
        assert!(matches!(
            store.append_with_observer(first.pin, input(11, 6, 21), records(21), &mut observer),
            Err(GenerationError::InjectedFailure(
                PublishPoint::RootAfterActiveLogSynced
            ))
        ));
        assert_eq!(
            store.load_current(&binding(), Some(first.pin)).unwrap().pin,
            first.pin
        );
        let audit = store.audit(&binding()).unwrap();
        let StoreAuditStatus::PointerBehind { heads } = audit.status else {
            panic!("fsynced generation/active pair must remain an explicit unpointed successor")
        };
        let successor = store.verify_generation(heads[0].hash, &binding()).unwrap();
        let active = inspect_active_log(&store.active_log_path(successor.pin), &successor).unwrap();
        assert_eq!(active.total_file_bytes, ACTIVE_HEADER_BYTES);
        assert_eq!(active.suffix, TornSuffix::Clean);
        store
            .activate_existing_successor(first.pin, successor.pin.hash)
            .unwrap();
        assert_eq!(
            store
                .load_current(&binding(), Some(successor.pin))
                .unwrap()
                .pin,
            successor.pin
        );
        assert!(store.active_log_path(first.pin).exists());
    }

    #[test]
    fn every_pre_current_active_fault_keeps_the_old_selected_pair() {
        for point in [
            PublishPoint::ActiveLogSynced,
            PublishPoint::RootAfterActiveLogSynced,
            PublishPoint::PointerFileSynced,
        ] {
            let parent = TestDirectory::new("active-pre-current-fault");
            let store = GenerationStore::new(parent.0.join("store"));
            let first = store.create_initial(input(10, 5, 20), records(20)).unwrap();
            let mut observer = RecordingObserver {
                fail_after: Some(point),
                ..RecordingObserver::default()
            };
            assert!(matches!(
                store.append_with_observer(
                    first.pin,
                    input(11, 6, 21),
                    records(21),
                    &mut observer,
                ),
                Err(GenerationError::InjectedFailure(failed)) if failed == point
            ));
            assert_eq!(
                store.load_current(&binding(), Some(first.pin)).unwrap().pin,
                first.pin
            );
            assert!(store.active_log_path(first.pin).exists());
        }
    }

    #[test]
    fn visible_post_rename_current_always_has_its_bound_active_log() {
        let parent = TestDirectory::new("active-post-current-fault");
        let store = GenerationStore::new(parent.0.join("store"));
        let first = store.create_initial(input(10, 5, 20), records(20)).unwrap();
        let mut observer = RecordingObserver {
            fail_after: Some(PublishPoint::PointerRenamed),
            ..RecordingObserver::default()
        };
        assert!(matches!(
            store.append_with_observer(first.pin, input(11, 6, 21), records(21), &mut observer),
            Err(GenerationError::InjectedFailure(
                PublishPoint::PointerRenamed
            ))
        ));
        let selected = store.load_current(&binding(), None).unwrap();
        assert_eq!(selected.pin.sequence, 1);
        assert_eq!(
            inspect_active_log(&store.active_log_path(selected.pin), &selected)
                .unwrap()
                .total_file_bytes,
            ACTIVE_HEADER_BYTES
        );
        assert!(store.active_log_path(first.pin).exists());
    }

    #[test]
    fn complete_checksum_corruption_is_not_misclassified_as_a_torn_suffix() {
        let parent = TestDirectory::new("checksum");
        let store = GenerationStore::new(parent.0.join("store"));
        let generation = store.create_initial(input(10, 5, 20), records(20)).unwrap();
        let path = generation.directory.join(RECORDS_FILE);
        let mut bytes = fs::read(&path).unwrap();
        *bytes.last_mut().unwrap() ^= 1;
        fs::write(&path, bytes).unwrap();
        let error = inspect_record_log(&path, generation.manifest.retained_records.limits)
            .expect_err("full bad checksum is corruption");
        assert!(error.to_string().contains("invalid checksum"));
    }

    fn copy_directory(source: &Path, destination: &Path) {
        fs::create_dir(destination).unwrap();
        for entry in fs::read_dir(source).unwrap() {
            let entry = entry.unwrap();
            fs::copy(entry.path(), destination.join(entry.file_name())).unwrap();
        }
    }
}
