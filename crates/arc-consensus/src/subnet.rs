// Add to lib.rs: pub mod subnet;

//! Subnet registry and state anchoring for ARC L1.
//!
//! Subnets are application-specific chains that settle their state roots to the
//! ARC L1, inheriting its security guarantees. This module provides:
//!
//! - **Registration**: create, query, and deregister subnets.
//! - **Anchoring**: subnets periodically submit state roots that are recorded on L1.
//! - **Validator management**: validators opt-in to subnets and must meet stake requirements.
//! - **Liveness checks**: detect subnets that have gone silent.

use arc_crypto::Hash256;
use arc_types::Address;
use dashmap::DashMap;
use serde::{Deserialize, Serialize};
use std::time::{SystemTime, UNIX_EPOCH};
use thiserror::Error;

// ── Type Aliases ────────────────────────────────────────────────────────────

/// Unique identifier for a subnet (32-byte hash).
pub type SubnetId = [u8; 32];

// ── Errors ──────────────────────────────────────────────────────────────────

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

// ── Subnet VM Type ──────────────────────────────────────────────────────────

/// Virtual machine type that a subnet runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SubnetVmType {
    /// Ethereum Virtual Machine compatible.
    Evm,
    /// WebAssembly runtime.
    Wasm,
    /// Custom / application-specific VM.
    Custom,
    /// ZK rollup proving system.
    ZkRollup,
}

// ── Subnet Status ───────────────────────────────────────────────────────────

/// Lifecycle status of a subnet.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SubnetStatus {
    /// Registered but not enough validators have joined.
    Pending,
    /// Running and anchoring state roots.
    Active,
    /// Temporarily halted by the owner.
    Paused,
    /// Permanently shut down.
    Deregistered,
}

// ── Subnet Config ───────────────────────────────────────────────────────────

/// Configuration for a subnet registered on ARC L1.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubnetConfig {
    /// Unique 32-byte identifier.
    pub subnet_id: SubnetId,
    /// Human-readable name.
    pub name: String,
    /// Address that registered this subnet (owner).
    pub owner: Address,
    /// Unix timestamp of registration.
    pub created_at: u64,
    /// VM type that the subnet runs.
    pub vm_type: SubnetVmType,
    /// How often the subnet anchors state (in subnet blocks).
    pub anchor_interval: u64,
    /// Minimum number of validators required for activation.
    pub min_validators: u32,
    /// Maximum allowed validators.
    pub max_validators: u32,
    /// ARC tokens each validator must stake to participate.
    pub stake_requirement: u64,
    /// Current lifecycle status.
    pub status: SubnetStatus,
}

// ── State Anchor ────────────────────────────────────────────────────────────

/// A state root anchor submitted from a subnet to the ARC L1.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StateAnchor {
    /// The subnet this anchor belongs to.
    pub subnet_id: SubnetId,
    /// Block height on the subnet at anchor time.
    pub subnet_height: u64,
    /// Merkle state root of the subnet at this height.
    pub state_root: Hash256,
    /// Transaction root of the subnet (enables tx inclusion proofs).
    pub tx_root: Hash256,
    /// L1 block height where this anchor was recorded.
    pub anchor_height: u64,
    /// Validator who submitted the anchor.
    pub submitter: Address,
    /// Unix timestamp of submission.
    pub timestamp: u64,
}

// ── Subnet Validator ────────────────────────────────────────────────────────

/// A validator that has opted-in to a specific subnet.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubnetValidator {
    /// Validator address.
    pub address: Address,
    /// Which subnet this validator participates in.
    pub subnet_id: SubnetId,
    /// Amount of ARC staked for this subnet.
    pub stake: u64,
    /// Unix timestamp when the validator joined.
    pub joined_at: u64,
    /// Whether this validator is currently active.
    pub is_active: bool,
}

// ── Subnet Registry ─────────────────────────────────────────────────────────

/// Main subnet registry that tracks all subnets, their anchors, and validators.
///
/// All maps are concurrent (`DashMap`) so the registry is safe to share across
/// threads without external locking.
pub struct SubnetRegistry {
    /// Subnet ID -> configuration.
    subnets: DashMap<SubnetId, SubnetConfig>,
    /// Subnet ID -> chronologically ordered anchors.
    anchors: DashMap<SubnetId, Vec<StateAnchor>>,
    /// Subnet ID -> list of opted-in validators.
    validators: DashMap<SubnetId, Vec<SubnetValidator>>,
    /// Subnet ID -> most recent anchor (quick lookup).
    latest_anchor: DashMap<SubnetId, StateAnchor>,
}

