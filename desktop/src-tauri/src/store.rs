use crate::types::{DataMigrationNotice, Identity, NodeConfig};
use serde::{Deserialize, Serialize};
use std::ffi::OsString;
use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use zeroize::Zeroizing;

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

const MAX_STORE_BYTES: u64 = 1024 * 1024;
static NEXT_STORE_SIDECAR: AtomicU64 = AtomicU64::new(0);

// Previously used the `directories` crate to resolve the app-data dir, but
// that crate returns a read-only path on Android (hits `Read-only file
// system (os error 30)` when save() tries to write). The canonical fix is
// to use Tauri's PathResolver which returns the per-platform writable dir:
//   macOS  → ~/Library/Application Support/network.arc.desktop
//   Linux  → ~/.local/share/network.arc.desktop
//   Windows→ %APPDATA%/network.arc.desktop
//   Android→ /data/user/0/network.arc.desktop/files
//   iOS    → <app sandbox>/Documents
// The resolved path is supplied by lib.rs `setup()` and stored in AppState.

#[derive(Default, Serialize, Deserialize)]
pub struct Store {
    pub identity: Option<Identity>,
    pub config: Option<NodeConfig>,
    #[serde(default)]
    pub data_migration_notice: Option<DataMigrationNotice>,
}

impl Store {
    fn file(dir: &Path) -> PathBuf {
        dir.join("store.json")
    }

    pub fn load_from(dir: &Path) -> Self {
        if let Err(error) = secure_store_directory(dir) {
            tracing::error!(
                path = %dir.display(),
                %error,
                "refusing to load identity store from an insecure app-data directory"
            );
            return Self::default();
        }
        let path = Self::file(dir);

        if let Some(store) = read_valid_store(&path) {
            return store;
        }

        // Windows replacement uses a complete `.previous` rollback file, and
        // every platform writes a fully fsynced `.store.json.<pid>.tmp` before
        // replacement. A power loss between those renames must not make the
        // wallet identity and node configuration appear erased. Prefer the
        // newest valid synced candidate, then rewrite a canonical store.json.
        let mut candidates = Vec::new();
        if let Ok(entries) = fs::read_dir(dir) {
            for entry in entries.flatten() {
                let name = entry.file_name();
                let name = name.to_string_lossy();
                if name.starts_with(".store.json.") && name.ends_with(".tmp") {
                    candidates.push(entry.path());
                }
            }
        }
        candidates.push(path.with_extension("json.previous"));
        candidates.sort_by_key(|candidate| {
            let modified = fs::metadata(candidate)
                .and_then(|metadata| metadata.modified())
                .unwrap_or(std::time::SystemTime::UNIX_EPOCH);
            let is_synced_new_file = candidate
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with(".store.json.") && name.ends_with(".tmp"));
            (modified, is_synced_new_file)
        });
        candidates.reverse();

        for candidate in candidates {
            let Some(store) = read_valid_store(&candidate) else {
                continue;
            };
            tracing::warn!(
                source = %candidate.display(),
                destination = %path.display(),
                "recovered ARC identity store after an interrupted atomic replacement"
            );
            match store.save_to(dir) {
                Ok(()) => {
                    // A PID-named temp is not the destination moved by
                    // save_to(), so remove the recovered stale copy only after
                    // the canonical replacement is durable.
                    let _ = fs::remove_file(&candidate);
                }
                Err(error) => {
                    // The in-memory recovery is still safer than silently
                    // returning an empty identity. Auto-start migration
                    // separately fails closed if it cannot persist its new
                    // data-dir pointer. Keep the candidate for the next retry.
                    tracing::error!(
                        %error,
                        "recovered ARC identity store in memory but could not restore store.json"
                    );
                }
            }
            return store;
        }

