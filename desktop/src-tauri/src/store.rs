use crate::types::{Identity, NodeConfig};
use serde::{Deserialize, Serialize};
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

#[cfg(unix)]
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

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
}

impl Store {
    fn file(dir: &Path) -> PathBuf {
        dir.join("store.json")
    }

    pub fn load_from(dir: &Path) -> Self {
        let path = Self::file(dir);
        if fs::symlink_metadata(&path).is_ok_and(|metadata| metadata.file_type().is_symlink()) {
            tracing::error!(path = %path.display(), "refusing to read identity store through a symbolic link");
            return Self::default();
        }
        match fs::read_to_string(&path) {
            Ok(s) => serde_json::from_str(&s).unwrap_or_default(),
            Err(_) => Self::default(),
        }
    }

    pub fn save_to(&self, dir: &Path) -> anyhow::Result<()> {
        fs::create_dir_all(dir)?;
        #[cfg(unix)]
        fs::set_permissions(dir, fs::Permissions::from_mode(0o700))?;

        let destination = Self::file(dir);
        if fs::symlink_metadata(&destination)
            .is_ok_and(|metadata| metadata.file_type().is_symlink())
        {
            anyhow::bail!(
                "refusing to write identity store through symbolic link {}",
                destination.display()
            );
        }

        // The store contains the recovery phrase. Never expose a partially
        // written JSON document after a crash, and never rely on the caller's
        // umask for its confidentiality.
        let temporary = dir.join(format!(".store.json.{}.tmp", std::process::id()));
        if temporary.exists() {
            fs::remove_file(&temporary)?;
        }
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        options.mode(0o600);
        let mut file = options.open(&temporary)?;
        file.write_all(&serde_json::to_vec_pretty(self)?)?;
        file.sync_all()?;
        drop(file);

        replace_atomically(&temporary, &destination)?;
        #[cfg(unix)]
        fs::set_permissions(&destination, fs::Permissions::from_mode(0o600))?;
        if let Ok(parent) = File::open(dir) {
            let _ = parent.sync_all();
        }
        Ok(())
    }
}

fn replace_atomically(temporary: &Path, destination: &Path) -> anyhow::Result<()> {
    #[cfg(unix)]
    {
        fs::rename(temporary, destination)?;
        return Ok(());
    }

    #[cfg(not(unix))]
    {
        // std::fs::rename cannot replace an existing Windows destination.
        // Keep the old complete file as a rollback point until the new one is
        // in place, then remove it.
        let backup = destination.with_extension("json.previous");
        if backup.exists() {
            fs::remove_file(&backup)?;
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
    fn store_refuses_symbolic_link_destination() {
        use std::os::unix::fs::symlink;

        let dir = test_dir();
        fs::create_dir_all(&dir).unwrap();
        let target = dir.join("attacker-controlled.json");
        fs::write(&target, b"do not overwrite").unwrap();
        symlink(&target, Store::file(&dir)).unwrap();
        assert!(Store::default().save_to(&dir).is_err());
        assert_eq!(fs::read(&target).unwrap(), b"do not overwrite");
        fs::remove_dir_all(dir).unwrap();
    }
}
