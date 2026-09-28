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
        durably_publish_existing_private_no_replace, open_owned_nofollow_directory, open_private,
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
pub const PUBLIC_WORKER_EXECUTION_BINDING_V1_DOMAIN: &str =
    "ARC-public-worker-execution-binding-v1";
pub const PUBLIC_WORKER_EXECUTION_BINDING_V1_VERSION: u16 = 1;
pub const PUBLIC_WORKER_EXECUTION_BINDING_V1_SCHEMA: &str =
    "arc.public-worker-execution-binding.v1";
pub const MAX_PUBLIC_WORKER_EXECUTION_BINDING_FILE_BYTES: u64 = 4 * 1024;
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

/// Operator-pinned execution identity for a public worker offer audience.
///
/// This is a local immutable binding, not a consensus authorization, paid
/// assignment, or proof that the loaded artifact has been verified. Callers
/// must compare it to independently validated node and execution identities
/// before deriving offer requirements.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PublicWorkerExecutionBindingV1 {
    chain_genesis: Hash256,
    recovery_epoch: u64,
    coordinator: Address,
    artifact_hash: Hash256,
    profile_hash: Hash256,
    generation_hash: Hash256,
    kernel_hash: Hash256,
    bundle_hash: Hash256,
}

/// Independently observed identity snapshot. Populate these values from the
/// recovered node state, the validated loaded-package record, and pinned
/// kernel/row-bundle manifests; never from an offer or request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PublicWorkerExecutionIdentities {
    pub chain_genesis: Hash256,
    pub recovery_epoch: u64,
    pub coordinator: Address,
    pub artifact_hash: Hash256,
    pub profile_hash: Hash256,
    pub generation_hash: Hash256,
    pub kernel_hash: Hash256,
    pub bundle_hash: Hash256,
}

/// Strict operator-owned file representation of the public execution binding.
///
/// This file is only a local pin. It does not establish the provenance of its
/// identities; callers must independently derive `current` from recovered
/// node state and validated loaded-package/kernel/bundle records.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PublicWorkerExecutionBindingConfigV1 {
    pub schema: String,
    pub version: u16,
    pub domain: String,
    pub identities: PublicWorkerExecutionIdentities,
}

