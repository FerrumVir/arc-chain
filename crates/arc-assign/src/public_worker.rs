//! Signed public-worker offers, independent of validator membership leases.
//!
//! An offer is an authenticated statement by a worker key. It does not prove
//! the claimed capacity, hardware, independence, complete job coverage, or an
//! entitlement to payment. Callers must separately measure and verify work.
//!
//! This module provides the signed offer contract and an opt-in bounded
//! durable admission book. It does not provide a network decoder or integrate
//! admission with a public endpoint. Any network adapter must cap encoded
//! frames before deserializing, then verify and durably admit before accepting.

use crate::{Address, Hash256};
use arc_crypto::{
    KeyPair, Signature,
    secret_file::{
        create_new_private, create_new_private_directory,
        durably_publish_existing_private_no_replace, open_owned_nofollow_directory,
        open_private_append_owned_migration, sync_parent_directory,
        try_acquire_private_directory_namespace_lock,
    },
};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashSet,
    fs,
    io::{self, Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
};

pub const PUBLIC_WORKER_OFFER_DOMAIN: &str = "ARC-public-worker-offer-v1";
pub const PUBLIC_WORKER_OFFER_VERSION: u16 = 1;
pub const MAX_TENSOR_RANGES: usize = 256;
pub const MAX_TENSOR_DIMENSION: u32 = 1_048_576;
pub const MAX_MEMORY_CAPACITY_BYTES: u64 = 1 << 60;
pub const MAX_CONCURRENCY: u16 = 1024;
pub const MAX_OFFER_LIFETIME_BLOCKS: u64 = 100_000;
const MAX_SIGNATURE_MATERIAL_BYTES: usize = 6_000;
/// Default durable replay capacity. At 256 bytes per record, the journal is
/// about 1 MiB at this limit.
pub const DEFAULT_PUBLIC_WORKER_ADMISSION_CAPACITY: usize = 4_096;
/// Hard limit for a single admission book (about 4 MiB of fixed records).
pub const MAX_PUBLIC_WORKER_ADMISSION_CAPACITY: usize = 16_384;

const ADMISSION_JOURNAL_FILE: &str = "public-worker-offers.v1.journal";
const ADMISSION_STAGE_FILE: &str = ".public-worker-offers.v1.journal.stage";
const ADMISSION_MAGIC: &[u8; 8] = b"ARCPWAB1";
const ADMISSION_VERSION: u16 = 1;
const ADMISSION_HEADER_PREFIX_LEN: usize = 8 + 2 + 4;
const ADMISSION_HEADER_LEN: usize = ADMISSION_HEADER_PREFIX_LEN + 32;
const ADMISSION_RECORD_DATA_LEN: usize = 8 + 32 + 8 + 32 + 32 + 32 + 8 + 8 + 32 + 32;
const ADMISSION_RECORD_LEN: usize = ADMISSION_RECORD_DATA_LEN + 32;

/// A half-open interval of complete tensor rows. Dimensions describe the full
/// tensor; a range always covers every column for each row in `[row_start,
/// row_end)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TensorRowRange {
    pub tensor_id: Hash256,
    pub tensor_rows: u32,
    pub tensor_columns: u32,
    pub row_start: u32,
    pub row_end: u32,
}

/// Worker-authored claims for one public-native execution binding.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PublicWorkerOfferBody {
    pub version: u16,
    pub chain_genesis: Hash256,
    pub recovery_epoch: u64,
    pub public_binding: Hash256,
    /// The coordinator audience for this offer. Relays cannot retarget it.
    pub coordinator: Address,
    /// Also the payee address. The protocol does not allow payout redirection.
    pub worker: Address,
    pub artifact_hash: Hash256,
    pub profile_hash: Hash256,
    pub generation_hash: Hash256,
    pub kernel_hash: Hash256,
    pub bundle_hash: Hash256,
    pub ranges: Vec<TensorRowRange>,
    /// Claimed memory capacity in bytes; not independently verified here.
    pub memory_capacity_bytes: u64,
    pub max_concurrency: u16,
    pub issued_at_height: u64,
    pub expires_at_height: u64,
    pub nonce: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PublicWorkerOffer {
    pub body: PublicWorkerOfferBody,
    pub signature: Signature,
}

/// Context a coordinator must supply when accepting an offer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PublicWorkerOfferRequirements {
    pub chain_genesis: Hash256,
    pub recovery_epoch: u64,
    pub public_binding: Hash256,
    pub coordinator: Address,
    pub artifact_hash: Hash256,
    pub profile_hash: Hash256,
    pub generation_hash: Hash256,
    pub kernel_hash: Hash256,
    pub bundle_hash: Hash256,
    pub now_height: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PublicWorkerOfferError {
    #[error("unsupported public-worker offer version {0}")]
    Version(u16),
    #[error("offer contains a zero context, identity, or execution commitment")]
    ZeroCommitment,
    #[error("offer ranges are empty or exceed the maximum of {MAX_TENSOR_RANGES}")]
    RangeCount,
    #[error("tensor range dimensions or row bounds are invalid")]
    InvalidRange,
    #[error("tensor ranges are not sorted, overlap, or disagree on dimensions")]
    RangeOrderOrOverlap,
    #[error("claimed memory capacity is outside the supported bounds")]
    MemoryCapacity,
    #[error("claimed concurrency is outside the supported bounds")]
    Concurrency,
    #[error("offer issue/expiry window is invalid or exceeds the maximum lifetime")]
    TimeWindow,
    #[error("offer nonce must be nonzero")]
    Nonce,
    #[error("offer is for another chain genesis")]
    WrongChain,
    #[error("offer is for another recovery epoch")]
    WrongRecoveryEpoch,
    #[error("offer is for another public binding")]
    WrongBinding,
    #[error("offer names another coordinator audience")]
    WrongAudience,
    #[error("offer execution commitments do not match the required model binding")]
    WrongExecution,
    #[error("offer was issued at height {issued}, but current height is {now}")]
    NotYetValid { issued: u64, now: u64 },
    #[error("offer expired at height {expires}, current height is {now}")]
    Expired { expires: u64, now: u64 },
    #[error("signing key does not match the worker/payee address")]
    WrongSigningKey,
    #[error("signature material exceeds the bounded offer limit")]
    SignatureTooLarge,
    #[error("offer signature is invalid")]
    Signature,
}

