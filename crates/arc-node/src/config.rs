use anyhow::{Context, Result, bail, ensure};
use arc_crypto::{Hash256, hash_bytes};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct GenesisConfig {
    pub chain: ChainInfo,
    #[serde(default)]
    pub accounts: Vec<GenesisAccount>,
    #[serde(default)]
    pub validators: Vec<GenesisValidator>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ChainInfo {
    pub name: String,
    #[serde(default = "default_chain_id")]
    pub chain_id: String,
    #[serde(default)]
    pub validator_set_complete: bool,
    #[serde(default)]
    pub community_rewards_v1_activation_height: Option<u64>,
    #[serde(default)]
    pub instance_id: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct GenesisAccount {
    pub address: String,
    pub balance: u64,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct GenesisValidator {
    #[serde(default)]
    pub address: Option<String>,
    #[serde(default)]
    pub insecure_dev_seed: Option<String>,
    #[serde(default = "default_stake")]
    pub stake: u64,
}

impl GenesisConfig {
    pub fn validated_accounts(&self) -> Result<Vec<(Hash256, u64)>> {
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

    pub fn network_hash(&self, allow_insecure_dev: bool) -> Result<Hash256> {
        let validators = self.validated_validator_set(allow_insecure_dev)?;
        self.canonical_network_hash(validators)
    }

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
        if let Some(instance) = &self.chain.instance_id {
            ensure!(
                !instance.is_empty() && instance.len() <= MAX_INSTANCE_ID_LEN,
                "chain.instance_id must be 1..={MAX_INSTANCE_ID_LEN} bytes"
            );
            ensure!(
                instance
                    .bytes()
                    .all(|byte| byte.is_ascii_graphic() || byte == b' '),
                "chain.instance_id must be printable ASCII"
            );
            canonical.extend_from_slice(b"instance\0");
            append_bytes(&mut canonical, instance.as_bytes());
        }
        Ok(hash_bytes(&canonical))
    }
}

const MAX_INSTANCE_ID_LEN: usize = 128;

fn append_bytes(canonical: &mut Vec<u8>, value: &[u8]) {
    append_u64(canonical, value.len() as u64);
    canonical.extend_from_slice(value);
}

fn append_u64(canonical: &mut Vec<u8>, value: u64) {
    canonical.extend_from_slice(&value.to_be_bytes());
}

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

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct CommunityConfig {
    #[serde(default)]
    pub rpc_urls: Vec<String>,
    #[serde(default)]
    pub allow_insecure_remote_http: bool,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct InferenceConfig {
    #[serde(default)]
    pub threads: usize,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct RpcConfig {
    #[serde(default = "default_rpc_listen")]
    pub listen: String,
    #[serde(default = "default_eth_port")]
    pub eth_port: u16,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct P2pConfig {
    #[serde(default = "default_p2p_port")]
    pub port: u16,
    #[serde(default)]
    pub peers: Vec<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ValidatorConfig {
    #[serde(default)]
    pub key_file: Option<String>,
    #[serde(default)]
    pub seed: Option<String>,
    #[serde(default = "default_node_stake")]
    pub stake: u64,
    #[serde(default = "default_min_stake")]
    pub min_stake: u64,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct StorageConfig {
    #[serde(default = "default_data_dir")]
    pub data_dir: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct BenchmarkConfig {
    #[serde(default = "default_bench_batch")]
    pub batch_size: usize,
    #[serde(default = "default_bench_interval")]
    pub interval_ms: u64,
    #[serde(default)]
    pub sender_start: u8,
    #[serde(default = "default_bench_sender_count")]
    pub sender_count: u8,
    #[serde(default = "default_bench_sign_threads")]
    pub sign_threads: usize,
    #[serde(default = "default_bench_rayon_threads")]
    pub rayon_threads: usize,
}

fn default_chain_id() -> String {
    "0x415243".to_string()
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

fn default_stake() -> u64 {
    5_000_000
}

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

pub fn load_genesis(path: &str) -> Result<GenesisConfig> {
    let contents = std::fs::read_to_string(path)
        .with_context(|| format!("Failed to read genesis config from '{}'", path))?;
    let config: GenesisConfig = toml::from_str(&contents)
        .with_context(|| format!("Failed to parse genesis config from '{}'", path))?;
    Ok(config)
}

pub fn load_config(path: &str) -> Result<NodeConfig> {
    let contents = std::fs::read_to_string(path)
        .with_context(|| format!("Failed to read node config from '{}'", path))?;
    let config: NodeConfig = toml::from_str(&contents)
        .with_context(|| format!("Failed to parse node config from '{}'", path))?;
    Ok(config)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_node_config() {
        let cfg = NodeConfig::default();
        assert_eq!(cfg.rpc.listen, "127.0.0.1:9944");
        assert_eq!(cfg.rpc.eth_port, 0);
        assert_eq!(cfg.p2p.port, 9945);
        assert!(cfg.p2p.peers.is_empty());
        assert!(cfg.validator.key_file.is_none());
        assert!(cfg.validator.seed.is_none());
        assert_eq!(cfg.validator.stake, 0);
        assert_eq!(cfg.validator.min_stake, 500_000);
        assert_eq!(cfg.storage.data_dir, "./arc-data");
        assert_eq!(cfg.inference.threads, 0);
        assert!(cfg.community.rpc_urls.is_empty());
        assert!(!cfg.community.allow_insecure_remote_http);
    }

    #[test]
    fn genesis_validator_stake_default_is_unchanged() {
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
    fn network_hash_separates_runs_of_the_same_genesis() {
        let address =
            crate::validator_identity::derive_insecure_seed_keypair("instance-test").address();
        let parse = |instance: Option<&str>| {
            let line = match instance {
                Some(value) => format!("instance_id = \"{value}\"\n"),
                None => String::new(),
            };
            toml::from_str::<GenesisConfig>(&format!(
                r#"
                    [chain]
                    name = "instance-network"
                    chain_id = "0x415243"
                    validator_set_complete = true
                    {line}

                    [[accounts]]
                    address = "{address}"
                    balance = 0

                    [[validators]]
                    address = "{address}"
                    stake = 5000000
                "#,
                address = address.to_hex(),
            ))
            .unwrap()
        };

        let unset = parse(None);
        let mut explicitly_none = unset.clone();
        explicitly_none.chain.instance_id = None;
        assert_eq!(
            unset.network_hash(false).unwrap(),
            explicitly_none.network_hash(false).unwrap()
        );

        let run_a = parse(Some("run-a"));
        let run_b = parse(Some("run-b"));
        assert_ne!(
            run_a.network_hash(false).unwrap(),
            run_b.network_hash(false).unwrap(),
            "a chain run must not share a network identity with another run"
        );
        assert_ne!(
            unset.network_hash(false).unwrap(),
            run_a.network_hash(false).unwrap()
        );

        let mut empty = unset.clone();
        empty.chain.instance_id = Some(String::new());
        assert!(
            empty
                .network_hash(false)
                .unwrap_err()
                .to_string()
                .contains("instance_id"),
            "an empty instance id must be refused, not treated as unset"
        );
        let mut oversized = unset.clone();
        oversized.chain.instance_id = Some("x".repeat(MAX_INSTANCE_ID_LEN + 1));
        assert!(oversized.network_hash(false).is_err());
        let mut unprintable = unset;
        unprintable.chain.instance_id = Some("run\na".into());
        assert!(unprintable.network_hash(false).is_err());
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
