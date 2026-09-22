//! Atomic, identity-bound local state snapshots and bounded WAL-tail replay.
//!
//! Without a snapshot a node replays its entire WAL on every open, so recovery
//! time grows without bound for the life of the chain. A snapshot is the
//! trusted base that makes replay proportional to what happened since it was
//! taken rather than to everything that ever happened.
//!
//! Two properties are what make it safe to trust:
//!
//! * **Atomic.** The payload is written to a temporary file, fsynced, renamed,
//!   and the directory entry fsynced, and only then is the manifest published
//!   the same way. A crash at any point leaves either no manifest (full replay,
//!   exactly as before) or a manifest whose payload is already durable. There
//!   is no window in which a partial snapshot is loadable.
//!
//! * **Identity-bound.** The manifest names the height, the state root and a
//!   digest over the exact payload bytes. Loading verifies the digest before
//!   decoding anything and recomputes the state root after installing, so a
//!   truncated, corrupt or substituted payload is refused rather than adopted.
//!   A snapshot is not a certificate: it authorises nothing about which chain
//!   this is. That binding belongs to the checkpoint envelope, which is a
//!   separate trust boundary.
//!
//! The payload covers the state that WAL replay reconstructs and the most
//! recent window of history; older history is rebuilt at open from the WAL
//! records below the snapshot, which are decoded and validated anyway. So
//! "install the snapshot, rebuild history from the prefix, then replay the
//! tail" and "replay everything" reach the same state and the same history,
//! while the snapshot itself stays the size of the state rather than the
//! length of the chain. That equivalence is asserted directly by test rather
//! than argued for.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use arc_crypto::{Hash256, hash_bytes};
use arc_types::{Account, Address, Block, EventLog, Identity, Transaction, TxReceipt};
use serde::{Deserialize, Serialize};

use crate::recovery::RecoveryContext;

/// Magic and version on both files. Neither is self-describing otherwise, and
/// an unreadable snapshot must be recognised as such rather than decoded into
/// something plausible.
const SNAPSHOT_MAGIC: &[u8] = b"ARC-SNAPSHOT";
/// Version 2: the history collections carry a recent window only; the rest is
/// rebuilt from the WAL at open. A version-1 reader would install such a
/// snapshot and replay only the tail, silently missing the older history, so
/// the version is bumped and each side refuses the other's snapshot - which
/// costs one full replay, never correctness.
const SNAPSHOT_VERSION: u8 = 2;

/// Heights of history a snapshot carries by default: enough for the chain
/// linkage of the next block and for recent receipt lookups on a node that
/// was bootstrapped from a checkpoint, and nothing that grows with the chain.
pub const DEFAULT_HISTORY_WINDOW: u64 = 256;

pub const PAYLOAD_FILE: &str = "state-snapshot.bin";
pub const MANIFEST_FILE: &str = "state-snapshot.manifest";

/// What a snapshot claims to be. The digest is over the payload file's exact
/// bytes, so it identifies the snapshot itself rather than a description of it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SnapshotIdentity {
    pub height: u64,
    pub state_root: Hash256,
    pub digest: Hash256,
}

/// The manifest, published only after the payload it names is durable.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SnapshotManifestV1 {
    pub identity: SnapshotIdentity,
    /// The first WAL sequence this snapshot does NOT contain - the point
    /// replay resumes from, inclusive.
    ///
    /// Named for what it means rather than for what it was read from. The
    /// writer's `sequence()` is the NEXT number to hand out, not the last one
    /// used, and storing it as "the sequence I contain" skipped the record
    /// that was about to be assigned it: a snapshot at height 12 lost block
    /// 13 while every other height and the state root still matched. That is
    /// the same off-by-one that made a restarted validator re-commit an
    /// anchor, so the field says which end it means.
    pub resume_from_sequence: u64,
    /// The hash of the block at `identity.height`, so tail validation can
    /// resume its chain-linkage check from the snapshot instead of genesis.
    pub block_hash: Hash256,
}