impl PublicWorkerOfferBody {
    /// Validate all variable-size and range inputs before encoding or hashing.
    pub fn validate(&self) -> Result<(), PublicWorkerOfferError> {
        if self.version != PUBLIC_WORKER_OFFER_VERSION {
            return Err(PublicWorkerOfferError::Version(self.version));
        }
        if [
            self.chain_genesis,
            self.public_binding,
            self.coordinator,
            self.worker,
            self.artifact_hash,
            self.profile_hash,
            self.generation_hash,
            self.kernel_hash,
            self.bundle_hash,
        ]
        .contains(&Hash256::ZERO)
        {
            return Err(PublicWorkerOfferError::ZeroCommitment);
        }
        if self.ranges.is_empty() || self.ranges.len() > MAX_TENSOR_RANGES {
            return Err(PublicWorkerOfferError::RangeCount);
        }
        let mut previous: Option<TensorRowRange> = None;
        for range in &self.ranges {
            if range.tensor_id == Hash256::ZERO
                || range.tensor_rows == 0
                || range.tensor_columns == 0
                || range.tensor_rows > MAX_TENSOR_DIMENSION
                || range.tensor_columns > MAX_TENSOR_DIMENSION
                || range.row_start >= range.row_end
                || range.row_end > range.tensor_rows
            {
                return Err(PublicWorkerOfferError::InvalidRange);
            }
            if let Some(prev) = previous {
                let ordering = range
                    .tensor_id
                    .0
                    .cmp(&prev.tensor_id.0)
                    .then_with(|| range.row_start.cmp(&prev.row_start));
                if ordering != std::cmp::Ordering::Greater {
                    return Err(PublicWorkerOfferError::RangeOrderOrOverlap);
                }
                if range.tensor_id == prev.tensor_id
                    && (range.tensor_rows != prev.tensor_rows
                        || range.tensor_columns != prev.tensor_columns
                        || range.row_start < prev.row_end)
                {
                    return Err(PublicWorkerOfferError::RangeOrderOrOverlap);
                }
            }
            previous = Some(*range);
        }
        if self.memory_capacity_bytes == 0 || self.memory_capacity_bytes > MAX_MEMORY_CAPACITY_BYTES
        {
            return Err(PublicWorkerOfferError::MemoryCapacity);
        }
        if self.max_concurrency == 0 || self.max_concurrency > MAX_CONCURRENCY {
            return Err(PublicWorkerOfferError::Concurrency);
        }
        if self.issued_at_height >= self.expires_at_height
            || self
                .expires_at_height
                .checked_sub(self.issued_at_height)
                .is_none_or(|lifetime| lifetime > MAX_OFFER_LIFETIME_BLOCKS)
        {
            return Err(PublicWorkerOfferError::TimeWindow);
        }
        if self.nonce == 0 {
            return Err(PublicWorkerOfferError::Nonce);
        }
        Ok(())
    }

    /// Fixed-width canonical transcript bytes. This intentionally does not use
    /// serde/bincode: field order, integer width, and range framing are explicit.
    pub fn canonical_transcript(&self) -> Result<Vec<u8>, PublicWorkerOfferError> {
        self.validate()?;
        let mut out = Vec::with_capacity(334 + self.ranges.len() * 48);
        put_u16(&mut out, self.version);
        put_hash(&mut out, self.chain_genesis);
        put_u64(&mut out, self.recovery_epoch);
        put_hash(&mut out, self.public_binding);
        put_hash(&mut out, self.coordinator);
        put_hash(&mut out, self.worker);
        put_hash(&mut out, self.artifact_hash);
        put_hash(&mut out, self.profile_hash);
        put_hash(&mut out, self.generation_hash);
        put_hash(&mut out, self.kernel_hash);
        put_hash(&mut out, self.bundle_hash);
        put_u16(&mut out, self.ranges.len() as u16);
        for range in &self.ranges {
            put_hash(&mut out, range.tensor_id);
            put_u32(&mut out, range.tensor_rows);
            put_u32(&mut out, range.tensor_columns);
            put_u32(&mut out, range.row_start);
            put_u32(&mut out, range.row_end);
        }
        put_u64(&mut out, self.memory_capacity_bytes);
        put_u16(&mut out, self.max_concurrency);
        put_u64(&mut out, self.issued_at_height);
        put_u64(&mut out, self.expires_at_height);
        put_u64(&mut out, self.nonce);
        Ok(out)
    }

    pub fn transcript(&self) -> Result<Hash256, PublicWorkerOfferError> {
        let bytes = self.canonical_transcript()?;
        let mut hasher = blake3::Hasher::new_derive_key(PUBLIC_WORKER_OFFER_DOMAIN);
        hasher.update(&bytes);
        Ok(Hash256(*hasher.finalize().as_bytes()))
    }
}

impl PublicWorkerOffer {
    pub fn sign(
        body: PublicWorkerOfferBody,
        key: &KeyPair,
    ) -> Result<Self, PublicWorkerOfferError> {
        body.validate()?;
        if body.worker != key.address() {
            return Err(PublicWorkerOfferError::WrongSigningKey);
        }
        let transcript = body.transcript()?;
        let signature = key
            .sign(&transcript)
            .map_err(|_| PublicWorkerOfferError::Signature)?;
        Ok(Self { body, signature })
    }

    pub fn verify(
        &self,
        requirements: &PublicWorkerOfferRequirements,
    ) -> Result<(), PublicWorkerOfferError> {
        // Check attacker-controlled signature vectors before computing any hash.
        if signature_material_len(&self.signature) > MAX_SIGNATURE_MATERIAL_BYTES {
            return Err(PublicWorkerOfferError::SignatureTooLarge);
        }
        let body = &self.body;
        body.validate()?;
        if body.chain_genesis != requirements.chain_genesis {
            return Err(PublicWorkerOfferError::WrongChain);
        }
        if body.recovery_epoch != requirements.recovery_epoch {
            return Err(PublicWorkerOfferError::WrongRecoveryEpoch);
        }
        if body.public_binding != requirements.public_binding {
            return Err(PublicWorkerOfferError::WrongBinding);
        }
        if body.coordinator != requirements.coordinator {
            return Err(PublicWorkerOfferError::WrongAudience);
        }
        if body.artifact_hash != requirements.artifact_hash
            || body.profile_hash != requirements.profile_hash
            || body.generation_hash != requirements.generation_hash
            || body.kernel_hash != requirements.kernel_hash
            || body.bundle_hash != requirements.bundle_hash
        {
            return Err(PublicWorkerOfferError::WrongExecution);
        }
        if requirements.now_height < body.issued_at_height {
            return Err(PublicWorkerOfferError::NotYetValid {
                issued: body.issued_at_height,
                now: requirements.now_height,
            });
        }
        if requirements.now_height >= body.expires_at_height {
            return Err(PublicWorkerOfferError::Expired {
                expires: body.expires_at_height,
                now: requirements.now_height,
            });
        }
        self.signature
            .verify(&body.transcript()?, &body.worker)
            .map_err(|_| PublicWorkerOfferError::Signature)
    }

