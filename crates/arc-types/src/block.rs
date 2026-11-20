use crate::Account;
use arc_crypto::Hash256;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BlockHeader {
    pub height: u64,
    pub timestamp: u64,
    pub parent_hash: Hash256,
    pub tx_root: Hash256,
    pub state_root: Hash256,
    pub proof_hash: Hash256,
    pub tx_count: u32,
    pub producer: Hash256,
    #[serde(default = "ProtocolVersion::genesis_default")]
    pub protocol_version: ProtocolVersion,
    #[serde(default)]
    pub state_diff: Option<StateDiff>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Block {
    pub header: BlockHeader,
    pub tx_hashes: Vec<Hash256>,
    pub hash: Hash256,
}

impl Block {
    pub fn new(header: BlockHeader, tx_hashes: Vec<Hash256>) -> Self {
        let hash = Self::compute_hash(&header);
        Self {
            header,
            tx_hashes,
            hash,
        }
    }

    pub fn compute_hash(header: &BlockHeader) -> Hash256 {
        let bytes = bincode::serialize(header).expect("serializable header");
        let mut hasher = blake3::Hasher::new_derive_key("ARC-chain-block-v1");
        hasher.update(&bytes);
        Hash256(*hasher.finalize().as_bytes())
    }

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

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct StateDiff {
    pub changes: Vec<AccountChange>,
    pub new_root: Hash256,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AccountChange {
    pub address: Hash256,
    pub account: Account,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct ProtocolVersion {
    pub major: u16,
    pub minor: u16,
    pub patch: u16,
}

impl ProtocolVersion {
    pub const GENESIS: Self = Self {
        major: 0,
        minor: 1,
        patch: 0,
    };

    pub const V1: Self = Self {
        major: 1,
        minor: 0,
        patch: 0,
    };

    pub fn new(major: u16, minor: u16, patch: u16) -> Self {
        Self {
            major,
            minor,
            patch,
        }
    }

    pub fn is_compatible_with(&self, other: &Self) -> bool {
        self.major == other.major
    }

    fn genesis_default() -> Self {
        Self::GENESIS
    }
}

impl std::fmt::Display for ProtocolVersion {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum FeatureFlag {
    BlockSTM,
    FalconSignatures,
    EncryptedMempool,
    JmtState,
    ProposeVerify,
    EvmSupport,
    SubnetAnchoring,
    RecursiveProofs,
    Custom(String),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProtocolUpgrade {
    pub version: ProtocolVersion,
    pub activation_height: u64,
    pub description: String,
    pub features: Vec<FeatureFlag>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UpgradeSchedule {
    pub upgrades: BTreeMap<u64, ProtocolUpgrade>,
    pub current_version: ProtocolVersion,
}

impl UpgradeSchedule {
    pub fn new() -> Self {
        Self {
            upgrades: BTreeMap::new(),
            current_version: ProtocolVersion::GENESIS,
        }
    }

    pub fn schedule_upgrade(&mut self, upgrade: ProtocolUpgrade) -> Result<(), String> {
        let height = upgrade.activation_height;

        if self.upgrades.contains_key(&height) {
            return Err(format!(
                "an upgrade is already scheduled at height {}",
                height
            ));
        }

        if let Some((&last_activated_height, _last_activated)) = self
            .upgrades
            .iter()
            .rev()
            .find(|(_, u)| u.version <= self.current_version)
            && height <= last_activated_height
        {
            return Err(format!(
                "cannot schedule upgrade at height {} - already past activated height {}",
                height, last_activated_height
            ));
        }

        self.upgrades.insert(height, upgrade);
        Ok(())
    }

    pub fn version_at_height(&self, height: u64) -> ProtocolVersion {
        self.upgrades
            .range(..=height)
            .next_back()
            .map(|(_, u)| u.version)
            .unwrap_or(ProtocolVersion::GENESIS)
    }

    pub fn pending_upgrades(&self, current_height: u64) -> Vec<&ProtocolUpgrade> {
        self.upgrades
            .range((current_height + 1)..)
            .map(|(_, u)| u)
            .collect()
    }

    pub fn is_feature_active(&self, flag: &FeatureFlag, height: u64) -> bool {
        self.upgrades
            .range(..=height)
            .any(|(_, u)| u.features.contains(flag))
    }

    pub fn next_upgrade(&self, current_height: u64) -> Option<&ProtocolUpgrade> {
        self.upgrades
            .range((current_height + 1)..)
            .next()
            .map(|(_, u)| u)
    }

    pub fn activate_upgrades(&mut self, current_height: u64) {
        if let Some((_, upgrade)) = self.upgrades.range(..=current_height).next_back()
            && upgrade.version > self.current_version
        {
            self.current_version = upgrade.version;
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

        assert_eq!(schedule.version_at_height(0), ProtocolVersion::GENESIS);
        assert_eq!(schedule.version_at_height(999), ProtocolVersion::GENESIS);

        assert_eq!(schedule.version_at_height(1000), ProtocolVersion::V1);
        assert_eq!(schedule.version_at_height(2500), ProtocolVersion::V1);

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

        let pending = schedule.pending_upgrades(0);
        assert_eq!(pending.len(), 2);

        assert_eq!(schedule.pending_upgrades(999).len(), 2);

        let pending = schedule.pending_upgrades(1000);
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].version, ProtocolVersion::new(2, 0, 0));

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

        assert!(!schedule.is_feature_active(&FeatureFlag::BlockSTM, 999));
        assert!(!schedule.is_feature_active(&FeatureFlag::FalconSignatures, 999));

        assert!(schedule.is_feature_active(&FeatureFlag::BlockSTM, 1000));
        assert!(schedule.is_feature_active(&FeatureFlag::JmtState, 1000));
        assert!(!schedule.is_feature_active(&FeatureFlag::FalconSignatures, 1000));

        assert!(schedule.is_feature_active(&FeatureFlag::BlockSTM, 5000));
        assert!(schedule.is_feature_active(&FeatureFlag::FalconSignatures, 5000));
        assert!(schedule.is_feature_active(&FeatureFlag::EncryptedMempool, 5000));

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

        schedule.activate_upgrades(1000);
        assert_eq!(schedule.current_version, ProtocolVersion::V1);

        let result = schedule.schedule_upgrade(ProtocolUpgrade {
            version: ProtocolVersion::new(1, 1, 0),
            activation_height: 500,
            description: "should fail".into(),
            features: vec![],
        });
        assert!(result.is_err());

        let result = schedule.schedule_upgrade(ProtocolUpgrade {
            version: ProtocolVersion::new(1, 1, 0),
            activation_height: 1000,
            description: "should also fail".into(),
            features: vec![],
        });
        assert!(result.is_err());

        assert!(
            schedule
                .schedule_upgrade(ProtocolUpgrade {
                    version: ProtocolVersion::new(1, 1, 0),
                    activation_height: 2000,
                    description: "future is fine".into(),
                    features: vec![],
                })
                .is_ok()
        );
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

        schedule.activate_upgrades(500);
        assert_eq!(schedule.current_version, ProtocolVersion::GENESIS);

        schedule.activate_upgrades(1000);
        assert_eq!(schedule.current_version, ProtocolVersion::V1);

        schedule.activate_upgrades(3000);
        assert_eq!(schedule.current_version, ProtocolVersion::V1);

        schedule.activate_upgrades(5000);
        assert_eq!(schedule.current_version, ProtocolVersion::new(2, 0, 0));

        schedule.activate_upgrades(99999);
        assert_eq!(schedule.current_version, ProtocolVersion::new(2, 0, 0));
    }

    #[test]
    fn test_next_upgrade() {
        let mut schedule = UpgradeSchedule::new();

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

        let next = schedule.next_upgrade(0).unwrap();
        assert_eq!(next.version, ProtocolVersion::V1);
        assert_eq!(next.activation_height, 1000);

        let next = schedule.next_upgrade(1000).unwrap();
        assert_eq!(next.version, ProtocolVersion::new(2, 0, 0));

        assert!(schedule.next_upgrade(5000).is_none());
    }

    #[test]
    fn test_genesis_block_has_protocol_version() {
        let genesis = Block::genesis();
        assert_eq!(genesis.header.protocol_version, ProtocolVersion::GENESIS);
    }
}
