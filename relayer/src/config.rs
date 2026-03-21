use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Configuration for the ARC Chain bridge relayer.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RelayerConfig {
    /// Ethereum JSON-RPC endpoint (e.g. "https://mainnet.infura.io/v3/...")
    pub eth_rpc_url: String,
    /// ARC Chain RPC endpoint (e.g. "http://localhost:9090")
    pub arc_rpc_url: String,
    /// ArcBridge.sol contract address on Ethereum
    pub bridge_contract: String,
    /// Ed25519 private key (hex) for signing ARC Chain transactions
    pub relayer_private_key: String,
    /// Ethereum private key (hex) for signing ETH transactions
    pub eth_private_key: String,
    /// Number of block confirmations required before processing a lock event
    pub confirmations: u64,
    /// Polling interval in seconds
    pub poll_interval_secs: u64,
    /// Path to the SQLite database for tracking processed events
    pub db_path: String,
}

impl Default for RelayerConfig {
    fn default() -> Self {
        Self {
            eth_rpc_url: "https://mainnet.infura.io/v3/YOUR_KEY".into(),
            arc_rpc_url: "http://localhost:9090".into(),
            bridge_contract: "0x672fdBA7055bddFa8fD6bD45B1455cE5eB97f499".into(),
            relayer_private_key: String::new(),
            eth_private_key: String::new(),
            confirmations: 12,
            poll_interval_secs: 15,
            db_path: "relayer.db".into(),
        }
    }
}

impl RelayerConfig {
    /// Load configuration from a TOML file at the given path.
    pub fn from_file(path: &PathBuf) -> anyhow::Result<Self> {
        let contents = std::fs::read_to_string(path)?;
        let config: RelayerConfig = toml::from_str(&contents)?;
        Ok(config)
    }

    /// Validate that all required fields are populated.
    pub fn validate(&self) -> anyhow::Result<()> {
        if self.eth_rpc_url.is_empty() {
            anyhow::bail!("eth_rpc_url must be set");
        }
        if self.arc_rpc_url.is_empty() {
            anyhow::bail!("arc_rpc_url must be set");
        }
        if self.bridge_contract.is_empty() {
            anyhow::bail!("bridge_contract must be set");
        }
        if self.relayer_private_key.is_empty() {
            anyhow::bail!("relayer_private_key must be set");
        }
        if self.eth_private_key.is_empty() {
            anyhow::bail!("eth_private_key must be set");
        }
        Ok(())
    }

    /// Parse the bridge contract address into a 20-byte array.
    pub fn bridge_contract_bytes(&self) -> anyhow::Result<[u8; 20]> {
        let stripped = self.bridge_contract.strip_prefix("0x")
            .unwrap_or(&self.bridge_contract);
        let bytes = hex::decode(stripped)?;
        if bytes.len() != 20 {
            anyhow::bail!("bridge_contract must be 20 bytes, got {}", bytes.len());
        }
        let mut addr = [0u8; 20];
        addr.copy_from_slice(&bytes);
        Ok(addr)
    }
}
