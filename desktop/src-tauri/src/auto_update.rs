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
    fn legacy_check_consent_does_not_opt_into_installation() {
        let config = crate::types::NodeConfig::default();
        let mut stored = serde_json::to_value(config).unwrap();
        stored.as_object_mut().unwrap().remove("autoInstallUpdates");
        let migrated: crate::types::NodeConfig = serde_json::from_value(stored).unwrap();
        assert!(migrated.auto_update);
        assert!(!migrated.auto_install_updates);
    }
}
