use serde::{Deserialize, Serialize};
use std::fmt;
use thiserror::Error;

#[derive(Debug, Clone, Error, PartialEq, Eq)]
pub enum RecoveryError {
    #[error("address is not a guardian")]
    NotGuardian,
    #[error("guardian has already approved this recovery")]
    AlreadyApproved,
    #[error("guardian approval threshold has not been met")]
    ThresholdNotMet,
    #[error("recovery process not found")]
    RecoveryNotFound,
    #[error("recovery is still within its time-delay period")]
    StillInDelay,
    #[error("recovery has already been executed")]
    AlreadyExecuted,
    #[error("account is still in the post-recovery cooldown period")]
    InCooldown,
    #[error("cannot have fewer guardians than the recovery threshold")]
    TooFewGuardians,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum GuardianType {
    Wallet,
    Email,
    Phone,
    Hardware,
    Social,
    Institution,
}

impl fmt::Display for GuardianType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Wallet => write!(f, "Wallet"),
            Self::Email => write!(f, "Email"),
            Self::Phone => write!(f, "Phone"),
            Self::Hardware => write!(f, "Hardware"),
            Self::Social => write!(f, "Social"),
            Self::Institution => write!(f, "Institution"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum RecoveryStatus {
    Initiated,
    PendingDelay,
    ReadyToExecute,
    Executed,
    Cancelled,
    Expired,
}

impl fmt::Display for RecoveryStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Initiated => write!(f, "Initiated"),
            Self::PendingDelay => write!(f, "PendingDelay"),
            Self::ReadyToExecute => write!(f, "ReadyToExecute"),
            Self::Executed => write!(f, "Executed"),
            Self::Cancelled => write!(f, "Cancelled"),
            Self::Expired => write!(f, "Expired"),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecoveryGuardian {
    pub address: [u8; 32],
    pub name: String,
    pub weight: u32,
    pub added_at: u64,
    pub guardian_type: GuardianType,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GuardianApproval {
    pub guardian: [u8; 32],
    pub approved_at: u64,
    pub proof: Vec<u8>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecoveryProcess {
    pub id: [u8; 32],
    pub account: [u8; 32],
    pub new_owner: [u8; 32],
    pub initiated_at: u64,
    pub delay_until: u64,
    pub approvals: Vec<GuardianApproval>,
    pub status: RecoveryStatus,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecoveryConfig {
    pub guardians: Vec<RecoveryGuardian>,
    pub threshold: u32,
    pub recovery_delay: u64,
    pub cooldown_after_recovery: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SocialRecoveryManager {
    pub config: RecoveryConfig,
    pub processes: Vec<RecoveryProcess>,
    pub last_recovery_at: Option<u64>,
    nonce: u64,
}

impl SocialRecoveryManager {
    fn is_guardian(&self, addr: &[u8; 32]) -> bool {
        self.config.guardians.iter().any(|g| g.address == *addr)
    }

    #[allow(dead_code)]
    fn guardian_weight(&self, addr: &[u8; 32]) -> u32 {
        self.config
            .guardians
            .iter()
            .find(|g| g.address == *addr)
            .map(|g| g.weight)
            .unwrap_or(0)
    }

    fn find_process_mut(
        &mut self,
        recovery_id: &[u8; 32],
    ) -> Result<&mut RecoveryProcess, RecoveryError> {
        self.processes
            .iter_mut()
            .find(|p| p.id == *recovery_id)
            .ok_or(RecoveryError::RecoveryNotFound)
    }

    fn approval_weight(process: &RecoveryProcess, config: &RecoveryConfig) -> u32 {
        process
            .approvals
            .iter()
            .map(|a| {
                config
                    .guardians
                    .iter()
                    .find(|g| g.address == a.guardian)
                    .map(|g| g.weight)
                    .unwrap_or(0)
            })
            .sum()
    }

    fn derive_process_id(nonce: u64, account: &[u8; 32], new_owner: &[u8; 32]) -> [u8; 32] {
        let mut hasher = blake3::Hasher::new();
        hasher.update(b"social-recovery-v1");
        hasher.update(&nonce.to_le_bytes());
        hasher.update(account);
        hasher.update(new_owner);
        *hasher.finalize().as_bytes()
    }

    pub fn new(config: RecoveryConfig) -> Self {
        Self {
            config,
            processes: Vec::new(),
            last_recovery_at: None,
            nonce: 0,
        }
    }

    pub fn initiate_recovery(
        &mut self,
        account: [u8; 32],
        new_owner: [u8; 32],
    ) -> Result<[u8; 32], RecoveryError> {
        let process_id = Self::derive_process_id(self.nonce, &account, &new_owner);
        self.nonce += 1;

        let process = RecoveryProcess {
            id: process_id,
            account,
            new_owner,
            initiated_at: 0,
            delay_until: 0,
            approvals: Vec::new(),
            status: RecoveryStatus::Initiated,
        };

        self.processes.push(process);
        Ok(process_id)
    }

    pub fn approve_recovery(
        &mut self,
        recovery_id: [u8; 32],
        guardian: [u8; 32],
    ) -> Result<bool, RecoveryError> {
        if !self.is_guardian(&guardian) {
            return Err(RecoveryError::NotGuardian);
        }

        let threshold = self.config.threshold;
        let delay = self.config.recovery_delay;
        let config = self.config.clone();

        let process = self.find_process_mut(&recovery_id)?;

        if process.status == RecoveryStatus::Executed {
            return Err(RecoveryError::AlreadyExecuted);
        }

        if process.approvals.iter().any(|a| a.guardian == guardian) {
            return Err(RecoveryError::AlreadyApproved);
        }

        process.approvals.push(GuardianApproval {
            guardian,
            approved_at: 0,
            proof: Vec::new(),
        });

        let total = Self::approval_weight(process, &config);
        if total >= threshold {
            process.status = RecoveryStatus::PendingDelay;
            process.delay_until = process.initiated_at + delay;
            Ok(true)
        } else {
            Ok(false)
        }
    }

    pub fn execute_recovery(
        &mut self,
        recovery_id: [u8; 32],
        now: u64,
    ) -> Result<[u8; 32], RecoveryError> {
        let threshold = self.config.threshold;
        let config = self.config.clone();
        let cooldown = self.config.cooldown_after_recovery;
        let in_cooldown = self.is_in_cooldown(now);

        let process = self.find_process_mut(&recovery_id)?;

        if process.status == RecoveryStatus::Executed {
            return Err(RecoveryError::AlreadyExecuted);
        }

        if in_cooldown {
            return Err(RecoveryError::InCooldown);
        }

        let total = Self::approval_weight(process, &config);
        if total < threshold {
            return Err(RecoveryError::ThresholdNotMet);
        }

        if now < process.delay_until {
            return Err(RecoveryError::StillInDelay);
        }

        process.status = RecoveryStatus::Executed;
        let new_owner = process.new_owner;

        self.last_recovery_at = Some(now);
        let _ = cooldown;

        Ok(new_owner)
    }

    pub fn cancel_recovery(
        &mut self,
        recovery_id: [u8; 32],
        _account_owner: [u8; 32],
    ) -> Result<(), RecoveryError> {
        let process = self.find_process_mut(&recovery_id)?;
        if process.status == RecoveryStatus::Executed {
            return Err(RecoveryError::AlreadyExecuted);
        }
        process.status = RecoveryStatus::Cancelled;
        Ok(())
    }

    pub fn add_guardian(&mut self, guardian: RecoveryGuardian) -> Result<(), RecoveryError> {
        if self.is_guardian(&guardian.address) {
            return Err(RecoveryError::AlreadyApproved);
        }
        self.config.guardians.push(guardian);
        Ok(())
    }

    pub fn remove_guardian(&mut self, address: [u8; 32]) -> Result<(), RecoveryError> {
        let idx = self
            .config
            .guardians
            .iter()
            .position(|g| g.address == address)
            .ok_or(RecoveryError::NotGuardian)?;

        let remaining_weight: u32 = self
            .config
            .guardians
            .iter()
            .enumerate()
            .filter(|(i, _)| *i != idx)
            .map(|(_, g)| g.weight)
            .sum();
        if remaining_weight < self.config.threshold {
            return Err(RecoveryError::TooFewGuardians);
        }

        self.config.guardians.remove(idx);
        Ok(())
    }

    pub fn is_in_cooldown(&self, now: u64) -> bool {
        match self.last_recovery_at {
            Some(last) => now < last + self.config.cooldown_after_recovery,
            None => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn addr(id: u8) -> [u8; 32] {
        let mut a = [0u8; 32];
        a[0] = id;
        a
    }

    fn guardian(id: u8, weight: u32, gtype: GuardianType) -> RecoveryGuardian {
        RecoveryGuardian {
            address: addr(id),
            name: format!("guardian-{}", id),
            weight,
            added_at: 1000,
            guardian_type: gtype,
        }
    }

    fn default_config() -> RecoveryConfig {
        RecoveryConfig {
            guardians: vec![
                guardian(10, 1, GuardianType::Wallet),
                guardian(11, 1, GuardianType::Email),
                guardian(12, 1, GuardianType::Hardware),
            ],
            threshold: 2,
            recovery_delay: 86400,
            cooldown_after_recovery: 604800,
        }
    }

    fn default_manager() -> SocialRecoveryManager {
        SocialRecoveryManager::new(default_config())
    }

    #[test]
    fn test_initiate_recovery() {
        let mut mgr = default_manager();
        let rid = mgr.initiate_recovery(addr(1), addr(2)).unwrap();
        assert_eq!(mgr.processes.len(), 1);
        assert_eq!(mgr.processes[0].id, rid);
        assert_eq!(mgr.processes[0].status, RecoveryStatus::Initiated);
    }

    #[test]
    fn test_approve_recovery_threshold_not_met() {
        let mut mgr = default_manager();
        let rid = mgr.initiate_recovery(addr(1), addr(2)).unwrap();
        let met = mgr.approve_recovery(rid, addr(10)).unwrap();
        assert!(!met);
    }

    #[test]
    fn test_approve_recovery_threshold_met() {
        let mut mgr = default_manager();
        let rid = mgr.initiate_recovery(addr(1), addr(2)).unwrap();
        mgr.approve_recovery(rid, addr(10)).unwrap();
        let met = mgr.approve_recovery(rid, addr(11)).unwrap();
        assert!(met);
        let process = &mgr.processes[0];
        assert_eq!(process.status, RecoveryStatus::PendingDelay);
    }

    #[test]
    fn test_approve_not_guardian() {
        let mut mgr = default_manager();
        let rid = mgr.initiate_recovery(addr(1), addr(2)).unwrap();
        let result = mgr.approve_recovery(rid, addr(99));
        assert_eq!(result.unwrap_err(), RecoveryError::NotGuardian);
    }

    #[test]
    fn test_approve_already_approved() {
        let mut mgr = default_manager();
        let rid = mgr.initiate_recovery(addr(1), addr(2)).unwrap();
        mgr.approve_recovery(rid, addr(10)).unwrap();
        let result = mgr.approve_recovery(rid, addr(10));
        assert_eq!(result.unwrap_err(), RecoveryError::AlreadyApproved);
    }

    #[test]
    fn test_execute_recovery_still_in_delay() {
        let mut mgr = default_manager();
        let rid = mgr.initiate_recovery(addr(1), addr(2)).unwrap();
        mgr.approve_recovery(rid, addr(10)).unwrap();
        mgr.approve_recovery(rid, addr(11)).unwrap();

        let result = mgr.execute_recovery(rid, 100);
        assert_eq!(result.unwrap_err(), RecoveryError::StillInDelay);
    }

    #[test]
    fn test_execute_recovery_success() {
        let mut mgr = default_manager();
        let rid = mgr.initiate_recovery(addr(1), addr(2)).unwrap();
        mgr.approve_recovery(rid, addr(10)).unwrap();
        mgr.approve_recovery(rid, addr(11)).unwrap();

        let new_owner = mgr.execute_recovery(rid, 100_000).unwrap();
        assert_eq!(new_owner, addr(2));
        assert_eq!(mgr.processes[0].status, RecoveryStatus::Executed);
        assert_eq!(mgr.last_recovery_at, Some(100_000));
    }

    #[test]
    fn test_execute_recovery_already_executed() {
        let mut mgr = default_manager();
        let rid = mgr.initiate_recovery(addr(1), addr(2)).unwrap();
        mgr.approve_recovery(rid, addr(10)).unwrap();
        mgr.approve_recovery(rid, addr(11)).unwrap();
        mgr.execute_recovery(rid, 100_000).unwrap();

        let result = mgr.execute_recovery(rid, 200_000);
        assert_eq!(result.unwrap_err(), RecoveryError::AlreadyExecuted);
    }

    #[test]
    fn test_cancel_recovery() {
        let mut mgr = default_manager();
        let rid = mgr.initiate_recovery(addr(1), addr(2)).unwrap();
        mgr.cancel_recovery(rid, addr(1)).unwrap();
        assert_eq!(mgr.processes[0].status, RecoveryStatus::Cancelled);
    }

    #[test]
    fn test_cooldown_period() {
        let mut mgr = default_manager();
        let rid = mgr.initiate_recovery(addr(1), addr(2)).unwrap();
        mgr.approve_recovery(rid, addr(10)).unwrap();
        mgr.approve_recovery(rid, addr(11)).unwrap();
        mgr.execute_recovery(rid, 100_000).unwrap();

        assert!(mgr.is_in_cooldown(100_000 + 604_799));
        assert!(!mgr.is_in_cooldown(100_000 + 604_800));

        let rid2 = mgr.initiate_recovery(addr(1), addr(3)).unwrap();
        mgr.approve_recovery(rid2, addr(10)).unwrap();
        mgr.approve_recovery(rid2, addr(11)).unwrap();
        let result = mgr.execute_recovery(rid2, 100_000 + 1000);
        assert_eq!(result.unwrap_err(), RecoveryError::InCooldown);
    }

    #[test]
    fn test_add_guardian() {
        let mut mgr = default_manager();
        let new_g = guardian(20, 2, GuardianType::Phone);
        mgr.add_guardian(new_g).unwrap();
        assert_eq!(mgr.config.guardians.len(), 4);
    }

    #[test]
    fn test_add_duplicate_guardian() {
        let mut mgr = default_manager();
        let dup = guardian(10, 1, GuardianType::Wallet);
        let result = mgr.add_guardian(dup);
        assert_eq!(result.unwrap_err(), RecoveryError::AlreadyApproved);
    }

    #[test]
    fn test_remove_guardian() {
        let mut mgr = default_manager();
        mgr.remove_guardian(addr(12)).unwrap();
        assert_eq!(mgr.config.guardians.len(), 2);
    }

    #[test]
    fn test_remove_guardian_too_few() {
        let mut mgr = default_manager();
        mgr.remove_guardian(addr(12)).unwrap();
        let result = mgr.remove_guardian(addr(11));
        assert_eq!(result.unwrap_err(), RecoveryError::TooFewGuardians);
    }

    #[test]
    fn test_weighted_guardians() {
        let config = RecoveryConfig {
            guardians: vec![
                guardian(10, 3, GuardianType::Hardware),
                guardian(11, 1, GuardianType::Email),
            ],
            threshold: 3,
            recovery_delay: 100,
            cooldown_after_recovery: 1000,
        };
        let mut mgr = SocialRecoveryManager::new(config);
        let rid = mgr.initiate_recovery(addr(1), addr(2)).unwrap();

        let met = mgr.approve_recovery(rid, addr(10)).unwrap();
        assert!(met);
    }

    #[test]
    fn test_execute_without_threshold() {
        let mut mgr = default_manager();
        let rid = mgr.initiate_recovery(addr(1), addr(2)).unwrap();
        mgr.approve_recovery(rid, addr(10)).unwrap();
        let result = mgr.execute_recovery(rid, 200_000);
        assert_eq!(result.unwrap_err(), RecoveryError::ThresholdNotMet);
    }
}
