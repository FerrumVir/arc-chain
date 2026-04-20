use crate::types::{Identity, NodeConfig};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

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
        match std::fs::read_to_string(Self::file(dir)) {
            Ok(s) => serde_json::from_str(&s).unwrap_or_default(),
            Err(_) => Self::default(),
        }
    }

    pub fn save_to(&self, dir: &Path) -> anyhow::Result<()> {
        std::fs::create_dir_all(dir)?;
        std::fs::write(Self::file(dir), serde_json::to_vec_pretty(self)?)?;
        Ok(())
    }
}
