use anyhow::{Context, Result, bail, ensure};
use arc_crypto::{Hash256, hash_bytes};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;

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
    /// Must be explicitly true before this file can start a production node.
    /// This makes validator-key migration fail closed instead of silently
    /// booting a partial or legacy seed-derived validator set.
    #[serde(default)]
    pub validator_set_complete: bool,
    /// Consensus activation height for `CommunityInferenceReward` (tx 0x25).
    /// Absent means disabled. Because this value is committed by
    /// `network_hash`, validators with different schedules cannot handshake
    /// as though they were on the same chain.
    #[serde(default)]
    pub community_rewards_v1_activation_height: Option<u64>,
}

/// A prefunded account in the genesis state.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct GenesisAccount {
    /// 64-character hex string (32 bytes) - the account address.
    pub address: String,
    /// Initial balance in ARC (smallest unit).
    pub balance: u64,
}

/// A validator included in the genesis validator set.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct GenesisValidator {
    /// Public ARC address emitted by `arc keygen`. Never put a secret key or
    /// deterministic validator seed in a production genesis file.
    #[serde(default)]
    pub address: Option<String>,
    /// Local-test-only deterministic identity. Accepted only when the genesis
    /// is marked incomplete AND the node has the explicit insecure dev flag.
    #[serde(default)]
    pub insecure_dev_seed: Option<String>,
    #[serde(default = "default_stake")]
    pub stake: u64,
}

impl GenesisConfig {
    /// Parse the exact account set that will be materialized into state.
    /// Network authentication and state initialization must share this input;
    /// neither may add an account derived from the local signing identity.
    fn validated_accounts(&self) -> Result<Vec<(Hash256, u64)>> {
        let mut seen = HashSet::new();
        let mut accounts = Vec::with_capacity(self.accounts.len());
        for (index, account) in self.accounts.iter().enumerate() {
            let address =
                crate::validator_identity::parse_address(&account.address).with_context(|| {
                    format!("genesis account #{} has an invalid address", index + 1)
                })?;
            ensure!(
                seen.insert(address),
                "genesis contains duplicate account address {}",
                address
            );
            accounts.push((address, account.balance));
        }
        Ok(accounts)
    }

    /// Validate and materialize the validator set without ever reading a
    /// production secret from genesis. Incomplete sets are usable only for an
    /// explicitly insecure disposable development network.
    pub fn validated_validator_set(&self, allow_insecure_dev: bool) -> Result<Vec<(Hash256, u64)>> {
        if !self.chain.validator_set_complete && !allow_insecure_dev {
            bail!(
                "genesis validator migration is incomplete: [chain].validator_set_complete is false. Refusing production startup. Generate one Ed25519 keyfile per validator with `arc keygen --scheme ed25519`, keep each keyfile off-repo with mode 0600, put only its public `address` in [[validators]], then set validator_set_complete = true"
            );
        }
        if self.chain.validator_set_complete && self.validators.is_empty() {
            bail!(
                "genesis declares validator_set_complete = true but contains no [[validators]] public addresses"
            );
        }

        let production_accounts: HashSet<Hash256> = if self.chain.validator_set_complete {
            self.validated_accounts()?
                .into_iter()
                .map(|(address, _)| address)
                .collect()
        } else {
            HashSet::new()
        };

        let mut seen = HashSet::new();
        let mut validators = Vec::with_capacity(self.validators.len());
        for (index, validator) in self.validators.iter().enumerate() {
            ensure!(
                validator.stake > 0,
                "genesis validator #{} has zero stake",
                index + 1
            );

            let address = match (
                validator.address.as_deref(),
                validator.insecure_dev_seed.as_deref(),
            ) {
                (Some(address), None) => crate::validator_identity::parse_address(address)
                    .with_context(|| {
                        format!(
                            "genesis validator #{} has an invalid public address",
                            index + 1
                        )
                    })?,
                (None, Some(seed)) if allow_insecure_dev && !self.chain.validator_set_complete => {
                    crate::validator_identity::derive_insecure_seed_keypair(seed).address()
                }
                (None, Some(_)) => bail!(
                    "genesis validator #{} uses insecure_dev_seed, which is forbidden for a complete/production validator set",
                    index + 1
                ),
                (Some(_), Some(_)) => bail!(
                    "genesis validator #{} configures both address and insecure_dev_seed; choose exactly one",
                    index + 1
                ),
                (None, None) => bail!(
                    "genesis validator #{} is missing its public address",
                    index + 1
                ),
            };

            ensure!(
                seen.insert(address),
                "genesis contains duplicate validator address {}",
                address
            );
            if self.chain.validator_set_complete {
                ensure!(
                    production_accounts.contains(&address),
                    "genesis validator #{} address {} is missing from [[accounts]]; every production validator must be declared in the shared genesis state",
                    index + 1,
                    address
                );
            }
            validators.push((address, validator.stake));
        }

        Ok(validators)
    }

