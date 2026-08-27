//! Atomic, bounded generations for the post-recovery consensus DAG WAL.
//!
//! A generation is immutable and content addressed. Its manifest binds the
//! recovery checkpoint/domain, the validator set, the canonical state baseline,
//! the DAG cursors, and the exact retained record log. Publishing is two phase:
//! all generation files and directories are fsynced first, then `CURRENT` is
//! atomically replaced and the store directory is fsynced. Previous generations
//! are never removed by this module.
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
use std::io::{self, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use thiserror::Error;

const MANIFEST_SCHEMA: &str = "arc.recovery.dag-wal-generation.v1";
const POINTER_SCHEMA: &str = "arc.recovery.dag-wal-current.v1";
const RECORD_MAGIC: &[u8; 8] = b"ARCDAGW1";
const RECORD_SCHEMA: u8 = 1;
const MANIFEST_FILE: &str = "manifest.json";
const RECORDS_FILE: &str = "records.bin";
const CURRENT_FILE: &str = "CURRENT";
const WRITE_LOCK_FILE: &str = ".WRITE.lock";
const MAX_MANIFEST_BYTES: u64 = 64 * 1024;
const MAX_POINTER_BYTES: u64 = 4 * 1024;
const FRAME_FIXED_BODY_BYTES: u64 = 1 + 1 + 8 + 32 + 4;
const FRAME_OVERHEAD_BYTES: u64 = 4 + FRAME_FIXED_BODY_BYTES + 32;
const MAX_GENERATIONS_TO_AUDIT: usize = 10_000;

/// Absolute fail-closed caps. A manifest may select lower limits, never higher.
pub const HARD_MAX_RETAINED_RECORDS: u64 = 100_000;
pub const HARD_MAX_RETAINED_PAYLOAD_BYTES: u64 = 256 * 1024 * 1024;
pub const HARD_MAX_SINGLE_RECORD_PAYLOAD_BYTES: u64 = 64 * 1024 * 1024;
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
    pub first_round: Option<u64>,
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
    pub first_round: Option<u64>,
    pub last_round: Option<u64>,
    pub valid_prefix_hash: Hash256,
    pub complete_file_hash: Hash256,
    pub suffix: TornSuffix,
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
    sequence: u64,
    previous_generation: Option<Hash256>,
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
        let result = self.publish_generation(Some(&current), input, records, observer);
        lock.release()?;
        result
    }
}

struct StoreLock {
    root: PathBuf,
    path: PathBuf,
    released: bool,
}

impl StoreLock {
    fn acquire(root: &Path) -> Result<Self> {
        let path = root.join(WRITE_LOCK_FILE);
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        set_private_file_mode(&mut options);
        let mut file = match options.open(&path) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                return Err(GenerationError::Locked(path));
            }
            Err(error) => return Err(io_error("create", &path, error)),
        };
        let value = format!(
            "schema=arc.recovery.dag-wal-lock.v1\npid={}\nnonce={}\n",
            std::process::id(),
            uuid::Uuid::new_v4()
        );
        file.write_all(value.as_bytes())
            .map_err(|error| io_error("write", &path, error))?;
        file.sync_all()
            .map_err(|error| io_error("fsync", &path, error))?;
        fsync_directory(root)?;
        Ok(Self {
            root: root.to_path_buf(),
            path,
            released: false,
        })
    }

    fn release(mut self) -> Result<()> {
        fs::remove_file(&self.path).map_err(|error| io_error("remove", &self.path, error))?;
        fsync_directory(&self.root)?;
        self.released = true;
        Ok(())
    }
}

impl Drop for StoreLock {
    fn drop(&mut self) {
        if !self.released {
            let _ = fs::remove_file(&self.path);
            let _ = fsync_directory(&self.root);
        }
    }
}

fn validate_input(input: &GenerationInput) -> Result<()> {
    validate_binding(&input.binding)?;
    validate_baseline(&input.baseline_state)?;
    validate_cursor(&input.dag_cursor)?;
    validate_limits(input.retention_limits)
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

fn validate_record_sequence(
    record: &RetainedDagRecord,
    previous_round: Option<u64>,
    seen: &mut HashSet<(RetainedRecordKind, u64, [u8; 32])>,
) -> Result<()> {
    validate_record(record)?;
    if previous_round.is_some_and(|round| record.round < round) {
        return Err(GenerationError::Invalid(
            "retained records are not ordered by nondecreasing round".into(),
        ));
    }
    if !seen.insert((record.kind, record.round, record.object_hash.0)) {
        return Err(GenerationError::Invalid(
            "duplicate retained record identity in one generation".into(),
        ));
    }
    Ok(())
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
    let mut first_round = None;
    let mut last_round = None;
    let mut seen = HashSet::new();
    for record in records {
        validate_record_sequence(&record, last_round, &mut seen)?;
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
        first_round.get_or_insert(record.round);
        last_round = Some(record.round);
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
    let mut first_round = None;
    let mut last_round = None;
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
        validate_record_sequence(&record, last_round, &mut seen)?;
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
        first_round.get_or_insert(record.round);
        last_round = Some(record.round);
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
