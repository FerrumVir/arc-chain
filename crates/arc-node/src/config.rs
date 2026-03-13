use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

// ─── Genesis Configuration ──────────────────────────────────────────────

/// Top-level genesis configuration loaded from a TOML file.
/// Defines the initial chain state: prefunded accounts, validators, and chain metadata.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct GenesisConfig {
    pub chain: ChainInfo,
    #[serde(default)]
    pub accounts: Vec<GenesisAccount>,
    #[serde(default)]
    pub validators: Vec<GenesisValidator>,
}

/// Chain identity and metadata baked into the genesis block.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ChainInfo {
    pub name: String,
    #[serde(default = "default_chain_id")]
    pub chain_id: String,
}

/// A prefunded account in the genesis state.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct GenesisAccount {
    /// 64-character hex string (32 bytes) — the account address.
    pub address: String,
    /// Initial balance in ARC (smallest unit).
    pub balance: u64,
}

/// A validator included in the genesis validator set.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct GenesisValidator {
    /// Seed string used to deterministically derive the validator keypair.
    pub seed: String,
    #[serde(default = "default_stake")]
    pub stake: u64,
}

// ─── Node Configuration ────────────────────────────────────────────────

/// Top-level node runtime configuration loaded from a TOML file.
/// All sections are optional and fall back to defaults matching the CLI defaults.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct NodeConfig {
    #[serde(default)]
    pub rpc: RpcConfig,
    #[serde(default)]
    pub p2p: P2pConfig,
    #[serde(default)]
    pub validator: ValidatorConfig,
    #[serde(default)]
    pub storage: StorageConfig,
    #[serde(default)]
    pub benchmark: BenchmarkConfig,
}

/// RPC server configuration.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct RpcConfig {
    /// Listen address for the native ARC RPC (default: "0.0.0.0:9090").
    #[serde(default = "default_rpc_listen")]
    pub listen: String,
    /// Port for the ETH-compatible JSON-RPC server (default: 8545, 0 = disabled).
    #[serde(default = "default_eth_port")]
    pub eth_port: u16,
}

/// P2P networking configuration.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct P2pConfig {
    /// QUIC listen port (default: 9091).
    #[serde(default = "default_p2p_port")]
    pub port: u16,
    /// Bootstrap peer addresses (host:port).
    #[serde(default)]
    pub peers: Vec<String>,
}

/// Validator identity and staking configuration.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ValidatorConfig {
    /// Seed string for deterministic keypair derivation (default: "arc-validator-0").
    #[serde(default = "default_validator_seed")]
    pub seed: String,
    /// Staked ARC amount (default: 5,000,000).
    #[serde(default = "default_stake")]
    pub stake: u64,
    /// Minimum stake required to run as a validator (default: 500,000).
    #[serde(default = "default_min_stake")]
    pub min_stake: u64,
}

/// Persistent storage configuration.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct StorageConfig {
    /// Directory for WAL, snapshots, and state data (default: "./arc-data").
    #[serde(default = "default_data_dir")]
    pub data_dir: String,
}

/// Benchmark mode configuration (only relevant when --benchmark is set).
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct BenchmarkConfig {
    /// Transactions per batch (default: 500).
    #[serde(default = "default_bench_batch")]
    pub batch_size: usize,
    /// Milliseconds between benchmark batches (default: 200).
    #[serde(default = "default_bench_interval")]
    pub interval_ms: u64,
    /// First sender index, 0-49 (default: 0).
    #[serde(default)]
    pub sender_start: u8,
    /// Number of senders this node owns (default: 50).
    #[serde(default = "default_bench_sender_count")]
    pub sender_count: u8,
    /// Number of signing threads (default: 4).
    #[serde(default = "default_bench_sign_threads")]
    pub sign_threads: usize,
    /// Number of rayon threads for batch verification (default: 6).
    #[serde(default = "default_bench_rayon_threads")]
    pub rayon_threads: usize,
}

// ─── Default value functions ────────────────────────────────────────────

fn default_chain_id() -> String {
    "0x415243".to_string() // "ARC" in hex
}

fn default_rpc_listen() -> String {
    "0.0.0.0:9090".to_string()
}

fn default_eth_port() -> u16 {
    8545
}

fn default_p2p_port() -> u16 {
    9091
}

fn default_validator_seed() -> String {
    "arc-validator-0".to_string()
}

fn default_stake() -> u64 {
    5_000_000
}

fn default_min_stake() -> u64 {
    500_000
}

fn default_data_dir() -> String {
    "./arc-data".to_string()
}

fn default_bench_batch() -> usize {
    500
}

fn default_bench_interval() -> u64 {
    200
}

fn default_bench_sender_count() -> u8 {
    50
}

fn default_bench_sign_threads() -> usize {
    4
}

fn default_bench_rayon_threads() -> usize {
    6
}

// ─── Default trait implementations ──────────────────────────────────────

impl Default for NodeConfig {
    fn default() -> Self {
        Self {
            rpc: RpcConfig::default(),
            p2p: P2pConfig::default(),
            validator: ValidatorConfig::default(),
            storage: StorageConfig::default(),
            benchmark: BenchmarkConfig::default(),
        }
    }
}

impl Default for RpcConfig {
    fn default() -> Self {
        Self {
            listen: default_rpc_listen(),
            eth_port: default_eth_port(),
        }
    }
}