        Self::default()
    }

    pub fn save_to(&self, dir: &Path) -> anyhow::Result<()> {
        secure_store_directory(dir)?;

        let destination = Self::file(dir);
        match arc_crypto::secret_file::open_private_owned_migration(&destination) {
            Ok(file) => drop(file),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                anyhow::bail!(
                    "existing identity store failed private owner/permission validation {}: {error}",
                    destination.display()
                );
            }
        }

        // The store contains the recovery phrase. Never expose a partially
        // written JSON document after a crash, and never rely on the caller's
        // umask for its confidentiality.
        let (temporary, mut file) = create_store_sidecar(dir)?;
        let mut pending = PendingStoreSidecar(Some(temporary.clone()));
        let encoded = Zeroizing::new(serde_json::to_vec_pretty(self)?);
        file.write_all(&encoded)?;
        file.sync_all()?;
        drop(file);

        replace_atomically(&temporary, &destination)?;
        pending.0 = None;
        drop(arc_crypto::secret_file::open_private(&destination)?);
        arc_crypto::secret_file::sync_parent_directory(&destination)?;
        Ok(())
    }

    /// Fence an unbound v0.7 WAL before a newly updated v0.8 desktop can
    /// auto-start its matched protocol-v3 node.
    ///
    /// Older desktop builds used the same `~/.arc` root for executables,
    /// models, and chain state. Renaming that directory would therefore move
    /// things that are not chain history; rewriting/deleting its WAL would
    /// destroy forensic evidence. Instead, retain every old byte in place and
    /// atomically persist only a config pointer to a new, empty `data-v3*`
    /// child. A later launch sees that fresh path and does not migrate again.
    pub fn protect_legacy_v07_data(&mut self) -> anyhow::Result<Option<DataMigrationNotice>> {
        let Some(config) = self.config.as_ref() else {
            return Ok(None);
        };
        let legacy_dir = crate::paths::expand_tilde(&config.data_dir);
        self.protect_legacy_v07_data_at(&legacy_dir)
    }

    /// Fence a live v0.7 process's exact data path. Public Windows tags could
    /// resolve `~/.arc` relative to the child cwd when HOME was absent, so the
    /// persisted config string alone is not always the directory being used.
    pub fn protect_legacy_v07_data_at(
        &mut self,
        legacy_dir: &Path,
    ) -> anyhow::Result<Option<DataMigrationNotice>> {
        self.protect_legacy_v07_data_at_inner(legacy_dir, false)
    }

    /// Fence the exact data directory advertised by a currently live v0.7
    /// desktop process. Even an empty directory is unsafe to reuse: the old
    /// process may still be initializing and can create/write its WAL after
    /// this preflight. The caller has already proven the strict public-tag
    /// argv/seed/executable identity and will reconcile that process only
    /// after this new pointer is durably persisted.
    pub fn protect_running_legacy_v07_data_at(
        &mut self,
        legacy_dir: &Path,
    ) -> anyhow::Result<Option<DataMigrationNotice>> {
        self.protect_legacy_v07_data_at_inner(legacy_dir, true)
    }

    fn protect_legacy_v07_data_at_inner(
        &mut self,
        legacy_dir: &Path,
        exact_legacy_process_is_live: bool,
    ) -> anyhow::Result<Option<DataMigrationNotice>> {
        let Some(config) = self.config.as_mut() else {
            return Ok(None);
        };
        let legacy_dir = legacy_dir.to_path_buf();
        match fs::symlink_metadata(&legacy_dir) {
            Ok(metadata) if metadata.file_type().is_symlink() => anyhow::bail!(
                "refusing protocol-v3 migration through symbolic-link data directory {}",
                legacy_dir.display()
            ),
            Ok(metadata) if !metadata.file_type().is_dir() => anyhow::bail!(
                "refusing protocol-v3 migration because data path {} is not a directory",
                legacy_dir.display()
            ),
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        }
        let binding = legacy_dir.join("genesis.network-hash");
        let state_wal = legacy_dir.join("state.wal");
        let dag_wal = legacy_dir.join("dag-wal");

        // A syntactically authenticated marker means this is protocol-v3
        // state. Its exact hash remains arc-node's authority; the desktop must
        // not guess whether a different valid hash belongs to this network.
        // A malformed regular marker is different: arc-node can never replay
        // it safely. Preserve that entire ambiguous directory and explicitly
        // fence it behind a fresh pointer instead of allowing an opaque startup
        // failure or editing the marker in place.
        let malformed_binding_reason = match fs::symlink_metadata(&binding) {
            Ok(metadata) if metadata.file_type().is_file() => {
                let bytes = if metadata.len() <= 1024 {
                    fs::read(&binding)?
                } else {
                    Vec::new()
                };
                let valid = std::str::from_utf8(&bytes).ok().is_some_and(|value| {
                    let value = value.trim();
                    value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
                });
                if valid && !exact_legacy_process_is_live {
                    return Ok(None);
                }
                Some(if valid {
                    "an exact live pre-v0.8 desktop node was using this chain directory; original bytes were preserved and never replayed by protocol v3"
                        .to_string()
                } else {
                    "malformed genesis.network-hash made the existing chain directory ambiguous; original bytes were preserved and never replayed"
                        .to_string()
                })
            }
            Ok(_) => anyhow::bail!(
                "refusing protocol-v3 migration because {} is not a regular file",
                binding.display()
            ),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => return Err(error.into()),
        };

        // v0.7 always persisted state.wal. Retain dag-wal as a second
        // discriminator for interrupted/partial old nodes without treating
        // ~/.arc/bin or ~/.arc/models as chain history.
        if !exact_legacy_process_is_live
            && malformed_binding_reason.is_none()
            && !state_wal.exists()
            && !dag_wal.exists()
        {
            return Ok(None);
        }

        let mut active_dir = None;
        for attempt in 0..100u8 {
            let name = if attempt == 0 {
                "data-v3".to_string()
            } else {
                format!("data-v3-{attempt}")
            };
            let candidate = legacy_dir.join(name);
            match fs::symlink_metadata(&candidate) {
                Ok(metadata) if metadata.file_type().is_dir() => {
                    if fs::read_dir(&candidate)?.next().is_none() {
                        active_dir = Some(candidate);
                        break;
                    }
                }
                Ok(_) => continue,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    fs::create_dir(&candidate)?;
                    #[cfg(unix)]
                    fs::set_permissions(&candidate, fs::Permissions::from_mode(0o700))?;
                    active_dir = Some(candidate);
                    break;
                }
                Err(error) => return Err(error.into()),
            }
        }
        let active_dir = active_dir.ok_or_else(|| {
            anyhow::anyhow!(
                "could not allocate an empty protocol-v3 data directory beneath {}",
                legacy_dir.display()
            )
        })?;
        let notice = DataMigrationNotice {
            legacy_data_dir: legacy_dir.to_string_lossy().into_owned(),
            active_data_dir: active_dir.to_string_lossy().into_owned(),
            migrated_at: chrono::Utc::now().timestamp_millis(),
            reason: malformed_binding_reason.unwrap_or_else(|| {
                if exact_legacy_process_is_live {
                    "an exact live pre-v0.8 desktop node could still create or mutate its unbound WAL; its original directory was preserved before reconciliation"
                        .to_string()
                } else {
                    "pre-v3 WAL had no authenticated genesis.network-hash binding".to_string()
                }
            }),
        };
        config.data_dir = notice.active_data_dir.clone();
        self.data_migration_notice = Some(notice.clone());
        Ok(Some(notice))
    }
}