#[derive(Debug, thiserror::Error)]
pub enum PublicWorkerExecutionBindingLoadError {
    #[error("could not read the private operator execution-binding file: {0}")]
    Io(#[from] io::Error),
    #[error("operator execution-binding file exceeds the bounded size limit")]
    TooLarge,
    #[error("operator execution-binding file changed while it was read")]
    ChangedDuringRead,
    #[error("operator execution-binding file is not valid strict JSON: {0}")]
    Encoding(#[from] serde_json::Error),
    #[error("operator execution-binding schema, version, or domain is unsupported")]
    Schema,
    #[error(transparent)]
    Offer(#[from] PublicWorkerOfferError),
}

impl PublicWorkerExecutionBindingV1 {
    pub fn new(
        identities: PublicWorkerExecutionIdentities,
    ) -> Result<Self, PublicWorkerOfferError> {
        validate_execution_identities(&identities)?;
        Ok(Self {
            chain_genesis: identities.chain_genesis,
            recovery_epoch: identities.recovery_epoch,
            coordinator: identities.coordinator,
            artifact_hash: identities.artifact_hash,
            profile_hash: identities.profile_hash,
            generation_hash: identities.generation_hash,
            kernel_hash: identities.kernel_hash,
            bundle_hash: identities.bundle_hash,
        })
    }

    /// Load one explicit owner-private operator file and require an exact
    /// match with identities independently observed by the caller. The file
    /// must be a private regular file (mode 0600 / protected owner ACL), and
    /// its JSON is bounded and rejects unknown fields. This does not validate
    /// how the caller obtained `current` and does not authorize network work.
    pub fn load_operator_file(
        path: impl AsRef<Path>,
        current: &PublicWorkerExecutionIdentities,
    ) -> Result<Self, PublicWorkerExecutionBindingLoadError> {
        let mut file = open_private(path.as_ref())?;
        let expected_len = file.metadata()?.len();
        if expected_len > MAX_PUBLIC_WORKER_EXECUTION_BINDING_FILE_BYTES {
            return Err(PublicWorkerExecutionBindingLoadError::TooLarge);
        }
        let mut bytes = Vec::with_capacity(expected_len as usize);
        file.take(MAX_PUBLIC_WORKER_EXECUTION_BINDING_FILE_BYTES + 1)
            .read_to_end(&mut bytes)?;
        if bytes.len() as u64 != expected_len {
            return Err(PublicWorkerExecutionBindingLoadError::ChangedDuringRead);
        }

        let config: PublicWorkerExecutionBindingConfigV1 = serde_json::from_slice(&bytes)?;
        if config.schema != PUBLIC_WORKER_EXECUTION_BINDING_V1_SCHEMA
            || config.version != PUBLIC_WORKER_EXECUTION_BINDING_V1_VERSION
            || config.domain != PUBLIC_WORKER_EXECUTION_BINDING_V1_DOMAIN
        {
            return Err(PublicWorkerExecutionBindingLoadError::Schema);
        }
        let binding = Self::new(config.identities)?;
        binding.validate_identities(current)?;
        Ok(binding)
    }

    /// Domain-separated commitment over a fixed-width, versioned encoding.
    pub fn commitment(&self) -> Hash256 {
        let mut hasher = blake3::Hasher::new_derive_key(PUBLIC_WORKER_EXECUTION_BINDING_V1_DOMAIN);
        hasher.update(&PUBLIC_WORKER_EXECUTION_BINDING_V1_VERSION.to_be_bytes());
        hasher.update(self.chain_genesis.as_bytes());
        hasher.update(&self.recovery_epoch.to_be_bytes());
        hasher.update(self.coordinator.as_bytes());
        hasher.update(self.artifact_hash.as_bytes());
        hasher.update(self.profile_hash.as_bytes());
        hasher.update(self.generation_hash.as_bytes());
        hasher.update(self.kernel_hash.as_bytes());
        hasher.update(self.bundle_hash.as_bytes());
        Hash256(*hasher.finalize().as_bytes())
    }

    /// Validate the pinned binding against independently sourced current
    /// identities, then derive requirements without accepting caller-supplied
    /// hash fields.
    pub fn requirements(
        &self,
        current: &PublicWorkerExecutionIdentities,
        now_height: u64,
    ) -> Result<PublicWorkerOfferRequirements, PublicWorkerOfferError> {
        self.validate_identities(current)?;
        Ok(PublicWorkerOfferRequirements {
            chain_genesis: self.chain_genesis,
            recovery_epoch: self.recovery_epoch,
            public_binding: self.commitment(),
            coordinator: self.coordinator,
            artifact_hash: self.artifact_hash,
            profile_hash: self.profile_hash,
            generation_hash: self.generation_hash,
            kernel_hash: self.kernel_hash,
            bundle_hash: self.bundle_hash,
            now_height,
        })
    }

    fn validate_identities(
        &self,
        current: &PublicWorkerExecutionIdentities,
    ) -> Result<(), PublicWorkerOfferError> {
        validate_execution_identities(current)?;
        if self.identities() != *current {
            return Err(PublicWorkerOfferError::ExecutionIdentityMismatch);
        }
        Ok(())
    }

    fn identities(&self) -> PublicWorkerExecutionIdentities {
        PublicWorkerExecutionIdentities {
            chain_genesis: self.chain_genesis,
            recovery_epoch: self.recovery_epoch,
            coordinator: self.coordinator,
            artifact_hash: self.artifact_hash,
            profile_hash: self.profile_hash,
            generation_hash: self.generation_hash,
            kernel_hash: self.kernel_hash,
            bundle_hash: self.bundle_hash,
        }
    }
}

fn validate_execution_identities(
    identities: &PublicWorkerExecutionIdentities,
) -> Result<(), PublicWorkerOfferError> {
    if [
        identities.chain_genesis,
        identities.coordinator,
        identities.artifact_hash,
        identities.profile_hash,
        identities.generation_hash,
        identities.kernel_hash,
        identities.bundle_hash,
    ]
    .contains(&Hash256::ZERO)
    {
        return Err(PublicWorkerOfferError::ZeroCommitment);
    }
    Ok(())
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
    chain_genesis: Hash256,
    recovery_epoch: u64,
    public_binding: Hash256,
    coordinator: Address,
    artifact_hash: Hash256,
    profile_hash: Hash256,
    generation_hash: Hash256,
    kernel_hash: Hash256,
    bundle_hash: Hash256,
    now_height: u64,
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
    #[error("offer execution commitments do not match the required execution binding")]
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
    #[error("offer binding differs from independently observed node or execution identity")]
    ExecutionIdentityMismatch,
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

// The complete-offer store is intentionally a separate, opt-in journal from
// `PublicWorkerOfferAdmissionBook`. The latter's fixed digest-only v1 format
// and API remain unchanged; callers must provision this store in its own new
// dedicated private directory. No endpoint or scheduler is wired to it here.
pub const DEFAULT_PUBLIC_WORKER_OFFER_STORE_ENTRIES: usize = 128;
pub const MAX_PUBLIC_WORKER_OFFER_STORE_ENTRIES: usize = 256;
pub const DEFAULT_PUBLIC_WORKER_OFFER_STORE_BYTES: usize = 16 * 1024 * 1024;
pub const MAX_PUBLIC_WORKER_OFFER_STORE_BYTES: usize = 16 * 1024 * 1024;
const MAX_STORED_OFFER_BYTES: usize = 96 * 1024;
const OFFER_STORE_JOURNAL_FILE: &str = "public-worker-offers.v1.full.journal";
const OFFER_STORE_STAGE_FILE: &str = ".public-worker-offers.v1.full.journal.stage";
const OFFER_STORE_MAGIC: &[u8; 8] = b"ARCPWFS1";
const OFFER_STORE_VERSION: u16 = 1;
const OFFER_STORE_HEADER_PREFIX_LEN: usize = 8 + 2 + 4 + 8;
const OFFER_STORE_HEADER_LEN: usize = OFFER_STORE_HEADER_PREFIX_LEN + 32;
const OFFER_STORE_FRAME_PREFIX_LEN: usize = 8 + 4 + 32 + 8 + 32 + 32 + 8 + 32;
const OFFER_STORE_FRAME_HASH_LEN: usize = 32;

/// Bounded append-only storage for complete, signed public-worker offers.
///
/// The journal stores each complete offer and its nonce replay key in the
/// same hash-chained frame. Entries are never evicted, even after expiry, so
/// an expired nonce cannot be reused. A valid-prefix rollback cannot be
/// detected without an independent monotonic anchor. Callers remain
/// responsible for supplying authoritative requirements; offers are claims,
/// not proof of capacity, entitlement, payment, or scheduled work.
#[derive(Debug, Clone)]
pub struct PublicWorkerOfferStore {
    directory: PathBuf,
    entry_capacity: usize,
    byte_capacity: usize,
}

#[derive(Debug, thiserror::Error)]
pub enum PublicWorkerOfferStoreError {
    #[error(transparent)]
    Offer(#[from] PublicWorkerOfferError),
    #[error("public-worker offer store is busy; retry without blocking the runtime")]
    Busy,
    #[error("public-worker offer store is full at its configured entry or byte limit")]
    Full,
    #[error("public-worker offer store limits are invalid or differ from its durable header")]
    LimitsMismatch,
    #[error("public-worker offer store limits are invalid or exceed hard bounds")]
    InvalidLimits,
    #[error("public-worker offer nonce was already stored for this worker and audience")]
    Replay,
    #[error("public-worker offer store is corrupt, truncated, oversized, or unsupported")]
    Corrupt,
    #[error("serialized signed offer exceeds the bounded offer-store limit")]
    OfferTooLarge,
    #[error("public-worker offer could not be encoded canonically")]
    Encoding,
    #[error("public-worker offer append could not be confirmed durable")]
    PersistenceUncertain,
    #[error("public-worker offer store I/O failed: {0}")]
    Io(#[from] io::Error),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct OfferStoreReplayKey {
    chain_genesis: Hash256,
    recovery_epoch: u64,
    coordinator: Address,
    worker: Address,
    nonce: u64,
}

impl OfferStoreReplayKey {
    fn from_offer(offer: &PublicWorkerOffer) -> Self {
        Self {
            chain_genesis: offer.body.chain_genesis,
            recovery_epoch: offer.body.recovery_epoch,
            coordinator: offer.body.coordinator,
            worker: offer.body.worker,
            nonce: offer.body.nonce,
        }
    }
}

#[derive(Debug)]
struct OfferStoreJournal {
    offers: Vec<PublicWorkerOffer>,
    replay_keys: HashSet<OfferStoreReplayKey>,
    last_record_hash: Hash256,
    encoded_len: usize,
}

struct BoundedOfferWriter {
    bytes: Vec<u8>,
    limit: usize,
    overflowed: bool,
}

impl Write for BoundedOfferWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let Some(next_len) = self.bytes.len().checked_add(bytes.len()) else {
            self.overflowed = true;
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "offer encoding overflow",
            ));
        };
        if next_len > self.limit {
            self.overflowed = true;
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "offer encoding too large",
            ));
        }
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn encode_stored_offer(offer: &PublicWorkerOffer) -> Result<Vec<u8>, PublicWorkerOfferStoreError> {
    offer.body.validate()?;
    if signature_material_len(&offer.signature) > MAX_SIGNATURE_MATERIAL_BYTES {
        return Err(PublicWorkerOfferError::SignatureTooLarge.into());
    }
    let mut writer = BoundedOfferWriter {
        bytes: Vec::with_capacity(4096),
        limit: MAX_STORED_OFFER_BYTES,
        overflowed: false,
    };
    if serde_json::to_writer(&mut writer, offer).is_err() {
        return if writer.overflowed {
            Err(PublicWorkerOfferStoreError::OfferTooLarge)
        } else {
            Err(PublicWorkerOfferStoreError::Encoding)
        };
    }
    Ok(writer.bytes)
}

impl PublicWorkerOfferStore {
    /// Open or initialize the complete-offer journal in a dedicated private
    /// directory. An existing directory without this journal fails closed;
    /// in particular, this does not migrate or reset a digest-only admission
    /// book. Capacity is fixed by the durable header.
    pub fn open(directory: impl AsRef<Path>) -> Result<Self, PublicWorkerOfferStoreError> {
        Self::open_with_limits(
            directory,
            DEFAULT_PUBLIC_WORKER_OFFER_STORE_ENTRIES,
            DEFAULT_PUBLIC_WORKER_OFFER_STORE_BYTES,
        )
    }

    pub fn open_with_limits(
        directory: impl AsRef<Path>,
        entry_capacity: usize,
        byte_capacity: usize,
    ) -> Result<Self, PublicWorkerOfferStoreError> {
        validate_offer_store_limits(entry_capacity, byte_capacity)?;
        let _lock = acquire_offer_store_lock(directory.as_ref())?;
        let directory = _lock.target().to_path_buf();
        let created = ensure_offer_store_directory(&directory)?;
        let mut journal = if created {
            initialize_offer_store_journal(&directory, entry_capacity, byte_capacity)?
        } else {
            open_existing_offer_store_journal(&directory)?
        };
        let _snapshot = read_offer_store_journal(&mut journal, entry_capacity, byte_capacity)?;
        Ok(Self {
            directory,
            entry_capacity,
            byte_capacity,
        })
    }

    /// Verify and append the complete offer plus its replay key. Success is
    /// returned only after the appended frame is synced and a complete bounded
    /// reread verifies its hash chain, signature, payload, and replay key.
    pub fn store(
        &self,
        offer: &PublicWorkerOffer,
        requirements: &PublicWorkerOfferRequirements,
    ) -> Result<Hash256, PublicWorkerOfferStoreError> {
        offer.verify(requirements)?;
        let digest = offer.digest()?;
        let payload = encode_stored_offer(offer)?;
        let replay_key = OfferStoreReplayKey::from_offer(offer);

        let _lock = acquire_offer_store_lock(&self.directory)?;
        let directory = _lock.target();
        let _directory_pin = open_owned_nofollow_directory(directory)?;
        let mut journal = open_existing_offer_store_journal(directory)?;
        let mut state =
            read_offer_store_journal(&mut journal, self.entry_capacity, self.byte_capacity)?;
        if state.replay_keys.contains(&replay_key) {
            return Err(PublicWorkerOfferStoreError::Replay);
        }
        if state.offers.len() >= self.entry_capacity {
            return Err(PublicWorkerOfferStoreError::Full);
        }

        let sequence =
            u64::try_from(state.offers.len()).map_err(|_| PublicWorkerOfferStoreError::Corrupt)?;
        let frame =
            encode_offer_store_frame(sequence, replay_key, state.last_record_hash, &payload)?;
        let next_len = state
            .encoded_len
            .checked_add(frame.len())
            .ok_or(PublicWorkerOfferStoreError::Full)?;
        if next_len > self.byte_capacity {
            return Err(PublicWorkerOfferStoreError::Full);
        }
        if journal.write_all(&frame).is_err() || journal.sync_all().is_err() {
            return Err(PublicWorkerOfferStoreError::PersistenceUncertain);
        }
        state = read_offer_store_journal(&mut journal, self.entry_capacity, self.byte_capacity)?;
        if state.offers.last() != Some(offer)
            || state.replay_keys.len() != sequence as usize + 1
            || state.encoded_len != next_len
        {
            return Err(PublicWorkerOfferStoreError::PersistenceUncertain);
        }
        Ok(digest)
    }

    /// Return a stable snapshot of offers that match all caller-supplied
    /// context and are valid at `requirements.now_height`. Every stored entry
    /// is structurally and cryptographically verified during the bounded
    /// journal reread; expired entries remain in the replay set and are never
    /// evicted here.
    pub fn verified_unexpired_offers(
        &self,
        requirements: &PublicWorkerOfferRequirements,
    ) -> Result<Vec<PublicWorkerOffer>, PublicWorkerOfferStoreError> {
        let _lock = acquire_offer_store_lock(&self.directory)?;
        let directory = _lock.target();
        let _directory_pin = open_owned_nofollow_directory(directory)?;
        let mut journal = open_existing_offer_store_journal(directory)?;
        let state =
            read_offer_store_journal(&mut journal, self.entry_capacity, self.byte_capacity)?;
        let mut matches = Vec::new();
        for offer in state.offers {
            if !offer_matches_context(&offer, requirements) {
                continue;
            }
            match offer.verify(requirements) {
                Ok(()) => matches.push(offer),
                Err(PublicWorkerOfferError::Expired { .. })
                | Err(PublicWorkerOfferError::NotYetValid { .. }) => {}
                Err(_) => return Err(PublicWorkerOfferStoreError::Corrupt),
            }
        }
        Ok(matches)
    }
}

fn validate_offer_store_limits(
    entry_capacity: usize,
    byte_capacity: usize,
) -> Result<(), PublicWorkerOfferStoreError> {
    let minimum = OFFER_STORE_HEADER_LEN
        .checked_add(OFFER_STORE_FRAME_PREFIX_LEN + OFFER_STORE_FRAME_HASH_LEN + 1)
        .ok_or(PublicWorkerOfferStoreError::InvalidLimits)?;
    if entry_capacity == 0
        || entry_capacity > MAX_PUBLIC_WORKER_OFFER_STORE_ENTRIES
        || byte_capacity < minimum
        || byte_capacity > MAX_PUBLIC_WORKER_OFFER_STORE_BYTES
    {
        return Err(PublicWorkerOfferStoreError::InvalidLimits);
    }
    Ok(())
}

fn acquire_offer_store_lock(
    directory: &Path,
) -> Result<arc_crypto::secret_file::PrivateDirectoryNamespaceLock, PublicWorkerOfferStoreError> {
    match try_acquire_private_directory_namespace_lock(directory) {
        Ok(lock) => Ok(lock),
        Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
            Err(PublicWorkerOfferStoreError::Busy)
        }
        Err(error) => Err(error.into()),
    }
}

fn ensure_offer_store_directory(directory: &Path) -> Result<bool, PublicWorkerOfferStoreError> {
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

fn offer_store_journal_path(directory: &Path) -> PathBuf {
    directory.join(OFFER_STORE_JOURNAL_FILE)
}

fn open_existing_offer_store_journal(
    directory: &Path,
) -> Result<fs::File, PublicWorkerOfferStoreError> {
    let path = offer_store_journal_path(directory);
    match open_private_append_owned_migration(&path) {
        Ok(file) => Ok(file),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            Err(PublicWorkerOfferStoreError::Corrupt)
        }
        Err(error) => Err(error.into()),
    }
}

fn initialize_offer_store_journal(
    directory: &Path,
    entry_capacity: usize,
    byte_capacity: usize,
) -> Result<fs::File, PublicWorkerOfferStoreError> {
    let path = offer_store_journal_path(directory);
    let stage = directory.join(OFFER_STORE_STAGE_FILE);
    let mut file = match create_new_private(&stage) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
            return Err(PublicWorkerOfferStoreError::Corrupt);
        }
        Err(error) => return Err(error.into()),
    };
    file.write_all(&encode_offer_store_header(entry_capacity, byte_capacity))?;
    file.sync_all()?;
    drop(file);
    match durably_publish_existing_private_no_replace(&stage, &path) {
        Ok(()) => {
            sync_parent_directory(&path)?;
            open_private_append_owned_migration(&path).map_err(Into::into)
        }
        Err(publish_error) => {
            // The no-replace publish can report an error after the name was
            // committed. Only accept that state after revalidating and
            // syncing the exact published journal; never reset ambiguous data.
            if let Ok(mut published) = open_private_append_owned_migration(&path)
                && read_offer_store_journal(&mut published, entry_capacity, byte_capacity).is_ok()
                && published.sync_all().is_ok()
                && sync_parent_directory(&path).is_ok()
            {
                return Ok(published);
            }
            Err(publish_error.into())
        }
    }
}

