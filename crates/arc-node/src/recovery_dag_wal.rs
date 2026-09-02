//! Atomic, bounded generations for the post-recovery consensus DAG WAL.
//!
//! A generation is immutable and content addressed. Its manifest binds the
//! recovery checkpoint/domain, the validator set, the canonical state baseline,
//! the DAG cursors, and the exact retained record log. Publishing is two phase:
//! all generation files and namespace entries cross the platform durability
//! barrier first, then `CURRENT` is atomically and durably replaced. Unix uses
//! parent-directory fsync; Windows uses write-through namespace moves. Previous
//! generations are never removed by ordinary generation publication.
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
const GC_ANCHOR_FILE: &str = "GC-ANCHOR.json";
const GC_ANCHOR_SCHEMA: &str = "arc.recovery.dag-wal-gc-anchor.v1";
const MAX_MANIFEST_BYTES: u64 = 64 * 1024;
const MAX_POINTER_BYTES: u64 = 4 * 1024;
const MAX_GC_ANCHOR_BYTES: u64 = 2 * 1024 * 1024;
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

/// Exact non-mutating usage projection for one active-log batch after
/// idempotent duplicate elimination against both the immutable generation and
/// the already-written active prefix.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ActiveBatchProjection {
    pub requested_records: u64,
    pub appended_records: u64,
    pub idempotently_omitted_records: u64,
    pub appended_payload_bytes: u64,
    pub minimum_round: Option<u64>,
    pub maximum_round: Option<u64>,
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

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct GcAnchor {
    schema: String,
    binding: RecoveryDagBinding,
    authorized_by: GenerationPin,
    retained_boundary: GenerationPin,
    missing_parent: GenerationPin,
    pruned: Vec<GenerationPin>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AncestorGcReport {
    pub current: GenerationPin,
    pub retained_predecessor: Option<GenerationPin>,
    pub pruned_generations: Vec<GenerationPin>,
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum GcPoint {
    AnchorFileSynced,
    AnchorRenamed,
    RootAfterAnchorSynced,
    GenerationRenamed(GenerationPin),
    RootAfterGenerationRenameSynced(GenerationPin),
    ActiveLogRenamed(GenerationPin),
    RootAfterActiveLogRenameSynced(GenerationPin),
    GenerationRemoved(GenerationPin),
    RootAfterGenerationRemoveSynced(GenerationPin),
    ActiveLogRemoved(GenerationPin),
    RootAfterActiveLogRemoveSynced(GenerationPin),
}

trait GcObserver {
    fn reached(&mut self, _point: GcPoint) -> Result<()> {
        Ok(())
    }
}

impl GcObserver for NoopObserver {}

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

    /// Project the exact records and payload bytes a later [`Self::append_batch`]
    /// would add, without enforcing this generation's combined capacity. This
    /// lets a rollover controller distinguish true new bytes from transaction
    /// bodies repeated byte-identically by several validators in one round.
    /// Identity conflicts and malformed records still fail closed.
    pub fn project_batch_usage(
        &self,
        records: &[RetainedDagRecord],
    ) -> Result<ActiveBatchProjection> {
        if let Some(reason) = &self.poisoned {
            return Err(GenerationError::ActiveWriterPoisoned(reason.clone()));
        }
        let mut batch_seen = HashMap::new();
        let mut appended_records = 0u64;
        let mut appended_payload_bytes = 0u64;
        let mut minimum_round: Option<u64> = None;
        let mut maximum_round: Option<u64> = None;
        for record in records {
            validate_record(record)?;
            let identity = record_identity(record);
            let fingerprint = record_payload_fingerprint(record);
            let existing = batch_seen
                .get(&identity)
                .or_else(|| self.seen.get(&identity));
            if let Some(existing) = existing {
                if *existing != fingerprint {
                    return Err(GenerationError::Invalid(format!(
                        "record key {:?}/round {}/{} was reused with different payload",
                        record.kind, record.round, record.object_hash
                    )));
                }
                continue;
            }
            batch_seen.insert(identity, fingerprint);
            appended_records = appended_records
                .checked_add(1)
                .ok_or_else(|| GenerationError::Invalid("batch record count overflow".into()))?;
            appended_payload_bytes = appended_payload_bytes
                .checked_add(record.payload.len() as u64)
                .ok_or_else(|| GenerationError::Invalid("batch payload size overflow".into()))?;
            minimum_round = Some(minimum_round.map_or(record.round, |old| old.min(record.round)));
            maximum_round = Some(maximum_round.map_or(record.round, |old| old.max(record.round)));
        }
        let requested_records = records.len() as u64;
        Ok(ActiveBatchProjection {
            requested_records,
            appended_records,
            idempotently_omitted_records: requested_records - appended_records,
            appended_payload_bytes,
            minimum_round,
            maximum_round,
        })
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
    /// Construct a store rooted at one application-owned leaf. The immediate
    /// parent must already exist; refusing to recursively invent missing
    /// ancestors lets the sibling namespace lock durably prove the whole
    /// store name on Windows instead of acknowledging state below an
    /// unbarriered parent.
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

    /// Resume the one valid crash window where the deterministic sequence-zero
    /// generation was published but `CURRENT` was not. No history is selected
    /// unless the sole generation is the exact empty generation independently
    /// derived by the caller for this recovery boundary.
    pub fn resume_unselected_initial(
        &self,
        expected: &GenerationInput,
    ) -> Result<Option<VerifiedGeneration>> {
        validate_input(expected)?;
        self.ensure_root()?;
        let lock = StoreLock::acquire(&self.root)?;
        let current_path = self.root.join(CURRENT_FILE);
        match fs::symlink_metadata(&current_path) {
            Ok(_) => {
                return Err(GenerationError::Invalid(
                    "cannot resume an unselected initial generation when CURRENT exists".into(),
                ));
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(io_error("inspect", &current_path, error)),
        }
        if self.read_gc_anchor(&expected.binding)?.is_some() {
            return Err(GenerationError::Invalid(
                "unselected initial generation cannot coexist with a GC anchor".into(),
            ));
        }
        let manifests = self.read_all_manifests(&expected.binding)?;
        if manifests.is_empty() {
            lock.release()?;
            return Ok(None);
        }
        if manifests.len() != 1 {
            return Err(GenerationError::Invalid(
                "CURRENT-less recovery store must contain exactly one resumable initial generation"
                    .into(),
            ));
        }
        let (&hash, manifest) = manifests.iter().next().expect("one manifest exists");
        if manifest.sequence != 0
            || manifest.previous_generation.is_some()
            || manifest.binding != expected.binding
            || manifest.baseline_state != expected.baseline_state
            || manifest.dag_cursor != expected.dag_cursor
            || manifest.retained_records.limits != expected.retention_limits
            || manifest.retained_records.record_count != 0
            || manifest.retained_records.payload_bytes != 0
            || manifest.retained_records.first_round.is_some()
            || manifest.retained_records.last_round.is_some()
        {
            return Err(GenerationError::Invalid(
                "unselected initial generation differs from the exact empty recovery boundary"
                    .into(),
            ));
        }
        let generation = self.verify_generation(hash, &expected.binding)?;
        self.rebarrier_unselected_generation(generation.pin)?;
        let generation = self.verify_generation(hash, &expected.binding)?;
        self.ensure_empty_active_log(&generation)?;
        fsync_directory(&self.root)?;
        let mut observer = NoopObserver;
        self.publish_pointer(&generation, &mut observer)?;
        let selected = self.load_current(&expected.binding, Some(generation.pin))?;
        lock.release()?;
        Ok(Some(selected))
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
        let gc_anchor = self.read_gc_anchor(expected_binding)?;
        let generation_count = manifests.len();
        let mut referenced = HashSet::new();
        for (hash, manifest) in &manifests {
            if let Some(parent) = manifest.previous_generation {
                if let Some(parent_manifest) = manifests.get(&parent) {
                    if parent_manifest.sequence.checked_add(1) != Some(manifest.sequence) {
                        return Err(GenerationError::Invalid(format!(
                            "generation {hash} sequence does not follow parent {parent}"
                        )));
                    }
                    referenced.insert(parent);
                } else if !gc_anchor.as_ref().is_some_and(|anchor| {
                    anchor.retained_boundary.hash == *hash
                        && anchor.retained_boundary.sequence == manifest.sequence
                        && anchor.missing_parent.hash == parent
                        && anchor.missing_parent.sequence.checked_add(1) == Some(manifest.sequence)
                }) {
                    return Err(GenerationError::Invalid(format!(
                        "generation {hash} references missing parent {parent} without an exact GC anchor"
                    )));
                }
            } else if manifest.sequence != 0 {
                return Err(GenerationError::Invalid(format!(
                    "generation {hash} has no parent at nonzero sequence {}",
                    manifest.sequence
                )));
            }
        }
        if let Some(anchor) = gc_anchor.as_ref() {
            validate_gc_anchor(anchor, &current, &manifests)?;
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

    /// Finish any generation-GC operation whose authorization anchor was
    /// fsynced before a crash. Only hashes explicitly named by that anchor are
    /// touched, and the current generation plus its exact predecessor are
    /// re-derived from `CURRENT` and excluded before every rename/removal.
    pub fn recover_interrupted_ancestor_gc(
        &self,
        expected_binding: &RecoveryDagBinding,
    ) -> Result<AncestorGcReport> {
        self.ensure_root()?;
        let lock = StoreLock::acquire(&self.root)?;
        let current = self.load_current(expected_binding, None)?;
        let predecessor = current
            .manifest
            .previous_generation
            .map(|hash| GenerationPin {
                sequence: current.pin.sequence.saturating_sub(1),
                hash,
            });
        let Some(anchor) = self.read_gc_anchor(expected_binding)? else {
            lock.release()?;
            return Ok(AncestorGcReport {
                current: current.pin,
                retained_predecessor: predecessor,
                pruned_generations: Vec::new(),
            });
        };
        // Do not run the ordinary ancestry audit yet: a crash may have moved
        // an early authorized target while a later target still names it as a
        // parent. Validate the selected head, anchor path, and every remaining
        // live/tombstoned target structurally, finish the complete authorized
        // set, and only then require a clean audit.
        let manifests = self.read_all_manifests(expected_binding)?;
        validate_gc_anchor(&anchor, &current, &manifests)?;
        let anchor_bytes = canonical_json(&anchor, "GC anchor")?;
        replace_synced_file_durably(
            &self.root.join(GC_ANCHOR_FILE),
            &anchor_bytes,
            "rebarrier GC anchor before recovery deletion",
        )?;
        self.recover_legacy_gc_active_dual_names(&anchor.pruned)?;
        self.validate_gc_target_namespaces(expected_binding, &anchor.pruned)?;
        let mut observer = NoopObserver;
        self.finish_gc_targets(&current, predecessor, &anchor.pruned, &mut observer)?;
        let clean = self.audit(expected_binding)?;
        if !matches!(clean.status, StoreAuditStatus::Clean) {
            return Err(GenerationError::Invalid(
                "recovery DAG store is not clean after interrupted ancestor GC".into(),
            ));
        }
        lock.release()?;
        Ok(AncestorGcReport {
            current: current.pin,
            retained_predecessor: predecessor,
            pruned_generations: anchor.pruned,
        })
    }

    /// Bound on-disk generation history after the caller has durably advanced
    /// both `CURRENT` and its independent external pin. The selected generation
    /// and its exact immediate predecessor are never eligible. Older verified
    /// generations are first authorized in an fsynced GC anchor, then renamed
    /// out of the live namespace with a root-directory fsync before removal.
    pub fn prune_ancestors_keep_current_and_predecessor(
        &self,
        expected_binding: &RecoveryDagBinding,
        expected_current: GenerationPin,
    ) -> Result<AncestorGcReport> {
        let mut observer = NoopObserver;
        self.prune_ancestors_with_observer(expected_binding, expected_current, &mut observer)
    }

    fn prune_ancestors_with_observer<O: GcObserver>(
        &self,
        expected_binding: &RecoveryDagBinding,
        expected_current: GenerationPin,
        observer: &mut O,
    ) -> Result<AncestorGcReport> {
        self.ensure_root()?;
        let lock = StoreLock::acquire(&self.root)?;
        let current = self.load_current(expected_binding, Some(expected_current))?;
        let audit = self.audit(expected_binding)?;
        if !matches!(audit.status, StoreAuditStatus::Clean) {
            return Err(GenerationError::Invalid(
                "ancestor GC requires one clean generation head".into(),
            ));
        }
        let manifests = self.read_all_manifests(expected_binding)?;
        let predecessor = match current.manifest.previous_generation {
            Some(hash) => Some(GenerationPin {
                sequence: current.pin.sequence.checked_sub(1).ok_or_else(|| {
                    GenerationError::Invalid("current predecessor sequence underflows".into())
                })?,
                hash,
            }),
            None => None,
        };
        let mut targets: Vec<_> = manifests
            .iter()
            .filter_map(|(hash, manifest)| {
                let pin = GenerationPin {
                    sequence: manifest.sequence,
                    hash: *hash,
                };
                (pin != current.pin && Some(pin) != predecessor).then_some(pin)
            })
            .collect();
        targets.sort_by_key(|pin| (pin.sequence, pin.hash.0));
        if targets.is_empty() {
            lock.release()?;
            return Ok(AncestorGcReport {
                current: current.pin,
                retained_predecessor: predecessor,
                pruned_generations: Vec::new(),
            });
        }
        let predecessor = predecessor.ok_or_else(|| {
            GenerationError::Invalid("initial generation cannot have GC ancestors".into())
        })?;
        let predecessor_manifest = manifests.get(&predecessor.hash).ok_or_else(|| {
            GenerationError::Invalid("current generation predecessor is missing".into())
        })?;
        let missing_parent = predecessor_manifest
            .previous_generation
            .map(|hash| GenerationPin {
                sequence: predecessor.sequence.saturating_sub(1),
                hash,
            })
            .ok_or_else(|| {
                GenerationError::Invalid(
                    "GC targets exist before a sequence-zero predecessor".into(),
                )
            })?;
        if targets.last() != Some(&missing_parent) {
            return Err(GenerationError::Invalid(
                "GC targets are not the complete retained ancestry before the predecessor".into(),
            ));
        }
        for target in &targets {
            let generation = self.verify_generation(target.hash, expected_binding)?;
            if generation.pin != *target {
                return Err(GenerationError::Invalid(
                    "GC target pin differs from its verified manifest".into(),
                ));
            }
            inspect_active_log(&self.active_log_path(*target), &generation)?;
        }
        let anchor = GcAnchor {
            schema: GC_ANCHOR_SCHEMA.to_owned(),
            binding: expected_binding.clone(),
            authorized_by: current.pin,
            retained_boundary: predecessor,
            missing_parent,
            pruned: targets.clone(),
        };
        self.write_gc_anchor(&anchor, observer)?;
        self.finish_gc_targets(&current, Some(predecessor), &targets, observer)?;
        let clean = self.audit(expected_binding)?;
        if !matches!(clean.status, StoreAuditStatus::Clean)
            || clean.current.pin != current.pin
            || clean.generation_count != 2
        {
            return Err(GenerationError::Invalid(
                "ancestor GC did not preserve exactly current plus predecessor".into(),
            ));
        }
        lock.release()?;
        Ok(AncestorGcReport {
            current: current.pin,
            retained_predecessor: Some(predecessor),
            pruned_generations: targets,
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
        self.rebarrier_unselected_generation(successor.pin)?;
        let successor = self.verify_generation(successor_hash, &current.manifest.binding)?;
        validate_successor(&current, &successor.manifest)?;
        self.ensure_empty_active_log(&successor)?;
        fsync_directory(&self.root)?;
        let mut observer = NoopObserver;
        self.publish_pointer(&successor, &mut observer)?;
        lock.release()?;
        Ok(successor)
    }

    /// Re-publish the exact selected pointer through every namespace barrier.
    /// Startup uses this before creating an independent external pin so a
    /// second crash cannot preserve the pin while losing a late-visible
    /// `CURRENT` from the first crash.
    pub fn rebarrier_current_pointer(
        &self,
        expected_binding: &RecoveryDagBinding,
        expected_pin: GenerationPin,
    ) -> Result<VerifiedGeneration> {
        self.ensure_root()?;
        let lock = StoreLock::acquire(&self.root)?;
        let generation = self.load_current(expected_binding, Some(expected_pin))?;
        let mut observer = NoopObserver;
        self.publish_pointer(&generation, &mut observer)?;
        let selected = self.load_current(expected_binding, Some(expected_pin))?;
        lock.release()?;
        Ok(selected)
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

        let retained_records = write_record_log_durably(
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
        write_new_synced_file_durably(&manifest_path, &manifest_bytes)?;
        observer.reached(PublishPoint::ManifestSynced)?;
        fsync_directory(&pending)?;
        observer.reached(PublishPoint::GenerationDirectorySynced)?;

        let final_directory = self.generation_path(generation_hash);
        if final_directory.exists() {
            return Err(GenerationError::Invalid(format!(
                "content-addressed generation {generation_hash} already exists"
            )));
        }
        rename_for_durable_publish(&pending, &final_directory, false, "rename generation into")?;
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
        rename_for_durable_publish(&temporary, &current, true, "atomically replace")?;
        observer.reached(PublishPoint::PointerRenamed)?;
        fsync_directory(&self.root)?;
        observer.reached(PublishPoint::RootAfterPointerSynced)?;
        Ok(())
    }

    /// Recover the only dual-name state an older Unix no-replace helper could
    /// create: hard-linking an active log into its authorized GC tombstone and
    /// crashing before unlinking the live name. Both names must identify the
    /// exact same regular inode; otherwise ambiguity remains fail-closed.
    fn recover_legacy_gc_active_dual_names(&self, targets: &[GenerationPin]) -> Result<()> {
        for target in targets {
            let active = self.active_log_path(*target);
            let active_tombstone = self
                .root
                .join(format!(".gc-active-{}.bin", target.hash.to_hex()));
            if !active.exists() || !active_tombstone.exists() {
                continue;
            }
            #[cfg(unix)]
            {
                use std::os::unix::fs::MetadataExt as _;

                let live = regular_file_metadata(&active)?;
                let tombstone = regular_file_metadata(&active_tombstone)?;
                if live.dev() != tombstone.dev() || live.ino() != tombstone.ino() {
                    return Err(GenerationError::Invalid(
                        "GC active log exists as two different live/tombstone files".into(),
                    ));
                }
                fs::remove_file(&active).map_err(|error| {
                    io_error("finish legacy active-log GC rename", &active, error)
                })?;
                fsync_directory(&self.root)?;
            }
            #[cfg(not(unix))]
            {
                return Err(GenerationError::Invalid(
                    "GC target exists in both live and tombstone namespaces".into(),
                ));
            }
        }
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

    fn read_gc_anchor(&self, expected_binding: &RecoveryDagBinding) -> Result<Option<GcAnchor>> {
        let path = self.root.join(GC_ANCHOR_FILE);
        if !path.exists() {
            return Ok(None);
        }
        let bytes = read_small_regular_file(&path, MAX_GC_ANCHOR_BYTES)?;
        let anchor: GcAnchor = serde_json::from_slice(&bytes).map_err(|error| {
            GenerationError::Invalid(format!("GC anchor JSON is invalid: {error}"))
        })?;
        require_canonical_json(&bytes, &anchor, "GC anchor")?;
        if anchor.schema != GC_ANCHOR_SCHEMA || &anchor.binding != expected_binding {
            return Err(GenerationError::Invalid(
                "GC anchor has a foreign schema or recovery binding".into(),
            ));
        }
        Ok(Some(anchor))
    }

    fn write_gc_anchor<O: GcObserver>(&self, anchor: &GcAnchor, observer: &mut O) -> Result<()> {
        let bytes = canonical_json(anchor, "GC anchor")?;
        if bytes.len() as u64 > MAX_GC_ANCHOR_BYTES {
            return Err(GenerationError::Invalid(
                "GC anchor exceeds its hard byte cap".into(),
            ));
        }
        let temporary = self
            .root
            .join(format!(".GC-ANCHOR-{}.tmp", uuid::Uuid::new_v4()));
        write_new_synced_file(&temporary, &bytes)?;
        observer.reached(GcPoint::AnchorFileSynced)?;
        let destination = self.root.join(GC_ANCHOR_FILE);
        rename_for_durable_publish(&temporary, &destination, true, "atomically replace")?;
        observer.reached(GcPoint::AnchorRenamed)?;
        fsync_directory(&self.root)?;
        observer.reached(GcPoint::RootAfterAnchorSynced)
    }

    fn finish_gc_targets<O: GcObserver>(
        &self,
        current: &VerifiedGeneration,
        predecessor: Option<GenerationPin>,
        targets: &[GenerationPin],
        observer: &mut O,
    ) -> Result<()> {
        for target in targets {
            if *target == current.pin || Some(*target) == predecessor {
                return Err(GenerationError::Invalid(
                    "GC anchor attempts to remove current or its predecessor".into(),
                ));
            }
            let generation = self.generation_path(target.hash);
            let active = self.active_log_path(*target);
            let generation_tombstone = self.root.join(format!(".gc-gen-{}", target.hash.to_hex()));
            let active_tombstone = self
                .root
                .join(format!(".gc-active-{}.bin", target.hash.to_hex()));
            if generation.exists() && generation_tombstone.exists()
                || active.exists() && active_tombstone.exists()
            {
                return Err(GenerationError::Invalid(
                    "GC target exists in both live and tombstone namespaces".into(),
                ));
            }
            if generation.exists() {
                ensure_real_directory(&generation)?;
                rename_for_durable_publish(
                    &generation,
                    &generation_tombstone,
                    false,
                    "rename generation GC tombstone",
                )?;
                observer.reached(GcPoint::GenerationRenamed(*target))?;
                fsync_directory(&self.root)?;
                observer.reached(GcPoint::RootAfterGenerationRenameSynced(*target))?;
            }
            if active.exists() {
                regular_file_metadata(&active)?;
                rename_for_durable_publish(
                    &active,
                    &active_tombstone,
                    false,
                    "rename active-log GC tombstone",
                )?;
                observer.reached(GcPoint::ActiveLogRenamed(*target))?;
                fsync_directory(&self.root)?;
                observer.reached(GcPoint::RootAfterActiveLogRenameSynced(*target))?;
            }
            if generation_tombstone.exists() {
                ensure_real_directory(&generation_tombstone)?;
                let removed = match fs::remove_dir_all(&generation_tombstone) {
                    Ok(()) => true,
                    Err(error) if error.kind() == io::ErrorKind::NotFound => true,
                    #[cfg(windows)]
                    Err(error) => {
                        // The write-through rename already retired the live
                        // generation namespace. Antivirus/indexer locks may
                        // delay physical directory removal; the fsynced GC
                        // anchor authorizes an exact retry on next startup.
                        tracing::warn!(path = %generation_tombstone.display(), %error, "deferring recovery DAG generation tombstone cleanup");
                        false
                    }
                    #[cfg(not(windows))]
                    Err(error) => {
                        return Err(io_error(
                            "remove generation GC tombstone",
                            &generation_tombstone,
                            error,
                        ));
                    }
                };
                if removed {
                    observer.reached(GcPoint::GenerationRemoved(*target))?;
                    fsync_directory(&self.root)?;
                    observer.reached(GcPoint::RootAfterGenerationRemoveSynced(*target))?;
                }
            }
            if active_tombstone.exists() {
                regular_file_metadata(&active_tombstone)?;
                let removed = match fs::remove_file(&active_tombstone) {
                    Ok(()) => true,
                    Err(error) if error.kind() == io::ErrorKind::NotFound => true,
                    #[cfg(windows)]
                    Err(error) => {
                        tracing::warn!(path = %active_tombstone.display(), %error, "deferring recovery DAG active-log tombstone cleanup");
                        false
                    }
                    #[cfg(not(windows))]
                    Err(error) => {
                        return Err(io_error(
                            "remove active-log GC tombstone",
                            &active_tombstone,
                            error,
                        ));
                    }
                };
                if removed {
                    observer.reached(GcPoint::ActiveLogRemoved(*target))?;
                    fsync_directory(&self.root)?;
                    observer.reached(GcPoint::RootAfterActiveLogRemoveSynced(*target))?;
                }
            }
        }
        Ok(())
    }

    fn validate_gc_target_namespaces(
        &self,
        expected_binding: &RecoveryDagBinding,
        targets: &[GenerationPin],
    ) -> Result<()> {
        for target in targets {
            let generation = self.generation_path(target.hash);
            let active = self.active_log_path(*target);
            let generation_tombstone = self.root.join(format!(".gc-gen-{}", target.hash.to_hex()));
            let active_tombstone = self
                .root
                .join(format!(".gc-active-{}.bin", target.hash.to_hex()));
            if generation.exists() && generation_tombstone.exists()
                || active.exists() && active_tombstone.exists()
            {
                return Err(GenerationError::Invalid(
                    "GC target exists in both live and tombstone namespaces".into(),
                ));
            }
            if generation.exists() {
                let verified = self.verify_generation(target.hash, expected_binding)?;
                if verified.pin != *target {
                    return Err(GenerationError::Invalid(
                        "live GC target pin differs from its anchor".into(),
                    ));
                }
                if active.exists() {
                    inspect_active_log(&active, &verified)?;
                }
            }
            if active.exists() {
                regular_file_metadata(&active)?;
            }
            if generation_tombstone.exists() {
                ensure_real_directory(&generation_tombstone)?;
            }
            if active_tombstone.exists() {
                regular_file_metadata(&active_tombstone)?;
            }
        }
        Ok(())
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

    fn rebarrier_unselected_generation(&self, pin: GenerationPin) -> Result<()> {
        let generation = self.generation_path(pin.hash);
        ensure_real_directory(&generation)?;
        #[cfg(windows)]
        {
            // An unselected generation may have become visible even though a
            // prior write-through rename reported a late error. Temporarily
            // move it through one deterministic recovery namespace, then
            // write-through move it back before CURRENT can depend on it. The
            // locked startup path restores that exact intermediate if either
            // move is interrupted.
            let staging = self
                .root
                .join(format!(".generation-rebarrier-{}", pin.hash.to_hex()));
            if staging.exists() {
                return Err(GenerationError::Invalid(format!(
                    "generation rebarrier staging already exists: {}",
                    staging.display()
                )));
            }
            rename_for_durable_publish(
                &generation,
                &staging,
                false,
                "stage unselected generation rebarrier",
            )?;
            fsync_directory(&self.root)?;
            rename_for_durable_publish(
                &staging,
                &generation,
                false,
                "restore rebarriered unselected generation",
            )?;
            fsync_directory(&self.root)
        }
        #[cfg(not(windows))]
        {
            // Unix generation publication already fsyncs the parent. Repeat
            // that inexpensive barrier without creating a live-name gap.
            fsync_directory(&self.root)
        }
    }

    fn create_empty_active_log(&self, generation: &VerifiedGeneration) -> Result<()> {
        let path = self.active_log_path(generation.pin);
        let header = ActiveLogHeader {
            generation_pin: generation.pin,
            binding: generation.manifest.binding.clone(),
            limits: generation.manifest.retained_records.limits,
        };
        let bytes = encode_active_header(&header);
        write_new_synced_file_durably(&path, &bytes)
    }

    fn ensure_empty_active_log(&self, generation: &VerifiedGeneration) -> Result<()> {
        let path = self.active_log_path(generation.pin);
        if !path.exists() {
            return self.create_empty_active_log(generation);
        }
        let header = ActiveLogHeader {
            generation_pin: generation.pin,
            binding: generation.manifest.binding.clone(),
            limits: generation.manifest.retained_records.limits,
        };
        let bytes = encode_active_header(&header);
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
        // Re-publish the exact header through the durable replace primitive.
        // This closes the retry window where an earlier namespace operation
        // made the name visible but returned before its durability guarantee.
        replace_synced_file_durably(&path, &bytes, "rebarrier empty active log")
    }

    fn ensure_root(&self) -> Result<()> {
        let parent = self
            .root
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        parent.canonicalize().map_err(|error| {
            io_error(
                "canonicalize existing generation-store parent",
                parent,
                error,
            )
        })?;
        let namespace_lock =
            arc_crypto::secret_file::acquire_private_directory_namespace_lock(&self.root)
                .map_err(|error| io_error("lock generation-store namespace", &self.root, error))?;
        // Restore before any absent-root check. Creating a fresh empty live
        // root while the complete store is staged would strand chain history.
        namespace_lock
            .restore_interrupted()
            .map_err(|error| io_error("restore generation-store namespace", &self.root, error))?;
        if namespace_lock.target().exists() {
            ensure_real_directory(namespace_lock.target())?;
        }
        arc_crypto::secret_file::create_private_directory_tree(namespace_lock.target())
            .map_err(|error| io_error("secure generation store", &self.root, error))
    }

    #[cfg(test)]
    fn create_initial_with_observer<I, O>(
        &self,
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
        if self.root.join(CURRENT_FILE).exists() || self.generation_directory_count()? != 0 {
            return Err(GenerationError::Invalid(
                "test initial generation store is not empty".into(),
            ));
        }
        let result = self.publish_generation(None, input, records, observer);
        lock.release()?;
        result
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

#[derive(Clone, Copy)]
enum IncompleteStoreArtifact {
    File,
    Directory,
}

fn uuid_staging_name(name: &str, prefix: &str, suffix: &str) -> bool {
    name.strip_prefix(prefix)
        .and_then(|value| value.strip_suffix(suffix))
        .is_some_and(|value| uuid::Uuid::parse_str(value).is_ok())
}

fn incomplete_store_artifact(name: &str) -> Option<IncompleteStoreArtifact> {
    if uuid_staging_name(name, ".pending-", "") {
        return Some(IncompleteStoreArtifact::Directory);
    }
    if uuid_staging_name(name, ".CURRENT-", ".tmp")
        || uuid_staging_name(name, ".GC-ANCHOR-", ".tmp")
        || ["file", "replacement", "records"]
            .iter()
            .any(|label| uuid_staging_name(name, &format!(".arc-recovery-dag-{label}-"), ".tmp"))
    {
        return Some(IncompleteStoreArtifact::File);
    }
    None
}

fn generation_rebarrier_hash(name: &str) -> Option<Hash256> {
    let value = name.strip_prefix(".generation-rebarrier-")?;
    if value.len() != 64
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return None;
    }
    Hash256::from_hex(value).ok()
}

fn restore_interrupted_generation_rebarriers(root: &Path) -> Result<()> {
    let entries = fs::read_dir(root)
        .map_err(|error| io_error("inspect generation rebarrier staging in", root, error))?;
    for entry in entries {
        let entry = entry
            .map_err(|error| io_error("inspect generation rebarrier staging in", root, error))?;
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        let Some(hash) = generation_rebarrier_hash(&name) else {
            continue;
        };
        let staging = entry.path();
        ensure_real_directory(&staging)?;
        let generation = root.join(format!("gen-{}", hash.to_hex()));
        if generation.exists() {
            return Err(GenerationError::Invalid(format!(
                "generation exists in both live and rebarrier namespaces: {}",
                hash
            )));
        }
        rename_for_durable_publish(
            &staging,
            &generation,
            false,
            "restore interrupted generation rebarrier",
        )?;
        fsync_directory(root)?;
    }
    Ok(())
}

/// Reclaim only names that ARC itself reserves for unpublished work. The
/// advisory writer lock must already be held: without it, a second process
/// could mistake an actively-written generation for a crash orphan.
fn cleanup_incomplete_store_staging(root: &Path) -> Result<()> {
    let entries = fs::read_dir(root)
        .map_err(|error| io_error("inspect incomplete generation staging in", root, error))?;
    for entry in entries {
        let entry = entry
            .map_err(|error| io_error("inspect incomplete generation staging in", root, error))?;
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        let Some(kind) = incomplete_store_artifact(&name) else {
            continue;
        };
        let path = entry.path();
        let metadata = fs::symlink_metadata(&path)
            .map_err(|error| io_error("inspect incomplete generation staging", &path, error))?;
        let valid_type = !metadata.file_type().is_symlink()
            && match kind {
                IncompleteStoreArtifact::File => metadata.is_file(),
                IncompleteStoreArtifact::Directory => metadata.is_dir(),
            };
        if !valid_type {
            return Err(GenerationError::Invalid(format!(
                "recognized recovery DAG staging artifact has an invalid type: {}",
                path.display()
            )));
        }
        let removal = match kind {
            IncompleteStoreArtifact::File => fs::remove_file(&path),
            IncompleteStoreArtifact::Directory => fs::remove_dir_all(&path),
        };
        if let Err(error) = removal {
            if error.kind() != io::ErrorKind::NotFound {
                tracing::warn!(path = %path.display(), %error, "deferring recovery DAG staging cleanup");
            }
            continue;
        }
        if let Err(error) = fsync_directory(root) {
            // These names are never interpreted as live state. If a late
            // namespace-barrier error permits resurrection, the next locked
            // startup recognizes and removes the exact same name again.
            tracing::warn!(path = %path.display(), %error, "recovery DAG staging cleanup barrier will be retried");
        }
    }
    Ok(())
}

impl StoreLock {
    fn acquire(root: &Path) -> Result<Self> {
        let namespace_lock =
            arc_crypto::secret_file::acquire_private_directory_namespace_lock(root)
                .map_err(|error| io_error("lock generation-store namespace", root, error))?;
        namespace_lock
            .restore_interrupted()
            .map_err(|error| io_error("restore generation-store namespace", root, error))?;
        arc_crypto::secret_file::create_private_directory_tree(namespace_lock.target())
            .map_err(|error| io_error("secure generation-store root", root, error))?;
        let root = namespace_lock.target();
        let path = root.join(WRITE_LOCK_FILE);
        let file = open_and_try_lock_store_file(&path)?;

        // NTFS rejects a directory rename while any descendant file handle is
        // open, even when that handle allows FILE_SHARE_DELETE (MS-FSA
        // 2.1.5.15.12). Probe the legacy inner lock first, then close our own
        // descendant handle while the stable sibling namespace lock remains
        // held. The second try-lock prevents any legacy writer that appeared
        // during the rename window from being followed by patched mutation.
        #[cfg(windows)]
        let file = {
            file.unlock().map_err(|error| {
                io_error(
                    "release advisory lock before Windows namespace rebarrier on",
                    &path,
                    error,
                )
            })?;
            drop(file);
            namespace_lock
                .rebarrier_existing()
                .map_err(|error| io_error("rebarrier generation-store namespace", root, error))?;
            open_and_try_lock_store_file(&path)?
        };
        #[cfg(not(windows))]
        namespace_lock
            .rebarrier_existing()
            .map_err(|error| io_error("rebarrier generation-store namespace", root, error))?;
        let mut file = file;
        restore_interrupted_generation_rebarriers(root)?;
        cleanup_incomplete_store_staging(root)?;
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

fn open_and_try_lock_store_file(path: &Path) -> Result<File> {
    match fs::symlink_metadata(path) {
        Ok(_) => {
            regular_file_metadata(path)?;
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(io_error("inspect", path, error)),
    }
    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true);
    set_private_file_mode(&mut options);
    let file = options
        .open(path)
        .map_err(|error| io_error("open/create", path, error))?;
    if !file
        .metadata()
        .map_err(|error| io_error("inspect open", path, error))?
        .is_file()
    {
        return Err(GenerationError::Invalid(format!(
            "{} is not a regular lock file",
            path.display()
        )));
    }
    match file.try_lock() {
        Ok(()) => Ok(file),
        Err(std::fs::TryLockError::WouldBlock) => Err(GenerationError::Locked(path.to_path_buf())),
        Err(std::fs::TryLockError::Error(error)) => {
            Err(io_error("acquire advisory lock on", path, error))
        }
    }
}

impl Drop for StoreLock {
    fn drop(&mut self) {
        if !self.released {
            let _ = self.file.unlock();
        }
    }
}

fn validate_gc_anchor(
    anchor: &GcAnchor,
    current: &VerifiedGeneration,
    manifests: &HashMap<Hash256, GenerationManifest>,
) -> Result<()> {
    if anchor.pruned.is_empty()
        || anchor.retained_boundary.sequence == 0
        || anchor.missing_parent.sequence.checked_add(1) != Some(anchor.retained_boundary.sequence)
        || anchor.pruned.last() != Some(&anchor.missing_parent)
    {
        return Err(GenerationError::Invalid(
            "GC anchor has an invalid retained/pruned boundary".into(),
        ));
    }
    for pair in anchor.pruned.windows(2) {
        if pair[0].sequence.checked_add(1) != Some(pair[1].sequence) {
            return Err(GenerationError::Invalid(
                "GC anchor pruned pins are not sequence-contiguous".into(),
            ));
        }
    }
    let boundary = manifests
        .get(&anchor.retained_boundary.hash)
        .ok_or_else(|| GenerationError::Invalid("GC retained boundary is missing".into()))?;
    if boundary.sequence != anchor.retained_boundary.sequence
        || boundary.previous_generation != Some(anchor.missing_parent.hash)
    {
        return Err(GenerationError::Invalid(
            "GC retained boundary does not commit to its named missing parent".into(),
        ));
    }
    for pin in &anchor.pruned {
        if let Some(manifest) = manifests.get(&pin.hash)
            && manifest.sequence != pin.sequence
        {
            return Err(GenerationError::Invalid(
                "GC anchor pin differs from its still-present manifest".into(),
            ));
        }
    }

    // The generation that authorized deletion must still be on the exact
    // present path from externally selected CURRENT down to the retained
    // boundary. This prevents a stale anchor from authorizing a fork head.
    let mut cursor = current.pin;
    let mut saw_authorizer = false;
    loop {
        if cursor == anchor.authorized_by {
            saw_authorizer = true;
        }
        if cursor == anchor.retained_boundary {
            break;
        }
        let manifest = manifests.get(&cursor.hash).ok_or_else(|| {
            GenerationError::Invalid("GC anchor authorization path is incomplete".into())
        })?;
        let parent = manifest.previous_generation.ok_or_else(|| {
            GenerationError::Invalid("GC anchor boundary is not an ancestor of CURRENT".into())
        })?;
        cursor = GenerationPin {
            sequence: cursor.sequence.checked_sub(1).ok_or_else(|| {
                GenerationError::Invalid("GC authorization sequence underflows".into())
            })?,
            hash: parent,
        };
    }
    if !saw_authorizer {
        return Err(GenerationError::Invalid(
            "GC authorizing generation is not on the current retained chain".into(),
        ));
    }
    Ok(())
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

fn write_record_log_durably<I>(
    path: &Path,
    limits: RetentionLimits,
    cursor: &DagCursor,
    records: I,
) -> Result<RetainedRecordSet>
where
    I: IntoIterator<Item = RetainedDagRecord>,
{
    let staging = durable_staging_path(path, "records");
    let retained = match write_record_log(&staging, limits, cursor, records) {
        Ok(retained) => retained,
        Err(error) => {
            let _ = fs::remove_file(&staging);
            return Err(error);
        }
    };
    if let Err(error) =
        rename_for_durable_publish(&staging, path, false, "publish retained record log")
    {
        let _ = fs::remove_file(&staging);
        return Err(error);
    }
    fsync_directory(path.parent().unwrap_or_else(|| Path::new(".")))?;
    Ok(retained)
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
        // A prior publish can make the exact name visible and still return a
        // late durability error. Replacing it with the same synced bytes makes
        // the evidence namespace durable before the source can be truncated.
        replace_synced_file_durably(path, bytes, "rebarrier active quarantine")?;
        return Ok(());
    }
    match write_new_synced_file_durably(path, bytes) {
        Ok(()) => {}
        Err(GenerationError::Io { source, .. })
            if source.kind() == io::ErrorKind::AlreadyExists =>
        {
            return persist_exact_quarantine(root, path, bytes, expected_hash);
        }
        Err(error) => return Err(error),
    }
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

/// Create a recovery directory and durably publish its name before returning.
/// Production callers hold the node data-directory lock while this briefly
/// rebarriers an existing root; standalone callers must provide equivalent
/// semantic serialization.
pub fn create_private_directory_durably(path: &Path) -> Result<()> {
    let namespace_lock = arc_crypto::secret_file::acquire_private_directory_namespace_lock(path)
        .map_err(|error| io_error("lock store directory namespace", path, error))?;
    namespace_lock
        .restore_interrupted()
        .map_err(|error| io_error("restore store directory namespace", path, error))?;
    arc_crypto::secret_file::create_private_directory_tree(namespace_lock.target())
        .map_err(|error| io_error("secure/create store directory", path, error))?;
    namespace_lock
        .rebarrier_existing()
        .map_err(|error| io_error("rebarrier store directory namespace", path, error))
}

fn set_private_file_mode(_options: &mut OpenOptions) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        _options.mode(0o600);
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

fn durable_staging_path(destination: &Path, label: &str) -> PathBuf {
    let parent = destination
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    parent.join(format!(
        ".arc-recovery-dag-{label}-{}.tmp",
        uuid::Uuid::new_v4()
    ))
}

fn write_new_synced_file_durably(path: &Path, bytes: &[u8]) -> Result<()> {
    let staging = durable_staging_path(path, "file");
    if let Err(error) = write_new_synced_file(&staging, bytes) {
        let _ = fs::remove_file(&staging);
        return Err(error);
    }
    if let Err(error) = rename_for_durable_publish(&staging, path, false, "publish synced file") {
        let _ = fs::remove_file(&staging);
        return Err(error);
    }
    fsync_directory(path.parent().unwrap_or_else(|| Path::new(".")))
}

fn replace_synced_file_durably(path: &Path, bytes: &[u8], operation: &'static str) -> Result<()> {
    let staging = durable_staging_path(path, "replacement");
    if let Err(error) = write_new_synced_file(&staging, bytes) {
        let _ = fs::remove_file(&staging);
        return Err(error);
    }
    if let Err(error) = rename_for_durable_publish(&staging, path, true, operation) {
        let _ = fs::remove_file(&staging);
        return Err(error);
    }
    fsync_directory(path.parent().unwrap_or_else(|| Path::new(".")))
}

#[cfg(any(target_os = "linux", target_os = "android"))]
fn rename_no_replace(source: &Path, destination: &Path) -> io::Result<()> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt as _;

    let source = CString::new(source.as_os_str().as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "source path contains NUL"))?;
    let destination = CString::new(destination.as_os_str().as_bytes()).map_err(|_| {
        io::Error::new(io::ErrorKind::InvalidInput, "destination path contains NUL")
    })?;
    if unsafe {
        libc::renameat2(
            libc::AT_FDCWD,
            source.as_ptr(),
            libc::AT_FDCWD,
            destination.as_ptr(),
            libc::RENAME_NOREPLACE,
        )
    } == 0
    {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

#[cfg(any(target_os = "macos", target_os = "ios"))]
fn rename_no_replace(source: &Path, destination: &Path) -> io::Result<()> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt as _;

    let source = CString::new(source.as_os_str().as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "source path contains NUL"))?;
    let destination = CString::new(destination.as_os_str().as_bytes()).map_err(|_| {
        io::Error::new(io::ErrorKind::InvalidInput, "destination path contains NUL")
    })?;
    if unsafe { libc::renamex_np(source.as_ptr(), destination.as_ptr(), libc::RENAME_EXCL) } == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

#[cfg(all(
    unix,
    not(any(
        target_os = "linux",
        target_os = "android",
        target_os = "macos",
        target_os = "ios"
    ))
))]
fn rename_no_replace(_source: &Path, _destination: &Path) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "atomic create-only directory rename is unsupported on this Unix platform",
    ))
}

/// Rename one already-synced recovery artifact into its live namespace.
///
/// Windows performs the namespace mutation with `MOVEFILE_WRITE_THROUGH`;
/// Unix callers must fsync the destination parent after this returns.
pub fn rename_for_durable_publish(
    source: &Path,
    destination: &Path,
    replace_existing: bool,
    operation: &'static str,
) -> Result<()> {
    #[cfg(windows)]
    {
        // Windows exposes no documented parent-directory fsync. Its supported
        // durability primitive is a write-through namespace move, which also
        // works for same-volume directory publication.
        arc_crypto::secret_file::windows_move_path_write_through(
            source,
            destination,
            replace_existing,
        )
        .map_err(|error| io_error(operation, destination, error))?;
        Ok(())
    }
    #[cfg(unix)]
    {
        if replace_existing {
            return fs::rename(source, destination)
                .map_err(|error| io_error(operation, destination, error));
        }
        // Linux and Darwin expose an atomic create-only rename for files and
        // directories. Using one namespace operation avoids the dual-name
        // crash state produced by hard-link-then-unlink publication.
        rename_no_replace(source, destination)
            .map_err(|error| io_error(operation, destination, error))
    }
    #[cfg(not(any(unix, windows)))]
    {
        if !replace_existing && destination.exists() {
            return Err(io_error(
                operation,
                destination,
                io::Error::new(io::ErrorKind::AlreadyExists, "destination already exists"),
            ));
        }
        fs::rename(source, destination).map_err(|error| io_error(operation, destination, error))
    }
}

fn fsync_directory(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        File::open(path)
            .map_err(|error| io_error("open directory", path, error))?
            .sync_all()
            .map_err(|error| io_error("fsync directory", path, error))
    }
    #[cfg(not(unix))]
    {
        // Windows namespace mutations use MoveFileExW with
        // MOVEFILE_WRITE_THROUGH at the mutation site. Other non-Unix targets
        // do not expose a portable directory-fsync primitive.
        let _ = path;
        Ok(())
    }
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

    #[test]
    fn locked_startup_reclaims_only_exact_incomplete_store_staging() {
        let directory = TestDirectory::new("staging-cleanup");
        let pending = directory
            .0
            .join(format!(".pending-{}", uuid::Uuid::new_v4()));
        fs::create_dir(&pending).unwrap();
        fs::write(pending.join(RECORDS_FILE), vec![7u8; 1024 * 1024]).unwrap();
        let stale_files = [
            directory
                .0
                .join(format!(".CURRENT-{}.tmp", uuid::Uuid::new_v4())),
            directory
                .0
                .join(format!(".GC-ANCHOR-{}.tmp", uuid::Uuid::new_v4())),
            directory.0.join(format!(
                ".arc-recovery-dag-file-{}.tmp",
                uuid::Uuid::new_v4()
            )),
            directory.0.join(format!(
                ".arc-recovery-dag-replacement-{}.tmp",
                uuid::Uuid::new_v4()
            )),
            directory.0.join(format!(
                ".arc-recovery-dag-records-{}.tmp",
                uuid::Uuid::new_v4()
            )),
        ];
        for path in &stale_files {
            fs::write(path, b"incomplete internal staging").unwrap();
        }
        let unrelated = [
            directory.0.join(".pending-not-a-uuid"),
            directory.0.join(".CURRENT-not-a-uuid.tmp"),
            directory.0.join(".arc-recovery-dag-records-not-a-uuid.tmp"),
        ];
        for path in &unrelated {
            fs::write(path, b"operator file").unwrap();
        }

        StoreLock::acquire(&directory.0).unwrap().release().unwrap();
        assert!(!pending.exists());
        assert!(stale_files.iter().all(|path| !path.exists()));
        assert!(unrelated.iter().all(|path| path.exists()));
    }

    #[test]
    fn locked_startup_rejects_a_recognized_staging_name_with_wrong_type() {
        let directory = TestDirectory::new("staging-type");
        let invalid = directory
            .0
            .join(format!(".pending-{}", uuid::Uuid::new_v4()));
        fs::write(&invalid, b"not a directory").unwrap();
        assert!(matches!(
            StoreLock::acquire(&directory.0),
            Err(GenerationError::Invalid(message))
                if message.contains("staging artifact has an invalid type")
        ));
    }

    #[test]
    fn locked_startup_restores_an_interrupted_generation_rebarrier() {
        let directory = TestDirectory::new("generation-rebarrier-restore");
        let hash = h(b"interrupted-generation-rebarrier");
        let staging = directory
            .0
            .join(format!(".generation-rebarrier-{}", hash.to_hex()));
        fs::create_dir(&staging).unwrap();
        fs::write(staging.join(MANIFEST_FILE), b"preserved candidate").unwrap();

        StoreLock::acquire(&directory.0).unwrap().release().unwrap();
        let restored = directory.0.join(format!("gen-{}", hash.to_hex()));
        assert!(!staging.exists());
        assert_eq!(
            fs::read(restored.join(MANIFEST_FILE)).unwrap(),
            b"preserved candidate"
        );
    }

    #[test]
    fn generation_store_requires_an_existing_parent_then_secures_the_root() {
        let directory = TestDirectory::new("nested-root");
        let root = directory.0.join("missing").join("nested").join("store");
        let store = GenerationStore::new(&root);
        assert!(store.ensure_root().is_err());
        assert!(!directory.0.join("missing").exists());

        arc_crypto::secret_file::create_private_directory_tree(root.parent().unwrap()).unwrap();
        store.ensure_root().unwrap();
        assert!(root.is_dir());
        arc_crypto::secret_file::validate_private_directory(&root).unwrap();
        arc_crypto::secret_file::validate_private_directory(root.parent().unwrap()).unwrap();
    }

    #[cfg(windows)]
    #[test]
    fn generation_store_restores_a_staged_full_root_before_absent_create() {
        let directory = TestDirectory::new("staged-full-root");
        let root = directory.0.join("store");
        arc_crypto::secret_file::create_private_directory_tree(&root).unwrap();
        let sentinel = root.join("canonical-history");
        fs::write(&sentinel, b"must not be stranded").unwrap();
        let digest = arc_crypto::secret_file::namespace_path_digest(&root).unwrap();
        let staged = directory.0.join(format!(
            ".arc-private-directory-namespace-{digest}.rebarrier"
        ));
        arc_crypto::secret_file::windows_move_path_write_through(&root, &staged, false).unwrap();

        let store = GenerationStore::new(&root);
        store.ensure_root().unwrap();
        assert_eq!(fs::read(&sentinel).unwrap(), b"must not be stranded");
        assert!(!staged.exists());
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

    #[test]
    fn durable_publish_moves_directories_and_replaces_files() {
        let directory = TestDirectory::new("durable-publish");
        let directly_published = directory.0.join("directly-published");
        write_new_synced_file_durably(&directly_published, b"synced").unwrap();
        assert_eq!(fs::read(directly_published).unwrap(), b"synced");

        let source_directory = directory.0.join("source-directory");
        let destination_directory = directory.0.join("destination-directory");
        create_private_directory(&source_directory).unwrap();
        write_new_synced_file(&source_directory.join("payload"), b"directory").unwrap();
        rename_for_durable_publish(
            &source_directory,
            &destination_directory,
            false,
            "test publish directory",
        )
        .unwrap();
        fsync_directory(&directory.0).unwrap();
        assert_eq!(
            fs::read(destination_directory.join("payload")).unwrap(),
            b"directory"
        );

        let first = directory.0.join("first.tmp");
        let second = directory.0.join("second.tmp");
        let current = directory.0.join("CURRENT");
        write_new_synced_file(&first, b"first").unwrap();
        rename_for_durable_publish(&first, &current, false, "test publish file").unwrap();
        let blocked = directory.0.join("blocked.tmp");
        write_new_synced_file(&blocked, b"blocked").unwrap();
        assert!(
            rename_for_durable_publish(&blocked, &current, false, "test reject replacement",)
                .is_err()
        );
        assert!(blocked.exists());
        assert_eq!(fs::read(&current).unwrap(), b"first");
        write_new_synced_file(&second, b"second").unwrap();
        rename_for_durable_publish(&second, &current, true, "test replace file").unwrap();
        fsync_directory(&directory.0).unwrap();
        assert_eq!(fs::read(current).unwrap(), b"second");
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

    fn five_generation_chain(store: &GenerationStore) -> Vec<VerifiedGeneration> {
        let mut generations = vec![store.create_initial(input(10, 5, 20), records(20)).unwrap()];
        for offset in 1..5u64 {
            let previous = generations.last().unwrap().pin;
            generations.push(
                store
                    .append(
                        previous,
                        input(10 + offset, 5 + offset, 20 + offset),
                        records(20 + offset),
                    )
                    .unwrap(),
            );
        }
        generations
    }

    struct FailingGcObserver {
        fail_at: GcPoint,
        reached_failure: bool,
    }

    impl GcObserver for FailingGcObserver {
        fn reached(&mut self, point: GcPoint) -> Result<()> {
            if point == self.fail_at {
                self.reached_failure = true;
                return Err(GenerationError::Invalid(format!(
                    "injected ancestor GC crash after {point:?}"
                )));
            }
            Ok(())
        }
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
    fn ancestor_gc_recovers_every_multi_target_rename_remove_and_fsync_crash() {
        let probe_parent = TestDirectory::new("gc-crash-probe");
        let probe_store = GenerationStore::new(probe_parent.0.join("store"));
        let probe_generations = five_generation_chain(&probe_store);
        let targets: Vec<_> = probe_generations[..3]
            .iter()
            .map(|generation| generation.pin)
            .collect();
        let mut crash_points = vec![
            GcPoint::AnchorFileSynced,
            GcPoint::AnchorRenamed,
            GcPoint::RootAfterAnchorSynced,
        ];
        for target in &targets {
            crash_points.extend([
                GcPoint::GenerationRenamed(*target),
                GcPoint::RootAfterGenerationRenameSynced(*target),
                GcPoint::ActiveLogRenamed(*target),
                GcPoint::RootAfterActiveLogRenameSynced(*target),
                GcPoint::GenerationRemoved(*target),
                GcPoint::RootAfterGenerationRemoveSynced(*target),
                GcPoint::ActiveLogRemoved(*target),
                GcPoint::RootAfterActiveLogRemoveSynced(*target),
            ]);
        }
        drop(probe_generations);
        drop(probe_store);
        drop(probe_parent);

        for crash_point in crash_points {
            let parent = TestDirectory::new("gc-crash-matrix");
            let store = GenerationStore::new(parent.0.join("store"));
            let generations = five_generation_chain(&store);
            let current = generations[4].pin;
            let predecessor = generations[3].pin;
            assert_eq!(
                generations[..3]
                    .iter()
                    .map(|generation| generation.pin)
                    .collect::<Vec<_>>(),
                targets,
                "content-addressed fixture pins must be deterministic"
            );
            let mut observer = FailingGcObserver {
                fail_at: crash_point,
                reached_failure: false,
            };
            let error = store
                .prune_ancestors_with_observer(&binding(), current, &mut observer)
                .expect_err("fault observer must interrupt ancestor GC");
            assert!(observer.reached_failure, "missing point {crash_point:?}");
            assert!(error.to_string().contains("injected ancestor GC crash"));

            // Startup recovery must never run the ordinary ancestry audit
            // before finishing an anchored multi-target deletion. Pre-anchor
            // faults legitimately leave the original clean chain intact.
            store.recover_interrupted_ancestor_gc(&binding()).unwrap();
            let recovered = store.audit(&binding()).unwrap();
            assert_eq!(recovered.current.pin, current);
            assert!(matches!(recovered.status, StoreAuditStatus::Clean));
            assert!(matches!(recovered.generation_count, 2 | 5));
            if recovered.generation_count == 5 {
                store
                    .prune_ancestors_keep_current_and_predecessor(&binding(), current)
                    .unwrap();
            }

            let final_audit = store.audit(&binding()).unwrap();
            assert_eq!(final_audit.current.pin, current);
            assert_eq!(final_audit.generation_count, 2);
            assert_eq!(final_audit.status, StoreAuditStatus::Clean);
            assert!(store.generation_path(current.hash).is_dir());
            assert!(store.active_log_path(current).is_file());
            assert!(store.generation_path(predecessor.hash).is_dir());
            assert!(store.active_log_path(predecessor).is_file());
            for target in &targets {
                assert!(!store.generation_path(target.hash).exists());
                assert!(!store.active_log_path(*target).exists());
                assert!(
                    !store
                        .root
                        .join(format!(".gc-gen-{}", target.hash.to_hex()))
                        .exists()
                );
                assert!(
                    !store
                        .root
                        .join(format!(".gc-active-{}.bin", target.hash.to_hex()))
                        .exists()
                );
            }
        }
    }

    #[cfg(unix)]
    #[test]
    fn ancestor_gc_recovers_legacy_hard_link_dual_active_names() {
        let parent = TestDirectory::new("gc-legacy-dual-active");
        let store = GenerationStore::new(parent.0.join("store"));
        let generations = five_generation_chain(&store);
        let current = generations[4].pin;
        let target = generations[0].pin;
        let mut observer = FailingGcObserver {
            fail_at: GcPoint::ActiveLogRenamed(target),
            reached_failure: false,
        };
        store
            .prune_ancestors_with_observer(&binding(), current, &mut observer)
            .expect_err("fault observer must interrupt active-log GC");
        assert!(observer.reached_failure);

        let live = store.active_log_path(target);
        let tombstone = store
            .root
            .join(format!(".gc-active-{}.bin", target.hash.to_hex()));
        assert!(!live.exists());
        assert!(tombstone.is_file());
        fs::hard_link(&tombstone, &live).unwrap();
        fsync_directory(&store.root).unwrap();

        store.recover_interrupted_ancestor_gc(&binding()).unwrap();
        let audit = store.audit(&binding()).unwrap();
        assert_eq!(audit.current.pin, current);
        assert_eq!(audit.generation_count, 2);
        assert_eq!(audit.status, StoreAuditStatus::Clean);
        assert!(!live.exists());
        assert!(!tombstone.exists());
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
    fn interrupted_initial_publication_resumes_only_the_exact_empty_boundary() {
        for point in [
            PublishPoint::GenerationPublished,
            PublishPoint::RootAfterGenerationSynced,
            PublishPoint::ActiveLogSynced,
            PublishPoint::RootAfterActiveLogSynced,
            PublishPoint::PointerFileSynced,
        ] {
            let parent = TestDirectory::new("resume-initial");
            let store = GenerationStore::new(parent.0.join("store"));
            let expected = input(10, 5, 20);
            let mut observer = RecordingObserver {
                fail_after: Some(point),
                ..RecordingObserver::default()
            };
            assert!(matches!(
                store.create_initial_with_observer(
                    expected.clone(),
                    std::iter::empty(),
                    &mut observer,
                ),
                Err(GenerationError::InjectedFailure(failed)) if failed == point
            ));
            assert!(!store.root.join(CURRENT_FILE).exists());

            let resumed = store
                .resume_unselected_initial(&expected)
                .unwrap()
                .expect("published sequence-zero generation must resume");
            assert_eq!(resumed.pin.sequence, 0);
            let active = inspect_active_log(&store.active_log_path(resumed.pin), &resumed).unwrap();
            assert_eq!(active.suffix, TornSuffix::Clean);
            assert_eq!(active.total_file_bytes, ACTIVE_HEADER_BYTES);
            assert_eq!(active.record_count, 0);
            let audit = store.audit(&expected.binding).unwrap();
            assert_eq!(audit.status, StoreAuditStatus::Clean);
            assert_eq!(audit.generation_count, 1);
            assert!(store.resume_unselected_initial(&expected).is_err());
        }

        let parent = TestDirectory::new("reject-nonempty-initial");
        let store = GenerationStore::new(parent.0.join("store"));
        let expected = input(10, 5, 20);
        let mut observer = RecordingObserver {
            fail_after: Some(PublishPoint::RootAfterGenerationSynced),
            ..RecordingObserver::default()
        };
        assert!(
            store
                .create_initial_with_observer(expected.clone(), records(20), &mut observer)
                .is_err()
        );
        assert!(store.resume_unselected_initial(&expected).is_err());
        assert!(!store.root.join(CURRENT_FILE).exists());
    }

    #[test]
    fn late_visible_initial_current_is_rebarriered_before_external_pinning() {
        let parent = TestDirectory::new("rebarrier-initial-current");
        let store = GenerationStore::new(parent.0.join("store"));
        let expected = input(10, 5, 20);
        let mut observer = RecordingObserver {
            fail_after: Some(PublishPoint::PointerRenamed),
            ..RecordingObserver::default()
        };
        assert!(matches!(
            store
                .create_initial_with_observer(expected.clone(), std::iter::empty(), &mut observer,),
            Err(GenerationError::InjectedFailure(
                PublishPoint::PointerRenamed
            ))
        ));
        let visible = store.load_current(&expected.binding, None).unwrap();
        let rebarriered = store
            .rebarrier_current_pointer(&expected.binding, visible.pin)
            .unwrap();
        assert_eq!(rebarriered.pin, visible.pin);
        assert_eq!(
            store.audit(&expected.binding).unwrap().status,
            StoreAuditStatus::Clean
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
