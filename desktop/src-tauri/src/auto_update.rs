//! Native ownership of app-originated work while an unattended update drains.
//! These guards live in AppState, not an IPC future: cancellation cannot reopen
//! signing while a prepared installer transaction still owns the node.

use std::path::Path;
use std::sync::Arc;
use tokio::sync::{Mutex, OwnedMutexGuard};

pub struct RequestFence {
    _community: OwnedMutexGuard<()>,
    _wallet: OwnedMutexGuard<()>,
}

impl RequestFence {
    pub fn try_acquire(community: &Arc<Mutex<()>>, wallet: &Arc<Mutex<()>>) -> Option<Self> {
        let community = community.clone().try_lock_owned().ok()?;
        let wallet = wallet.clone().try_lock_owned().ok()?;
        Some(Self {
            _community: community,
            _wallet: wallet,
        })
    }
}

#[derive(Debug, PartialEq, Eq, serde::Serialize)]
#[serde(tag = "status", rename_all = "lowercase")]
pub enum PrepareResult {
    Prepared,
    Busy {
        reason: String,
        #[serde(rename = "activeJobs")]
        active_jobs: u64,
    },
}

impl PrepareResult {
    pub fn busy(reason: impl Into<String>, active_jobs: u64) -> Self {
        Self::Busy {
            reason: reason.into(),
            active_jobs,
        }
    }
}

/// Records that this app-data directory has had the v0.8.14 unattended-install
/// default. It is a file beside store.json, not a field in it: v0.8.13
/// rewrites store.json without fields it does not know, so a marker inside
/// would vanish on a downgrade, and the re-upgrade would turn an "off" chosen
/// in between back on.
const UNATTENDED_INSTALL_DEFAULT_MARKER: &str = "unattended-install-default.v1";

/// Give a store written before v0.8.14 the unattended-install default, once
/// per app-data directory. Through v0.8.13 installs waited for a click, and a
/// desktop that never got one stayed on a release with a known fix.
/// Afterwards the Settings switch is the user's alone. The marker is written
/// only after the store is durable, so a failed save applies the default
/// again on the next launch. Returns whether the stored config changed.
pub fn apply_unattended_install_default(
    store: &mut crate::store::Store,
    store_dir: &Path,
) -> anyhow::Result<bool> {
    let marker = store_dir.join(UNATTENDED_INSTALL_DEFAULT_MARKER);
    if marker.exists() {
        return Ok(false);
    }
    let changed = match store.config.as_mut() {
        Some(config) if !config.auto_install_updates => {
            config.auto_install_updates = true;
            true
        }
        _ => false,
    };
    if changed {
        store.save_to(store_dir)?;
    } else {
        // A first launch has no store directory yet. Recording the marker
        // now keeps a later "off" from being undone on the next launch.
        crate::store::secure_store_directory(store_dir)?;
    }
    arc_crypto::secret_file::durably_publish_new_private(
        &marker,
        b"arc.desktop.unattended-install-default.v1\n",
    )?;
    Ok(changed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::Store;
    use crate::types::NodeConfig;

    #[tokio::test]
    async fn busy_wallet_does_not_leave_community_fenced() {
        let community = Arc::new(Mutex::new(()));
        let wallet = Arc::new(Mutex::new(()));
        let signing = wallet.lock().await;
        assert!(RequestFence::try_acquire(&community, &wallet).is_none());
        assert!(community.try_lock().is_ok());
        drop(signing);
        let fence = RequestFence::try_acquire(&community, &wallet).unwrap();
        assert!(community.try_lock().is_err());
        assert!(wallet.try_lock().is_err());
        drop(fence);
        assert!(community.try_lock().is_ok());
        assert!(wallet.try_lock().is_ok());
    }

    #[test]
    fn new_installs_install_signed_updates_unattended() {
        let config = NodeConfig::default();
        assert!(config.auto_update);
        assert!(config.auto_install_updates);
    }

    /// Stores from v0.8.13 and earlier hold the old default, written out or
    /// missing. They get the new default once. A later "off" sticks, also
    /// after a v0.8.13 downgrade rewrites store.json without new fields.
    #[test]
    fn older_stores_get_unattended_installs_once_and_keep_a_later_opt_out() {
        let mut written = serde_json::to_value(NodeConfig::default()).unwrap();
        written["autoInstallUpdates"] = false.into();
        let mut missing = written.clone();
        missing
            .as_object_mut()
            .unwrap()
            .remove("autoInstallUpdates");

        for stored in [written, missing] {
            let dir = tempfile::tempdir().unwrap();
            let config: NodeConfig = serde_json::from_value(stored).unwrap();
            assert!(!config.auto_install_updates);
            let mut store = Store {
                config: Some(config),
                ..Store::default()
            };
            assert!(apply_unattended_install_default(&mut store, dir.path()).unwrap());
            assert!(
                Store::load_from(dir.path())
                    .config
                    .unwrap()
                    .auto_install_updates
            );

            // "Off" in Settings, then the downgraded app saves store.json.
            store.config.as_mut().unwrap().auto_install_updates = false;
            store.save_to(dir.path()).unwrap();
            let mut relaunched = Store::load_from(dir.path());
            assert!(!apply_unattended_install_default(&mut relaunched, dir.path()).unwrap());
            assert!(!relaunched.config.unwrap().auto_install_updates);
        }
    }

    /// A first launch has no config and no store directory yet. The marker is
    /// recorded anyway, so an "off" chosen after onboarding is kept.
    #[test]
    fn a_first_launch_records_the_default_before_any_config_exists() {
        let parent = tempfile::tempdir().unwrap();
        let dir = parent.path().join("app-data");
        let mut store = Store::default();
        assert!(!apply_unattended_install_default(&mut store, &dir).unwrap());
        assert!(store.config.is_none());

        store.config = Some(NodeConfig {
            auto_install_updates: false,
            ..NodeConfig::default()
        });
        store.save_to(&dir).unwrap();
        let mut relaunched = Store::load_from(&dir);
        assert!(!apply_unattended_install_default(&mut relaunched, &dir).unwrap());
        assert!(!relaunched.config.unwrap().auto_install_updates);
    }
}