    /// Canonical semantic hash used to separate P2P networks at handshake.
    /// TOML formatting and entry order do not affect it; every effective
    /// chain/account/validator field does. Secret seed text is never hashed or
    /// exposed: insecure dev entries first become their public addresses.
    pub fn network_hash(&self, allow_insecure_dev: bool) -> Result<Hash256> {
        let validators = self.validated_validator_set(allow_insecure_dev)?;
        self.canonical_network_hash(validators)
    }

    /// Hash an intentionally empty validator set for a stake-zero community
    /// node while the shipped production genesis is awaiting public keys.
    /// Such a node must not start P2P or consensus; this hash exists so the
    /// parsed placeholder still has an unambiguous public identity for status.
    pub fn migration_observer_network_hash(&self) -> Result<Hash256> {
        ensure!(
            !self.chain.validator_set_complete,
            "migration observer hashing is only valid for an incomplete genesis"
        );
        ensure!(
            self.validators.is_empty(),
            "an incomplete community-observer genesis must not contain a partial validator list"
        );
        self.canonical_network_hash(Vec::new())
    }

    fn canonical_network_hash(&self, mut validators: Vec<(Hash256, u64)>) -> Result<Hash256> {
        validators.sort_unstable_by_key(|(address, _)| address.0);

        let mut accounts = self.validated_accounts()?;
        accounts.sort_unstable_by_key(|(address, _)| address.0);

        let mut canonical = Vec::with_capacity(
            64 + self.chain.name.len()
                + self.chain.chain_id.len()
                + accounts.len() * 40
                + validators.len() * 40,
        );
        canonical.extend_from_slice(b"ARC-genesis-config-v3\0");
        append_bytes(&mut canonical, self.chain.name.as_bytes());
        append_bytes(&mut canonical, self.chain.chain_id.as_bytes());
        canonical.push(u8::from(self.chain.validator_set_complete));
        match self.chain.community_rewards_v1_activation_height {
            Some(height) => {
                canonical.push(1);
                append_u64(&mut canonical, height);
            }
            None => canonical.push(0),
        }
        append_u64(&mut canonical, accounts.len() as u64);
        for (address, balance) in accounts {
            canonical.extend_from_slice(&address.0);
            append_u64(&mut canonical, balance);
        }
        append_u64(&mut canonical, validators.len() as u64);
        for (address, stake) in validators {
            canonical.extend_from_slice(&address.0);
            append_u64(&mut canonical, stake);
        }
        Ok(hash_bytes(&canonical))
    }
}

fn append_bytes(canonical: &mut Vec<u8>, value: &[u8]) {
    append_u64(canonical, value.len() as u64);
    canonical.extend_from_slice(value);
}

fn append_u64(canonical: &mut Vec<u8>, value: u64) {
    canonical.extend_from_slice(&value.to_be_bytes());
}