/// Every piece of state that `apply_wal_op` reconstructs. Anything replay does
/// not write is deliberately absent: including it would make the snapshot and
/// a full replay disagree, which is the one thing that must not happen.
///
/// The history collections (`blocks`, `receipts`, `full_transactions`,
/// `event_logs`) hold only the most recent window of heights - see
/// `DEFAULT_HISTORY_WINDOW` - and opening a node rebuilds the rest from the
/// WAL.
///
/// Every collection is ordered by key so the encoding - and therefore the
/// digest - is reproducible from the same state.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct SnapshotPayload {
    pub height: u64,
    pub accounts: Vec<(Address, Account)>,
    pub storage: Vec<(Address, Vec<(Hash256, Vec<u8>)>)>,
    pub contracts: Vec<(Address, Vec<u8>)>,
    pub identities: Vec<(Address, Identity)>,
    pub blocks: Vec<(u64, Block)>,
    pub receipts: Vec<(Hash256, TxReceipt)>,
    pub full_transactions: Vec<(Hash256, Transaction)>,
    pub event_logs: Vec<(u64, Vec<EventLog>)>,
    pub validators: Vec<(Address, u64)>,
    pub staking_pool: u64,
    pub recovery_context: Option<RecoveryContext>,
    pub community_rewards_activation_height: u64,
    pub native_inference_pending: Vec<(Hash256, u64)>,
}

impl SnapshotPayload {
    /// Whether a certified state root over this payload authenticates
    /// everything in it, on a chain whose own recovery context is `local`.
    ///
    /// Only the recovery state root commits to every consensus domain
    /// (accounts, storage, contracts, identities, validators, staking). On a
    /// chain without a recovery context the root is the legacy account-only
    /// Merkle root, so the rest of a checkpoint would be taken on the
    /// serving peer's word. A payload from a different recovery context is
    /// another chain's state. Both are refused; see
    /// docs/design/checkpoint-rejoin.md.
    pub fn root_covers_everything_under(
        &self,
        local: Option<&RecoveryContext>,
    ) -> Result<(), &'static str> {
        match (local, self.recovery_context.as_ref()) {
            (None, _) => Err(
                "this chain's state root commits to accounts only; a checkpoint cannot \
                 authenticate its other domains",
            ),
            (Some(local), Some(payload)) if local == payload => Ok(()),
            (Some(_), _) => Err("the checkpoint belongs to a different recovery context"),
        }
    }

    /// Put every collection in key order. Called before encoding so two nodes
    /// holding the same state produce the same bytes and the same digest.
    pub fn canonicalize(&mut self) {
        self.accounts.sort_by_key(|(address, _)| address.0);
        for (_, entries) in self.storage.iter_mut() {
            entries.sort_by_key(|(key, _)| key.0);
        }
        self.storage.sort_by_key(|(address, _)| address.0);
        self.contracts.sort_by_key(|(address, _)| address.0);
        self.identities.sort_by_key(|(address, _)| address.0);
        self.blocks.sort_by_key(|(height, _)| *height);
        self.receipts.sort_by_key(|(hash, _)| hash.0);
        self.full_transactions.sort_by_key(|(hash, _)| hash.0);
        self.event_logs.sort_by_key(|(height, _)| *height);
        self.validators.sort_by_key(|(address, _)| address.0);
        self.native_inference_pending.sort_by_key(|(id, _)| id.0);
    }

    /// The encoded payload file body, and its digest.
    pub fn encode(&self) -> (Vec<u8>, Hash256) {
        let mut bytes = Vec::with_capacity(1024);
        bytes.extend_from_slice(SNAPSHOT_MAGIC);
        bytes.push(SNAPSHOT_VERSION);
        bytes.extend_from_slice(&bincode::serialize(self).expect("payload is serialisable"));
        let digest = hash_bytes(&bytes);
        (bytes, digest)
    }

    /// Decode a payload file body. The digest is checked by the caller against
    /// the manifest BEFORE this runs, so nothing here trusts the contents.
    pub fn decode(bytes: &[u8]) -> Result<Self, SnapshotError> {
        let rest = bytes
            .strip_prefix(SNAPSHOT_MAGIC)
            .ok_or(SnapshotError::NotASnapshot)?;
        let (version, body) = rest.split_first().ok_or(SnapshotError::Truncated)?;
        if *version != SNAPSHOT_VERSION {
            return Err(SnapshotError::UnknownVersion(*version));
        }
        bincode::deserialize(body).map_err(|error| SnapshotError::Malformed(error.to_string()))
    }
}