impl SubnetRegistry {
    /// Create an empty subnet registry.
    pub fn new() -> Self {
        Self {
            subnets: DashMap::new(),
            anchors: DashMap::new(),
            validators: DashMap::new(),
            latest_anchor: DashMap::new(),
        }
    }

    // ── Registration ────────────────────────────────────────────────────

    /// Register a new subnet on the L1. Returns the subnet ID on success.
    ///
    /// Fails if a subnet with the same ID already exists.
    pub fn register_subnet(&self, config: SubnetConfig) -> Result<SubnetId, SubnetError> {
        let id = config.subnet_id;

        // Reject duplicates.
        if self.subnets.contains_key(&id) {
            return Err(SubnetError::AlreadyExists);
        }

        self.subnets.insert(id, config);
        self.anchors.insert(id, Vec::new());
        self.validators.insert(id, Vec::new());

        Ok(id)
    }

    /// Deregister a subnet. Only the owner may do this.
    ///
    /// Sets the status to `Deregistered` but does not remove data so that
    /// historical anchors remain queryable.
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

    /// Look up a subnet by ID.
    pub fn get_subnet(&self, subnet_id: &SubnetId) -> Option<SubnetConfig> {
        self.subnets.get(subnet_id).map(|r| r.clone())
    }

    /// Return all registered subnets (including deregistered ones).
    pub fn list_subnets(&self) -> Vec<SubnetConfig> {
        self.subnets.iter().map(|r| r.value().clone()).collect()
    }

    /// Count subnets that are currently `Active`.
    pub fn active_subnet_count(&self) -> usize {
        self.subnets
            .iter()
            .filter(|r| r.value().status == SubnetStatus::Active)
            .count()
    }

    // ── Anchoring ───────────────────────────────────────────────────────

    /// Submit a state root anchor from a subnet.
    ///
    /// Validates:
    /// - The subnet exists and is active.
    /// - The anchor height is strictly greater than the previous anchor's height.
    pub fn submit_anchor(&self, anchor: StateAnchor) -> Result<(), SubnetError> {
        let subnet = self
            .subnets
            .get(&anchor.subnet_id)
            .ok_or(SubnetError::NotFound(anchor.subnet_id))?;

        if subnet.status != SubnetStatus::Active {
            return Err(SubnetError::NotActive);
        }
        drop(subnet);

        // Enforce monotonically increasing subnet heights.
        if let Some(latest) = self.latest_anchor.get(&anchor.subnet_id) {
            if anchor.subnet_height <= latest.subnet_height {
                return Err(SubnetError::InvalidAnchorHeight {
                    new: anchor.subnet_height,
                    latest: latest.subnet_height,
                });
            }
        }

        // Store the anchor.
        self.latest_anchor
            .insert(anchor.subnet_id, anchor.clone());

        self.anchors
            .entry(anchor.subnet_id)
            .or_default()
            .push(anchor);

        Ok(())
    }

    /// Get the most recent anchor for a subnet.
    pub fn get_latest_anchor(&self, subnet_id: &SubnetId) -> Option<StateAnchor> {
        self.latest_anchor.get(subnet_id).map(|r| r.clone())
    }

    /// Find the anchor at a specific subnet height (exact match).
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

    /// Total number of anchors recorded for a subnet.
    pub fn anchor_count(&self, subnet_id: &SubnetId) -> usize {
        self.anchors
            .get(subnet_id)
            .map(|a| a.len())
            .unwrap_or(0)
    }

    // ── Validators ──────────────────────────────────────────────────────

    /// A validator opts-in to a subnet.
    ///
    /// Validates:
    /// - The subnet exists.
    /// - The validator meets the stake requirement.
    /// - The validator hasn't already joined.
    ///
    /// When the validator count reaches `min_validators`, the subnet is
    /// automatically promoted from `Pending` to `Active`.
    pub fn join_subnet(
        &self,
        subnet_id: &SubnetId,
        validator: SubnetValidator,
    ) -> Result<(), SubnetError> {
        let subnet = self
            .subnets
            .get(subnet_id)
            .ok_or(SubnetError::NotFound(*subnet_id))?;

        // Stake check.
        if validator.stake < subnet.stake_requirement {
            return Err(SubnetError::InsufficientStake {
                have: validator.stake,
                need: subnet.stake_requirement,
            });
        }
        drop(subnet);

        // Duplicate check & insert.
        let mut vals = self.validators.entry(*subnet_id).or_default();
        if vals.iter().any(|v| v.address == validator.address) {
            return Err(SubnetError::ValidatorAlreadyJoined);
        }
        vals.push(validator);
        let active_count = vals.iter().filter(|v| v.is_active).count() as u32;
        drop(vals);

        // Auto-activate subnet when minimum validators reached.
        if let Some(mut subnet) = self.subnets.get_mut(subnet_id) {
            if subnet.status == SubnetStatus::Pending && active_count >= subnet.min_validators {
                subnet.status = SubnetStatus::Active;
            }
        }

        Ok(())
    }