impl Default for P2pConfig {
    fn default() -> Self {
        Self {
            port: default_p2p_port(),
            peers: Vec::new(),
        }
    }
}

impl Default for ValidatorConfig {
    fn default() -> Self {
        Self {
            seed: default_validator_seed(),
            stake: default_stake(),
            min_stake: default_min_stake(),
        }
    }
}

impl Default for StorageConfig {
    fn default() -> Self {
        Self {
            data_dir: default_data_dir(),
        }
    }
}

impl Default for BenchmarkConfig {
    fn default() -> Self {
        Self {
            batch_size: default_bench_batch(),
            interval_ms: default_bench_interval(),
            sender_start: 0,
            sender_count: default_bench_sender_count(),
            sign_threads: default_bench_sign_threads(),
            rayon_threads: default_bench_rayon_threads(),
        }
    }
}

// ─── Loader functions ───────────────────────────────────────────────────

/// Load a genesis configuration from a TOML file at the given path.
pub fn load_genesis(path: &str) -> Result<GenesisConfig> {
    let contents = std::fs::read_to_string(path)
        .with_context(|| format!("Failed to read genesis config from '{}'", path))?;
    let config: GenesisConfig = toml::from_str(&contents)
        .with_context(|| format!("Failed to parse genesis config from '{}'", path))?;
    Ok(config)
}

/// Load a node configuration from a TOML file at the given path.
pub fn load_config(path: &str) -> Result<NodeConfig> {
    let contents = std::fs::read_to_string(path)
        .with_context(|| format!("Failed to read node config from '{}'", path))?;
    let config: NodeConfig = toml::from_str(&contents)
        .with_context(|| format!("Failed to parse node config from '{}'", path))?;
    Ok(config)
}

// ─── Tests ──────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_node_config() {
        let cfg = NodeConfig::default();
        assert_eq!(cfg.rpc.listen, "0.0.0.0:9090");
        assert_eq!(cfg.rpc.eth_port, 8545);
        assert_eq!(cfg.p2p.port, 9091);
        assert!(cfg.p2p.peers.is_empty());
        assert_eq!(cfg.validator.seed, "arc-validator-0");
        assert_eq!(cfg.validator.stake, 5_000_000);
        assert_eq!(cfg.validator.min_stake, 500_000);
        assert_eq!(cfg.storage.data_dir, "./arc-data");
    }

    #[test]
    fn test_parse_minimal_node_config() {
        let toml_str = r#"
            [rpc]
            listen = "127.0.0.1:9999"
        "#;
        let cfg: NodeConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(cfg.rpc.listen, "127.0.0.1:9999");
        // All other fields should use defaults
        assert_eq!(cfg.rpc.eth_port, 8545);
        assert_eq!(cfg.p2p.port, 9091);
        assert_eq!(cfg.validator.seed, "arc-validator-0");
    }

    #[test]
    fn test_parse_genesis_config() {
        let toml_str = r#"
            [chain]
            name = "arc-testnet"
            chain_id = "0x415243"

            [[accounts]]
            address = "af1349b9f5f9a1a6a0404dea36dcc9499bcb25c9adc112b7cc9a93cae41f3262"
            balance = 1_000_000_000_000

            [[validators]]
            seed = "arc-validator-0"
            stake = 5_000_000
        "#;
        let cfg: GenesisConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(cfg.chain.name, "arc-testnet");
        assert_eq!(cfg.chain.chain_id, "0x415243");
        assert_eq!(cfg.accounts.len(), 1);
        assert_eq!(cfg.accounts[0].balance, 1_000_000_000_000);
        assert_eq!(cfg.validators.len(), 1);
        assert_eq!(cfg.validators[0].seed, "arc-validator-0");
        assert_eq!(cfg.validators[0].stake, 5_000_000);
    }

    #[test]
    fn test_parse_full_node_config() {
        let toml_str = r#"
            [rpc]
            listen = "0.0.0.0:8080"
            eth_port = 8546

            [p2p]
            port = 9092
            peers = ["1.2.3.4:9091", "5.6.7.8:9091"]

            [validator]
            seed = "my-validator"
            stake = 10_000_000
            min_stake = 1_000_000

            [storage]
            data_dir = "/var/arc/data"

            [benchmark]
            batch_size = 1000
            interval_ms = 100
            sender_start = 10
            sender_count = 20
            sign_threads = 8
            rayon_threads = 12
        "#;
        let cfg: NodeConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(cfg.rpc.listen, "0.0.0.0:8080");
        assert_eq!(cfg.rpc.eth_port, 8546);
        assert_eq!(cfg.p2p.port, 9092);
        assert_eq!(cfg.p2p.peers.len(), 2);
        assert_eq!(cfg.validator.seed, "my-validator");
        assert_eq!(cfg.validator.stake, 10_000_000);
        assert_eq!(cfg.validator.min_stake, 1_000_000);
        assert_eq!(cfg.storage.data_dir, "/var/arc/data");
        assert_eq!(cfg.benchmark.batch_size, 1000);
        assert_eq!(cfg.benchmark.interval_ms, 100);
        assert_eq!(cfg.benchmark.sender_start, 10);
        assert_eq!(cfg.benchmark.sender_count, 20);
        assert_eq!(cfg.benchmark.sign_threads, 8);
        assert_eq!(cfg.benchmark.rayon_threads, 12);
    }
}