/// A voting node must use exactly the address and stake declared by genesis.
pub fn verify_staked_identity(
    validators: &[(Hash256, u64)],
    address: Hash256,
    stake: u64,
) -> Result<()> {
    let Some((_, genesis_stake)) = validators
        .iter()
        .find(|(candidate, _)| *candidate == address)
    else {
        bail!(
            "staked validator identity {} is not present in the genesis validator set; use the keyfile whose public address was approved in genesis",
            address
        );
    };
    ensure!(
        *genesis_stake == stake,
        "validator {} stake mismatch: node configured {}, genesis declares {}",
        address,
        stake,
        genesis_stake
    );
    Ok(())
}

// ─── Node Configuration ────────────────────────────────────────────────

/// Top-level node runtime configuration loaded from a TOML file.
/// All sections are optional and fall back to defaults matching the CLI defaults.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
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
    #[serde(default)]
    pub inference: InferenceConfig,
    #[serde(default)]
    pub community: CommunityConfig,
}

/// Outbound authenticated community/reward RPC configuration. Kept separate
/// from `[p2p] peers`: a QUIC consensus address is never an HTTP trust or TLS
/// configuration.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct CommunityConfig {
    /// Absolute RPC origins, e.g. `https://seed-1.arc.network`.
    #[serde(default)]
    pub rpc_urls: Vec<String>,
    /// Disposable development only. Remote production origins require HTTPS;
    /// loopback HTTP does not need this override.
    #[serde(default)]
    pub allow_insecure_remote_http: bool,
}

/// Inference runtime configuration.
///
/// ── How thread width is actually decided ─────────────────────────────────
///
/// All inference compute (the `into_par_iter()` over attention heads in
/// `forward_shard_token`, and the `par_chunks_mut` inside every matmul) runs
/// on a rayon pool. Which pool, in priority order:
///
///   1. A DEDICATED pool, when `--threads N` / `[inference] threads = N` is
///      non-zero, or after a `POST /node/threads {"threads": N}` at runtime.
///      This is the only setting that can be changed without a restart.
///   2. Rayon's implicit GLOBAL pool otherwise. Rayon builds that pool lazily
///      on first use and sizes it from the `RAYON_NUM_THREADS` environment
///      variable when it is set and parses to a positive integer, and from
///      `std::thread::available_parallelism()` when it is not.
///
/// `RAYON_NUM_THREADS` is read by rayon itself, not by this crate, and only
/// at the moment the global pool is first built — exporting it after the
/// process has started has no effect. `GET /node/threads` reports which of
/// the two is in force, along with the env var's observed value.
///
/// Note that `[benchmark] rayon_threads` is a DIFFERENT knob: it calls
/// `build_global()` and only under `--benchmark`. It sizes the pool used for
/// batch signature verification, not inference.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct InferenceConfig {
    /// Dedicated inference pool width. 0 (default) = use rayon's global pool.
    #[serde(default)]
    pub threads: usize,
}

/// RPC server configuration.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct RpcConfig {
    /// Listen address for the native ARC RPC (default: "127.0.0.1:9944").
    #[serde(default = "default_rpc_listen")]
    pub listen: String,
    /// Port for the ETH-compatible JSON-RPC server (default: 0 = disabled).
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
    /// Mode-0600 Ed25519 JSON keyfile created by `arc keygen`.
    #[serde(default)]
    pub key_file: Option<String>,
    /// Local development / stake-zero compatibility only. Production staked
    /// nodes reject this unless the explicit insecure dev flag is present.
    #[serde(default)]
    pub seed: Option<String>,
    /// Staked ARC amount (default: 0 = observer / community node).
    ///
    /// Deliberately NOT `default_stake` (which is the GENESIS validator
    /// default and stays at 5,000,000). A node that omits `[validator] stake`
    /// must not silently become a voting validator on whatever network it is
    /// pointed at — see the `--stake` doc comment in main.rs for what that
    /// costs and why it is unrecoverable without restarting every seed.
    #[serde(default = "default_node_stake")]
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
    "127.0.0.1:9944".to_string()
}

fn default_eth_port() -> u16 {
    0
}

fn default_p2p_port() -> u16 {
    9945
}

