use crate::Account;
use arc_crypto::Hash256;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Block header — compact representation anchoring all transactions.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BlockHeader {
    /// Block height (monotonically increasing).
    pub height: u64,
    /// Timestamp (unix millis).
    pub timestamp: u64,
    /// Hash of the previous block header.
    pub parent_hash: Hash256,
    /// Merkle root of all transaction hashes in this block.
    pub tx_root: Hash256,
    /// Merkle root of the state tree after applying this block.
    pub state_root: Hash256,
    /// Aggregate proof hash (ZK batch proof for this block's transactions).
    pub proof_hash: Hash256,
    /// Number of transactions in this block.
    pub tx_count: u32,
    /// Block producer (validator address).
    pub producer: Hash256,
    /// Protocol version governing this block's rules.
    #[serde(default = "ProtocolVersion::genesis_default")]
    pub protocol_version: ProtocolVersion,
    /// Optional state diff for propose-verify mode.
    /// When present, verifiers apply the diff instead of re-executing.
    #[serde(default)]
    pub state_diff: Option<StateDiff>,
}

/// A full block including header and transaction hashes.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Block {
    pub header: BlockHeader,
    /// Transaction hashes included in this block (ordered).
    pub tx_hashes: Vec<Hash256>,
    /// Block hash (BLAKE3 of the serialized header).
    pub hash: Hash256,
}

impl Block {
    /// Create a new block from a header and transaction hashes.
    pub fn new(header: BlockHeader, tx_hashes: Vec<Hash256>) -> Self {
        let hash = Self::compute_hash(&header);
        Self {
            header,
            tx_hashes,
            hash,
        }
    }

    /// Compute the block hash from the header.
    pub fn compute_hash(header: &BlockHeader) -> Hash256 {
        let bytes = bincode::serialize(header).expect("serializable header");
        let mut hasher = blake3::Hasher::new_derive_key("ARC-chain-block-v1");
        hasher.update(&bytes);
        Hash256(*hasher.finalize().as_bytes())
    }

    /// Genesis block (block 0).
    pub fn genesis() -> Self {
        let header = BlockHeader {
            height: 0,
            timestamp: 0,
            parent_hash: Hash256::ZERO,
            tx_root: Hash256::ZERO,
            state_root: Hash256::ZERO,
            proof_hash: Hash256::ZERO,
            tx_count: 0,
            producer: Hash256::ZERO,
            protocol_version: ProtocolVersion::GENESIS,
            state_diff: None,
        };
        Self::new(header, Vec::new())
    }
}

// ─── Propose-Verify Protocol ──────────────────────────────────────────────────

/// A state diff produced by the block proposer after execution.
///
/// Contains the set of accounts that changed during block execution and the
/// resulting state root.  Verifiers apply the diff to their local state copy
/// and confirm the root matches — O(k) verification instead of full re-execution.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct StateDiff {
    /// Accounts that were created or modified.
    pub changes: Vec<AccountChange>,
    /// The expected state root after applying all changes.
    pub new_root: Hash256,
}

/// A single account change within a state diff.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AccountChange {
    /// The account address (key).
    pub address: Hash256,
    /// The new account state after the block.
    pub account: Account,
}

// ─── Protocol Versioning & Upgrade Scheduling ────────────────────────────────

/// Protocol version identifier — follows semantic versioning (major.minor.patch).
///
/// Breaking consensus changes increment `major`; backward-compatible rule
/// additions increment `minor`; non-consensus fixes increment `patch`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct ProtocolVersion {
    pub major: u16,
    pub minor: u16,
    pub patch: u16,
}

impl ProtocolVersion {
    /// Genesis protocol version (0.1.0).
    pub const GENESIS: Self = Self {
        major: 0,
        minor: 1,
        patch: 0,
    };

    /// First stable release (1.0.0).
    pub const V1: Self = Self {
        major: 1,
        minor: 0,
        patch: 0,
    };

    /// Create a new protocol version.
    pub fn new(major: u16, minor: u16, patch: u16) -> Self {
        Self {
            major,
            minor,
            patch,
        }
    }