/// Why a snapshot could not be written, read or trusted.
///
/// Every read-side variant means the same thing operationally: fall back to
/// full WAL replay. A snapshot is an optimisation over a durable log, so a
/// failure to use one is never a failure to recover.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SnapshotError {
    #[error("no snapshot manifest is present")]
    Absent,
    #[error("file does not begin with the snapshot magic")]
    NotASnapshot,
    #[error("file ends before its version tag")]
    Truncated,
    #[error("snapshot is version {0}, which this build does not understand")]
    UnknownVersion(u8),
    #[error("snapshot body is malformed: {0}")]
    Malformed(String),
    #[error("payload digest {found} does not match the manifest's {expected}")]
    DigestMismatch { expected: Hash256, found: Hash256 },
    #[error("state root after install is {found}, but the manifest claims {expected}")]
    StateRootMismatch { expected: Hash256, found: Hash256 },
    #[error("snapshot i/o failed: {0}")]
    Io(String),
}

/// A snapshot as it exists on disk, already verified against its manifest.
#[derive(Debug, Clone)]
pub struct VerifiedSnapshot {
    pub manifest: SnapshotManifestV1,
    pub payload: SnapshotPayload,
}

/// Write `bytes` to `path` so that a crash leaves either the old file or the
/// new one, never a mixture: temp file, fsync, rename, fsync the directory.
///
/// The directory fsync is not optional. Without it the rename itself can be
/// lost, which would publish a manifest that points at a payload the crash
/// discarded - the exact failure the manifest exists to prevent.
fn write_atomically(path: &Path, bytes: &[u8]) -> Result<(), SnapshotError> {
    use std::io::Write;
    let temporary = path.with_extension("tmp");
    let io = |error: std::io::Error| SnapshotError::Io(error.to_string());
    let mut file = std::fs::File::create(&temporary).map_err(io)?;
    file.write_all(bytes).map_err(io)?;
    file.sync_all().map_err(io)?;
    drop(file);
    std::fs::rename(&temporary, path).map_err(io)?;
    if let Some(parent) = path.parent() {
        std::fs::File::open(parent)
            .and_then(|dir| dir.sync_all())
            .map_err(io)?;
    }
    Ok(())
}

/// Publish a snapshot into `dir`.
///
/// Ordering is the whole design: the payload becomes durable first, and the
/// manifest that names it is published only afterwards. A crash before the
/// manifest lands leaves an orphan payload, which is ignored; a crash after it
/// lands leaves a snapshot whose payload is already on disk.
pub fn publish(
    dir: &Path,
    mut payload: SnapshotPayload,
    state_root: Hash256,
    resume_from_sequence: u64,
    block_hash: Hash256,
) -> Result<SnapshotManifestV1, SnapshotError> {
    payload.canonicalize();
    let height = payload.height;
    let (bytes, digest) = payload.encode();
    write_atomically(&dir.join(PAYLOAD_FILE), &bytes)?;

    let manifest = SnapshotManifestV1 {
        identity: SnapshotIdentity {
            height,
            state_root,
            digest,
        },
        resume_from_sequence,
        block_hash,
    };
    let mut encoded = Vec::with_capacity(128);
    encoded.extend_from_slice(SNAPSHOT_MAGIC);
    encoded.push(SNAPSHOT_VERSION);
    encoded.extend_from_slice(&bincode::serialize(&manifest).expect("manifest is serialisable"));
    write_atomically(&dir.join(MANIFEST_FILE), &encoded)?;
    Ok(manifest)
}

/// Read and verify the snapshot in `dir`, if there is one.
///
/// Verification here covers the payload's identity only. The caller must still
/// recompute the state root AFTER installing and compare it against
/// `manifest.identity.state_root`; a digest proves the bytes are the ones that
/// were written, not that they describe the state they claim to.
pub fn load(dir: &Path) -> Result<VerifiedSnapshot, SnapshotError> {
    let manifest_path = dir.join(MANIFEST_FILE);
    let raw = match std::fs::read(&manifest_path) {
        Ok(raw) => raw,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Err(SnapshotError::Absent);
        }
        Err(error) => return Err(SnapshotError::Io(error.to_string())),
    };
    let rest = raw
        .strip_prefix(SNAPSHOT_MAGIC)
        .ok_or(SnapshotError::NotASnapshot)?;
    let (version, body) = rest.split_first().ok_or(SnapshotError::Truncated)?;
    if *version != SNAPSHOT_VERSION {
        return Err(SnapshotError::UnknownVersion(*version));
    }
    let manifest: SnapshotManifestV1 =
        bincode::deserialize(body).map_err(|error| SnapshotError::Malformed(error.to_string()))?;

    let payload_bytes = std::fs::read(dir.join(PAYLOAD_FILE)).map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            // A manifest without its payload is the shape a lost rename would
            // leave. It is not recoverable and must not be half-applied.
            SnapshotError::Absent
        } else {
            SnapshotError::Io(error.to_string())
        }
    })?;
    let found = hash_bytes(&payload_bytes);
    if found != manifest.identity.digest {
        return Err(SnapshotError::DigestMismatch {
            expected: manifest.identity.digest,
            found,
        });
    }
    let payload = SnapshotPayload::decode(&payload_bytes)?;
    Ok(VerifiedSnapshot { manifest, payload })
}