    /// Stable identifier of this exact signed offer.
    pub fn digest(&self) -> Result<Hash256, PublicWorkerOfferError> {
        if signature_material_len(&self.signature) > MAX_SIGNATURE_MATERIAL_BYTES {
            return Err(PublicWorkerOfferError::SignatureTooLarge);
        }
        let transcript = self.body.transcript()?;
        let mut hasher = blake3::Hasher::new_derive_key("ARC-public-worker-offer-id-v1");
        hasher.update(transcript.as_bytes());
        hash_signature(&mut hasher, &self.signature);
        Ok(Hash256(*hasher.finalize().as_bytes()))
    }
}

/// Durable, bounded replay protection for already-verified public worker
/// offers. The path is a dedicated private directory. Operations use a
/// nonblocking cross-process namespace lock; callers should retry `Busy` from
/// a normal async scheduling path rather than block a runtime thread.
///
/// Entries are append-only and are never evicted, including after offer
/// expiry. A full book refuses new offers until an operator provisions a new
/// book under an independently validated chain/recovery lifecycle. The
/// journal is an integrity-checked fixed-record format and any malformed or
/// truncated state fails closed. Initialization is permitted only when this
/// call creates the dedicated directory; a missing journal in an existing
/// directory is corruption, never an invitation to reset. The format detects
/// modified records and torn tails, but cannot detect truncation to an earlier
/// valid prefix without an independent monotonic anchor.
#[derive(Debug, Clone)]
pub struct PublicWorkerOfferAdmissionBook {
    directory: PathBuf,
    capacity: usize,
}

#[derive(Debug, thiserror::Error)]
pub enum PublicWorkerOfferAdmissionError {
    #[error(transparent)]
    Offer(#[from] PublicWorkerOfferError),
    #[error("public-worker admission book is busy; retry without blocking the runtime")]
    Busy,
    #[error("public-worker admission book is full at its configured capacity")]
    Capacity,
    #[error("public-worker admission book capacity is invalid or differs from its durable header")]
    CapacityMismatch,
    #[error("public-worker admission capacity must be positive and within the hard maximum")]
    InvalidCapacity,
    #[error("public-worker offer nonce was already admitted for this worker and audience")]
    Replay,
    #[error("public-worker admission journal is corrupt, truncated, oversized, or unsupported")]
    Corrupt,
    #[error("public-worker admission write could not be confirmed durable")]
    PersistenceUncertain,
    #[error("public-worker admission storage I/O failed: {0}")]
    Io(#[from] io::Error),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct AdmissionRecord {
    sequence: u64,
    chain_genesis: Hash256,
    recovery_epoch: u64,
    public_binding: Hash256,
    coordinator: Address,
    worker: Address,
    nonce: u64,
    expires_at_height: u64,
    offer_digest: Hash256,
    previous_record_hash: Hash256,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct AdmissionReplayKey {
    chain_genesis: Hash256,
    recovery_epoch: u64,
    coordinator: Address,
    worker: Address,
    nonce: u64,
}

impl AdmissionRecord {
    fn from_offer(offer: &PublicWorkerOffer, digest: Hash256) -> Self {
        Self {
            sequence: 0,
            chain_genesis: offer.body.chain_genesis,
            recovery_epoch: offer.body.recovery_epoch,
            public_binding: offer.body.public_binding,
            coordinator: offer.body.coordinator,
            worker: offer.body.worker,
            nonce: offer.body.nonce,
            expires_at_height: offer.body.expires_at_height,
            offer_digest: digest,
            previous_record_hash: Hash256::ZERO,
        }
    }

    /// Binding and offer digest are provenance, not part of nonce uniqueness:
    /// changing either cannot authorize reuse of the same worker nonce.
    fn replay_key(&self) -> AdmissionReplayKey {
        AdmissionReplayKey {
            chain_genesis: self.chain_genesis,
            recovery_epoch: self.recovery_epoch,
            coordinator: self.coordinator,
            worker: self.worker,
            nonce: self.nonce,
        }
    }
}

#[derive(Debug)]
struct AdmissionJournal {
    records: Vec<AdmissionRecord>,
    replay_keys: HashSet<AdmissionReplayKey>,
    last_record_hash: Hash256,
}

impl PublicWorkerOfferAdmissionBook {
    pub fn open(directory: impl AsRef<Path>) -> Result<Self, PublicWorkerOfferAdmissionError> {
        Self::open_with_capacity(directory, DEFAULT_PUBLIC_WORKER_ADMISSION_CAPACITY)
    }

    /// Open or initialize a dedicated book directory with a persisted entry
    /// cap. Reopening with a different cap fails closed rather than silently
    /// changing the previously accepted resource bound.
    pub fn open_with_capacity(
        directory: impl AsRef<Path>,
        capacity: usize,
    ) -> Result<Self, PublicWorkerOfferAdmissionError> {
        if capacity == 0 || capacity > MAX_PUBLIC_WORKER_ADMISSION_CAPACITY {
            return Err(PublicWorkerOfferAdmissionError::InvalidCapacity);
        }
        let _lock = acquire_book_lock(directory.as_ref())?;
        let directory = _lock.target().to_path_buf();
        let created = ensure_book_directory(&directory)?;
        let mut journal = if created {
            initialize_journal(&directory, capacity)?
        } else {
            open_existing_journal(&directory)?
        };
        let _records = read_journal(&mut journal, capacity)?;
        drop(journal);
        Ok(Self {
            directory,
            capacity,
        })
    }

    /// Verify and durably record one offer. Success is returned only after the
    /// fixed record has been appended, synced, and re-read under the same
    /// exclusive namespace lock. `Busy` is explicit and bounded; it is never
    /// converted into success or an unbounded wait.
    pub fn admit(
        &self,
        offer: &PublicWorkerOffer,
        requirements: &PublicWorkerOfferRequirements,
    ) -> Result<Hash256, PublicWorkerOfferAdmissionError> {
        offer.verify(requirements)?;
        let digest = offer.digest()?;
        let mut record = AdmissionRecord::from_offer(offer, digest);
        let _lock = acquire_book_lock(&self.directory)?;
        let directory = _lock.target();
        let _directory_pin = open_owned_nofollow_directory(directory)?;
        let mut journal = open_existing_journal(directory)?;
        let state = read_journal(&mut journal, self.capacity)?;
        if state.replay_keys.contains(&record.replay_key()) {
            return Err(PublicWorkerOfferAdmissionError::Replay);
        }
        if state.records.len() >= self.capacity {
            return Err(PublicWorkerOfferAdmissionError::Capacity);
        }
        record.sequence = state.records.len() as u64;
        record.previous_record_hash = state.last_record_hash;

        let encoded = encode_record(record);
        if journal.write_all(&encoded).is_err() {
            // A short write may have left a partial tail. Re-read while still
            // locked, and acknowledge only if the complete record can still
            // pass a fresh durability barrier and a second complete reread.
            return match confirm_uncertain_append(&mut journal, self.capacity, &record) {
                Ok(true) => Ok(digest),
                Ok(false) => Err(PublicWorkerOfferAdmissionError::PersistenceUncertain),
                Err(error) => Err(error),
            };
        }
        if journal.sync_all().is_err() {
            // The bytes may be visible but not durable. Retry only after the
            // journal is validated and acknowledge only if the next barrier
            // and full reread both pass.
            if !confirm_uncertain_append(&mut journal, self.capacity, &record)? {
                return Err(PublicWorkerOfferAdmissionError::PersistenceUncertain);
            }
        }
        let verified = read_journal(&mut journal, self.capacity)?;
        if verified.records.last() != Some(&record) {
            return Err(PublicWorkerOfferAdmissionError::PersistenceUncertain);
        }
        Ok(digest)
    }
}

fn acquire_book_lock(
    directory: &Path,
) -> Result<arc_crypto::secret_file::PrivateDirectoryNamespaceLock, PublicWorkerOfferAdmissionError>
{
    match try_acquire_private_directory_namespace_lock(directory) {
        Ok(lock) => Ok(lock),
        Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
            Err(PublicWorkerOfferAdmissionError::Busy)
        }
        Err(error) => Err(error.into()),
    }
}

fn ensure_book_directory(directory: &Path) -> Result<bool, PublicWorkerOfferAdmissionError> {
    match open_owned_nofollow_directory(directory) {
        Ok(pin) => {
            drop(pin);
            Ok(false)
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            create_new_private_directory(directory)?;
            sync_parent_directory(directory)?;
            Ok(true)
        }
        Err(error) => Err(error.into()),
    }
}

fn journal_path(directory: &Path) -> PathBuf {
    directory.join(ADMISSION_JOURNAL_FILE)
}

fn open_existing_journal(directory: &Path) -> Result<fs::File, PublicWorkerOfferAdmissionError> {
    let path = journal_path(directory);
    match open_private_append_owned_migration(&path) {
        Ok(file) => Ok(file),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            Err(PublicWorkerOfferAdmissionError::Corrupt)
        }
        Err(error) => Err(error.into()),
    }
}

fn initialize_journal(
    directory: &Path,
    capacity: usize,
) -> Result<fs::File, PublicWorkerOfferAdmissionError> {
    let path = journal_path(directory);

    // A deterministic create-only staging name means an interrupted first
    // publication is noticed as AlreadyExists and fails closed. The namespace
    // lock excludes cooperating first creators.
    let stage = directory.join(ADMISSION_STAGE_FILE);
    let mut file = match create_new_private(&stage) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
            return Err(PublicWorkerOfferAdmissionError::Corrupt);
        }
        Err(error) => return Err(error.into()),
    };
    let header = encode_header(capacity);
    file.write_all(&header)?;
    file.sync_all()?;
    drop(file);
    match durably_publish_existing_private_no_replace(&stage, &path) {
        Ok(()) => open_private_append_owned_migration(&path).map_err(Into::into),
        Err(publish_error) => {
            // Publication helpers can report an error after the name was
            // committed. Re-open and sync that exact final state before
            // accepting it; never clean up or overwrite ambiguous files.
            if let Ok(mut published) = open_private_append_owned_migration(&path)
                && validate_empty_journal(&mut published, capacity).is_ok()
                && published.sync_all().is_ok()
                && sync_parent_directory(&path).is_ok()
            {
                return Ok(published);
            }
            Err(publish_error.into())
        }
    }
}