    /// A validator leaves a subnet. Marks them as inactive and removes them.
    pub fn leave_subnet(
        &self,
        subnet_id: &SubnetId,
        address: &Address,
    ) -> Result<(), SubnetError> {
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

    /// Return all validators currently in a subnet.
    pub fn subnet_validators(&self, subnet_id: &SubnetId) -> Vec<SubnetValidator> {
        self.validators
            .get(subnet_id)
            .map(|v| v.clone())
            .unwrap_or_default()
    }

    /// Check whether a subnet has submitted a recent anchor.
    ///
    /// Returns `true` if the latest anchor is within `max_anchor_age_secs` of
    /// the current wall clock. Returns `false` if there is no anchor or if the
    /// anchor is stale.
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

    // ── Verification ────────────────────────────────────────────────────

    /// Verify that anchors for a subnet have strictly increasing heights.
    ///
    /// Returns `true` if the sequence is valid (or empty). Returns `false` if
    /// any anchor height is not greater than the previous one.
    pub fn verify_anchor_sequence(&self, subnet_id: &SubnetId) -> bool {
        let anchors = match self.anchors.get(subnet_id) {
            Some(a) => a,
            None => return true, // No anchors yet is trivially valid.
        };

        anchors.windows(2).all(|w| w[0].subnet_height < w[1].subnet_height)
    }
}

impl Default for SubnetRegistry {
    fn default() -> Self {
        Self::new()
    }
}

// ── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    /// Helper: current unix timestamp.
    fn now_ts() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs()
    }

    /// Helper: create a SubnetConfig with sensible defaults.
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

    /// Helper: deterministic SubnetId from a single byte.
    fn subnet_id(byte: u8) -> SubnetId {
        let mut id = [0u8; 32];
        id[0] = byte;
        id
    }

    /// Helper: deterministic Address from a single byte.
    fn addr(byte: u8) -> Address {
        let mut a = [0u8; 32];
        a[0] = byte;
        Hash256(a)
    }

    /// Helper: create a validator ready to join a subnet.
    fn make_validator(address: Address, subnet_id: SubnetId, stake: u64) -> SubnetValidator {
        SubnetValidator {
            address,
            subnet_id,
            stake,
            joined_at: now_ts(),
            is_active: true,
        }
    }

    /// Helper: create a state anchor.
    fn make_anchor(
        subnet_id: SubnetId,
        subnet_height: u64,
        anchor_height: u64,
    ) -> StateAnchor {
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

    // ── 1. Register & retrieve ──────────────────────────────────────────

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

    // ── 2. Duplicate registration fails ─────────────────────────────────

    #[test]
    fn test_register_duplicate_fails() {
        let reg = SubnetRegistry::new();
        let id = subnet_id(2);
        let config = make_subnet(id, addr(0x01));

        assert!(reg.register_subnet(config.clone()).is_ok());
        let err = reg.register_subnet(config).unwrap_err();
        assert_eq!(err, SubnetError::AlreadyExists);
    }

    // ── 3. Owner can deregister ─────────────────────────────────────────

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

    // ── 4. Non-owner cannot deregister ──────────────────────────────────

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

    // ── 5. Submit and retrieve latest anchor ────────────────────────────

    #[test]
    fn test_submit_anchor() {
        let reg = SubnetRegistry::new();
        let id = subnet_id(5);
        let mut config = make_subnet(id, addr(0x01));
        config.status = SubnetStatus::Active; // pre-activate
        reg.register_subnet(config).unwrap();

        let anchor = make_anchor(id, 100, 50);
        assert!(reg.submit_anchor(anchor).is_ok());

        let latest = reg.get_latest_anchor(&id).expect("should have anchor");
        assert_eq!(latest.subnet_height, 100);
        assert_eq!(latest.anchor_height, 50);
        assert_eq!(reg.anchor_count(&id), 1);
    }

    // ── 6. Valid anchor sequence ────────────────────────────────────────

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

    // ── 7. Non-increasing anchor height rejected ────────────────────────

    #[test]
    fn test_anchor_sequence_invalid() {
        let reg = SubnetRegistry::new();
        let id = subnet_id(7);
        let mut config = make_subnet(id, addr(0x01));
        config.status = SubnetStatus::Active;
        reg.register_subnet(config).unwrap();

        reg.submit_anchor(make_anchor(id, 200, 1)).unwrap();

        // Same height should fail.
        let err = reg.submit_anchor(make_anchor(id, 200, 2)).unwrap_err();
        assert_eq!(
            err,
            SubnetError::InvalidAnchorHeight {
                new: 200,
                latest: 200,
            }
        );

        // Lower height should also fail.
        let err = reg.submit_anchor(make_anchor(id, 100, 3)).unwrap_err();
        assert_eq!(
            err,
            SubnetError::InvalidAnchorHeight {
                new: 100,
                latest: 200,
            }
        );
    }

    // ── 8. Validator joins subnet ───────────────────────────────────────

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

    // ── 9. Insufficient stake rejected ──────────────────────────────────

    #[test]
    fn test_join_insufficient_stake() {
        let reg = SubnetRegistry::new();
        let id = subnet_id(9);
        let config = make_subnet(id, addr(0x01)); // stake_requirement = 1_000
        reg.register_subnet(config).unwrap();

        let v = make_validator(addr(0x10), id, 500); // below requirement
        let err = reg.join_subnet(&id, v).unwrap_err();
        assert_eq!(
            err,
            SubnetError::InsufficientStake {
                have: 500,
                need: 1_000,
            }
        );
    }

    // ── 10. Validator leaves subnet ─────────────────────────────────────

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

    // ── 11. Auto-activation on min_validators reached ───────────────────

    #[test]
    fn test_subnet_activation() {
        let reg = SubnetRegistry::new();
        let id = subnet_id(11);
        let config = make_subnet(id, addr(0x01)); // min_validators = 2
        reg.register_subnet(config).unwrap();

        // Status should be Pending before enough validators join.
        assert_eq!(
            reg.get_subnet(&id).unwrap().status,
            SubnetStatus::Pending,
        );

        // First validator — still pending.
        reg.join_subnet(&id, make_validator(addr(0x10), id, 5_000))
            .unwrap();
        assert_eq!(
            reg.get_subnet(&id).unwrap().status,
            SubnetStatus::Pending,
        );

        // Second validator — should now be Active.
        reg.join_subnet(&id, make_validator(addr(0x11), id, 5_000))
            .unwrap();
        assert_eq!(
            reg.get_subnet(&id).unwrap().status,
            SubnetStatus::Active,
        );
    }

    // ── 12. Liveness check ──────────────────────────────────────────────

    #[test]
    fn test_check_liveness() {
        let reg = SubnetRegistry::new();
        let id = subnet_id(12);
        let mut config = make_subnet(id, addr(0x01));
        config.status = SubnetStatus::Active;
        reg.register_subnet(config).unwrap();

        // No anchor yet — not live.
        assert!(!reg.check_liveness(&id, 60));

        // Submit a fresh anchor.
        reg.submit_anchor(make_anchor(id, 100, 1)).unwrap();

        // Should be live (anchor was just submitted, within 60s).
        assert!(reg.check_liveness(&id, 60));

        // With a very tight window (0 seconds), it might or might not pass
        // depending on timing. Use 1 second to be safe.
        // But with max_age=0 it should be fine since timestamp == now.
        // Let's just test that a very old anchor fails by using a stale one.
        let mut stale_anchor = make_anchor(id, 200, 2);
        stale_anchor.timestamp = 1_000_000; // Way in the past (Jan 1970).
        reg.submit_anchor(stale_anchor).unwrap();

        assert!(!reg.check_liveness(&id, 60));
    }

    // ── 13. List subnets ────────────────────────────────────────────────

    #[test]
    fn test_list_subnets() {
        let reg = SubnetRegistry::new();

        // Empty registry.
        assert!(reg.list_subnets().is_empty());

        // Add three subnets.
        reg.register_subnet(make_subnet(subnet_id(20), addr(0x01)))
            .unwrap();
        reg.register_subnet(make_subnet(subnet_id(21), addr(0x02)))
            .unwrap();
        reg.register_subnet(make_subnet(subnet_id(22), addr(0x03)))
            .unwrap();

        let all = reg.list_subnets();
        assert_eq!(all.len(), 3);

        // active_subnet_count should be 0 since they're all Pending.
        assert_eq!(reg.active_subnet_count(), 0);
    }

    // ── 14. Get anchor at specific height ───────────────────────────────

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

        // Find exact height.
        let a = reg.get_anchor_at_height(&id, 200).expect("should find height 200");
        assert_eq!(a.subnet_height, 200);
        assert_eq!(a.anchor_height, 2);

        // Non-existent height returns None.
        assert!(reg.get_anchor_at_height(&id, 150).is_none());
    }
}