fn secure_store_directory(dir: &Path) -> anyhow::Result<()> {
    match arc_crypto::secret_file::secure_private_directory(dir) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let parent = dir.parent().ok_or_else(|| {
                anyhow::anyhow!("identity store directory has no parent: {}", dir.display())
            })?;
            // Tauri supplies an OS-owned per-user app-data root. Parents may
            // not exist on a first launch, but no secret bytes are written
            // until the final app directory is securely created and validated.
            fs::create_dir_all(parent)?;
            arc_crypto::secret_file::secure_private_directory(dir)?;
            Ok(())
        }
        Err(error) => Err(error.into()),
    }
}

struct PendingStoreSidecar(Option<PathBuf>);

impl Drop for PendingStoreSidecar {
    fn drop(&mut self) {
        if let Some(path) = self.0.as_ref() {
            let _ = fs::remove_file(path);
        }
    }
}

fn create_store_sidecar(dir: &Path) -> anyhow::Result<(PathBuf, File)> {
    for _ in 0..64 {
        let sequence = NEXT_STORE_SIDECAR.fetch_add(1, Ordering::Relaxed);
        let mut name = OsString::from(".store.json.");
        name.push(format!("{}-{sequence}.tmp", std::process::id()));
        let path = dir.join(name);
        match arc_crypto::secret_file::create_new_private(&path) {
            Ok(file) => return Ok((path, file)),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error.into()),
        }
    }
    anyhow::bail!("could not allocate a unique private store sidecar")
}

