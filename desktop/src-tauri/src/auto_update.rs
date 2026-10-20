//! Native ownership of app-originated work while an unattended update drains.
//! These guards live in AppState, not an IPC future: cancellation cannot reopen
//! signing while a prepared installer transaction still owns the node.

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

/// Give a store written before v0.8.14 the unattended-install default, once.
/// Through v0.8.13 installs waited for a click, and a desktop that never got
/// one stayed on a release with a known fix. Afterwards the Settings switch
/// is the user's alone. Returns whether the config changed, so the caller
/// persists only then.
pub fn apply_unattended_install_default(config: &mut crate::types::NodeConfig) -> bool {
    if config.auto_install_default_applied {
        return false;
    }
    config.auto_install_updates = true;
    config.auto_install_default_applied = true;
    true
}

#[cfg(test)]
mod tests {
    use super::*;

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
        let config = crate::types::NodeConfig::default();
        assert!(config.auto_update);
        assert!(config.auto_install_updates);
        assert!(config.auto_install_default_applied);
    }

    /// Stores from v0.8.13 and earlier hold the old default, written out or
    /// missing. They get the new default once; a later "off" sticks.
    #[test]
    fn older_stores_get_unattended_installs_once_and_keep_a_later_opt_out() {
        let config = crate::types::NodeConfig::default();
        let mut written = serde_json::to_value(config).unwrap();
        written
            .as_object_mut()
            .unwrap()
            .remove("autoInstallDefaultApplied");
        written["autoInstallUpdates"] = false.into();
        let mut missing = written.clone();
        missing
            .as_object_mut()
            .unwrap()
            .remove("autoInstallUpdates");

        for stored in [written, missing] {
            let mut migrated: crate::types::NodeConfig = serde_json::from_value(stored).unwrap();
            assert!(!migrated.auto_install_updates);
            assert!(apply_unattended_install_default(&mut migrated));
            assert!(migrated.auto_update);
            assert!(migrated.auto_install_updates);

            migrated.auto_install_updates = false;
            let mut relaunched: crate::types::NodeConfig =
                serde_json::from_value(serde_json::to_value(&migrated).unwrap()).unwrap();
            assert!(!apply_unattended_install_default(&mut relaunched));
            assert!(!relaunched.auto_install_updates);
        }
    }
}
