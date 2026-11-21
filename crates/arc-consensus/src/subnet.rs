use arc_crypto::Hash256;
use arc_types::Address;
use dashmap::DashMap;
use serde::{Deserialize, Serialize};
use std::time::{SystemTime, UNIX_EPOCH};
use thiserror::Error;

pub type SubnetId = [u8; 32];

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum SubnetError {
    #[error("subnet not found: {0:?}")]
    NotFound(SubnetId),

    #[error("subnet already exists")]
    AlreadyExists,

    #[error("not authorized: caller is not subnet owner")]
    NotAuthorized,

    #[error("insufficient validators: have {have}, need {need}")]
    InsufficientValidators { have: u32, need: u32 },

    #[error("invalid anchor: height {new} must be greater than {latest}")]
    InvalidAnchorHeight { new: u64, latest: u64 },

    #[error("subnet not active")]
    NotActive,

    #[error("validator already joined")]
    ValidatorAlreadyJoined,

    #[error("validator not found")]
    ValidatorNotFound,

    #[error("insufficient stake: have {have}, need {need}")]
    InsufficientStake { have: u64, need: u64 },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SubnetVmType {
    Evm,
    Wasm,
    Custom,
    ZkRollup,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SubnetStatus {
    Pending,
    Active,
    Paused,
    Deregistered,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubnetConfig {
    pub subnet_id: SubnetId,
    pub name: String,
    pub owner: Address,
    pub created_at: u64,
    pub vm_type: SubnetVmType,
    pub anchor_interval: u64,
    pub min_validators: u32,
    pub max_validators: u32,
    pub stake_requirement: u64,
    pub status: SubnetStatus,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StateAnchor {
    pub subnet_id: SubnetId,
    pub subnet_height: u64,
    pub state_root: Hash256,
    pub tx_root: Hash256,
    pub anchor_height: u64,
    pub submitter: Address,
    pub timestamp: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubnetValidator {
    pub address: Address,
    pub subnet_id: SubnetId,
    pub stake: u64,
    pub joined_at: u64,
    pub is_active: bool,
}

pub struct SubnetRegistry {
    subnets: DashMap<SubnetId, SubnetConfig>,
    anchors: DashMap<SubnetId, Vec<StateAnchor>>,
    validators: DashMap<SubnetId, Vec<SubnetValidator>>,
    latest_anchor: DashMap<SubnetId, StateAnchor>,
}

impl SubnetRegistry {
    pub fn new() -> Self {
        Self {
            subnets: DashMap::new(),
            anchors: DashMap::new(),
            validators: DashMap::new(),
            latest_anchor: DashMap::new(),
        }
    }

    pub fn register_subnet(&self, config: SubnetConfig) -> Result<SubnetId, SubnetError> {
        let id = config.subnet_id;

        if self.subnets.contains_key(&id) {
            return Err(SubnetError::AlreadyExists);
        }

        self.subnets.insert(id, config);
        self.anchors.insert(id, Vec::new());
        self.validators.insert(id, Vec::new());

        Ok(id)
    }

    pub fn deregister_subnet(
        &self,
        subnet_id: &SubnetId,
        caller: &Address,
    ) -> Result<(), SubnetError> {
        let mut entry = self
            .subnets
            .get_mut(subnet_id)
            .ok_or(SubnetError::NotFound(*subnet_id))?;

        if entry.owner != *caller {
            return Err(SubnetError::NotAuthorized);
        }

        entry.status = SubnetStatus::Deregistered;
        Ok(())
    }

    pub fn get_subnet(&self, subnet_id: &SubnetId) -> Option<SubnetConfig> {
        self.subnets.get(subnet_id).map(|r| r.clone())
    }

    pub fn list_subnets(&self) -> Vec<SubnetConfig> {
        self.subnets.iter().map(|r| r.value().clone()).collect()
    }

    pub fn active_subnet_count(&self) -> usize {
        self.subnets
            .iter()
            .filter(|r| r.value().status == SubnetStatus::Active)
            .count()
    }

    pub fn submit_anchor(&self, anchor: StateAnchor) -> Result<(), SubnetError> {
        let subnet = self
            .subnets
            .get(&anchor.subnet_id)
            .ok_or(SubnetError::NotFound(anchor.subnet_id))?;

        if subnet.status != SubnetStatus::Active {
            return Err(SubnetError::NotActive);
        }
        drop(subnet);

        if let Some(latest) = self.latest_anchor.get(&anchor.subnet_id)
            && anchor.subnet_height <= latest.subnet_height
        {
            return Err(SubnetError::InvalidAnchorHeight {
                new: anchor.subnet_height,
                latest: latest.subnet_height,
            });
        }

        self.latest_anchor.insert(anchor.subnet_id, anchor.clone());

        self.anchors
            .entry(anchor.subnet_id)
            .or_default()
            .push(anchor);

        Ok(())
    }

    pub fn get_latest_anchor(&self, subnet_id: &SubnetId) -> Option<StateAnchor> {
        self.latest_anchor.get(subnet_id).map(|r| r.clone())
    }

    pub fn get_anchor_at_height(
        &self,
        subnet_id: &SubnetId,
        subnet_height: u64,
    ) -> Option<StateAnchor> {
        self.anchors.get(subnet_id).and_then(|anchors| {
            anchors
                .iter()
                .find(|a| a.subnet_height == subnet_height)
                .cloned()
        })
    }

    pub fn anchor_count(&self, subnet_id: &SubnetId) -> usize {
        self.anchors.get(subnet_id).map(|a| a.len()).unwrap_or(0)
    }

    pub fn join_subnet(
        &self,
        subnet_id: &SubnetId,
        validator: SubnetValidator,
    ) -> Result<(), SubnetError> {
        let subnet = self
            .subnets
            .get(subnet_id)
            .ok_or(SubnetError::NotFound(*subnet_id))?;

        if validator.stake < subnet.stake_requirement {
            return Err(SubnetError::InsufficientStake {
                have: validator.stake,
                need: subnet.stake_requirement,
            });
        }
        drop(subnet);

        let mut vals = self.validators.entry(*subnet_id).or_default();
        if vals.iter().any(|v| v.address == validator.address) {
            return Err(SubnetError::ValidatorAlreadyJoined);
        }
        vals.push(validator);
        let active_count = vals.iter().filter(|v| v.is_active).count() as u32;
        drop(vals);

        if let Some(mut subnet) = self.subnets.get_mut(subnet_id)
            && subnet.status == SubnetStatus::Pending
            && active_count >= subnet.min_validators
        {
            subnet.status = SubnetStatus::Active;
        }

        Ok(())
    }

    pub fn leave_subnet(&self, subnet_id: &SubnetId, address: &Address) -> Result<(), SubnetError> {
        if !self.subnets.contains_key(subnet_id) {
            return Err(SubnetError::NotFound(*subnet_id));
        }

        let mut vals = self
            .validators
            .get_mut(subnet_id)
            .ok_or(SubnetError::ValidatorNotFound)?;

        let idx = vals
            .iter()
            .position(|v| v.address == *address)
            .ok_or(SubnetError::ValidatorNotFound)?;

        vals.remove(idx);
        Ok(())
    }

    pub fn subnet_validators(&self, subnet_id: &SubnetId) -> Vec<SubnetValidator> {
        self.validators
            .get(subnet_id)
            .map(|v| v.clone())
            .unwrap_or_default()
    }

    pub fn check_liveness(&self, subnet_id: &SubnetId, max_anchor_age_secs: u64) -> bool {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();

        self.latest_anchor
            .get(subnet_id)
            .map(|a| now.saturating_sub(a.timestamp) <= max_anchor_age_secs)
            .unwrap_or(false)
    }

    pub fn verify_anchor_sequence(&self, subnet_id: &SubnetId) -> bool {
        let anchors = match self.anchors.get(subnet_id) {
            Some(a) => a,
            None => return true,
        };

        anchors
            .windows(2)
            .all(|w| w[0].subnet_height < w[1].subnet_height)
    }
}

impl Default for SubnetRegistry {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn now_ts() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs()
    }

    fn make_subnet(id: SubnetId, owner: Address) -> SubnetConfig {
        SubnetConfig {
            subnet_id: id,
            name: "test-subnet".into(),
            owner,
            created_at: now_ts(),
            vm_type: SubnetVmType::Wasm,
            anchor_interval: 100,
            min_validators: 2,
            max_validators: 10,
            stake_requirement: 1_000,
            status: SubnetStatus::Pending,
        }
    }

    fn subnet_id(byte: u8) -> SubnetId {
        let mut id = [0u8; 32];
        id[0] = byte;
        id
    }

    fn addr(byte: u8) -> Address {
        let mut a = [0u8; 32];
        a[0] = byte;
        Hash256(a)
    }

    fn make_validator(address: Address, subnet_id: SubnetId, stake: u64) -> SubnetValidator {
        SubnetValidator {
            address,
            subnet_id,
            stake,
            joined_at: now_ts(),
            is_active: true,
        }
    }

    fn make_anchor(subnet_id: SubnetId, subnet_height: u64, anchor_height: u64) -> StateAnchor {
        StateAnchor {
            subnet_id,
            subnet_height,
            state_root: Hash256([subnet_height as u8; 32]),
            tx_root: Hash256([0xAB; 32]),
            anchor_height,
            submitter: addr(0xFF),
            timestamp: now_ts(),
        }
    }

    #[test]
    fn test_register_subnet() {
        let reg = SubnetRegistry::new();
        let id = subnet_id(1);
        let config = make_subnet(id, addr(0x01));

        let result = reg.register_subnet(config.clone());
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), id);

        let stored = reg.get_subnet(&id).expect("subnet should exist");
        assert_eq!(stored.name, "test-subnet");
        assert_eq!(stored.owner, addr(0x01));
        assert_eq!(stored.status, SubnetStatus::Pending);
    }

    #[test]
    fn test_register_duplicate_fails() {
        let reg = SubnetRegistry::new();
        let id = subnet_id(2);
        let config = make_subnet(id, addr(0x01));

        assert!(reg.register_subnet(config.clone()).is_ok());
        let err = reg.register_subnet(config).unwrap_err();
        assert_eq!(err, SubnetError::AlreadyExists);
    }

    #[test]
    fn test_deregister_subnet() {
        let reg = SubnetRegistry::new();
        let id = subnet_id(3);
        let owner = addr(0x01);
        let config = make_subnet(id, owner);

        reg.register_subnet(config).unwrap();
        assert!(reg.deregister_subnet(&id, &owner).is_ok());

        let stored = reg.get_subnet(&id).unwrap();
        assert_eq!(stored.status, SubnetStatus::Deregistered);
    }

    #[test]
    fn test_deregister_not_owner_fails() {
        let reg = SubnetRegistry::new();
        let id = subnet_id(4);
        let owner = addr(0x01);
        let impostor = addr(0x02);
        let config = make_subnet(id, owner);

        reg.register_subnet(config).unwrap();
        let err = reg.deregister_subnet(&id, &impostor).unwrap_err();
        assert_eq!(err, SubnetError::NotAuthorized);
    }

    #[test]
    fn test_submit_anchor() {
        let reg = SubnetRegistry::new();
        let id = subnet_id(5);
        let mut config = make_subnet(id, addr(0x01));
        config.status = SubnetStatus::Active;
        reg.register_subnet(config).unwrap();

        let anchor = make_anchor(id, 100, 50);
        assert!(reg.submit_anchor(anchor).is_ok());

        let latest = reg.get_latest_anchor(&id).expect("should have anchor");
        assert_eq!(latest.subnet_height, 100);
        assert_eq!(latest.anchor_height, 50);
        assert_eq!(reg.anchor_count(&id), 1);
    }

    #[test]
    fn test_anchor_sequence_valid() {
        let reg = SubnetRegistry::new();
        let id = subnet_id(6);
        let mut config = make_subnet(id, addr(0x01));
        config.status = SubnetStatus::Active;
        reg.register_subnet(config).unwrap();

        reg.submit_anchor(make_anchor(id, 100, 1)).unwrap();
        reg.submit_anchor(make_anchor(id, 200, 2)).unwrap();
        reg.submit_anchor(make_anchor(id, 300, 3)).unwrap();

        assert!(reg.verify_anchor_sequence(&id));
        assert_eq!(reg.anchor_count(&id), 3);
    }

    #[test]
    fn test_anchor_sequence_invalid() {
        let reg = SubnetRegistry::new();
        let id = subnet_id(7);
        let mut config = make_subnet(id, addr(0x01));
        config.status = SubnetStatus::Active;
        reg.register_subnet(config).unwrap();

        reg.submit_anchor(make_anchor(id, 200, 1)).unwrap();

        let err = reg.submit_anchor(make_anchor(id, 200, 2)).unwrap_err();
        assert_eq!(
            err,
            SubnetError::InvalidAnchorHeight {
                new: 200,
                latest: 200,
            }
        );

        let err = reg.submit_anchor(make_anchor(id, 100, 3)).unwrap_err();
        assert_eq!(
            err,
            SubnetError::InvalidAnchorHeight {
                new: 100,
                latest: 200,
            }
        );
    }

    #[test]
    fn test_join_subnet() {
        let reg = SubnetRegistry::new();
        let id = subnet_id(8);
        let config = make_subnet(id, addr(0x01));
        reg.register_subnet(config).unwrap();

        let v = make_validator(addr(0x10), id, 5_000);
        assert!(reg.join_subnet(&id, v).is_ok());

        let vals = reg.subnet_validators(&id);
        assert_eq!(vals.len(), 1);
        assert_eq!(vals[0].address, addr(0x10));
    }

    #[test]
    fn test_join_insufficient_stake() {
        let reg = SubnetRegistry::new();
        let id = subnet_id(9);
        let config = make_subnet(id, addr(0x01));
        reg.register_subnet(config).unwrap();

        let v = make_validator(addr(0x10), id, 500);
        let err = reg.join_subnet(&id, v).unwrap_err();
        assert_eq!(
            err,
            SubnetError::InsufficientStake {
                have: 500,
                need: 1_000,
            }
        );
    }

    #[test]
    fn test_leave_subnet() {
        let reg = SubnetRegistry::new();
        let id = subnet_id(10);
        let config = make_subnet(id, addr(0x01));
        reg.register_subnet(config).unwrap();

        let v = make_validator(addr(0x10), id, 5_000);
        reg.join_subnet(&id, v).unwrap();

        assert!(reg.leave_subnet(&id, &addr(0x10)).is_ok());
        assert!(reg.subnet_validators(&id).is_empty());
    }

    #[test]
    fn test_subnet_activation() {
        let reg = SubnetRegistry::new();
        let id = subnet_id(11);
        let config = make_subnet(id, addr(0x01));
        reg.register_subnet(config).unwrap();

        assert_eq!(reg.get_subnet(&id).unwrap().status, SubnetStatus::Pending,);

        reg.join_subnet(&id, make_validator(addr(0x10), id, 5_000))
            .unwrap();
        assert_eq!(reg.get_subnet(&id).unwrap().status, SubnetStatus::Pending,);

        reg.join_subnet(&id, make_validator(addr(0x11), id, 5_000))
            .unwrap();
        assert_eq!(reg.get_subnet(&id).unwrap().status, SubnetStatus::Active,);
    }

    #[test]
    fn test_check_liveness() {
        let reg = SubnetRegistry::new();
        let id = subnet_id(12);
        let mut config = make_subnet(id, addr(0x01));
        config.status = SubnetStatus::Active;
        reg.register_subnet(config).unwrap();

        assert!(!reg.check_liveness(&id, 60));

        reg.submit_anchor(make_anchor(id, 100, 1)).unwrap();

        assert!(reg.check_liveness(&id, 60));

        let mut stale_anchor = make_anchor(id, 200, 2);
        stale_anchor.timestamp = 1_000_000;
        reg.submit_anchor(stale_anchor).unwrap();

        assert!(!reg.check_liveness(&id, 60));
    }

    #[test]
    fn test_list_subnets() {
        let reg = SubnetRegistry::new();

        assert!(reg.list_subnets().is_empty());

        reg.register_subnet(make_subnet(subnet_id(20), addr(0x01)))
            .unwrap();
        reg.register_subnet(make_subnet(subnet_id(21), addr(0x02)))
            .unwrap();
        reg.register_subnet(make_subnet(subnet_id(22), addr(0x03)))
            .unwrap();

        let all = reg.list_subnets();
        assert_eq!(all.len(), 3);

        assert_eq!(reg.active_subnet_count(), 0);
    }

    #[test]
    fn test_get_anchor_at_height() {
        let reg = SubnetRegistry::new();
        let id = subnet_id(14);
        let mut config = make_subnet(id, addr(0x01));
        config.status = SubnetStatus::Active;
        reg.register_subnet(config).unwrap();

        reg.submit_anchor(make_anchor(id, 100, 1)).unwrap();
        reg.submit_anchor(make_anchor(id, 200, 2)).unwrap();
        reg.submit_anchor(make_anchor(id, 300, 3)).unwrap();

        let a = reg
            .get_anchor_at_height(&id, 200)
            .expect("should find height 200");
        assert_eq!(a.subnet_height, 200);
        assert_eq!(a.anchor_height, 2);

        assert!(reg.get_anchor_at_height(&id, 150).is_none());
    }
}