fn encode_offer_store_header(entry_capacity: usize, byte_capacity: usize) -> Vec<u8> {
    let mut header = Vec::with_capacity(OFFER_STORE_HEADER_LEN);
    header.extend_from_slice(OFFER_STORE_MAGIC);
    header.extend_from_slice(&OFFER_STORE_VERSION.to_le_bytes());
    header.extend_from_slice(&(entry_capacity as u32).to_le_bytes());
    header.extend_from_slice(&(byte_capacity as u64).to_le_bytes());
    let checksum = offer_store_hash("ARC-public-worker-offer-store-header-v1", &header);
    header.extend_from_slice(&checksum);
    header
}

fn encode_offer_store_frame(
    sequence: u64,
    replay_key: OfferStoreReplayKey,
    previous_record_hash: Hash256,
    payload: &[u8],
) -> Result<Vec<u8>, PublicWorkerOfferStoreError> {
    let payload_len =
        u32::try_from(payload.len()).map_err(|_| PublicWorkerOfferStoreError::OfferTooLarge)?;
    if payload.is_empty() || payload.len() > MAX_STORED_OFFER_BYTES {
        return Err(PublicWorkerOfferStoreError::OfferTooLarge);
    }
    let total_len = OFFER_STORE_FRAME_PREFIX_LEN
        .checked_add(payload.len())
        .and_then(|len| len.checked_add(OFFER_STORE_FRAME_HASH_LEN))
        .ok_or(PublicWorkerOfferStoreError::OfferTooLarge)?;
    let mut frame = Vec::with_capacity(total_len);
    frame.extend_from_slice(&sequence.to_le_bytes());
    frame.extend_from_slice(&payload_len.to_le_bytes());
    frame.extend_from_slice(replay_key.chain_genesis.as_bytes());
    frame.extend_from_slice(&replay_key.recovery_epoch.to_le_bytes());
    frame.extend_from_slice(replay_key.coordinator.as_bytes());
    frame.extend_from_slice(replay_key.worker.as_bytes());
    frame.extend_from_slice(&replay_key.nonce.to_le_bytes());
    frame.extend_from_slice(previous_record_hash.as_bytes());
    frame.extend_from_slice(payload);
    let checksum = offer_store_hash("ARC-public-worker-offer-store-frame-v1", &frame);
    frame.extend_from_slice(&checksum);
    Ok(frame)
}