/// Default stake for a GENESIS validator entry. Genesis files describe a
/// network being created, where a validator with no stake is meaningless.
fn default_stake() -> u64 {
    5_000_000
}

/// Default stake for THIS node. Zero: joining as a validator is an explicit act.
fn default_node_stake() -> u64 {
    0
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
//
// `NodeConfig` and `InferenceConfig` derive theirs (all-default fields); the
// rest need non-zero field defaults and are written out below.

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
            key_file: None,
            seed: None,
            stake: default_node_stake(),
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
        // Ports moved from 9090/9091 -> 9944/9945 to avoid collisions with
        // Prometheus (9090) and Transmission/BitTorrent (9091). These tests
        // assert the current defaults; update both together when rebinding.
        assert_eq!(cfg.rpc.listen, "127.0.0.1:9944");
        assert_eq!(cfg.rpc.eth_port, 0);
        assert_eq!(cfg.p2p.port, 9945);
        assert!(cfg.p2p.peers.is_empty());
        assert!(cfg.validator.key_file.is_none());
        assert!(cfg.validator.seed.is_none());
        // Zero by default: a node must opt IN to being a voting validator.
        // See ValidatorConfig::stake.
        assert_eq!(cfg.validator.stake, 0);
        assert_eq!(cfg.validator.min_stake, 500_000);
        assert_eq!(cfg.storage.data_dir, "./arc-data");
        assert_eq!(cfg.inference.threads, 0);
        assert!(cfg.community.rpc_urls.is_empty());
        assert!(!cfg.community.allow_insecure_remote_http);
    }

    #[test]
    fn genesis_validator_stake_default_is_unchanged() {
        // The node's own stake default moved to 0, but a genesis file that
        // omits `stake` for a validator still means a real validator.
        let toml_str = r#"
            [chain]
            name = "arc-testnet"
            validator_set_complete = true

            [[validators]]
            address = "af1349b9f5f9a1a6a0404dea36dcc9499bcb25c9adc112b7cc9a93cae41f3262"
        "#;
        let cfg: GenesisConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(cfg.validators[0].stake, 5_000_000);
    }

    #[test]
    fn inference_threads_parses() {
        let cfg: NodeConfig = toml::from_str("[inference]\nthreads = 12\n").unwrap();
        assert_eq!(cfg.inference.threads, 12);
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
        assert_eq!(cfg.rpc.eth_port, 0);
        assert_eq!(cfg.p2p.port, 9945);
        assert!(cfg.validator.key_file.is_none());
        assert!(cfg.validator.seed.is_none());
    }

    #[test]
    fn test_parse_genesis_config() {
        let toml_str = r#"
            [chain]
            name = "arc-testnet"
            chain_id = "0x415243"
            validator_set_complete = true

            [[accounts]]
            address = "af1349b9f5f9a1a6a0404dea36dcc9499bcb25c9adc112b7cc9a93cae41f3262"
            balance = 1_000_000_000_000

            [[validators]]
            address = "af1349b9f5f9a1a6a0404dea36dcc9499bcb25c9adc112b7cc9a93cae41f3262"
            stake = 5_000_000
        "#;
        let cfg: GenesisConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(cfg.chain.name, "arc-testnet");
        assert_eq!(cfg.chain.chain_id, "0x415243");
        assert!(cfg.chain.validator_set_complete);
        assert_eq!(cfg.accounts.len(), 1);
        assert_eq!(cfg.accounts[0].balance, 1_000_000_000_000);
        assert_eq!(cfg.validators.len(), 1);
        assert_eq!(
            cfg.validators[0].address.as_deref(),
            Some("af1349b9f5f9a1a6a0404dea36dcc9499bcb25c9adc112b7cc9a93cae41f3262")
        );
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
            key_file = "/run/secrets/arc-validator.json"
            stake = 10_000_000
            min_stake = 1_000_000

            [storage]
            data_dir = "/var/arc/data"

            [community]
            rpc_urls = ["https://seed-a.example", "https://seed-b.example"]
            allow_insecure_remote_http = false

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
        assert_eq!(
            cfg.validator.key_file.as_deref(),
            Some("/run/secrets/arc-validator.json")
        );
        assert!(cfg.validator.seed.is_none());
        assert_eq!(cfg.validator.stake, 10_000_000);
        assert_eq!(cfg.validator.min_stake, 1_000_000);
        assert_eq!(cfg.storage.data_dir, "/var/arc/data");
        assert_eq!(cfg.community.rpc_urls.len(), 2);
        assert!(!cfg.community.allow_insecure_remote_http);
        assert_eq!(cfg.benchmark.batch_size, 1000);
        assert_eq!(cfg.benchmark.interval_ms, 100);
        assert_eq!(cfg.benchmark.sender_start, 10);
        assert_eq!(cfg.benchmark.sender_count, 20);
        assert_eq!(cfg.benchmark.sign_threads, 8);
        assert_eq!(cfg.benchmark.rayon_threads, 12);
    }

    #[test]
    fn legacy_genesis_seed_field_is_rejected() {
        let error = toml::from_str::<GenesisConfig>(
            r#"
                [chain]
                name = "legacy"

                [[validators]]
                seed = "public-secret"
            "#,
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("unknown field `seed`"), "{error}");
    }

    #[test]
    fn incomplete_genesis_fails_closed_without_dev_flag() {
        let cfg: GenesisConfig = toml::from_str(
            r#"
                [chain]
                name = "migration-pending"
                validator_set_complete = false
            "#,
        )
        .unwrap();
        let error = cfg.validated_validator_set(false).unwrap_err().to_string();
        assert!(error.contains("migration is incomplete"), "{error}");
        assert!(error.contains("mode 0600"), "{error}");
    }

    #[test]
    fn complete_genesis_uses_public_addresses_and_verifies_stake() {
        let keypair = crate::validator_identity::derive_insecure_seed_keypair("test-only");
        let address = keypair.address();
        let cfg: GenesisConfig = toml::from_str(&format!(
            r#"
                [chain]
                name = "public-address-test"
                validator_set_complete = true

                [[accounts]]
                address = "{}"
                balance = 0

                [[validators]]
                address = "{}"
                stake = 7000000
            "#,
            address.to_hex(),
            address.to_hex()
        ))
        .unwrap();
        let validators = cfg.validated_validator_set(false).unwrap();
        verify_staked_identity(&validators, address, 7_000_000).unwrap();

        let error = verify_staked_identity(&validators, address, 5_000_000)
            .unwrap_err()
            .to_string();
        assert!(error.contains("stake mismatch"), "{error}");
    }

    #[test]
    fn insecure_dev_genesis_requires_incomplete_set_and_flag() {
        let cfg: GenesisConfig = toml::from_str(
            r#"
                [chain]
                name = "disposable-devnet"
                validator_set_complete = false

                [[validators]]
                insecure_dev_seed = "dev-only"
                stake = 5000000
            "#,
        )
        .unwrap();
        let validators = cfg.validated_validator_set(true).unwrap();
        let expected =
            crate::validator_identity::derive_insecure_seed_keypair("dev-only").address();
        assert_eq!(validators, vec![(expected, 5_000_000)]);

        let mut complete = cfg;
        complete.chain.validator_set_complete = true;
        let error = complete
            .validated_validator_set(true)
            .unwrap_err()
            .to_string();
        assert!(error.contains("forbidden"), "{error}");
    }

    #[test]
    fn network_hash_commits_to_chain_and_validator_set() {
        let address_a =
            crate::validator_identity::derive_insecure_seed_keypair("hash-test-a").address();
        let address_b =
            crate::validator_identity::derive_insecure_seed_keypair("hash-test-b").address();
        let parse = |name: &str, address: Hash256| {
            toml::from_str::<GenesisConfig>(&format!(
                r#"
                    [chain]
                    name = "{name}"
                    chain_id = "0x415243"
                    validator_set_complete = true

                    [[accounts]]
                    address = "{}"
                    balance = 0

                    [[validators]]
                    address = "{}"
                    stake = 5000000
                "#,
                address.to_hex(),
                address.to_hex()
            ))
            .unwrap()
        };

        let chain_a = parse("network-a", address_a);
        let different_validator = parse("network-a", address_b);
        let different_chain = parse("network-b", address_a);
        assert_ne!(
            chain_a.network_hash(false).unwrap(),
            different_validator.network_hash(false).unwrap()
        );
        assert_ne!(
            chain_a.network_hash(false).unwrap(),
            different_chain.network_hash(false).unwrap()
        );

        let mut scheduled = chain_a.clone();
        scheduled.chain.community_rewards_v1_activation_height = Some(10_000);
        assert_ne!(
            chain_a.network_hash(false).unwrap(),
            scheduled.network_hash(false).unwrap(),
            "consensus reward activation must be part of network identity"
        );
    }

    #[test]
    fn canonical_network_hash_ignores_toml_entry_order() {
        let address_a =
            crate::validator_identity::derive_insecure_seed_keypair("order-test-a").address();
        let address_b =
            crate::validator_identity::derive_insecure_seed_keypair("order-test-b").address();
        let parse = |reverse: bool| {
            let account_entries = if reverse {
                format!(
                    "[[accounts]]\naddress = \"{}\"\nbalance = 20\n[[accounts]]\naddress = \"{}\"\nbalance = 10",
                    address_b.to_hex(),
                    address_a.to_hex()
                )
            } else {
                format!(
                    "[[accounts]]\naddress = \"{}\"\nbalance = 10\n[[accounts]]\naddress = \"{}\"\nbalance = 20",
                    address_a.to_hex(),
                    address_b.to_hex()
                )
            };
            let validator_entries = if reverse {
                format!(
                    "[[validators]]\naddress = \"{}\"\nstake = 6000000\n[[validators]]\naddress = \"{}\"\nstake = 5000000",
                    address_b.to_hex(),
                    address_a.to_hex()
                )
            } else {
                format!(
                    "[[validators]]\naddress = \"{}\"\nstake = 5000000\n[[validators]]\naddress = \"{}\"\nstake = 6000000",
                    address_a.to_hex(),
                    address_b.to_hex()
                )
            };
            toml::from_str::<GenesisConfig>(&format!(
                r#"
                    [chain]
                    name = "canonical-order"
                    validator_set_complete = true

                    {account_entries}

                    {validator_entries}
                "#,
            ))
            .unwrap()
        };

        assert_eq!(
            parse(false).network_hash(false).unwrap(),
            parse(true).network_hash(false).unwrap()
        );
    }

    #[test]
    fn complete_genesis_rejects_validator_missing_from_accounts() {
        let address =
            crate::validator_identity::derive_insecure_seed_keypair("missing-account").address();
        let cfg: GenesisConfig = toml::from_str(&format!(
            r#"
                [chain]
                name = "missing-validator-account"
                validator_set_complete = true

                [[accounts]]
                address = "af1349b9f5f9a1a6a0404dea36dcc9499bcb25c9adc112b7cc9a93cae41f3262"
                balance = 100

                [[validators]]
                address = "{}"
                stake = 5000000
            "#,
            address.to_hex()
        ))
        .unwrap();

        let error = cfg.validated_validator_set(false).unwrap_err().to_string();
        assert!(error.contains("missing from [[accounts]]"), "{error}");
        assert!(cfg.network_hash(false).is_err());
    }

    #[test]
    fn incomplete_placeholder_has_observer_hash_but_no_validator_set() {
        let cfg: GenesisConfig = toml::from_str(
            r#"
                [chain]
                name = "migration-pending"
                validator_set_complete = false
            "#,
        )
        .unwrap();
        assert!(cfg.validated_validator_set(false).is_err());
        assert_ne!(
            cfg.migration_observer_network_hash().unwrap(),
            Hash256::ZERO
        );
    }
}