fn read_valid_store(path: &Path) -> Option<Store> {
    let mut file = arc_crypto::secret_file::open_private_owned_migration(path).ok()?;
    let metadata = file.metadata().ok()?;
    if metadata.len() > MAX_STORE_BYTES {
        return None;
    }
    let mut bytes = Zeroizing::new(Vec::with_capacity(metadata.len() as usize));
    Read::by_ref(&mut file)
        .take(MAX_STORE_BYTES + 1)
        .read_to_end(&mut bytes)
        .ok()?;
    if bytes.len() as u64 > MAX_STORE_BYTES {
        return None;
    }
    serde_json::from_slice(&bytes).ok()
}

fn replace_atomically(temporary: &Path, destination: &Path) -> anyhow::Result<()> {
    #[cfg(unix)]
    {
        fs::rename(temporary, destination)?;
        Ok(())
    }

    #[cfg(not(unix))]
    {
        // std::fs::rename cannot replace an existing Windows destination.
        // Keep the old complete file as a rollback point until the new one is
        // in place, then remove it.
        let backup = destination.with_extension("json.previous");
        match arc_crypto::secret_file::open_private_owned_migration(&backup) {
            Ok(file) => {
                drop(file);
                fs::remove_file(&backup)?;
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        if destination.exists() {
            fs::rename(destination, &backup)?;
        }
        if let Err(error) = fs::rename(temporary, destination) {
            if backup.exists() {
                let _ = fs::rename(&backup, destination);
            }
            return Err(error.into());
        }
        if backup.exists() {
            fs::remove_file(backup)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_dir() -> PathBuf {
        std::env::temp_dir().join(format!(
            "arc-desktop-store-{}-{}",
            std::process::id(),
            rand::random::<u64>()
        ))
    }

    fn write_private_fixture(path: &Path, bytes: &[u8]) {
        let mut file = arc_crypto::secret_file::create_new_private(path).unwrap();
        file.write_all(bytes).unwrap();
        file.sync_all().unwrap();
    }

    #[test]
    fn store_round_trip_is_complete_and_private() {
        let dir = test_dir();
        let store = Store {
            identity: Some(Identity {
                address: "11".repeat(32),
                public_key: format!("0x{}", "22".repeat(32)),
                seed_phrase: "test recovery phrase stays private".to_string(),
                created_at: 1,
            }),
            config: Some(NodeConfig::default()),
            data_migration_notice: None,
        };
        store.save_to(&dir).expect("save private store");
        let loaded = Store::load_from(&dir);
        assert_eq!(
            loaded
                .identity
                .as_ref()
                .map(|identity| identity.address.as_str()),
            store
                .identity
                .as_ref()
                .map(|identity| identity.address.as_str())
        );
        assert!(!dir
            .join(format!(".store.json.{}.tmp", std::process::id()))
            .exists());

        #[cfg(unix)]
        {
            let dir_mode = fs::metadata(&dir).unwrap().permissions().mode() & 0o777;
            let file_mode = fs::metadata(Store::file(&dir))
                .unwrap()
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(dir_mode, 0o700);
            assert_eq!(file_mode, 0o600);
        }
        fs::remove_dir_all(dir).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn v07_public_modes_are_tightened_without_losing_identity() {
        let dir = test_dir();
        fs::create_dir_all(&dir).unwrap();
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o755)).unwrap();
        let legacy = Store {
            identity: Some(Identity {
                address: "33".repeat(32),
                public_key: format!("0x{}", "44".repeat(32)),
                seed_phrase: "actual v07 recovery phrase remains available".into(),
                created_at: 7,
            }),
            config: Some(NodeConfig::default()),
            data_migration_notice: None,
        };
        let path = Store::file(&dir);
        fs::write(&path, serde_json::to_vec_pretty(&legacy).unwrap()).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();

        let loaded = Store::load_from(&dir);
        assert_eq!(
            loaded
                .identity
                .as_ref()
                .map(|identity| identity.address.as_str()),
            legacy
                .identity
                .as_ref()
                .map(|identity| identity.address.as_str())
        );
        assert_eq!(
            fs::metadata(&dir).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn interrupted_replacement_recovers_the_newest_synced_store() {
        let dir = test_dir();
        secure_store_directory(&dir).unwrap();

        let old = Store {
            identity: Some(Identity {
                address: "55".repeat(32),
                public_key: format!("0x{}", "66".repeat(32)),
                seed_phrase: "old but valid rollback identity".to_string(),
                created_at: 1,
            }),
            config: Some(NodeConfig::default()),
            data_migration_notice: None,
        };
        let new_config = NodeConfig {
            data_dir: dir.join("data-v3").to_string_lossy().into_owned(),
            ..NodeConfig::default()
        };
        let current = Store {
            identity: Some(Identity {
                address: "77".repeat(32),
                public_key: format!("0x{}", "88".repeat(32)),
                seed_phrase: "new synced identity survives interrupted rename".to_string(),
                created_at: 2,
            }),
            config: Some(new_config),
            data_migration_notice: None,
        };

        write_private_fixture(
            &dir.join("store.json.previous"),
            &serde_json::to_vec_pretty(&old).unwrap(),
        );
        let synced_temp = dir.join(".store.json.4242.tmp");
        write_private_fixture(&synced_temp, &serde_json::to_vec_pretty(&current).unwrap());

        let recovered = Store::load_from(&dir);
        assert_eq!(
            recovered.identity.as_ref().unwrap().address,
            current.identity.as_ref().unwrap().address,
            "the synced new store wins over its older rollback"
        );
        assert_eq!(
            Store::load_from(&dir).identity.as_ref().unwrap().address,
            current.identity.as_ref().unwrap().address,
            "recovery is durably restored to canonical store.json"
        );
        assert!(!synced_temp.exists());
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn interrupted_replacement_falls_back_to_complete_previous_store() {
        let dir = test_dir();
        secure_store_directory(&dir).unwrap();
        let rollback = Store {
            identity: Some(Identity {
                address: "99".repeat(32),
                public_key: format!("0x{}", "aa".repeat(32)),
                seed_phrase: "rollback keeps the wallet recoverable".to_string(),
                created_at: 3,
            }),
            config: Some(NodeConfig::default()),
            data_migration_notice: None,
        };
        write_private_fixture(
            &dir.join("store.json.previous"),
            &serde_json::to_vec_pretty(&rollback).unwrap(),
        );
        write_private_fixture(&dir.join(".store.json.9999.tmp"), b"{partial");

        let recovered = Store::load_from(&dir);
        assert_eq!(
            recovered.identity.as_ref().unwrap().address,
            rollback.identity.as_ref().unwrap().address
        );
        assert!(Store::file(&dir).is_file());
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn legacy_v07_wal_migrates_once_and_relaunch_uses_fresh_v3_dir() {
        let app_dir = test_dir();
        let legacy_dir = app_dir.join("legacy-v07");
        let model = app_dir.join("models").join("worker.gguf");
        fs::create_dir_all(legacy_dir.join("dag-wal")).unwrap();
        fs::create_dir_all(model.parent().unwrap()).unwrap();
        let legacy_state = b"v0.7 state WAL and block history must remain byte-identical";
        let legacy_dag = b"v0.7 DAG segment must remain byte-identical";
        fs::write(legacy_dir.join("state.wal"), legacy_state).unwrap();
        fs::write(legacy_dir.join("dag-wal").join("segment-0.wal"), legacy_dag).unwrap();
        fs::write(&model, b"model bytes are not chain data").unwrap();

        let config = NodeConfig {
            data_dir: legacy_dir.to_string_lossy().into_owned(),
            model_path: Some(model.to_string_lossy().into_owned()),
            ..NodeConfig::default()
        };
        let original_model = config.model_path.clone();
        let identity = Identity {
            address: "33".repeat(32),
            public_key: format!("0x{}", "44".repeat(32)),
            seed_phrase: "fixture identity must survive an updater relaunch".to_string(),
            created_at: 7,
        };
        let original_address = identity.address.clone();
        let store = Store {
            identity: Some(identity),
            config: Some(config),
            data_migration_notice: None,
        };
        store.save_to(&app_dir).unwrap();

        // First v0.8 launch after the updater: move only the active pointer.
        let mut upgraded = Store::load_from(&app_dir);
        let notice = upgraded
            .protect_legacy_v07_data()
            .unwrap()
            .expect("unbound v0.7 WAL must be fenced");
        upgraded.save_to(&app_dir).unwrap();
        assert_eq!(notice.legacy_data_dir, legacy_dir.to_string_lossy());
        assert!(notice.active_data_dir.ends_with("data-v3"));
        assert!(Path::new(&notice.active_data_dir).is_dir());
        assert_eq!(
            fs::read(legacy_dir.join("state.wal")).unwrap(),
            legacy_state
        );
        assert_eq!(
            fs::read(legacy_dir.join("dag-wal").join("segment-0.wal")).unwrap(),
            legacy_dag
        );
        assert_eq!(fs::read(&model).unwrap(), b"model bytes are not chain data");
        assert_eq!(
            upgraded.config.as_ref().unwrap().model_path,
            original_model,
            "model selection is configuration, not chain state"
        );
        assert_eq!(
            upgraded.identity.as_ref().unwrap().address,
            original_address,
            "wallet/node identity must survive migration"
        );

        // Relaunch: the persisted pointer is already fresh, so no second
        // archive directory is allocated and the notice remains available.
        let mut relaunched = Store::load_from(&app_dir);
        let active = relaunched.config.as_ref().unwrap().data_dir.clone();
        assert!(relaunched.protect_legacy_v07_data().unwrap().is_none());
        assert_eq!(relaunched.config.as_ref().unwrap().data_dir, active);
        assert_eq!(relaunched.data_migration_notice, Some(notice));
        assert!(!legacy_dir.join("data-v3-1").exists());

        fs::remove_dir_all(app_dir).unwrap();
    }

    #[test]
    fn exact_live_v07_process_is_fenced_even_before_its_first_wal_write() {
        let app_dir = test_dir();
        let legacy_dir = app_dir.join("live-v07-still-initializing");
        fs::create_dir_all(&legacy_dir).unwrap();
        let mut store = Store {
            identity: None,
            config: Some(NodeConfig {
                data_dir: legacy_dir.to_string_lossy().into_owned(),
                ..NodeConfig::default()
            }),
            data_migration_notice: None,
        };

        assert!(
            store
                .protect_legacy_v07_data_at(&legacy_dir)
                .unwrap()
                .is_none(),
            "an empty inactive directory is normally safe"
        );
        let notice = store
            .protect_running_legacy_v07_data_at(&legacy_dir)
            .unwrap()
            .expect("a proven live v0.7 process can write its first WAL after preflight");
        assert_eq!(notice.legacy_data_dir, legacy_dir.to_string_lossy());
        assert!(Path::new(&notice.active_data_dir).is_dir());
        assert_ne!(notice.active_data_dir, notice.legacy_data_dir);
        assert!(notice.reason.contains("exact live pre-v0.8"));
        fs::remove_dir_all(app_dir).unwrap();
    }

    #[test]
    fn malformed_genesis_binding_is_explicitly_fenced_without_touching_history() {
        let app_dir = test_dir();
        let ambiguous_dir = app_dir.join("ambiguous-v3");
        fs::create_dir_all(&ambiguous_dir).unwrap();
        let wal = b"block history behind a malformed binding must stay byte-identical";
        let malformed_binding = b"truncated-not-a-network-hash\n";
        fs::write(ambiguous_dir.join("state.wal"), wal).unwrap();
        fs::write(
            ambiguous_dir.join("genesis.network-hash"),
            malformed_binding,
        )
        .unwrap();

        let mut store = Store {
            identity: None,
            config: Some(NodeConfig {
                data_dir: ambiguous_dir.to_string_lossy().into_owned(),
                ..NodeConfig::default()
            }),
            data_migration_notice: None,
        };
        let notice = store
            .protect_legacy_v07_data()
            .unwrap()
            .expect("malformed binding must be fenced to a recoverable fresh directory");

        assert!(notice.reason.contains("malformed genesis.network-hash"));
        assert_eq!(notice.legacy_data_dir, ambiguous_dir.to_string_lossy());
        assert_ne!(notice.active_data_dir, notice.legacy_data_dir);
        assert!(Path::new(&notice.active_data_dir).is_dir());
        assert_eq!(fs::read(ambiguous_dir.join("state.wal")).unwrap(), wal);
        assert_eq!(
            fs::read(ambiguous_dir.join("genesis.network-hash")).unwrap(),
            malformed_binding
        );
        assert_eq!(
            store.config.as_ref().unwrap().data_dir,
            notice.active_data_dir,
            "the node must never auto-start against the ambiguous source"
        );

        store.save_to(&app_dir).unwrap();
        let mut relaunched = Store::load_from(&app_dir);
        assert!(relaunched.protect_legacy_v07_data().unwrap().is_none());
        assert_eq!(relaunched.data_migration_notice, Some(notice));
        fs::remove_dir_all(app_dir).unwrap();
    }

    #[test]
    fn authenticated_v3_wal_is_never_repointed() {
        let app_dir = test_dir();
        let data_dir = app_dir.join("already-v3");
        fs::create_dir_all(&data_dir).unwrap();
        fs::write(data_dir.join("state.wal"), b"v3 state").unwrap();
        fs::write(data_dir.join("genesis.network-hash"), "11".repeat(32)).unwrap();
        let config = NodeConfig {
            data_dir: data_dir.to_string_lossy().into_owned(),
            ..NodeConfig::default()
        };
        let mut store = Store {
            identity: None,
            config: Some(config),
            data_migration_notice: None,
        };
        assert!(store.protect_legacy_v07_data().unwrap().is_none());
        assert_eq!(
            store.config.as_ref().unwrap().data_dir,
            data_dir.to_string_lossy()
        );
        fs::remove_dir_all(app_dir).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_legacy_data_directory_is_never_migrated_or_modified() {
        use std::os::unix::fs::symlink;

        let app_dir = test_dir();
        let actual = app_dir.join("actual-legacy-data");
        let configured = app_dir.join("configured-data");
        fs::create_dir_all(&actual).unwrap();
        let wal = b"history behind a symlink must remain untouched";
        fs::write(actual.join("state.wal"), wal).unwrap();
        symlink(&actual, &configured).unwrap();
        let mut store = Store {
            identity: None,
            config: Some(NodeConfig {
                data_dir: configured.to_string_lossy().into_owned(),
                ..NodeConfig::default()
            }),
            data_migration_notice: None,
        };

        let error = store
            .protect_legacy_v07_data()
            .expect_err("a symlinked legacy root must remain fail-closed");
        assert!(error.to_string().contains("symbolic-link data directory"));
        assert_eq!(fs::read(actual.join("state.wal")).unwrap(), wal);
        assert!(!actual.join("data-v3").exists());
        assert_eq!(
            store.config.as_ref().unwrap().data_dir,
            configured.to_string_lossy()
        );
        fs::remove_dir_all(app_dir).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn store_refuses_symbolic_link_destination() {
        use std::os::unix::fs::symlink;

        let dir = test_dir();
        secure_store_directory(&dir).unwrap();
        let target = dir.join("attacker-controlled.json");
        fs::write(&target, b"do not overwrite").unwrap();
        symlink(&target, Store::file(&dir)).unwrap();
        assert!(Store::default().save_to(&dir).is_err());
        assert_eq!(fs::read(&target).unwrap(), b"do not overwrite");
        fs::remove_dir_all(dir).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn store_refuses_symbolic_link_app_data_directory() {
        use std::os::unix::fs::symlink;

        let root = test_dir();
        fs::create_dir(&root).unwrap();
        let actual = root.join("actual");
        arc_crypto::secret_file::create_new_private_directory(&actual).unwrap();
        let configured = root.join("configured");
        symlink(&actual, &configured).unwrap();

        assert!(Store::default().save_to(&configured).is_err());
        assert!(!Store::file(&actual).exists());
        fs::remove_dir_all(root).unwrap();
    }
}