fn offer_store_hash(domain: &str, bytes: &[u8]) -> [u8; 32] {
    let mut hasher = blake3::Hasher::new_derive_key(domain);
    hasher.update(bytes);
    *hasher.finalize().as_bytes()
}

fn read_offer_store_journal(
    file: &mut fs::File,
    expected_entries: usize,
    expected_bytes: usize,
) -> Result<OfferStoreJournal, PublicWorkerOfferStoreError> {
    let length = file.metadata()?.len();
    if length < OFFER_STORE_HEADER_LEN as u64 || length > expected_bytes as u64 {
        return Err(PublicWorkerOfferStoreError::Corrupt);
    }
    file.seek(SeekFrom::Start(0))?;
    // `expected_bytes` is validated against the fixed hard bound before this
    // allocation. The file length check above precedes every payload allocation.
    let mut bytes = Vec::with_capacity(length as usize);
    (&mut *file)
        .take(expected_bytes as u64 + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 != length || bytes.len() > expected_bytes {
        return Err(PublicWorkerOfferStoreError::Corrupt);
    }
    parse_offer_store_journal(&bytes, expected_entries, expected_bytes)
}

fn parse_offer_store_journal(
    bytes: &[u8],
    expected_entries: usize,
    expected_bytes: usize,
) -> Result<OfferStoreJournal, PublicWorkerOfferStoreError> {
    if bytes.len() < OFFER_STORE_HEADER_LEN
        || &bytes[..8] != OFFER_STORE_MAGIC
        || u16::from_le_bytes([bytes[8], bytes[9]]) != OFFER_STORE_VERSION
    {
        return Err(PublicWorkerOfferStoreError::Corrupt);
    }
    let entry_capacity = u32::from_le_bytes([bytes[10], bytes[11], bytes[12], bytes[13]]) as usize;
    let byte_capacity = usize::try_from(u64::from_le_bytes([
        bytes[14], bytes[15], bytes[16], bytes[17], bytes[18], bytes[19], bytes[20], bytes[21],
    ]))
    .map_err(|_| PublicWorkerOfferStoreError::Corrupt)?;
    if entry_capacity != expected_entries || byte_capacity != expected_bytes {
        return Err(PublicWorkerOfferStoreError::LimitsMismatch);
    }
    if offer_store_hash(
        "ARC-public-worker-offer-store-header-v1",
        &bytes[..OFFER_STORE_HEADER_PREFIX_LEN],
    ) != bytes[OFFER_STORE_HEADER_PREFIX_LEN..OFFER_STORE_HEADER_LEN]
    {
        return Err(PublicWorkerOfferStoreError::Corrupt);
    }

    let mut cursor = OFFER_STORE_HEADER_LEN;
    let mut offers = Vec::new();
    let mut replay_keys = HashSet::new();
    let mut previous_record_hash = Hash256::ZERO;
    while cursor < bytes.len() {
        if offers.len() >= expected_entries
            || bytes.len() - cursor < OFFER_STORE_FRAME_PREFIX_LEN + OFFER_STORE_FRAME_HASH_LEN
        {
            return Err(PublicWorkerOfferStoreError::Corrupt);
        }
        let frame_start = cursor;
        let sequence = store_read_u64(bytes, &mut cursor)?;
        let payload_len = store_read_u32(bytes, &mut cursor)? as usize;
        if payload_len == 0 || payload_len > MAX_STORED_OFFER_BYTES {
            return Err(PublicWorkerOfferStoreError::Corrupt);
        }
        let replay_key = store_read_replay_key(bytes, &mut cursor)?;
        let stored_previous_hash = store_read_hash(bytes, &mut cursor)?;
        let frame_end = frame_start
            .checked_add(OFFER_STORE_FRAME_PREFIX_LEN)
            .and_then(|end| end.checked_add(payload_len))
            .and_then(|end| end.checked_add(OFFER_STORE_FRAME_HASH_LEN))
            .ok_or(PublicWorkerOfferStoreError::Corrupt)?;
        if frame_end > bytes.len() {
            return Err(PublicWorkerOfferStoreError::Corrupt);
        }
        let payload_end = cursor
            .checked_add(payload_len)
            .ok_or(PublicWorkerOfferStoreError::Corrupt)?;
        let payload = &bytes[cursor..payload_end];
        cursor = payload_end;
        let stored_hash = &bytes[cursor..frame_end];
        let calculated_hash = offer_store_hash(
            "ARC-public-worker-offer-store-frame-v1",
            &bytes[frame_start..cursor],
        );
        if stored_hash != calculated_hash || stored_previous_hash != previous_record_hash {
            return Err(PublicWorkerOfferStoreError::Corrupt);
        }
        let expected_sequence =
            u64::try_from(offers.len()).map_err(|_| PublicWorkerOfferStoreError::Corrupt)?;
        if sequence != expected_sequence {
            return Err(PublicWorkerOfferStoreError::Corrupt);
        }

        let offer: PublicWorkerOffer =
            serde_json::from_slice(payload).map_err(|_| PublicWorkerOfferStoreError::Corrupt)?;
        let canonical =
            encode_stored_offer(&offer).map_err(|_| PublicWorkerOfferStoreError::Corrupt)?;
        if canonical != payload {
            return Err(PublicWorkerOfferStoreError::Corrupt);
        }
        verify_offer_signature_only(&offer).map_err(|_| PublicWorkerOfferStoreError::Corrupt)?;
        if OfferStoreReplayKey::from_offer(&offer) != replay_key || !replay_keys.insert(replay_key)
        {
            return Err(PublicWorkerOfferStoreError::Corrupt);
        }
        previous_record_hash = Hash256(
            stored_hash
                .try_into()
                .map_err(|_| PublicWorkerOfferStoreError::Corrupt)?,
        );
        cursor = frame_end;
        offers.push(offer);
    }
    Ok(OfferStoreJournal {
        offers,
        replay_keys,
        last_record_hash: previous_record_hash,
        encoded_len: bytes.len(),
    })
}

fn verify_offer_signature_only(offer: &PublicWorkerOffer) -> Result<(), PublicWorkerOfferError> {
    if signature_material_len(&offer.signature) > MAX_SIGNATURE_MATERIAL_BYTES {
        return Err(PublicWorkerOfferError::SignatureTooLarge);
    }
    offer.body.validate()?;
    offer
        .signature
        .verify(&offer.body.transcript()?, &offer.body.worker)
        .map_err(|_| PublicWorkerOfferError::Signature)
}

fn offer_matches_context(
    offer: &PublicWorkerOffer,
    requirements: &PublicWorkerOfferRequirements,
) -> bool {
    let body = &offer.body;
    body.chain_genesis == requirements.chain_genesis
        && body.recovery_epoch == requirements.recovery_epoch
        && body.public_binding == requirements.public_binding
        && body.coordinator == requirements.coordinator
        && body.artifact_hash == requirements.artifact_hash
        && body.profile_hash == requirements.profile_hash
        && body.generation_hash == requirements.generation_hash
        && body.kernel_hash == requirements.kernel_hash
        && body.bundle_hash == requirements.bundle_hash
}

fn store_read_u32(bytes: &[u8], cursor: &mut usize) -> Result<u32, PublicWorkerOfferStoreError> {
    let end = cursor
        .checked_add(4)
        .ok_or(PublicWorkerOfferStoreError::Corrupt)?;
    let value = u32::from_le_bytes(
        bytes
            .get(*cursor..end)
            .ok_or(PublicWorkerOfferStoreError::Corrupt)?
            .try_into()
            .map_err(|_| PublicWorkerOfferStoreError::Corrupt)?,
    );
    *cursor = end;
    Ok(value)
}

fn store_read_u64(bytes: &[u8], cursor: &mut usize) -> Result<u64, PublicWorkerOfferStoreError> {
    let end = cursor
        .checked_add(8)
        .ok_or(PublicWorkerOfferStoreError::Corrupt)?;
    let value = u64::from_le_bytes(
        bytes
            .get(*cursor..end)
            .ok_or(PublicWorkerOfferStoreError::Corrupt)?
            .try_into()
            .map_err(|_| PublicWorkerOfferStoreError::Corrupt)?,
    );
    *cursor = end;
    Ok(value)
}

fn store_read_hash(
    bytes: &[u8],
    cursor: &mut usize,
) -> Result<Hash256, PublicWorkerOfferStoreError> {
    let end = cursor
        .checked_add(32)
        .ok_or(PublicWorkerOfferStoreError::Corrupt)?;
    let mut value = [0u8; 32];
    value.copy_from_slice(
        bytes
            .get(*cursor..end)
            .ok_or(PublicWorkerOfferStoreError::Corrupt)?,
    );
    *cursor = end;
    Ok(Hash256(value))
}

fn store_read_replay_key(
    bytes: &[u8],
    cursor: &mut usize,
) -> Result<OfferStoreReplayKey, PublicWorkerOfferStoreError> {
    let chain_genesis = store_read_hash(bytes, cursor)?;
    let recovery_epoch = store_read_u64(bytes, cursor)?;
    let coordinator = store_read_hash(bytes, cursor)?;
    let worker = store_read_hash(bytes, cursor)?;
    let nonce = store_read_u64(bytes, cursor)?;
    if chain_genesis == Hash256::ZERO
        || coordinator == Hash256::ZERO
        || worker == Hash256::ZERO
        || nonce == 0
    {
        return Err(PublicWorkerOfferStoreError::Corrupt);
    }
    Ok(OfferStoreReplayKey {
        chain_genesis,
        recovery_epoch,
        coordinator,
        worker,
        nonce,
    })
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

    fn identities() -> PublicWorkerExecutionIdentities {
        PublicWorkerExecutionIdentities {
            chain_genesis: h(1),
            recovery_epoch: 7,
            coordinator: h(3),
            artifact_hash: h(4),
            profile_hash: h(5),
            generation_hash: h(6),
            kernel_hash: h(7),
            bundle_hash: h(8),
        }
    }

    fn binding() -> PublicWorkerExecutionBindingV1 {
        PublicWorkerExecutionBindingV1::new(identities()).unwrap()
    }

    fn operator_binding_config(
        identities: PublicWorkerExecutionIdentities,
    ) -> PublicWorkerExecutionBindingConfigV1 {
        PublicWorkerExecutionBindingConfigV1 {
            schema: PUBLIC_WORKER_EXECUTION_BINDING_V1_SCHEMA.into(),
            version: PUBLIC_WORKER_EXECUTION_BINDING_V1_VERSION,
            domain: PUBLIC_WORKER_EXECUTION_BINDING_V1_DOMAIN.into(),
            identities,
        }
    }

    fn write_private_bytes(directory: &Path, bytes: &[u8]) -> PathBuf {
        let path = directory.join("public-worker-binding.json");
        let mut file = create_new_private(&path).unwrap();
        file.write_all(bytes).unwrap();
        file.sync_all().unwrap();
        path
    }

    fn write_operator_binding(
        directory: &Path,
        identities: PublicWorkerExecutionIdentities,
    ) -> PathBuf {
        let bytes = serde_json::to_vec(&operator_binding_config(identities)).unwrap();
        write_private_bytes(directory, &bytes)
    }

    fn body(worker: Address) -> PublicWorkerOfferBody {
        PublicWorkerOfferBody {
            version: PUBLIC_WORKER_OFFER_VERSION,
            chain_genesis: h(1),
            recovery_epoch: 7,
            public_binding: binding().commitment(),
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
        binding().requirements(&identities(), 150).unwrap()
    }

    #[test]
    fn execution_binding_is_deterministic_versioned_and_derives_requirements() {
        let binding = binding();
        let commitment = binding.commitment();
        assert_eq!(
            commitment,
            Hash256([
                0x04, 0xcc, 0xc6, 0x9a, 0xc1, 0xc1, 0xa5, 0x97, 0x63, 0x28, 0x3f, 0x1e, 0xcb, 0x16,
                0x61, 0x6d, 0x85, 0xf0, 0x5e, 0xba, 0x69, 0xe6, 0x68, 0x76, 0x9d, 0xfc, 0xb6, 0x1c,
                0x8f, 0x87, 0x42, 0x49,
            ])
        );
        assert_eq!(binding.commitment(), commitment);
        let requirements = binding.requirements(&identities(), 150).unwrap();
        assert_eq!(requirements.public_binding, commitment);
        assert_eq!(requirements.coordinator, h(3));
        assert_eq!(requirements.artifact_hash, h(4));
    }

    #[test]
    fn operator_binding_file_is_private_bounded_and_matches_runtime_identities() {
        let directory = tempfile::tempdir().unwrap();
        let path = write_operator_binding(directory.path(), identities());
        let loaded = PublicWorkerExecutionBindingV1::load_operator_file(&path, &identities())
            .expect("matching explicit operator binding should load");
        assert_eq!(loaded.commitment(), binding().commitment());
        let requirements = loaded.requirements(&identities(), 150).unwrap();
        assert_eq!(requirements.public_binding, binding().commitment());
        assert_eq!(requirements.now_height, 150);
    }

    #[test]
    fn operator_binding_file_rejects_each_runtime_identity_mismatch() {
        let directory = tempfile::tempdir().unwrap();
        let path = write_operator_binding(directory.path(), identities());
        for field in 0..8 {
            let mut changed = identities();
            match field {
                0 => changed.chain_genesis = h(21),
                1 => changed.recovery_epoch += 1,
                2 => changed.coordinator = h(22),
                3 => changed.artifact_hash = h(23),
                4 => changed.profile_hash = h(24),
                5 => changed.generation_hash = h(25),
                6 => changed.kernel_hash = h(26),
                7 => changed.bundle_hash = h(27),
                _ => unreachable!(),
            }
            assert!(matches!(
                PublicWorkerExecutionBindingV1::load_operator_file(&path, &changed),
                Err(PublicWorkerExecutionBindingLoadError::Offer(
                    PublicWorkerOfferError::ExecutionIdentityMismatch
                ))
            ));
        }
    }

    #[test]
    fn operator_binding_file_rejects_unsupported_schema_version_and_domain() {
        let directory = tempfile::tempdir().unwrap();
        for mutation in 0..3 {
            let mut config = operator_binding_config(identities());
            match mutation {
                0 => config.schema.push_str("-unknown"),
                1 => config.version += 1,
                2 => config.domain.push_str("-unknown"),
                _ => unreachable!(),
            }
            let path = directory.path().join(format!("binding-{mutation}.json"));
            let mut file = create_new_private(&path).unwrap();
            file.write_all(&serde_json::to_vec(&config).unwrap())
                .unwrap();
            file.sync_all().unwrap();
            assert!(matches!(
                PublicWorkerExecutionBindingV1::load_operator_file(&path, &identities()),
                Err(PublicWorkerExecutionBindingLoadError::Schema)
            ));
        }
    }

    #[test]
    fn operator_binding_file_rejects_unknown_json_fields() {
        let directory = tempfile::tempdir().unwrap();
        let mut value = serde_json::to_value(operator_binding_config(identities())).unwrap();
        value["unrecognized"] = serde_json::json!(true);
        let path = write_private_bytes(directory.path(), &serde_json::to_vec(&value).unwrap());
        assert!(matches!(
            PublicWorkerExecutionBindingV1::load_operator_file(&path, &identities()),
            Err(PublicWorkerExecutionBindingLoadError::Encoding(_))
        ));
    }

    #[test]
    fn operator_binding_file_rejects_oversize_input() {
        let directory = tempfile::tempdir().unwrap();
        let path = write_private_bytes(
            directory.path(),
            &vec![b' '; MAX_PUBLIC_WORKER_EXECUTION_BINDING_FILE_BYTES as usize + 1],
        );
        assert!(matches!(
            PublicWorkerExecutionBindingV1::load_operator_file(&path, &identities()),
            Err(PublicWorkerExecutionBindingLoadError::TooLarge)
        ));
    }

    #[cfg(unix)]
    #[test]
    fn operator_binding_file_rejects_symlink_and_nonprivate_mode() {
        use std::os::unix::fs::{PermissionsExt, symlink};

        let directory = tempfile::tempdir().unwrap();
        let path = write_operator_binding(directory.path(), identities());
        let link = directory.path().join("binding-link.json");
        symlink(&path, &link).unwrap();
        assert!(matches!(
            PublicWorkerExecutionBindingV1::load_operator_file(&link, &identities()),
            Err(PublicWorkerExecutionBindingLoadError::Io(_))
        ));

        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(matches!(
            PublicWorkerExecutionBindingV1::load_operator_file(&path, &identities()),
            Err(PublicWorkerExecutionBindingLoadError::Io(_))
        ));
    }

    #[test]
    fn execution_binding_refuses_each_changed_identity_and_zero_hashes() {
        let binding = binding();
        let mutations: [fn(&mut PublicWorkerExecutionIdentities); 8] = [
            |i| i.chain_genesis = h(11),
            |i| i.recovery_epoch += 1,
            |i| i.coordinator = h(12),
            |i| i.artifact_hash = h(13),
            |i| i.profile_hash = h(14),
            |i| i.generation_hash = h(15),
            |i| i.kernel_hash = h(16),
            |i| i.bundle_hash = h(17),
        ];
        for mutate in mutations {
            let mut changed = identities();
            mutate(&mut changed);
            assert_ne!(
                PublicWorkerExecutionBindingV1::new(changed)
                    .unwrap()
                    .commitment(),
                binding.commitment()
            );
            assert_eq!(
                binding.requirements(&changed, 150),
                Err(PublicWorkerOfferError::ExecutionIdentityMismatch)
            );
        }

        let mut zero = identities();
        zero.bundle_hash = Hash256::ZERO;
        assert_eq!(
            PublicWorkerExecutionBindingV1::new(zero),
            Err(PublicWorkerOfferError::ZeroCommitment)
        );
    }

    #[test]
    fn execution_binding_changes_when_domain_or_version_changes() {
        let binding = binding();
        let expected = binding.commitment();
        let commitment_for = |domain: &str, version: u16| {
            let mut hasher = blake3::Hasher::new_derive_key(domain);
            hasher.update(&version.to_be_bytes());
            hasher.update(h(1).as_bytes());
            hasher.update(&7_u64.to_be_bytes());
            hasher.update(h(3).as_bytes());
            hasher.update(h(4).as_bytes());
            hasher.update(h(5).as_bytes());
            hasher.update(h(6).as_bytes());
            hasher.update(h(7).as_bytes());
            hasher.update(h(8).as_bytes());
            Hash256(*hasher.finalize().as_bytes())
        };
        assert_ne!(
            commitment_for(
                "ARC-other-domain-v1",
                PUBLIC_WORKER_EXECUTION_BINDING_V1_VERSION
            ),
            expected
        );
        assert_ne!(
            commitment_for(
                PUBLIC_WORKER_EXECUTION_BINDING_V1_DOMAIN,
                PUBLIC_WORKER_EXECUTION_BINDING_V1_VERSION + 1
            ),
            expected
        );
    }

    #[test]
    fn canonical_transcript_has_a_golden_hash() {
        let mut body = body(h(10));
        // Preserve the original generic offer transcript vector independently
        // of the V1 binding-commitment vector above.
        body.public_binding = h(2);
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

    fn full_store_journal_path(directory: &Path) -> PathBuf {
        offer_store_journal_path(directory)
    }

    #[test]
    fn full_offer_store_reopens_and_returns_only_matching_unexpired_offers() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("full-store");
        let key = KeyPair::generate_ed25519();
        let offer = signed_offer(&key, 41);
        {
            let store = PublicWorkerOfferStore::open_with_limits(&path, 4, 1 << 20).unwrap();
            assert_eq!(
                store.store(&offer, &requirements()).unwrap(),
                offer.digest().unwrap()
            );
        }

        let store = PublicWorkerOfferStore::open_with_limits(&path, 4, 1 << 20).unwrap();
        assert_eq!(
            store.verified_unexpired_offers(&requirements()).unwrap(),
            vec![offer.clone()]
        );
        assert!(matches!(
            store.store(&offer, &requirements()),
            Err(PublicWorkerOfferStoreError::Replay)
        ));

        let mut expired = requirements();
        expired.now_height = offer.body.expires_at_height;
        assert!(
            store
                .verified_unexpired_offers(&expired)
                .unwrap()
                .is_empty()
        );

        // Expiry filters the read result, but never removes its replay key.
        let mut renewed_body = offer.body.clone();
        renewed_body.issued_at_height = 200;
        renewed_body.expires_at_height = 300;
        let renewed = PublicWorkerOffer::sign(renewed_body, &key).unwrap();
        let mut renewed_requirements = requirements();
        renewed_requirements.now_height = 250;
        assert!(matches!(
            store.store(&renewed, &renewed_requirements),
            Err(PublicWorkerOfferStoreError::Replay)
        ));

        let mut wrong_context = requirements();
        wrong_context.public_binding = h(88);
        assert!(
            store
                .verified_unexpired_offers(&wrong_context)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn full_offer_store_fails_closed_on_torn_frame_and_never_resets_missing_journal() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("torn-store");
        let key = KeyPair::generate_ed25519();
        let store = PublicWorkerOfferStore::open_with_limits(&path, 4, 1 << 20).unwrap();
        store
            .store(&signed_offer(&key, 1), &requirements())
            .unwrap();
        let journal = full_store_journal_path(&path);
        drop(store);

        OpenOptions::new()
            .append(true)
            .open(&journal)
            .unwrap()
            .write_all(&[0xa5; 7])
            .unwrap();
        assert!(matches!(
            PublicWorkerOfferStore::open_with_limits(&path, 4, 1 << 20),
            Err(PublicWorkerOfferStoreError::Corrupt)
        ));
        fs::remove_file(&journal).unwrap();
        assert!(matches!(
            PublicWorkerOfferStore::open_with_limits(&path, 4, 1 << 20),
            Err(PublicWorkerOfferStoreError::Corrupt)
        ));
        assert!(!journal.exists());
    }

    #[test]
    fn full_offer_store_rejects_body_tampering_even_with_recomputed_frame_hash() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("body-tamper");
        let key = KeyPair::generate_ed25519();
        let store = PublicWorkerOfferStore::open_with_limits(&path, 4, 1 << 20).unwrap();
        store
            .store(&signed_offer(&key, 2), &requirements())
            .unwrap();
        drop(store);

        let journal = full_store_journal_path(&path);
        let mut bytes = fs::read(&journal).unwrap();
        let payload_start = OFFER_STORE_HEADER_LEN + OFFER_STORE_FRAME_PREFIX_LEN;
        let frame_end = bytes.len();
        let payload_end = frame_end - OFFER_STORE_FRAME_HASH_LEN;
        let payload = std::str::from_utf8(&bytes[payload_start..payload_end]).unwrap();
        let modified = payload.replace(
            "\"memory_capacity_bytes\":1073741824",
            "\"memory_capacity_bytes\":1073741825",
        );
        assert_ne!(modified, payload, "test offer JSON field spelling changed");
        assert_eq!(modified.len(), payload.len());
        bytes[payload_start..payload_end].copy_from_slice(modified.as_bytes());
        let frame_hash = offer_store_hash(
            "ARC-public-worker-offer-store-frame-v1",
            &bytes[OFFER_STORE_HEADER_LEN..payload_end],
        );
        bytes[payload_end..].copy_from_slice(&frame_hash);
        fs::write(&journal, bytes).unwrap();

        assert!(matches!(
            PublicWorkerOfferStore::open_with_limits(&path, 4, 1 << 20),
            Err(PublicWorkerOfferStoreError::Corrupt)
        ));
    }

    #[test]
    fn full_offer_store_enforces_persisted_entry_and_byte_bounds() {
        let directory = tempfile::tempdir().unwrap();
        let key = KeyPair::generate_ed25519();
        let path = directory.path().join("entry-cap");
        let store = PublicWorkerOfferStore::open_with_limits(&path, 1, 1 << 20).unwrap();
        store
            .store(&signed_offer(&key, 1), &requirements())
            .unwrap();
        assert!(matches!(
            store.store(&signed_offer(&key, 2), &requirements()),
            Err(PublicWorkerOfferStoreError::Full)
        ));
        assert!(matches!(
            PublicWorkerOfferStore::open_with_limits(&path, 2, 1 << 20),
            Err(PublicWorkerOfferStoreError::LimitsMismatch)
        ));

        let byte_path = directory.path().join("byte-cap");
        let offer = signed_offer(&key, 3);
        let payload_len = encode_stored_offer(&offer).unwrap().len();
        let byte_capacity = OFFER_STORE_HEADER_LEN
            + OFFER_STORE_FRAME_PREFIX_LEN
            + payload_len
            + OFFER_STORE_FRAME_HASH_LEN
            - 1;
        let bounded =
            PublicWorkerOfferStore::open_with_limits(&byte_path, 2, byte_capacity).unwrap();
        let journal = full_store_journal_path(&byte_path);
        let before = fs::metadata(&journal).unwrap().len();
        assert!(matches!(
            bounded.store(&offer, &requirements()),
            Err(PublicWorkerOfferStoreError::Full)
        ));
        assert_eq!(fs::metadata(journal).unwrap().len(), before);
    }

    #[test]
    fn full_store_does_not_migrate_or_overwrite_digest_only_admission_book() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("existing-admission-book");
        PublicWorkerOfferAdmissionBook::open_with_capacity(&path, 4).unwrap();
        let old_journal = journal_path(&path);
        let old_bytes = fs::read(&old_journal).unwrap();
        assert!(matches!(
            PublicWorkerOfferStore::open_with_limits(&path, 4, 1 << 20),
            Err(PublicWorkerOfferStoreError::Corrupt)
        ));
        assert_eq!(fs::read(old_journal).unwrap(), old_bytes);
        assert!(!full_store_journal_path(&path).exists());
    }
}