    /// Two versions are compatible if they share the same major version.
    pub fn is_compatible_with(&self, other: &Self) -> bool {
        self.major == other.major
    }

    /// Serde default helper — returns GENESIS.  Used by `#[serde(default)]` on
    /// `BlockHeader::protocol_version` so that blocks serialized before this
    /// field existed deserialize without error.
    fn genesis_default() -> Self {
        Self::GENESIS
    }
}

impl std::fmt::Display for ProtocolVersion {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)
    }
}

/// Feature flags for conditional behaviour across protocol versions.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum FeatureFlag {
    /// Block-STM parallel execution engine.
    BlockSTM,
    /// Post-quantum Falcon signatures.
    FalconSignatures,
    /// Encrypted mempool for MEV protection.
    EncryptedMempool,
    /// Jellyfish Merkle Tree incremental state.
    JmtState,
    /// Propose-verify consensus mode.
    ProposeVerify,
    /// EVM bytecode runtime support.
    EvmSupport,
    /// Subnet state anchoring to the root chain.
    SubnetAnchoring,
    /// Recursive STARK proof composition.
    RecursiveProofs,
    /// Extensible flag for governance-activated features.
    Custom(String),
}

/// A scheduled protocol upgrade that activates at a specific block height.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProtocolUpgrade {
    /// The protocol version this upgrade transitions to.
    pub version: ProtocolVersion,
    /// Block height at which this upgrade activates.
    pub activation_height: u64,
    /// Human-readable description of the upgrade.
    pub description: String,
    /// Feature flags enabled by this upgrade.
    pub features: Vec<FeatureFlag>,
}

/// Manages the full upgrade schedule and resolves the active protocol version
/// for any block height.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UpgradeSchedule {
    /// Height-ordered map of scheduled upgrades.
    pub upgrades: BTreeMap<u64, ProtocolUpgrade>,
    /// The version currently considered "active" by the node.
    pub current_version: ProtocolVersion,
}

impl UpgradeSchedule {
    /// Create a new, empty upgrade schedule starting at the genesis version.
    pub fn new() -> Self {
        Self {
            upgrades: BTreeMap::new(),
            current_version: ProtocolVersion::GENESIS,
        }
    }

    /// Schedule a future protocol upgrade.
    ///
    /// Returns `Err` if:
    /// - An upgrade is already scheduled at the same activation height.
    /// - The activation height is at or below the height of the most recent
    ///   already-activated upgrade (i.e. trying to schedule in the past).
    pub fn schedule_upgrade(&mut self, upgrade: ProtocolUpgrade) -> Result<(), String> {
        let height = upgrade.activation_height;

        // Reject if an upgrade already occupies this height.
        if self.upgrades.contains_key(&height) {
            return Err(format!(
                "an upgrade is already scheduled at height {}",
                height
            ));
        }

        // Reject if scheduling at or below any already-activated height.
        // "Already activated" means the height is <= the highest key whose
        // version matches `current_version`.
        if let Some((&last_activated_height, _last_activated)) = self
            .upgrades
            .iter()
            .rev()
            .find(|(_, u)| u.version <= self.current_version)
        {
            if height <= last_activated_height {
                return Err(format!(
                    "cannot schedule upgrade at height {} — already past activated height {}",
                    height, last_activated_height
                ));
            }
        }

        self.upgrades.insert(height, upgrade);
        Ok(())
    }

    /// Return the protocol version that governs a given block height.
    ///
    /// Walks the schedule in reverse and returns the version of the latest
    /// upgrade whose activation height is <= `height`.  Falls back to GENESIS
    /// if no upgrade has activated yet.
    pub fn version_at_height(&self, height: u64) -> ProtocolVersion {
        self.upgrades
            .range(..=height)
            .next_back()
            .map(|(_, u)| u.version)
            .unwrap_or(ProtocolVersion::GENESIS)
    }

    /// List all upgrades whose activation height is strictly greater than
    /// `current_height` (i.e. upgrades that have not yet activated).
    pub fn pending_upgrades(&self, current_height: u64) -> Vec<&ProtocolUpgrade> {
        self.upgrades
            .range((current_height + 1)..)
            .map(|(_, u)| u)
            .collect()
    }

