use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use arc_crypto::{Hash256, hash_bytes};
use arc_types::{Account, Address, Block, EventLog, Identity, Transaction, TxReceipt};
use serde::{Deserialize, Serialize};

use crate::recovery::RecoveryContext;

const SNAPSHOT_MAGIC: &[u8] = b"ARC-SNAPSHOT";
const SNAPSHOT_VERSION: u8 = 2;

pub const DEFAULT_HISTORY_WINDOW: u64 = 256;

pub const PAYLOAD_FILE: &str = "state-snapshot.bin";
pub const MANIFEST_FILE: &str = "state-snapshot.manifest";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SnapshotIdentity {
    pub height: u64,
    pub state_root: Hash256,
    pub digest: Hash256,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SnapshotManifestV1 {
    pub identity: SnapshotIdentity,
    pub resume_from_sequence: u64,
    pub block_hash: Hash256,
}

pub const MAX_PEER_PAYLOAD_BYTES: usize = 16 * 1024 * 1024;

pub type StorageRows = Vec<(Address, Vec<(Hash256, Vec<u8>)>)>;

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct SnapshotPayload {
    pub height: u64,
    pub accounts: Vec<(Address, Account)>,
    pub storage: StorageRows,
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

    pub fn encode(&self) -> (Vec<u8>, Hash256) {
        let mut bytes = Vec::with_capacity(1024);
        bytes.extend_from_slice(SNAPSHOT_MAGIC);
        bytes.push(SNAPSHOT_VERSION);
        bytes.extend_from_slice(&bincode::serialize(self).expect("payload is serialisable"));
        let digest = hash_bytes(&bytes);
        (bytes, digest)
    }

    pub fn decode_from_peer(bytes: &[u8]) -> Result<Self, SnapshotError> {
        let rest = bytes
            .strip_prefix(SNAPSHOT_MAGIC)
            .ok_or(SnapshotError::NotASnapshot)?;
        let (version, body) = rest.split_first().ok_or(SnapshotError::Truncated)?;
        if *version != SNAPSHOT_VERSION {
            return Err(SnapshotError::UnknownVersion(*version));
        }
        bincode::deserialize_limited_exact::<Self, MAX_PEER_PAYLOAD_BYTES>(body)
            .map_err(|error| SnapshotError::Malformed(error.to_string()))
    }

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

#[derive(Debug, Clone)]
pub struct VerifiedSnapshot {
    pub manifest: SnapshotManifestV1,
    pub payload: SnapshotPayload,
}

fn write_atomically(path: &Path, bytes: &[u8]) -> Result<(), SnapshotError> {
    use std::io::Write;
    let temporary = path.with_extension("tmp");
    let io = |error: std::io::Error| SnapshotError::Io(error.to_string());
    let mut file = std::fs::File::create(&temporary).map_err(io)?;
    file.write_all(bytes).map_err(io)?;
    file.sync_all().map_err(io)?;
    drop(file);
    #[cfg(windows)]
    arc_crypto::secret_file::windows_move_path_write_through(&temporary, path, true).map_err(io)?;
    #[cfg(not(windows))]
    {
        std::fs::rename(&temporary, path).map_err(io)?;
        arc_crypto::secret_file::sync_parent_directory(path).map_err(io)?;
    }
    Ok(())
}

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

pub fn remove(dir: &Path) -> Result<(), SnapshotError> {
    let io = |error: std::io::Error| SnapshotError::Io(error.to_string());
    for name in [MANIFEST_FILE, PAYLOAD_FILE] {
        match std::fs::remove_file(dir.join(name)) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(io(error)),
        }
    }
    let _ = arc_crypto::secret_file::sync_parent_directory(&dir.join(PAYLOAD_FILE));
    Ok(())
}

pub fn directory_for(wal_dir: &Path) -> PathBuf {
    wal_dir.to_path_buf()
}

pub fn storage_rows(rows: BTreeMap<Address, BTreeMap<Hash256, Vec<u8>>>) -> StorageRows {
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
        let mut first = SnapshotPayload {
            accounts: vec![
                (hash_bytes(b"z"), Account::new(hash_bytes(b"z"), 1)),
                (hash_bytes(b"a"), Account::new(hash_bytes(b"a"), 2)),
            ],
            ..SnapshotPayload::default()
        };
        let mut second = SnapshotPayload {
            accounts: vec![
                (hash_bytes(b"a"), Account::new(hash_bytes(b"a"), 2)),
                (hash_bytes(b"z"), Account::new(hash_bytes(b"z"), 1)),
            ],
            ..SnapshotPayload::default()
        };
        first.canonicalize();
        second.canonicalize();
        assert_eq!(first.encode().1, second.encode().1);
    }

    #[test]
    fn a_corrupt_payload_is_refused_rather_than_decoded() {
        let dir = tempfile::tempdir().unwrap();
        publish(
            dir.path(),
            payload_at(5),
            hash_bytes(b"r"),
            1,
            hash_bytes(b"b"),
        )
        .unwrap();
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
        publish(
            dir.path(),
            payload_at(5),
            hash_bytes(b"r"),
            1,
            hash_bytes(b"b"),
        )
        .unwrap();
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
        let dir = tempfile::tempdir().unwrap();
        publish(
            dir.path(),
            payload_at(5),
            hash_bytes(b"r"),
            1,
            hash_bytes(b"b"),
        )
        .unwrap();
        std::fs::remove_file(dir.path().join(MANIFEST_FILE)).unwrap();
        assert!(matches!(load(dir.path()), Err(SnapshotError::Absent)));
    }

    #[test]
    fn a_manifest_without_its_payload_is_not_a_snapshot() {
        let dir = tempfile::tempdir().unwrap();
        publish(
            dir.path(),
            payload_at(5),
            hash_bytes(b"r"),
            1,
            hash_bytes(b"b"),
        )
        .unwrap();
        std::fs::remove_file(dir.path().join(PAYLOAD_FILE)).unwrap();
        assert!(matches!(load(dir.path()), Err(SnapshotError::Absent)));
    }

    #[test]
    fn a_snapshot_from_a_newer_build_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        publish(
            dir.path(),
            payload_at(5),
            hash_bytes(b"r"),
            1,
            hash_bytes(b"b"),
        )
        .unwrap();
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
        publish(
            dir.path(),
            payload_at(5),
            hash_bytes(b"r1"),
            1,
            hash_bytes(b"b1"),
        )
        .unwrap();
        let second = publish(
            dir.path(),
            payload_at(50),
            hash_bytes(b"r2"),
            500,
            hash_bytes(b"b2"),
        )
        .unwrap();
        let loaded = load(dir.path()).unwrap();
        assert_eq!(loaded.manifest, second);
        assert_eq!(loaded.payload.height, 50);
        assert!(!dir.path().join("state-snapshot.tmp").exists());
    }

    #[test]
    fn removing_a_snapshot_leaves_nothing_loadable() {
        let dir = tempfile::tempdir().unwrap();
        publish(
            dir.path(),
            payload_at(5),
            hash_bytes(b"r"),
            1,
            hash_bytes(b"b"),
        )
        .unwrap();
        remove(dir.path()).unwrap();
        assert!(matches!(load(dir.path()), Err(SnapshotError::Absent)));
        remove(dir.path()).expect("removing twice is not an error");
    }
}