fn encode_header(capacity: usize) -> Vec<u8> {
    let mut header = Vec::with_capacity(ADMISSION_HEADER_LEN);
    header.extend_from_slice(ADMISSION_MAGIC);
    header.extend_from_slice(&ADMISSION_VERSION.to_le_bytes());
    header.extend_from_slice(&(capacity as u32).to_le_bytes());
    let checksum = admission_hash("ARC-public-worker-admission-header-v1", &header);
    header.extend_from_slice(&checksum);
    header
}

fn encode_record(record: AdmissionRecord) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(ADMISSION_RECORD_LEN);
    bytes.extend_from_slice(&record.sequence.to_le_bytes());
    bytes.extend_from_slice(record.chain_genesis.as_bytes());
    bytes.extend_from_slice(&record.recovery_epoch.to_le_bytes());
    bytes.extend_from_slice(record.public_binding.as_bytes());
    bytes.extend_from_slice(record.coordinator.as_bytes());
    bytes.extend_from_slice(record.worker.as_bytes());
    bytes.extend_from_slice(&record.nonce.to_le_bytes());
    bytes.extend_from_slice(&record.expires_at_height.to_le_bytes());
    bytes.extend_from_slice(record.offer_digest.as_bytes());
    bytes.extend_from_slice(record.previous_record_hash.as_bytes());
    let checksum = admission_hash("ARC-public-worker-admission-record-v1", &bytes);
    bytes.extend_from_slice(&checksum);
    bytes
}

fn admission_hash(domain: &str, bytes: &[u8]) -> [u8; 32] {
    let mut hasher = blake3::Hasher::new_derive_key(domain);
    hasher.update(bytes);
    *hasher.finalize().as_bytes()
}