/// Remove a snapshot, manifest first.
///
/// The manifest goes first for the same reason it is published last: an orphan
/// payload is inert, while a manifest naming a deleted payload is a snapshot
/// that cannot be loaded.
pub fn remove(dir: &Path) -> Result<(), SnapshotError> {
    let io = |error: std::io::Error| SnapshotError::Io(error.to_string());
    for name in [MANIFEST_FILE, PAYLOAD_FILE] {
        match std::fs::remove_file(dir.join(name)) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(io(error)),
        }
    }
    if let Ok(dir_handle) = std::fs::File::open(dir) {
        let _ = dir_handle.sync_all();
    }
    Ok(())
}

/// Where snapshots for a given persistence directory live.
pub fn directory_for(wal_dir: &Path) -> PathBuf {
    wal_dir.to_path_buf()
}

/// Convert a `BTreeMap` of per-address storage into the payload's shape.
pub fn storage_rows(
    rows: BTreeMap<Address, BTreeMap<Hash256, Vec<u8>>>,
) -> Vec<(Address, Vec<(Hash256, Vec<u8>)>)> {
    rows.into_iter()
        .map(|(address, entries)| (address, entries.into_iter().collect()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn payload_at(height: u64) -> SnapshotPayload {
        SnapshotPayload {
            height,
            accounts: vec![(hash_bytes(b"a"), Account::new(hash_bytes(b"a"), 42))],
            staking_pool: 7,
            ..Default::default()
        }
    }

    #[test]
    fn only_a_full_domain_root_under_the_same_context_authenticates_a_checkpoint() {
        let context = RecoveryContext::new("test", hash_bytes(b"genesis"), 1, 1);
        let other = RecoveryContext::new("test", hash_bytes(b"genesis"), 2, 1);
        let mut payload = payload_at(10);
        // Account-only root: refused whatever the payload claims.
        assert!(payload.root_covers_everything_under(None).is_err());
        payload.recovery_context = Some(context.clone());
        assert!(payload.root_covers_everything_under(None).is_err());
        // Full-domain root, same context: authenticated.
        assert!(payload.root_covers_everything_under(Some(&context)).is_ok());
        // Another context, or none in the payload: refused.
        assert!(payload.root_covers_everything_under(Some(&other)).is_err());
        payload.recovery_context = None;
        assert!(payload.root_covers_everything_under(Some(&context)).is_err());
    }

    #[test]
    fn a_published_snapshot_loads_back_identically() {
        let dir = tempfile::tempdir().unwrap();
        let root = hash_bytes(b"root");
        let block = hash_bytes(b"block");
        let manifest = publish(dir.path(), payload_at(10), root, 99, block).unwrap();
        assert_eq!(manifest.identity.height, 10);
        assert_eq!(manifest.identity.state_root, root);
        assert_eq!(manifest.resume_from_sequence, 99);

        let loaded = load(dir.path()).unwrap();
        assert_eq!(loaded.manifest, manifest);
        let mut expected = payload_at(10);
        expected.canonicalize();
        assert_eq!(
            loaded.payload.encode().1,
            expected.encode().1,
            "the loaded payload must be byte-identical to the one published"
        );
    }

    #[test]
    fn the_digest_is_reproducible_from_the_same_state() {
        // Two payloads built in different orders must encode identically, or
        // the digest identifies the encoding rather than the state.
        let mut first = SnapshotPayload::default();
        first.accounts = vec![
            (hash_bytes(b"z"), Account::new(hash_bytes(b"z"), 1)),
            (hash_bytes(b"a"), Account::new(hash_bytes(b"a"), 2)),
        ];
        let mut second = SnapshotPayload::default();
        second.accounts = vec![
            (hash_bytes(b"a"), Account::new(hash_bytes(b"a"), 2)),
            (hash_bytes(b"z"), Account::new(hash_bytes(b"z"), 1)),
        ];
        first.canonicalize();
        second.canonicalize();
        assert_eq!(first.encode().1, second.encode().1);
    }

    #[test]
    fn a_corrupt_payload_is_refused_rather_than_decoded() {
        let dir = tempfile::tempdir().unwrap();
        publish(dir.path(), payload_at(5), hash_bytes(b"r"), 1, hash_bytes(b"b")).unwrap();
        let payload_path = dir.path().join(PAYLOAD_FILE);
        let mut bytes = std::fs::read(&payload_path).unwrap();
        let last = bytes.len() - 1;
        bytes[last] ^= 0xff;
        std::fs::write(&payload_path, &bytes).unwrap();

        match load(dir.path()) {
            Err(SnapshotError::DigestMismatch { .. }) => {}
            other => panic!("a corrupt payload must be refused, got {other:?}"),
        }
    }

    #[test]
    fn a_truncated_payload_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        publish(dir.path(), payload_at(5), hash_bytes(b"r"), 1, hash_bytes(b"b")).unwrap();
        let payload_path = dir.path().join(PAYLOAD_FILE);
        let bytes = std::fs::read(&payload_path).unwrap();
        std::fs::write(&payload_path, &bytes[..bytes.len() / 2]).unwrap();
        assert!(matches!(
            load(dir.path()),
            Err(SnapshotError::DigestMismatch { .. })
        ));
    }

    #[test]
    fn an_orphan_payload_is_not_a_snapshot() {
        // The shape a crash between the two writes leaves. It must read as
        // "no snapshot", so the node falls back to full replay.
        let dir = tempfile::tempdir().unwrap();
        publish(dir.path(), payload_at(5), hash_bytes(b"r"), 1, hash_bytes(b"b")).unwrap();
        std::fs::remove_file(dir.path().join(MANIFEST_FILE)).unwrap();
        assert!(matches!(load(dir.path()), Err(SnapshotError::Absent)));
    }

    #[test]
    fn a_manifest_without_its_payload_is_not_a_snapshot() {
        let dir = tempfile::tempdir().unwrap();
        publish(dir.path(), payload_at(5), hash_bytes(b"r"), 1, hash_bytes(b"b")).unwrap();
        std::fs::remove_file(dir.path().join(PAYLOAD_FILE)).unwrap();
        assert!(matches!(load(dir.path()), Err(SnapshotError::Absent)));
    }

    #[test]
    fn a_snapshot_from_a_newer_build_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        publish(dir.path(), payload_at(5), hash_bytes(b"r"), 1, hash_bytes(b"b")).unwrap();
        let manifest_path = dir.path().join(MANIFEST_FILE);
        let mut bytes = std::fs::read(&manifest_path).unwrap();
        bytes[SNAPSHOT_MAGIC.len()] = SNAPSHOT_VERSION + 1;
        std::fs::write(&manifest_path, &bytes).unwrap();
        assert!(matches!(
            load(dir.path()),
            Err(SnapshotError::UnknownVersion(_))
        ));
    }

    #[test]
    fn republishing_replaces_the_previous_snapshot_atomically() {
        let dir = tempfile::tempdir().unwrap();
        publish(dir.path(), payload_at(5), hash_bytes(b"r1"), 1, hash_bytes(b"b1")).unwrap();
        let second =
            publish(dir.path(), payload_at(50), hash_bytes(b"r2"), 500, hash_bytes(b"b2")).unwrap();
        let loaded = load(dir.path()).unwrap();
        assert_eq!(loaded.manifest, second);
        assert_eq!(loaded.payload.height, 50);
        // No temporary is left behind for a later open to trip over.
        assert!(!dir.path().join("state-snapshot.tmp").exists());
    }

    #[test]
    fn removing_a_snapshot_leaves_nothing_loadable() {
        let dir = tempfile::tempdir().unwrap();
        publish(dir.path(), payload_at(5), hash_bytes(b"r"), 1, hash_bytes(b"b")).unwrap();
        remove(dir.path()).unwrap();
        assert!(matches!(load(dir.path()), Err(SnapshotError::Absent)));
        remove(dir.path()).expect("removing twice is not an error");
    }
}
