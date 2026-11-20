use serde::{Deserialize, Serialize};

pub type EvmAddress = [u8; 20];

pub const ZERO_EVM_ADDRESS: EvmAddress = [0u8; 20];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ChainId {
    ArcMainnet,
    Ethereum,
    Base,
    Arbitrum,
    Optimism,
    Polygon,
    Bsc,
    Avalanche,
    Custom(u64),
}

impl ChainId {
    pub fn evm_chain_id(&self) -> u64 {
        match self {
            ChainId::ArcMainnet => 0xA4C,
            ChainId::Ethereum => 1,
            ChainId::Base => 8453,
            ChainId::Arbitrum => 42161,
            ChainId::Optimism => 10,
            ChainId::Polygon => 137,
            ChainId::Bsc => 56,
            ChainId::Avalanche => 43114,
            ChainId::Custom(id) => *id,
        }
    }

    pub fn is_evm_compatible(&self) -> bool {
        true
    }

    pub fn name(&self) -> &str {
        match self {
            ChainId::ArcMainnet => "ARC Mainnet",
            ChainId::Ethereum => "Ethereum",
            ChainId::Base => "Base",
            ChainId::Arbitrum => "Arbitrum One",
            ChainId::Optimism => "Optimism",
            ChainId::Polygon => "Polygon",
            ChainId::Bsc => "BNB Smart Chain",
            ChainId::Avalanche => "Avalanche C-Chain",
            ChainId::Custom(id) => {
                let _ = id;
                "Custom EVM Chain"
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum BridgeStatus {
    Pending,
    SourceLocked,
    Proving,
    DestMinted,
    Completed,
    Failed,
    Expired,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BridgeTransfer {
    pub transfer_id: [u8; 32],
    pub source_chain: ChainId,
    pub dest_chain: ChainId,
    pub source_address: EvmAddress,
    pub dest_address: EvmAddress,
    pub token_address: EvmAddress,
    pub amount: u128,
    pub nonce: u64,
    pub timestamp: u64,
    pub status: BridgeStatus,
}

impl BridgeTransfer {
    pub fn new(
        source: ChainId,
        dest: ChainId,
        from: EvmAddress,
        to: EvmAddress,
        token: EvmAddress,
        amount: u128,
        nonce: u64,
    ) -> Self {
        let timestamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();

        let mut transfer = Self {
            transfer_id: [0u8; 32],
            source_chain: source,
            dest_chain: dest,
            source_address: from,
            dest_address: to,
            token_address: token,
            amount,
            nonce,
            timestamp,
            status: BridgeStatus::Pending,
        };
        transfer.transfer_id = transfer.compute_transfer_id();
        transfer
    }

    pub fn compute_transfer_id(&self) -> [u8; 32] {
        let mut hasher = blake3::Hasher::new_derive_key("ARC-bridge-transfer-v1");
        hasher.update(&self.source_chain.evm_chain_id().to_le_bytes());
        hasher.update(&self.dest_chain.evm_chain_id().to_le_bytes());
        hasher.update(&self.source_address);
        hasher.update(&self.dest_address);
        hasher.update(&self.token_address);
        hasher.update(&self.amount.to_le_bytes());
        hasher.update(&self.nonce.to_le_bytes());
        hasher.update(&self.timestamp.to_le_bytes());
        *hasher.finalize().as_bytes()
    }

    pub fn is_native_token(&self) -> bool {
        self.token_address == ZERO_EVM_ADDRESS
    }

    pub fn is_finalized(&self) -> bool {
        matches!(
            self.status,
            BridgeStatus::Completed | BridgeStatus::Failed | BridgeStatus::Expired
        )
    }

    pub fn estimated_fee(&self, config: &BridgeConfig) -> u128 {
        self.amount.saturating_mul(config.fee_bps as u128) / 10_000
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BridgeProof {
    pub transfer_id: [u8; 32],
    pub source_block_hash: [u8; 32],
    pub source_block_height: u64,
    pub merkle_siblings: Vec<[u8; 32]>,
    pub leaf_index: u64,
    pub leaf_hash: [u8; 32],
}

impl BridgeProof {
    pub fn verify(&self, expected_root: &[u8; 32]) -> bool {
        let mut current = self.leaf_hash;
        let mut index = self.leaf_index;

        for sibling in &self.merkle_siblings {
            let mut hasher = blake3::Hasher::new();
            if index.is_multiple_of(2) {
                hasher.update(&current);
                hasher.update(sibling);
            } else {
                hasher.update(sibling);
                hasher.update(&current);
            }
            current = *hasher.finalize().as_bytes();
            index /= 2;
        }

        current == *expected_root
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BridgeRelayMessage {
    pub transfer: BridgeTransfer,
    pub proof: BridgeProof,
    pub relayer_address: EvmAddress,
    pub relayer_signature: Vec<u8>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BridgeConfig {
    pub source_chain: ChainId,
    pub dest_chain: ChainId,
    pub bridge_contract: EvmAddress,
    pub min_confirmations: u32,
    pub max_transfer_amount: u128,
    pub min_transfer_amount: u128,
    pub fee_bps: u16,
    pub timeout_blocks: u64,
    pub is_active: bool,
}

impl BridgeConfig {
    pub fn default_eth_to_arc() -> Self {
        Self {
            source_chain: ChainId::Ethereum,
            dest_chain: ChainId::ArcMainnet,
            bridge_contract: [0u8; 20],
            min_confirmations: 12,
            max_transfer_amount: 1_000_000_000_000_000_000_000,
            min_transfer_amount: 10_000_000_000_000_000,
            fee_bps: 30,
            timeout_blocks: 7200,
            is_active: true,
        }
    }

    pub fn default_arc_to_eth() -> Self {
        Self {
            source_chain: ChainId::ArcMainnet,
            dest_chain: ChainId::Ethereum,
            bridge_contract: [0u8; 20],
            min_confirmations: 1,
            max_transfer_amount: 1_000_000_000_000_000_000_000,
            min_transfer_amount: 10_000_000_000_000_000,
            fee_bps: 30,
            timeout_blocks: 14400,
            is_active: true,
        }
    }

    pub fn is_amount_valid(&self, amount: u128) -> bool {
        amount >= self.min_transfer_amount && amount <= self.max_transfer_amount
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TokenMapping {
    pub source_chain: ChainId,
    pub dest_chain: ChainId,
    pub source_token: EvmAddress,
    pub dest_token: EvmAddress,
    pub decimals_source: u8,
    pub decimals_dest: u8,
    pub symbol: String,
    pub name: String,
}

#[derive(Debug, Clone, Default)]
pub struct BridgeStats {
    pub total_transfers: u64,
    pub total_volume: u128,
    pub pending_transfers: u64,
    pub completed_transfers: u64,
    pub failed_transfers: u64,
    pub unique_users: u64,
}

impl BridgeStats {
    pub fn record_transfer(&mut self, transfer: &BridgeTransfer) {
        self.total_transfers += 1;
        self.total_volume = self.total_volume.saturating_add(transfer.amount);

        match transfer.status {
            BridgeStatus::Completed => {
                self.completed_transfers += 1;
            }
            BridgeStatus::Failed | BridgeStatus::Expired => {
                self.failed_transfers += 1;
            }
            _ => {
                self.pending_transfers += 1;
            }
        }
    }

    pub fn success_rate(&self) -> f64 {
        let finalized = self.completed_transfers + self.failed_transfers;
        if finalized == 0 {
            return 0.0;
        }
        self.completed_transfers as f64 / finalized as f64
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_evm_addr(n: u8) -> EvmAddress {
        let mut addr = [0u8; 20];
        addr[19] = n;
        addr
    }

    #[test]
    fn test_chain_id_evm_ids() {
        assert_eq!(ChainId::Ethereum.evm_chain_id(), 1);
        assert_eq!(ChainId::Base.evm_chain_id(), 8453);
        assert_eq!(ChainId::Arbitrum.evm_chain_id(), 42161);
        assert_eq!(ChainId::Optimism.evm_chain_id(), 10);
        assert_eq!(ChainId::Polygon.evm_chain_id(), 137);
        assert_eq!(ChainId::Bsc.evm_chain_id(), 56);
        assert_eq!(ChainId::Avalanche.evm_chain_id(), 43114);
        assert_eq!(ChainId::ArcMainnet.evm_chain_id(), 0xA4C);
        assert_eq!(ChainId::Custom(999).evm_chain_id(), 999);

        assert!(ChainId::Ethereum.is_evm_compatible());
        assert!(ChainId::ArcMainnet.is_evm_compatible());
        assert!(ChainId::Custom(42).is_evm_compatible());

        assert_eq!(ChainId::Ethereum.name(), "Ethereum");
        assert_eq!(ChainId::ArcMainnet.name(), "ARC Mainnet");
        assert_eq!(ChainId::Custom(777).name(), "Custom EVM Chain");
    }

    #[test]
    fn test_bridge_transfer_creation() {
        let from = test_evm_addr(1);
        let to = test_evm_addr(2);
        let token = test_evm_addr(0xAA);

        let transfer = BridgeTransfer::new(
            ChainId::Ethereum,
            ChainId::ArcMainnet,
            from,
            to,
            token,
            1_000_000_000_000_000_000,
            0,
        );

        assert_eq!(transfer.source_chain, ChainId::Ethereum);
        assert_eq!(transfer.dest_chain, ChainId::ArcMainnet);
        assert_eq!(transfer.source_address, from);
        assert_eq!(transfer.dest_address, to);
        assert_eq!(transfer.token_address, token);
        assert_eq!(transfer.amount, 1_000_000_000_000_000_000);
        assert_eq!(transfer.nonce, 0);
        assert_eq!(transfer.status, BridgeStatus::Pending);
        assert_ne!(transfer.transfer_id, [0u8; 32]);
    }

    #[test]
    fn test_transfer_id_deterministic() {
        let from = test_evm_addr(1);
        let to = test_evm_addr(2);
        let token = ZERO_EVM_ADDRESS;

        let mut a = BridgeTransfer::new(
            ChainId::Ethereum,
            ChainId::ArcMainnet,
            from,
            to,
            token,
            500,
            1,
        );
        a.timestamp = 1_700_000_000;
        a.transfer_id = a.compute_transfer_id();

        let mut b = BridgeTransfer::new(
            ChainId::Ethereum,
            ChainId::ArcMainnet,
            from,
            to,
            token,
            500,
            1,
        );
        b.timestamp = 1_700_000_000;
        b.transfer_id = b.compute_transfer_id();

        assert_eq!(a.transfer_id, b.transfer_id);
    }

    #[test]
    fn test_transfer_id_changes_with_nonce() {
        let from = test_evm_addr(1);
        let to = test_evm_addr(2);
        let token = ZERO_EVM_ADDRESS;

        let mut a = BridgeTransfer::new(
            ChainId::Ethereum,
            ChainId::ArcMainnet,
            from,
            to,
            token,
            500,
            0,
        );
        a.timestamp = 1_700_000_000;
        a.transfer_id = a.compute_transfer_id();

        let mut b = BridgeTransfer::new(
            ChainId::Ethereum,
            ChainId::ArcMainnet,
            from,
            to,
            token,
            500,
            1,
        );
        b.timestamp = 1_700_000_000;
        b.transfer_id = b.compute_transfer_id();

        assert_ne!(a.transfer_id, b.transfer_id);
    }

    #[test]
    fn test_native_token_detection() {
        let native = BridgeTransfer::new(
            ChainId::Ethereum,
            ChainId::ArcMainnet,
            test_evm_addr(1),
            test_evm_addr(2),
            ZERO_EVM_ADDRESS,
            1000,
            0,
        );
        assert!(native.is_native_token());

        let erc20 = BridgeTransfer::new(
            ChainId::Ethereum,
            ChainId::ArcMainnet,
            test_evm_addr(1),
            test_evm_addr(2),
            test_evm_addr(0xAA),
            1000,
            0,
        );
        assert!(!erc20.is_native_token());
    }

    #[test]
    fn test_bridge_proof_verification() {
        let leaves: Vec<[u8; 32]> = (0u8..4).map(|i| *blake3::hash(&[i]).as_bytes()).collect();

        let h01 = {
            let mut h = blake3::Hasher::new();
            h.update(&leaves[0]);
            h.update(&leaves[1]);
            *h.finalize().as_bytes()
        };
        let h23 = {
            let mut h = blake3::Hasher::new();
            h.update(&leaves[2]);
            h.update(&leaves[3]);
            *h.finalize().as_bytes()
        };
        let root = {
            let mut h = blake3::Hasher::new();
            h.update(&h01);
            h.update(&h23);
            *h.finalize().as_bytes()
        };

        let proof = BridgeProof {
            transfer_id: [0u8; 32],
            source_block_hash: [0u8; 32],
            source_block_height: 100,
            merkle_siblings: vec![leaves[1], h23],
            leaf_index: 0,
            leaf_hash: leaves[0],
        };

        assert!(proof.verify(&root));
    }

    #[test]
    fn test_bridge_proof_invalid() {
        let leaves: Vec<[u8; 32]> = (0u8..4).map(|i| *blake3::hash(&[i]).as_bytes()).collect();

        let h01 = {
            let mut h = blake3::Hasher::new();
            h.update(&leaves[0]);
            h.update(&leaves[1]);
            *h.finalize().as_bytes()
        };
        let h23 = {
            let mut h = blake3::Hasher::new();
            h.update(&leaves[2]);
            h.update(&leaves[3]);
            *h.finalize().as_bytes()
        };
        let root = {
            let mut h = blake3::Hasher::new();
            h.update(&h01);
            h.update(&h23);
            *h.finalize().as_bytes()
        };

        let proof = BridgeProof {
            transfer_id: [0u8; 32],
            source_block_hash: [0u8; 32],
            source_block_height: 100,
            merkle_siblings: vec![leaves[1], h23],
            leaf_index: 0,
            leaf_hash: *blake3::hash(b"tampered").as_bytes(),
        };

        assert!(!proof.verify(&root));

        let proof2 = BridgeProof {
            transfer_id: [0u8; 32],
            source_block_hash: [0u8; 32],
            source_block_height: 100,
            merkle_siblings: vec![*blake3::hash(b"wrong-sibling").as_bytes(), h23],
            leaf_index: 0,
            leaf_hash: leaves[0],
        };

        assert!(!proof2.verify(&root));
    }

    #[test]
    fn test_bridge_config_amount_validation() {
        let config = BridgeConfig::default_eth_to_arc();

        assert!(!config.is_amount_valid(1_000));
        assert!(config.is_amount_valid(config.min_transfer_amount));
        assert!(config.is_amount_valid(1_000_000_000_000_000_000));
        assert!(config.is_amount_valid(config.max_transfer_amount));
        assert!(!config.is_amount_valid(config.max_transfer_amount + 1));
    }

    #[test]
    fn test_bridge_stats_tracking() {
        let mut stats = BridgeStats::default();

        let t1 = BridgeTransfer::new(
            ChainId::Ethereum,
            ChainId::ArcMainnet,
            test_evm_addr(1),
            test_evm_addr(2),
            ZERO_EVM_ADDRESS,
            1000,
            0,
        );
        stats.record_transfer(&t1);
        assert_eq!(stats.total_transfers, 1);
        assert_eq!(stats.pending_transfers, 1);
        assert_eq!(stats.total_volume, 1000);

        let mut t2 = BridgeTransfer::new(
            ChainId::Ethereum,
            ChainId::ArcMainnet,
            test_evm_addr(3),
            test_evm_addr(4),
            ZERO_EVM_ADDRESS,
            2000,
            0,
        );
        t2.status = BridgeStatus::Completed;
        stats.record_transfer(&t2);
        assert_eq!(stats.total_transfers, 2);
        assert_eq!(stats.completed_transfers, 1);
        assert_eq!(stats.total_volume, 3000);

        let mut t3 = BridgeTransfer::new(
            ChainId::Ethereum,
            ChainId::ArcMainnet,
            test_evm_addr(5),
            test_evm_addr(6),
            ZERO_EVM_ADDRESS,
            500,
            0,
        );
        t3.status = BridgeStatus::Failed;
        stats.record_transfer(&t3);
        assert_eq!(stats.total_transfers, 3);
        assert_eq!(stats.failed_transfers, 1);
    }

    #[test]
    fn test_fee_calculation() {
        let config = BridgeConfig {
            fee_bps: 100,
            ..BridgeConfig::default_eth_to_arc()
        };

        let transfer = BridgeTransfer::new(
            ChainId::Ethereum,
            ChainId::ArcMainnet,
            test_evm_addr(1),
            test_evm_addr(2),
            ZERO_EVM_ADDRESS,
            1_000_000_000_000_000_000,
            0,
        );

        let fee = transfer.estimated_fee(&config);
        assert_eq!(fee, 10_000_000_000_000_000);

        let default_config = BridgeConfig::default_eth_to_arc();
        let fee_30bps = transfer.estimated_fee(&default_config);
        assert_eq!(fee_30bps, 3_000_000_000_000_000);
    }

    #[test]
    fn test_bridge_status_transitions() {
        let mut transfer = BridgeTransfer::new(
            ChainId::Ethereum,
            ChainId::ArcMainnet,
            test_evm_addr(1),
            test_evm_addr(2),
            ZERO_EVM_ADDRESS,
            1000,
            0,
        );

        assert_eq!(transfer.status, BridgeStatus::Pending);
        assert!(!transfer.is_finalized());

        transfer.status = BridgeStatus::SourceLocked;
        assert!(!transfer.is_finalized());

        transfer.status = BridgeStatus::Proving;
        assert!(!transfer.is_finalized());

        transfer.status = BridgeStatus::DestMinted;
        assert!(!transfer.is_finalized());

        transfer.status = BridgeStatus::Completed;
        assert!(transfer.is_finalized());

        transfer.status = BridgeStatus::Failed;
        assert!(transfer.is_finalized());

        transfer.status = BridgeStatus::Expired;
        assert!(transfer.is_finalized());
    }

    #[test]
    fn test_token_mapping() {
        let mapping = TokenMapping {
            source_chain: ChainId::Ethereum,
            dest_chain: ChainId::ArcMainnet,
            source_token: test_evm_addr(0xAA),
            dest_token: test_evm_addr(0xBB),
            decimals_source: 18,
            decimals_dest: 18,
            symbol: "WETH".to_string(),
            name: "Wrapped Ether".to_string(),
        };

        assert_eq!(mapping.source_chain, ChainId::Ethereum);
        assert_eq!(mapping.dest_chain, ChainId::ArcMainnet);
        assert_eq!(mapping.source_token[19], 0xAA);
        assert_eq!(mapping.dest_token[19], 0xBB);
        assert_eq!(mapping.decimals_source, 18);
        assert_eq!(mapping.decimals_dest, 18);
        assert_eq!(mapping.symbol, "WETH");
        assert_eq!(mapping.name, "Wrapped Ether");

        let encoded = bincode::serialize(&mapping).unwrap();
        let recovered: TokenMapping = bincode::deserialize(&encoded).unwrap();
        assert_eq!(recovered.symbol, "WETH");
        assert_eq!(recovered.source_token, mapping.source_token);
    }

    #[test]
    fn test_success_rate() {
        let mut stats = BridgeStats::default();
        assert!(stats.success_rate().abs() < f64::EPSILON);

        stats.completed_transfers = 9;
        stats.failed_transfers = 1;
        assert!((stats.success_rate() - 0.9).abs() < f64::EPSILON);

        stats.completed_transfers = 0;
        stats.failed_transfers = 5;
        assert!(stats.success_rate().abs() < f64::EPSILON);
    }
}