fn read_journal(
    file: &mut fs::File,
    expected_capacity: usize,
) -> Result<AdmissionJournal, PublicWorkerOfferAdmissionError> {
    let expected_max = ADMISSION_HEADER_LEN
        .checked_add(
            expected_capacity
                .checked_mul(ADMISSION_RECORD_LEN)
                .ok_or(PublicWorkerOfferAdmissionError::Corrupt)?,
        )
        .ok_or(PublicWorkerOfferAdmissionError::Corrupt)?;
    let length = file.metadata()?.len();
    if length < ADMISSION_HEADER_LEN as u64 || length > expected_max as u64 {
        return Err(PublicWorkerOfferAdmissionError::Corrupt);
    }
    file.seek(SeekFrom::Start(0))?;
    let mut bytes = Vec::with_capacity(length as usize);
    (&mut *file)
        .take(expected_max as u64 + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 != length || bytes.len() > expected_max {
        return Err(PublicWorkerOfferAdmissionError::Corrupt);
    }
    parse_journal(&bytes, expected_capacity)
}

fn validate_empty_journal(
    file: &mut fs::File,
    capacity: usize,
) -> Result<(), PublicWorkerOfferAdmissionError> {
    if read_journal(file, capacity)?.records.is_empty() {
        Ok(())
    } else {
        Err(PublicWorkerOfferAdmissionError::Corrupt)
    }
}

fn confirm_uncertain_append(
    file: &mut fs::File,
    capacity: usize,
    expected: &AdmissionRecord,
) -> Result<bool, PublicWorkerOfferAdmissionError> {
    let visible = read_journal(file, capacity)?;
    if visible.records.last() != Some(expected) || file.sync_all().is_err() {
        return Ok(false);
    }
    let durable = read_journal(file, capacity)?;
    Ok(durable.records.last() == Some(expected))
}

fn parse_journal(
    bytes: &[u8],
    expected_capacity: usize,
) -> Result<AdmissionJournal, PublicWorkerOfferAdmissionError> {
    if bytes.len() < ADMISSION_HEADER_LEN
        || &bytes[..8] != ADMISSION_MAGIC
        || u16::from_le_bytes([bytes[8], bytes[9]]) != ADMISSION_VERSION
    {
        return Err(PublicWorkerOfferAdmissionError::Corrupt);
    }
    let capacity = u32::from_le_bytes([bytes[10], bytes[11], bytes[12], bytes[13]]) as usize;
    if capacity != expected_capacity
        || capacity == 0
        || capacity > MAX_PUBLIC_WORKER_ADMISSION_CAPACITY
    {
        return Err(PublicWorkerOfferAdmissionError::CapacityMismatch);
    }
    if admission_hash(
        "ARC-public-worker-admission-header-v1",
        &bytes[..ADMISSION_HEADER_PREFIX_LEN],
    ) != bytes[ADMISSION_HEADER_PREFIX_LEN..ADMISSION_HEADER_LEN]
    {
        return Err(PublicWorkerOfferAdmissionError::Corrupt);
    }
    let tail = &bytes[ADMISSION_HEADER_LEN..];
    if !tail.len().is_multiple_of(ADMISSION_RECORD_LEN) {
        return Err(PublicWorkerOfferAdmissionError::Corrupt);
    }
    let count = tail.len() / ADMISSION_RECORD_LEN;
    if count > capacity {
        return Err(PublicWorkerOfferAdmissionError::Corrupt);
    }
    let mut records = Vec::with_capacity(count);
    let mut replay_keys = HashSet::with_capacity(count);
    let mut previous_record_hash = Hash256::ZERO;
    for (index, chunk) in tail.chunks_exact(ADMISSION_RECORD_LEN).enumerate() {
        let data = &chunk[..ADMISSION_RECORD_DATA_LEN];
        if admission_hash("ARC-public-worker-admission-record-v1", data)
            != chunk[ADMISSION_RECORD_DATA_LEN..]
        {
            return Err(PublicWorkerOfferAdmissionError::Corrupt);
        }
        let record = decode_record(data);
        if record.sequence != index as u64
            || record.previous_record_hash != previous_record_hash
            || record.chain_genesis == Hash256::ZERO
            || record.public_binding == Hash256::ZERO
            || record.coordinator == Hash256::ZERO
            || record.worker == Hash256::ZERO
            || record.nonce == 0
            || record.expires_at_height == 0
            || record.offer_digest == Hash256::ZERO
            || !replay_keys.insert(record.replay_key())
        {
            return Err(PublicWorkerOfferAdmissionError::Corrupt);
        }
        records.push(record);
        previous_record_hash = Hash256(
            chunk[ADMISSION_RECORD_DATA_LEN..]
                .try_into()
                .map_err(|_| PublicWorkerOfferAdmissionError::Corrupt)?,
        );
    }
    Ok(AdmissionJournal {
        records,
        replay_keys,
        last_record_hash: previous_record_hash,
    })
}

fn decode_record(data: &[u8]) -> AdmissionRecord {
    let mut offset = 0;
    let sequence = read_u64(data, &mut offset);
    let chain_genesis = read_hash(data, &mut offset);
    let recovery_epoch = read_u64(data, &mut offset);
    let public_binding = read_hash(data, &mut offset);
    let coordinator = read_hash(data, &mut offset);
    let worker = read_hash(data, &mut offset);
    let nonce = read_u64(data, &mut offset);
    let expires_at_height = read_u64(data, &mut offset);
    let offer_digest = read_hash(data, &mut offset);
    let previous_record_hash = read_hash(data, &mut offset);
    AdmissionRecord {
        sequence,
        chain_genesis,
        recovery_epoch,
        public_binding,
        coordinator,
        worker,
        nonce,
        expires_at_height,
        offer_digest,
        previous_record_hash,
    }
}

fn read_hash(data: &[u8], offset: &mut usize) -> Hash256 {
    let mut value = [0; 32];
    value.copy_from_slice(&data[*offset..*offset + 32]);
    *offset += 32;
    Hash256(value)
}

fn read_u64(data: &[u8], offset: &mut usize) -> u64 {
    let mut value = [0; 8];
    value.copy_from_slice(&data[*offset..*offset + 8]);
    *offset += 8;
    u64::from_le_bytes(value)
}

fn put_hash(out: &mut Vec<u8>, hash: Hash256) {
    out.extend_from_slice(hash.as_bytes());
}

fn put_u16(out: &mut Vec<u8>, value: u16) {
    out.extend_from_slice(&value.to_le_bytes());
}

fn put_u32(out: &mut Vec<u8>, value: u32) {
    out.extend_from_slice(&value.to_le_bytes());
}

fn put_u64(out: &mut Vec<u8>, value: u64) {
    out.extend_from_slice(&value.to_le_bytes());
}

fn signature_material_len(signature: &Signature) -> usize {
    match signature {
        Signature::Ed25519 { signature, .. } => signature.len(),
        Signature::Secp256k1 { signature } => signature.len(),
        Signature::MlDsa65 {
            public_key,
            signature,
        } => public_key.len().saturating_add(signature.len()),
        Signature::Falcon512 {
            public_key,
            signature,
        } => public_key.len().saturating_add(signature.len()),
    }
}

fn hash_signature(hasher: &mut blake3::Hasher, signature: &Signature) {
    match signature {
        Signature::Ed25519 {
            public_key,
            signature,
        } => {
            hasher.update(&[0]);
            hasher.update(public_key);
            put_len_and_bytes(hasher, signature);
        }
        Signature::Secp256k1 { signature } => {
            hasher.update(&[1]);
            put_len_and_bytes(hasher, signature);
        }
        Signature::MlDsa65 {
            public_key,
            signature,
        } => {
            hasher.update(&[2]);
            put_len_and_bytes(hasher, public_key);
            put_len_and_bytes(hasher, signature);
        }
        Signature::Falcon512 {
            public_key,
            signature,
        } => {
            hasher.update(&[3]);
            put_len_and_bytes(hasher, public_key);
            put_len_and_bytes(hasher, signature);
        }
    }
}

fn put_len_and_bytes(hasher: &mut blake3::Hasher, bytes: &[u8]) {
    hasher.update(&(bytes.len() as u32).to_le_bytes());
    hasher.update(bytes);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        fs::OpenOptions,
        sync::{Arc, Barrier},
        thread,
    };

    fn h(byte: u8) -> Hash256 {
        Hash256([byte; 32])
    }

    fn body(worker: Address) -> PublicWorkerOfferBody {
        PublicWorkerOfferBody {
            version: PUBLIC_WORKER_OFFER_VERSION,
            chain_genesis: h(1),
            recovery_epoch: 7,
            public_binding: h(2),
            coordinator: h(3),
            worker,
            artifact_hash: h(4),
            profile_hash: h(5),
            generation_hash: h(6),
            kernel_hash: h(7),
            bundle_hash: h(8),
            ranges: vec![TensorRowRange {
                tensor_id: h(9),
                tensor_rows: 16,
                tensor_columns: 8,
                row_start: 0,
                row_end: 4,
            }],
            memory_capacity_bytes: 1 << 30,
            max_concurrency: 2,
            issued_at_height: 100,
            expires_at_height: 200,
            nonce: 1,
        }
    }

    fn requirements() -> PublicWorkerOfferRequirements {
        PublicWorkerOfferRequirements {
            chain_genesis: h(1),
            recovery_epoch: 7,
            public_binding: h(2),
            coordinator: h(3),
            artifact_hash: h(4),
            profile_hash: h(5),
            generation_hash: h(6),
            kernel_hash: h(7),
            bundle_hash: h(8),
            now_height: 150,
        }
    }

    #[test]
    fn canonical_transcript_has_a_golden_hash() {
        let body = body(h(10));
        assert_eq!(
            body.transcript().unwrap(),
            Hash256([
                0xbc, 0x24, 0x56, 0x88, 0x0e, 0xfc, 0xdf, 0x6f, 0x0b, 0x42, 0x9f, 0x0b, 0x6f, 0x74,
                0x72, 0xc0, 0x98, 0x38, 0xc5, 0xd2, 0x6f, 0x8b, 0xa4, 0x8f, 0x6b, 0x6d, 0x8d, 0x54,
                0xcb, 0x47, 0xb9, 0xe2,
            ])
        );
    }

    #[test]
    fn signed_offer_verifies_for_exact_context_and_same_payee() {
        let key = KeyPair::generate_ed25519();
        let offer = PublicWorkerOffer::sign(body(key.address()), &key).unwrap();
        offer.verify(&requirements()).unwrap();
        assert_eq!(offer.body.worker, key.address());
    }

    #[test]
    fn signature_and_signed_fields_cannot_be_tampered() {
        let key = KeyPair::generate_ed25519();
        let offer = PublicWorkerOffer::sign(body(key.address()), &key).unwrap();
        let mut changed = offer.clone();
        changed.body.memory_capacity_bytes += 1;
        assert_eq!(
            changed.verify(&requirements()),
            Err(PublicWorkerOfferError::Signature)
        );

        let mut changed_sig = offer;
        if let Signature::Ed25519 { signature, .. } = &mut changed_sig.signature {
            signature[0] ^= 1;
        }
        assert_eq!(
            changed_sig.verify(&requirements()),
            Err(PublicWorkerOfferError::Signature)
        );
    }

    #[test]
    fn rejects_wrong_chain_and_coordinator_audience() {
        let key = KeyPair::generate_ed25519();
        let offer = PublicWorkerOffer::sign(body(key.address()), &key).unwrap();
        let mut req = requirements();
        req.chain_genesis = h(20);
        assert_eq!(offer.verify(&req), Err(PublicWorkerOfferError::WrongChain));
        req = requirements();
        req.coordinator = h(21);
        assert_eq!(
            offer.verify(&req),
            Err(PublicWorkerOfferError::WrongAudience)
        );
    }

    #[test]
    fn rejects_wrong_recovery_binding_and_execution_context() {
        let key = KeyPair::generate_ed25519();
        let offer = PublicWorkerOffer::sign(body(key.address()), &key).unwrap();
        let mut req = requirements();
        req.recovery_epoch += 1;
        assert_eq!(
            offer.verify(&req),
            Err(PublicWorkerOfferError::WrongRecoveryEpoch)
        );
        req = requirements();
        req.public_binding = h(22);
        assert_eq!(
            offer.verify(&req),
            Err(PublicWorkerOfferError::WrongBinding)
        );
        req = requirements();
        req.artifact_hash = h(23);
        assert_eq!(
            offer.verify(&req),
            Err(PublicWorkerOfferError::WrongExecution)
        );
    }

    #[test]
    fn rejects_not_yet_valid_and_expired_offers() {
        let key = KeyPair::generate_ed25519();
        let offer = PublicWorkerOffer::sign(body(key.address()), &key).unwrap();
        let mut req = requirements();
        req.now_height = 99;
        assert!(matches!(
            offer.verify(&req),
            Err(PublicWorkerOfferError::NotYetValid { .. })
        ));
        req.now_height = 200;
        assert!(matches!(
            offer.verify(&req),
            Err(PublicWorkerOfferError::Expired { .. })
        ));
    }

    #[test]
    fn rejects_unsupported_version_bad_shape_unsorted_and_overlapping_ranges() {
        let mut b = body(h(10));
        b.version = 2;
        assert_eq!(b.validate(), Err(PublicWorkerOfferError::Version(2)));
        b = body(h(10));
        b.ranges[0].row_end = 17;
        assert_eq!(b.validate(), Err(PublicWorkerOfferError::InvalidRange));
        b = body(h(10));
        let mut unsorted = b.ranges[0];
        unsorted.tensor_id = h(8);
        b.ranges.push(unsorted);
        assert_eq!(
            b.validate(),
            Err(PublicWorkerOfferError::RangeOrderOrOverlap)
        );
        b = body(h(10));
        let mut overlap = b.ranges[0];
        overlap.row_start = 3;
        overlap.row_end = 6;
        b.ranges.push(overlap);
        assert_eq!(
            b.validate(),
            Err(PublicWorkerOfferError::RangeOrderOrOverlap)
        );
        b = body(h(10));
        b.ranges[0].tensor_columns = 0;
        assert_eq!(b.validate(), Err(PublicWorkerOfferError::InvalidRange));
    }

    #[test]
    fn rejects_wrong_signer_and_oversized_range_list_before_transcript() {
        let key = KeyPair::generate_ed25519();
        assert_eq!(
            PublicWorkerOffer::sign(body(h(99)), &key),
            Err(PublicWorkerOfferError::WrongSigningKey)
        );
        let mut b = body(h(10));
        b.ranges.resize(MAX_TENSOR_RANGES + 1, b.ranges[0]);
        assert_eq!(
            b.canonical_transcript(),
            Err(PublicWorkerOfferError::RangeCount)
        );
    }

    #[test]
    fn rejects_zero_capacity_concurrency_and_excessive_lifetime() {
        let mut b = body(h(10));
        b.memory_capacity_bytes = 0;
        assert_eq!(b.validate(), Err(PublicWorkerOfferError::MemoryCapacity));
        b = body(h(10));
        b.max_concurrency = 0;
        assert_eq!(b.validate(), Err(PublicWorkerOfferError::Concurrency));
        b = body(h(10));
        b.expires_at_height = b.issued_at_height + MAX_OFFER_LIFETIME_BLOCKS + 1;
        assert_eq!(b.validate(), Err(PublicWorkerOfferError::TimeWindow));
    }

    fn signed_offer(key: &KeyPair, nonce: u64) -> PublicWorkerOffer {
        let mut body = body(key.address());
        body.nonce = nonce;
        PublicWorkerOffer::sign(body, key).unwrap()
    }

    fn book_path(root: &Path, name: &str) -> PathBuf {
        root.join(name)
    }

    #[test]
    fn durable_admission_survives_reopen_and_rejects_replay() {
        let directory = tempfile::tempdir().unwrap();
        let path = book_path(directory.path(), "book");
        let key = KeyPair::generate_ed25519();
        let offer = signed_offer(&key, 1);
        let digest = {
            let book = PublicWorkerOfferAdmissionBook::open_with_capacity(&path, 4).unwrap();
            book.admit(&offer, &requirements()).unwrap()
        };
        let reopened = PublicWorkerOfferAdmissionBook::open_with_capacity(&path, 4).unwrap();
        assert!(matches!(
            reopened.admit(&offer, &requirements()),
            Err(PublicWorkerOfferAdmissionError::Replay)
        ));
        assert_eq!(digest, offer.digest().unwrap());
    }

    #[test]
    fn missing_journal_never_resets_an_existing_book_or_allows_replay() {
        let directory = tempfile::tempdir().unwrap();
        let path = book_path(directory.path(), "book");
        let key = KeyPair::generate_ed25519();
        let offer = signed_offer(&key, 3);
        let book = PublicWorkerOfferAdmissionBook::open_with_capacity(&path, 4).unwrap();
        book.admit(&offer, &requirements()).unwrap();
        let journal = journal_path(&path);

        fs::remove_file(&journal).unwrap();
        assert!(matches!(
            book.admit(&offer, &requirements()),
            Err(PublicWorkerOfferAdmissionError::Corrupt)
        ));
        assert!(!journal.exists());
        assert!(matches!(
            PublicWorkerOfferAdmissionBook::open_with_capacity(&path, 4),
            Err(PublicWorkerOfferAdmissionError::Corrupt)
        ));
        assert!(!journal.exists());
    }

    #[test]
    fn preexisting_empty_directory_is_not_treated_as_a_fresh_book() {
        let directory = tempfile::tempdir().unwrap();
        let path = book_path(directory.path(), "book");
        create_new_private_directory(&path).unwrap();
        sync_parent_directory(&path).unwrap();
        assert!(matches!(
            PublicWorkerOfferAdmissionBook::open_with_capacity(&path, 4),
            Err(PublicWorkerOfferAdmissionError::Corrupt)
        ));
        assert!(!journal_path(&path).exists());
    }

    #[test]
    fn nonce_replay_key_ignores_changed_binding_and_offer_digest() {
        let directory = tempfile::tempdir().unwrap();
        let path = book_path(directory.path(), "book");
        let key = KeyPair::generate_ed25519();
        let book = PublicWorkerOfferAdmissionBook::open_with_capacity(&path, 4).unwrap();
        let original = signed_offer(&key, 9);
        let original_digest = book.admit(&original, &requirements()).unwrap();

        let mut changed_body = original.body.clone();
        changed_body.public_binding = h(22);
        changed_body.profile_hash = h(23);
        let changed = PublicWorkerOffer::sign(changed_body, &key).unwrap();
        let mut changed_requirements = requirements();
        changed_requirements.public_binding = h(22);
        changed_requirements.profile_hash = h(23);
        assert_ne!(original_digest, changed.digest().unwrap());
        assert!(matches!(
            book.admit(&changed, &changed_requirements),
            Err(PublicWorkerOfferAdmissionError::Replay)
        ));
    }

    #[test]
    fn invalid_signature_does_not_change_durable_journal() {
        let directory = tempfile::tempdir().unwrap();
        let path = book_path(directory.path(), "book");
        let book = PublicWorkerOfferAdmissionBook::open_with_capacity(&path, 4).unwrap();
        let journal = journal_path(&path);
        let before = fs::metadata(&journal).unwrap().len();
        let key = KeyPair::generate_ed25519();
        let mut offer = signed_offer(&key, 1);
        if let Signature::Ed25519 { signature, .. } = &mut offer.signature {
            signature[0] ^= 1;
        }
        assert!(matches!(
            book.admit(&offer, &requirements()),
            Err(PublicWorkerOfferAdmissionError::Offer(
                PublicWorkerOfferError::Signature
            ))
        ));
        assert_eq!(fs::metadata(journal).unwrap().len(), before);
    }

    #[test]
    fn simultaneous_book_handles_admit_a_nonce_at_most_once() {
        const CALLERS: usize = 12;
        let directory = tempfile::tempdir().unwrap();
        let path = book_path(directory.path(), "book");
        let books: Vec<_> = (0..CALLERS)
            .map(|_| PublicWorkerOfferAdmissionBook::open_with_capacity(&path, 16).unwrap())
            .collect();
        let key = KeyPair::generate_ed25519();
        let offer = Arc::new(signed_offer(&key, 17));
        let barrier = Arc::new(Barrier::new(CALLERS));
        let threads: Vec<_> = books
            .into_iter()
            .map(|book| {
                let offer = Arc::clone(&offer);
                let barrier = Arc::clone(&barrier);
                thread::spawn(move || {
                    barrier.wait();
                    book.admit(&offer, &requirements())
                })
            })
            .collect();
        let outcomes: Vec<_> = threads
            .into_iter()
            .map(|join| join.join().unwrap())
            .collect();
        assert_eq!(outcomes.iter().filter(|result| result.is_ok()).count(), 1);
        assert!(outcomes.iter().all(|result| match result {
            Ok(_) => true,
            Err(PublicWorkerOfferAdmissionError::Replay)
            | Err(PublicWorkerOfferAdmissionError::Busy) => true,
            Err(_) => false,
        }));
    }

    #[test]
    fn persisted_capacity_is_fixed_and_full_book_refuses_without_eviction() {
        let directory = tempfile::tempdir().unwrap();
        let path = book_path(directory.path(), "book");
        let key = KeyPair::generate_ed25519();
        let book = PublicWorkerOfferAdmissionBook::open_with_capacity(&path, 1).unwrap();
        book.admit(&signed_offer(&key, 1), &requirements()).unwrap();
        assert_eq!(
            PublicWorkerOfferAdmissionBook::open_with_capacity(&path, 2)
                .unwrap_err()
                .to_string(),
            PublicWorkerOfferAdmissionError::CapacityMismatch.to_string()
        );
        assert!(matches!(
            book.admit(&signed_offer(&key, 2), &requirements()),
            Err(PublicWorkerOfferAdmissionError::Capacity)
        ));
        let reopened = PublicWorkerOfferAdmissionBook::open_with_capacity(&path, 1).unwrap();
        assert!(matches!(
            reopened.admit(&signed_offer(&key, 2), &requirements()),
            Err(PublicWorkerOfferAdmissionError::Capacity)
        ));
    }

    #[test]
    fn corrupt_truncated_and_oversized_journals_fail_closed() {
        let directory = tempfile::tempdir().unwrap();
        let corrupt_path = book_path(directory.path(), "corrupt");
        PublicWorkerOfferAdmissionBook::open_with_capacity(&corrupt_path, 2).unwrap();
        let corrupt_journal = journal_path(&corrupt_path);
        let mut bytes = fs::read(&corrupt_journal).unwrap();
        bytes[0] ^= 1;
        fs::write(&corrupt_journal, bytes).unwrap();
        assert!(matches!(
            PublicWorkerOfferAdmissionBook::open_with_capacity(&corrupt_path, 2),
            Err(PublicWorkerOfferAdmissionError::Corrupt)
        ));

        let truncated_path = book_path(directory.path(), "truncated");
        let key = KeyPair::generate_ed25519();
        let truncated_book =
            PublicWorkerOfferAdmissionBook::open_with_capacity(&truncated_path, 2).unwrap();
        truncated_book
            .admit(&signed_offer(&key, 1), &requirements())
            .unwrap();
        let truncated_journal = journal_path(&truncated_path);
        let length = fs::metadata(&truncated_journal).unwrap().len();
        drop(truncated_book);
        OpenOptions::new()
            .write(true)
            .open(&truncated_journal)
            .unwrap()
            .set_len(length - 1)
            .unwrap();
        assert!(matches!(
            PublicWorkerOfferAdmissionBook::open_with_capacity(&truncated_path, 2),
            Err(PublicWorkerOfferAdmissionError::Corrupt)
        ));

        let oversized_path = book_path(directory.path(), "oversized");
        PublicWorkerOfferAdmissionBook::open_with_capacity(&oversized_path, 1).unwrap();
        let oversized_journal = journal_path(&oversized_path);
        let max_len = (ADMISSION_HEADER_LEN + ADMISSION_RECORD_LEN) as u64;
        OpenOptions::new()
            .append(true)
            .open(&oversized_journal)
            .unwrap()
            .write_all(&vec![0; ADMISSION_RECORD_LEN + 1])
            .unwrap();
        assert!(fs::metadata(&oversized_journal).unwrap().len() > max_len);
        assert!(matches!(
            PublicWorkerOfferAdmissionBook::open_with_capacity(&oversized_path, 1),
            Err(PublicWorkerOfferAdmissionError::Corrupt)
        ));
    }

    #[test]
    fn journal_chain_rejects_interior_record_deletion_and_reordering() {
        let directory = tempfile::tempdir().unwrap();
        let key = KeyPair::generate_ed25519();
        for (name, reorder) in [("deleted", false), ("reordered", true)] {
            let path = book_path(directory.path(), name);
            let book = PublicWorkerOfferAdmissionBook::open_with_capacity(&path, 4).unwrap();
            for nonce in 1..=3 {
                book.admit(&signed_offer(&key, nonce), &requirements())
                    .unwrap();
            }
            drop(book);

            let journal = journal_path(&path);
            let bytes = fs::read(&journal).unwrap();
            let record = |index: usize| {
                let start = ADMISSION_HEADER_LEN + index * ADMISSION_RECORD_LEN;
                &bytes[start..start + ADMISSION_RECORD_LEN]
            };
            let mut changed = bytes[..ADMISSION_HEADER_LEN].to_vec();
            if reorder {
                changed.extend_from_slice(record(2));
                changed.extend_from_slice(record(1));
                changed.extend_from_slice(record(0));
            } else {
                changed.extend_from_slice(record(0));
                changed.extend_from_slice(record(2));
            }
            fs::write(&journal, changed).unwrap();
            assert!(matches!(
                PublicWorkerOfferAdmissionBook::open_with_capacity(&path, 4),
                Err(PublicWorkerOfferAdmissionError::Corrupt)
            ));
        }
    }
}