    /// Check whether a given feature flag is active at `height`.
    ///
    /// A feature is active if **any** upgrade with activation height <= `height`
    /// includes it in its `features` list.
    pub fn is_feature_active(&self, flag: &FeatureFlag, height: u64) -> bool {
        self.upgrades
            .range(..=height)
            .any(|(_, u)| u.features.contains(flag))
    }

    /// Return the next upgrade that has not yet activated relative to
    /// `current_height`, if any.
    pub fn next_upgrade(&self, current_height: u64) -> Option<&ProtocolUpgrade> {
        self.upgrades
            .range((current_height + 1)..)
            .next()
            .map(|(_, u)| u)
    }

    /// Advance `current_version` to the latest upgrade whose activation height
    /// is <= `current_height`.
    pub fn activate_upgrades(&mut self, current_height: u64) {
        if let Some((_, upgrade)) = self.upgrades.range(..=current_height).next_back() {
            if upgrade.version > self.current_version {
                self.current_version = upgrade.version;
            }
        }
    }
}

impl Default for UpgradeSchedule {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── Existing tests ───────────────────────────────────────────────────

    #[test]
    fn test_genesis() {
        let genesis = Block::genesis();
        assert_eq!(genesis.header.height, 0);
        assert_ne!(genesis.hash, Hash256::ZERO);
    }

    #[test]
    fn test_block_hash_deterministic() {
        let a = Block::genesis();
        let b = Block::genesis();
        assert_eq!(a.hash, b.hash);
    }

    // ── Protocol versioning tests ────────────────────────────────────────

    #[test]
    fn test_protocol_version_ordering() {
        let v1_0 = ProtocolVersion::new(1, 0, 0);
        let v1_1 = ProtocolVersion::new(1, 1, 0);
        let v2_0 = ProtocolVersion::new(2, 0, 0);

        assert!(v1_0 < v1_1);
        assert!(v1_1 < v2_0);
        assert!(v1_0 < v2_0);
    }

    #[test]
    fn test_protocol_version_compatibility() {
        let v1_0 = ProtocolVersion::new(1, 0, 0);
        let v1_5 = ProtocolVersion::new(1, 5, 3);
        let v2_0 = ProtocolVersion::new(2, 0, 0);

        assert!(v1_0.is_compatible_with(&v1_5));
        assert!(v1_5.is_compatible_with(&v1_0));
        assert!(!v1_0.is_compatible_with(&v2_0));
    }

    #[test]
    fn test_protocol_version_display() {
        assert_eq!(ProtocolVersion::GENESIS.to_string(), "0.1.0");
        assert_eq!(ProtocolVersion::V1.to_string(), "1.0.0");
        assert_eq!(ProtocolVersion::new(3, 14, 159).to_string(), "3.14.159");
    }

    #[test]
    fn test_upgrade_schedule_empty() {
        let schedule = UpgradeSchedule::new();
        assert_eq!(schedule.current_version, ProtocolVersion::GENESIS);
        assert!(schedule.upgrades.is_empty());
        assert_eq!(schedule.version_at_height(0), ProtocolVersion::GENESIS);
        assert_eq!(schedule.version_at_height(999), ProtocolVersion::GENESIS);
    }

    #[test]
    fn test_schedule_upgrade() {
        let mut schedule = UpgradeSchedule::new();
        let upgrade = ProtocolUpgrade {
            version: ProtocolVersion::V1,
            activation_height: 1000,
            description: "First stable release".into(),
            features: vec![FeatureFlag::BlockSTM, FeatureFlag::JmtState],
        };
        assert!(schedule.schedule_upgrade(upgrade).is_ok());
        assert_eq!(schedule.upgrades.len(), 1);
        assert!(schedule.upgrades.contains_key(&1000));
    }

    #[test]
    fn test_version_at_height() {
        let mut schedule = UpgradeSchedule::new();
        schedule
            .schedule_upgrade(ProtocolUpgrade {
                version: ProtocolVersion::V1,
                activation_height: 1000,
                description: "V1".into(),
                features: vec![],
            })
            .unwrap();
        schedule
            .schedule_upgrade(ProtocolUpgrade {
                version: ProtocolVersion::new(2, 0, 0),
                activation_height: 5000,
                description: "V2".into(),
                features: vec![],
            })
            .unwrap();

        // Before any upgrade → GENESIS.
        assert_eq!(schedule.version_at_height(0), ProtocolVersion::GENESIS);
        assert_eq!(schedule.version_at_height(999), ProtocolVersion::GENESIS);

        // At and after V1 activation.
        assert_eq!(schedule.version_at_height(1000), ProtocolVersion::V1);
        assert_eq!(schedule.version_at_height(2500), ProtocolVersion::V1);

        // At and after V2 activation.
        assert_eq!(
            schedule.version_at_height(5000),
            ProtocolVersion::new(2, 0, 0)
        );
        assert_eq!(
            schedule.version_at_height(10000),
            ProtocolVersion::new(2, 0, 0)
        );
    }

    #[test]
    fn test_pending_upgrades() {
        let mut schedule = UpgradeSchedule::new();
        schedule
            .schedule_upgrade(ProtocolUpgrade {
                version: ProtocolVersion::V1,
                activation_height: 1000,
                description: "V1".into(),
                features: vec![],
            })
            .unwrap();
        schedule
            .schedule_upgrade(ProtocolUpgrade {
                version: ProtocolVersion::new(2, 0, 0),
                activation_height: 5000,
                description: "V2".into(),
                features: vec![],
            })
            .unwrap();

        // At height 0, both are pending.
        let pending = schedule.pending_upgrades(0);
        assert_eq!(pending.len(), 2);

        // At height 999, both still pending.
        assert_eq!(schedule.pending_upgrades(999).len(), 2);

        // At height 1000, V1 is no longer pending — only V2.
        let pending = schedule.pending_upgrades(1000);
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].version, ProtocolVersion::new(2, 0, 0));

        // At height 5000, nothing is pending.
        assert!(schedule.pending_upgrades(5000).is_empty());
    }

    #[test]
    fn test_feature_flags() {
        let mut schedule = UpgradeSchedule::new();
        schedule
            .schedule_upgrade(ProtocolUpgrade {
                version: ProtocolVersion::V1,
                activation_height: 1000,
                description: "V1".into(),
                features: vec![FeatureFlag::BlockSTM, FeatureFlag::JmtState],
            })
            .unwrap();
        schedule
            .schedule_upgrade(ProtocolUpgrade {
                version: ProtocolVersion::new(2, 0, 0),
                activation_height: 5000,
                description: "V2".into(),
                features: vec![FeatureFlag::FalconSignatures, FeatureFlag::EncryptedMempool],
            })
            .unwrap();

        // Before V1 — nothing active.
        assert!(!schedule.is_feature_active(&FeatureFlag::BlockSTM, 999));
        assert!(!schedule.is_feature_active(&FeatureFlag::FalconSignatures, 999));

        // At V1 — V1 features active.
        assert!(schedule.is_feature_active(&FeatureFlag::BlockSTM, 1000));
        assert!(schedule.is_feature_active(&FeatureFlag::JmtState, 1000));
        assert!(!schedule.is_feature_active(&FeatureFlag::FalconSignatures, 1000));

        // At V2 — all features active (V1 features carry forward).
        assert!(schedule.is_feature_active(&FeatureFlag::BlockSTM, 5000));
        assert!(schedule.is_feature_active(&FeatureFlag::FalconSignatures, 5000));
        assert!(schedule.is_feature_active(&FeatureFlag::EncryptedMempool, 5000));

        // Custom flag.
        schedule
            .schedule_upgrade(ProtocolUpgrade {
                version: ProtocolVersion::new(3, 0, 0),
                activation_height: 10000,
                description: "V3".into(),
                features: vec![FeatureFlag::Custom("my_feature".into())],
            })
            .unwrap();
        assert!(!schedule.is_feature_active(&FeatureFlag::Custom("my_feature".into()), 9999));
        assert!(schedule.is_feature_active(&FeatureFlag::Custom("my_feature".into()), 10000));
    }

    #[test]
    fn test_cannot_schedule_past_upgrade() {
        let mut schedule = UpgradeSchedule::new();
        schedule
            .schedule_upgrade(ProtocolUpgrade {
                version: ProtocolVersion::V1,
                activation_height: 1000,
                description: "V1".into(),
                features: vec![],
            })
            .unwrap();

        // Activate V1.
        schedule.activate_upgrades(1000);
        assert_eq!(schedule.current_version, ProtocolVersion::V1);

        // Attempt to schedule at height 500 (in the past) → error.
        let result = schedule.schedule_upgrade(ProtocolUpgrade {
            version: ProtocolVersion::new(1, 1, 0),
            activation_height: 500,
            description: "should fail".into(),
            features: vec![],
        });
        assert!(result.is_err());

        // Attempt at the same activated height → error.
        let result = schedule.schedule_upgrade(ProtocolUpgrade {
            version: ProtocolVersion::new(1, 1, 0),
            activation_height: 1000,
            description: "should also fail".into(),
            features: vec![],
        });
        assert!(result.is_err());

        // Scheduling in the future should still work.
        assert!(schedule
            .schedule_upgrade(ProtocolUpgrade {
                version: ProtocolVersion::new(1, 1, 0),
                activation_height: 2000,
                description: "future is fine".into(),
                features: vec![],
            })
            .is_ok());
    }

    #[test]
    fn test_activate_upgrades() {
        let mut schedule = UpgradeSchedule::new();
        assert_eq!(schedule.current_version, ProtocolVersion::GENESIS);

        schedule
            .schedule_upgrade(ProtocolUpgrade {
                version: ProtocolVersion::V1,
                activation_height: 1000,
                description: "V1".into(),
                features: vec![],
            })
            .unwrap();
        schedule
            .schedule_upgrade(ProtocolUpgrade {
                version: ProtocolVersion::new(2, 0, 0),
                activation_height: 5000,
                description: "V2".into(),
                features: vec![],
            })
            .unwrap();

        // Before any activation height.
        schedule.activate_upgrades(500);
        assert_eq!(schedule.current_version, ProtocolVersion::GENESIS);

        // At V1 activation.
        schedule.activate_upgrades(1000);
        assert_eq!(schedule.current_version, ProtocolVersion::V1);

        // Between V1 and V2 — still V1.
        schedule.activate_upgrades(3000);
        assert_eq!(schedule.current_version, ProtocolVersion::V1);

        // At V2 activation.
        schedule.activate_upgrades(5000);
        assert_eq!(schedule.current_version, ProtocolVersion::new(2, 0, 0));

        // Well past V2 — stays at V2.
        schedule.activate_upgrades(99999);
        assert_eq!(schedule.current_version, ProtocolVersion::new(2, 0, 0));
    }

    #[test]
    fn test_next_upgrade() {
        let mut schedule = UpgradeSchedule::new();

        // No upgrades → None.
        assert!(schedule.next_upgrade(0).is_none());

        schedule
            .schedule_upgrade(ProtocolUpgrade {
                version: ProtocolVersion::V1,
                activation_height: 1000,
                description: "V1".into(),
                features: vec![],
            })
            .unwrap();
        schedule
            .schedule_upgrade(ProtocolUpgrade {
                version: ProtocolVersion::new(2, 0, 0),
                activation_height: 5000,
                description: "V2".into(),
                features: vec![],
            })
            .unwrap();

        // At height 0, next upgrade is V1 at 1000.
        let next = schedule.next_upgrade(0).unwrap();
        assert_eq!(next.version, ProtocolVersion::V1);
        assert_eq!(next.activation_height, 1000);

        // At height 1000 (V1 just activated), next is V2.
        let next = schedule.next_upgrade(1000).unwrap();
        assert_eq!(next.version, ProtocolVersion::new(2, 0, 0));

        // At height 5000 (V2 just activated), no more upgrades.
        assert!(schedule.next_upgrade(5000).is_none());
    }

    #[test]
    fn test_genesis_block_has_protocol_version() {
        let genesis = Block::genesis();
        assert_eq!(genesis.header.protocol_version, ProtocolVersion::GENESIS);
    }
}
