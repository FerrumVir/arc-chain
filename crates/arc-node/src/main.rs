mod config;

use anyhow::Result;
use arc_crypto::{hash_bytes, Hash256, KeyPair};
use arc_mempool::Mempool;
use arc_net::transport::{run_transport, InboundMessage, OutboundMessage};
use arc_node::{benchmark::BenchmarkPool, consensus::ConsensusManager, rpc};
use arc_state::StateDB;
use arc_types::Block;
use clap::{CommandFactory, Parser};
use std::net::SocketAddr;
use std::sync::atomic::AtomicU32;
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::mpsc;
use tracing_subscriber::EnvFilter;

#[derive(Parser)]
#[command(name = "arc-node", version, about = "ARC Chain Node")]
struct Cli {
    /// RPC listen address (changed from 9090 to avoid Prometheus default port conflict)
    #[arg(long, default_value = "0.0.0.0:9944")]
    rpc: String,

    /// P2P listen port (QUIC) (changed from 9091 to avoid Transmission BitTorrent default)
    #[arg(long, default_value_t = 9945)]
    p2p_port: u16,

    /// Validator stake in ARC (0 = observer node)
    #[arg(long, default_value_t = 5_000_000)]
    stake: u64,

    /// Data directory for WAL/snapshots
    #[arg(long, default_value = "./arc-data")]
    data_dir: String,

    /// Bootstrap peer addresses (comma-separated host:port)
    #[arg(long, value_delimiter = ',')]
    peers: Vec<String>,

    /// Path to a seeds file (one peer address per line, # comments allowed).
    /// Seeds are merged with --peers. Useful for testnet bootstrap.
    #[arg(long)]
    seeds_file: Option<String>,

    /// Minimum staked ARC required to run this node
    #[arg(long, default_value_t = 500_000)]
    min_stake: u64,

    /// Validator identity seed (used to derive a unique address).
    /// Different seeds produce different validator addresses.
    /// Default: "arc-validator-0"
    #[arg(long, default_value = "arc-validator-0")]
    validator_seed: String,

    /// Archive mode — disable all pruning, keep full transaction history.
    /// Use for block explorers and analytics. Requires more disk space.
    /// Regular validators should NOT use this flag.
    #[arg(long, default_value_t = false)]
    archive: bool,

    /// Enable continuous transaction generation (testnet benchmark mode).
    /// Generates transfers between genesis accounts to keep the chain busy.
    #[arg(long, default_value_t = false)]
    benchmark: bool,

    /// Transactions per batch in benchmark mode.
    #[arg(long, default_value_t = 500)]
    bench_batch: usize,

    /// Milliseconds between benchmark batches.
    #[arg(long, default_value_t = 200)]
    bench_interval: u64,

    /// First sender index for benchmark mode (0-49). Use to partition senders
    /// across nodes in multi-node benchmarks to avoid nonce conflicts.
    #[arg(long, default_value_t = 0)]
    bench_sender_start: u8,

    /// Number of senders this node owns in benchmark mode.
    #[arg(long, default_value_t = 50)]
    bench_sender_count: u8,

    /// Number of signing threads in benchmark mode.
    #[arg(long, default_value_t = 4)]
    bench_sign_threads: usize,

    /// Number of rayon threads for batch verification.
    #[arg(long, default_value_t = 6)]
    bench_rayon_threads: usize,

    /// Enable proposer mode (GPU execution pipeline, state diff broadcast).
    /// Proposer nodes execute transactions and broadcast state diffs.
    /// Non-proposer nodes verify diffs without full re-execution.
    #[arg(long, default_value_t = false)]
    proposer_mode: bool,

    /// ETH-compatible JSON-RPC port (default: 8545).
    /// Enables MetaMask, Hardhat, Foundry, and other EVM tooling.
    /// Set to 0 to disable the ETH RPC server.
    #[arg(long, default_value_t = 8545)]
    eth_rpc_port: u16,

    /// Bootstrap from a peer's snapshot (e.g., "127.0.0.1:9090").
    /// Downloads the full state snapshot from a running node and imports it
    /// before starting, so this node doesn't need to replay from genesis.
    #[arg(long)]
    sync_from: Option<String>,

    /// Path to node config file (TOML).
    /// Values in the config file serve as defaults; explicit CLI args take precedence.
    #[arg(long, short = 'c')]
    config: Option<String>,

    /// Path to genesis config file (TOML).
    /// Defines prefunded accounts and initial validators for custom deployments.
    #[arg(long)]
    genesis: Option<String>,

    /// Path to a GGUF model file for on-chain inference.
    /// Loads the model into INT8 cached memory at startup.
    /// Enables the /inference/run RPC endpoint with real deterministic inference.
    #[arg(long)]
    model: Option<String>,

    /// Load only the tokenizer from the GGUF file (no weights).
    /// ~30MB instead of 4GB. Use for coordinator nodes that route
    /// inference to shard-holding nodes but don't compute locally.
    #[arg(long, default_value_t = false)]
    tokenizer_only: bool,

    /// First layer index to load (inclusive). Pipeline-parallel sharding.
    /// Together with --shard-end, makes this node a SHARD HOLDER for a slice
    /// of the model. Embeddings load only when --shard-start=0; output head
    /// loads only when --shard-end=n_layers.
    /// Example: 80-layer Llama-70B split 8 ways → node 0 uses --shard-start 0
    /// --shard-end 10, node 1 uses --shard-start 10 --shard-end 20, etc.
    #[arg(long)]
    shard_start: Option<usize>,

    /// Last layer index to load (exclusive). Pipeline-parallel sharding.
    #[arg(long)]
    shard_end: Option<usize>,

    /// Enable community-mode HTTP registration. When set, the node
    /// registers itself with all seeds via outbound HTTPS POST to
    /// /community/register every 60s and sends a heartbeat every 15s.
    /// This makes the node visible on the dashboard and lets it
    /// participate in compute contributions without requiring inbound
    /// connectivity (no port forwarding, no public IP needed).
    /// Recommended for home / residential installs.
    #[arg(long)]
    community_mode: bool,
}

/// Rewrites a pulled peer's `self_shard.socket_addr` in place when it carries
/// a stub (0.0.0.0 / 127.x / [::] / [::1] / empty), replacing the host with the
/// URL we just pulled from and keeping the declared port. When no port is
/// declared, falls back to the pulled URL's port or 9090.
///
/// Pure JSON mutation — no I/O, no async. Unit-testable against static fixtures.
/// Companion to the receiver-side `rewrite_stub_shard_addr` in `rpc.rs`.
fn rewrite_pulled_self_shard(self_shard: &mut serde_json::Value, pulled_from_addr: &str) {
    let Some(sa) = self_shard.get("socket_addr").and_then(|v| v.as_str()) else {
        return;
    };
    let is_stub = sa.starts_with("0.0.0.0")
        || sa.starts_with("127.")
        || sa.starts_with("[::]")
        || sa.starts_with("[::1]")
        || sa.is_empty();
    if !is_stub {
        return;
    }
    let declared_port = sa.rsplit(':').next().and_then(|p| p.parse::<u16>().ok());
    let fallback_port = pulled_from_addr
        .rsplit(':')
        .next()
        .and_then(|p| p.parse::<u16>().ok())
        .unwrap_or(9090);
    let port = declared_port.unwrap_or(fallback_port);
    let host = pulled_from_addr
        .rsplit_once(':')
        .map(|(h, _)| h)
        .unwrap_or(pulled_from_addr);
    if let Some(obj) = self_shard.as_object_mut() {
        obj.insert(
            "socket_addr".to_string(),
            serde_json::Value::String(format!("{}:{}", host, port)),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn pulled_stub_rewritten_to_seed_host_port() {
        // AMS announces self_shard with socket_addr=0.0.0.0:9090. We pulled
        // from http://136.244.109.1:9090/shards, so the routable addr for AMS
        // IS the URL we pulled from — use it.
        let mut v = json!({
            "start_layer": 10, "end_layer": 14, "socket_addr": "0.0.0.0:9090",
            "node_name": "AMS"
        });
        rewrite_pulled_self_shard(&mut v, "136.244.109.1:9090");
        assert_eq!(v["socket_addr"], "136.244.109.1:9090");
    }

    #[test]
    fn pulled_routable_addr_is_left_alone() {
        let mut v = json!({
            "start_layer": 10, "end_layer": 14, "socket_addr": "136.244.109.1:9090",
            "node_name": "AMS"
        });
        rewrite_pulled_self_shard(&mut v, "136.244.109.1:9090");
        assert_eq!(v["socket_addr"], "136.244.109.1:9090");
    }

    #[test]
    fn pulled_stub_uses_declared_port_over_pulled_url_port() {
        // Peer bound to 9090 but we pulled from its port 8545 (hypothetical) —
        // prefer the port the peer declared for its listener.
        let mut v = json!({
            "socket_addr": "0.0.0.0:9090", "node_name": "X"
        });
        rewrite_pulled_self_shard(&mut v, "1.2.3.4:8545");
        assert_eq!(v["socket_addr"], "1.2.3.4:9090");
    }

    #[test]
    fn pulled_stub_falls_back_to_pulled_url_port_when_declared_is_bad() {
        let mut v = json!({
            "socket_addr": "0.0.0.0:junk", "node_name": "X"
        });
        rewrite_pulled_self_shard(&mut v, "1.2.3.4:9090");
        assert_eq!(v["socket_addr"], "1.2.3.4:9090");
    }

    #[test]
    fn pulled_loopback_stub_also_rewritten() {
        // Seed set up with --rpc 127.0.0.1 on a misconfigured run would announce
        // 127.0.0.1:9090. The pulled URL is the routable address, use it.
        let mut v = json!({
            "socket_addr": "127.0.0.1:9090", "node_name": "X"
        });
        rewrite_pulled_self_shard(&mut v, "1.2.3.4:9090");
        assert_eq!(v["socket_addr"], "1.2.3.4:9090");
    }

    #[test]
    fn pulled_ipv6_stub_rewritten() {
        let mut v = json!({"socket_addr": "[::]:9090", "node_name": "X"});
        rewrite_pulled_self_shard(&mut v, "1.2.3.4:9090");
        assert_eq!(v["socket_addr"], "1.2.3.4:9090");

        let mut v = json!({"socket_addr": "[::1]:9090", "node_name": "X"});
        rewrite_pulled_self_shard(&mut v, "1.2.3.4:9090");
        assert_eq!(v["socket_addr"], "1.2.3.4:9090");
    }

    #[test]
    fn pulled_empty_addr_rewritten() {
        let mut v = json!({"socket_addr": "", "node_name": "X"});
        rewrite_pulled_self_shard(&mut v, "1.2.3.4:9090");
        assert_eq!(v["socket_addr"], "1.2.3.4:9090");
    }

    #[test]
    fn missing_socket_addr_field_is_noop() {
        // Malformed / partial self_shard JSON should not panic.
        let mut v = json!({"node_name": "X"});
        rewrite_pulled_self_shard(&mut v, "1.2.3.4:9090");
        assert!(v.get("socket_addr").is_none());
    }

    #[test]
    fn pulled_url_with_no_port_is_tolerated() {
        // Defensive: if seed_addrs_pull accidentally carries a host without a port,
        // rewrite still produces a sensible string (host + default 9090).
        let mut v = json!({"socket_addr": "0.0.0.0:9090", "node_name": "X"});
        rewrite_pulled_self_shard(&mut v, "136.244.109.1");
        assert_eq!(v["socket_addr"], "136.244.109.1:9090");
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env().add_directive("arc=info".parse()?))
        .init();

    let cli = Cli::parse();

    // ── Load config file and merge with CLI args ────────────────────────
    // Priority: explicit CLI arg > config file value > hardcoded default.
    // We use clap's ArgMatches to detect which args were explicitly provided.
    let matches = Cli::command().get_matches_from(std::env::args_os());

    let node_cfg = if let Some(config_path) = &cli.config {
        let cfg = config::load_config(config_path)
            .expect("Failed to load node config");
        tracing::info!("Loaded node config from {}", config_path);
        cfg
    } else {
        config::NodeConfig::default()
    };

    // Resolve each setting: CLI explicit > config file > default
    let rpc_addr = if matches.value_source("rpc") == Some(clap::parser::ValueSource::CommandLine) {
        cli.rpc.clone()
    } else {
        node_cfg.rpc.listen.clone()
    };

    let p2p_port = if matches.value_source("p2p_port") == Some(clap::parser::ValueSource::CommandLine) {
        cli.p2p_port
    } else {
        node_cfg.p2p.port
    };

    let stake = if matches.value_source("stake") == Some(clap::parser::ValueSource::CommandLine) {
        cli.stake
    } else {
        node_cfg.validator.stake
    };

    let data_dir = if matches.value_source("data_dir") == Some(clap::parser::ValueSource::CommandLine) {
        cli.data_dir.clone()
    } else {
        node_cfg.storage.data_dir.clone()
    };

    let min_stake = if matches.value_source("min_stake") == Some(clap::parser::ValueSource::CommandLine) {
        cli.min_stake
    } else {
        node_cfg.validator.min_stake
    };

    let validator_seed = if matches.value_source("validator_seed") == Some(clap::parser::ValueSource::CommandLine) {
        cli.validator_seed.clone()
    } else {
        node_cfg.validator.seed.clone()
    };

    let eth_rpc_port = if matches.value_source("eth_rpc_port") == Some(clap::parser::ValueSource::CommandLine) {
        cli.eth_rpc_port
    } else {
        node_cfg.rpc.eth_port
    };

    // Peers: merge CLI peers + config peers + seeds file
    let mut peers = if !cli.peers.is_empty() {
        cli.peers.clone()
    } else {
        node_cfg.p2p.peers.clone()
    };

    // Load additional seeds from file (if provided)
    if let Some(ref seeds_path) = cli.seeds_file {
        match std::fs::read_to_string(seeds_path) {
            Ok(contents) => {
                let seed_peers: Vec<String> = contents
                    .lines()
                    // Strip inline comments (everything after #) then trim whitespace.
                    // Supports format: "149.28.32.76:9091    # NYC (US East)"
                    .map(|l| l.split('#').next().unwrap_or("").trim())
                    .filter(|l| !l.is_empty())
                    .map(|l| l.to_string())
                    .collect();
                tracing::info!("Loaded {} seeds from {}", seed_peers.len(), seeds_path);
                peers.extend(seed_peers);
            }
            Err(e) => {
                tracing::warn!("Failed to read seeds file {}: {}", seeds_path, e);
            }
        }
    }

    // Deduplicate peers
    peers.sort();
    peers.dedup();

    // Benchmark settings: CLI > config > default
    let _bench_batch = if matches.value_source("bench_batch") == Some(clap::parser::ValueSource::CommandLine) {
        cli.bench_batch
    } else {
        node_cfg.benchmark.batch_size
    };

    let _bench_interval = if matches.value_source("bench_interval") == Some(clap::parser::ValueSource::CommandLine) {
        cli.bench_interval
    } else {
        node_cfg.benchmark.interval_ms
    };

    let bench_sender_start = if matches.value_source("bench_sender_start") == Some(clap::parser::ValueSource::CommandLine) {
        cli.bench_sender_start
    } else {
        node_cfg.benchmark.sender_start
    };

    let bench_sender_count = if matches.value_source("bench_sender_count") == Some(clap::parser::ValueSource::CommandLine) {
        cli.bench_sender_count
    } else {
        node_cfg.benchmark.sender_count
    };

    let bench_sign_threads = if matches.value_source("bench_sign_threads") == Some(clap::parser::ValueSource::CommandLine) {
        cli.bench_sign_threads
    } else {
        node_cfg.benchmark.sign_threads
    };

    let bench_rayon_threads = if matches.value_source("bench_rayon_threads") == Some(clap::parser::ValueSource::CommandLine) {
        cli.bench_rayon_threads
    } else {
        node_cfg.benchmark.rayon_threads
    };

    // ── Configure rayon thread pool ─────────────────────────────────────
    // In benchmark mode, limit rayon to leave CPU for signing threads
    if cli.benchmark {
        rayon::ThreadPoolBuilder::new()
            .num_threads(bench_rayon_threads)
            .build_global()
            .ok();
    }

    // ── Validate stake ──────────────────────────────────────────────────
    if stake < min_stake {
        eprintln!(
            "Error: stake {} ARC is below the minimum required {} ARC",
            stake, min_stake
        );
        std::process::exit(1);
    }

    // ── Derive validator keypair and address from seed ─────────────────
    // Deterministic: same seed → same keypair → same address across restarts.
    let validator_seed_bytes = blake3::derive_key("ARC-chain-validator-keypair-v1", validator_seed.as_bytes());
    let signing_key = ed25519_dalek::SigningKey::from_bytes(&validator_seed_bytes);
    let validator_keypair = KeyPair::Ed25519(signing_key);
    let validator_address = validator_keypair.address();

    // ── Determine stake tier for display ───────────────────────────────
    let tier = arc_consensus::StakeTier::from_stake(stake)
        .map(|t| format!("{:?}", t))
        .unwrap_or_else(|| "Below minimum".to_string());

    tracing::info!("╔═══════════════════════════════════════╗");
    tracing::info!("║   ARC Chain — Agent Runtime Chain     ║");
    tracing::info!("║   Testnet Node v0.3.0                 ║");
    tracing::info!("╚═══════════════════════════════════════╝");
    tracing::info!("Validator  : {}", validator_address);
    tracing::info!("Seed       : {}", validator_seed);
    tracing::info!("Stake      : {} ARC ({})", stake, tier);
    tracing::info!("RPC        : {}", rpc_addr);
    tracing::info!("P2P port   : {}", p2p_port);
    tracing::info!("Data dir   : {}", data_dir);
    if let Some(config_path) = &cli.config {
        tracing::info!("Config     : {}", config_path);
    }
    if let Some(genesis_path) = &cli.genesis {
        tracing::info!("Genesis    : {}", genesis_path);
    }
    if !peers.is_empty() {
        tracing::info!("Peers      : {:?}", peers);
    }

    // ── Genesis accounts — prefunded for testing ────────────────────────
    // Priority: --genesis file > hardcoded defaults.
    // In benchmark mode (without --genesis), use deterministic ed25519
    // keypair-derived addresses so signatures can be verified.
    // Extract genesis validators (for consensus) if --genesis is provided.
    // All nodes MUST use the same genesis → same validator set from round 0.
    let genesis_validators: Vec<(Hash256, u64)> = if let Some(genesis_path) = &cli.genesis {
        let genesis_cfg = config::load_genesis(genesis_path)
            .expect("Failed to load genesis config");
        genesis_cfg.validators.iter().map(|v| {
            let seed_bytes = blake3::derive_key("ARC-chain-validator-keypair-v1", v.seed.as_bytes());
            let sk = ed25519_dalek::SigningKey::from_bytes(&seed_bytes);
            let kp = KeyPair::Ed25519(sk);
            (kp.address(), v.stake)
        }).collect()
    } else {
        Vec::new()
    };

    let genesis_accounts: Vec<(Hash256, u64)> = if let Some(genesis_path) = &cli.genesis {
        let genesis_cfg = config::load_genesis(genesis_path)
            .expect("Failed to load genesis config");
        tracing::info!(
            "Genesis: {} ({} accounts, {} validators)",
            genesis_cfg.chain.name,
            genesis_cfg.accounts.len(),
            genesis_cfg.validators.len(),
        );
        genesis_cfg.accounts.iter().map(|a| {
            let mut bytes = [0u8; 32];
            hex::decode_to_slice(&a.address, &mut bytes)
                .unwrap_or_else(|e| {
                    eprintln!("Invalid genesis account address '{}': {}", a.address, e);
                    std::process::exit(1);
                });
            (Hash256(bytes), a.balance)
        }).collect()
    } else if cli.benchmark {
        // Benchmark mode: deterministic ed25519 keypair-derived addresses
        (0..100u8)
            .map(|i| (arc_crypto::benchmark_address(i), 1_000_000_000_000))
            .collect()
    } else {
        // Default: blake3-hashed addresses for testing
        (0..100u8)
            .map(|i| (hash_bytes(&[i]), 1_000_000_000_000))
            .collect()
    };

    // ── Ensure the validator/faucet address is funded ───────────────────
    // The faucet sends tokens from the validator address. If it's not already
    // a genesis account, add it so the faucet can actually fund new users.
    let genesis_accounts = {
        let mut accounts = genesis_accounts;
        if !accounts.iter().any(|(addr, _)| *addr == validator_address) {
            tracing::info!("Adding validator {} to genesis with faucet balance", validator_address);
            accounts.push((validator_address, 1_000_000_000_000));
        }
        accounts
    };

    let state = Arc::new({
        let mut db = StateDB::with_genesis_persistent(&genesis_accounts, &data_dir)
            .expect("Failed to initialize state with WAL persistence");
        if cli.archive {
            db.archive_mode = true;
            tracing::info!("Archive mode ENABLED — no pruning, full transaction history retained");
        }
        db
    });

    // ── State Sync Protocol (A5) — bootstrap from peer snapshot ─────
    // Auto-sync: if this node has peers configured and state is fresh (height 0),
    // automatically sync state from the first reachable peer. This allows new
    // nodes to join an existing network without manual --sync-from.
    let sync_peer = if cli.sync_from.is_some() {
        cli.sync_from.clone()
    } else if state.height() == 0 && !peers.is_empty() {
        // Try each peer until one responds
        // Quick check — try first 3 peers with 1s timeout each.
        // Don't block startup for unreachable peers.
        let mut found = None;
        for peer_addr in peers.iter().take(3) {
            let peer_rpc = peer_addr.replace(":9091", ":9090");
            let url = format!("http://{}/health", peer_rpc);
            match reqwest::Client::new().get(&url).timeout(std::time::Duration::from_secs(1)).send().await {
                Ok(resp) if resp.status().is_success() => {
                    tracing::info!("Auto-sync: peer {} reachable", peer_rpc);
                    found = Some(peer_rpc);
                    break;
                }
                _ => continue,
            }
        }
        found
    } else {
        None
    };

    if let Some(peer) = &sync_peer {
        tracing::info!("Bootstrapping from peer: {}", peer);

        let sync_mgr = arc_node::state_sync::StateSyncManager::new();
        match sync_mgr.sync_from_peer(peer, &state).await {
            Ok(height) => {
                tracing::info!("State sync complete, height = {}", height);
            }
            Err(e) => {
                tracing::warn!("Sync from peer failed ({}), continuing from genesis state", e);
                // Don't crash — the node will start from genesis and catch
                // up via DAG consensus. This is fine for testnet.
            }
        }
    }

    let mempool = Arc::new(Mempool::new(10_000_000));

    // ── Initialize candle float backend FIRST (for coherent inference) ──────
    // For GGUF files, load candle FIRST (lightweight Q4), then load tokenizer-only
    // from the same GGUF. This avoids loading 7GB INT8 weights on 8GB nodes.
    //
    // EXCEPTION: shard holders (--shard-start / --shard-end set) do NOT load
    // candle. Candle loads the FULL Q4 model (~4 GB on Llama-7B), and a shard
    // holder never runs /inference/run — only /inference/forward_shard — so
    // that 4 GB is pure waste. On 8 GB VPS this pushes the process into swap
    // and destroys forward_shard latency (observed: 20+ seconds/token on
    // swapping NYC). Disabling candle when the node is a shard-only role
    // keeps the RSS under 4 GB and makes the integer path run at real speed.
    let is_shard_holder = cli.shard_start.is_some() || cli.shard_end.is_some();
    let (candle_engine, candle_model_id): (Option<Arc<arc_inference::candle_backend::GgufEngine>>, Option<arc_crypto::Hash256>) =
        if is_shard_holder {
            tracing::info!("Shard holder mode — candle backend SKIPPED to save ~4 GB RAM");
            (None, None)
        } else if let Some(model_path) = &cli.model {
            if !model_path.ends_with(".arc-int8") {
                let engine = Arc::new(arc_inference::candle_backend::GgufEngine::new(120_000));
                match engine.load_gguf_file(model_path) {
                    Ok(mid) => {
                        tracing::info!("Candle float inference ENABLED (Q4 GGUF)");
                        (Some(engine), Some(mid))
                    }
                    Err(e) => {
                        tracing::warn!("Candle backend failed: {} — falling back to INT8", e);
                        (None, None)
                    }
                }
            } else {
                (None, None) // .arc-int8 files use integer engine only
            }
        } else {
            (None, None)
        };

    // ── Load tokenizer model (lightweight: vocab-only from TinyLlama if available, else from GGUF) ──
    let inference_model: Option<Arc<arc_inference::cached_integer_model::CachedIntegerModel>> =
        if let Some(model_path) = &cli.model {
            // If candle is handling inference via GGUF, we only need the tokenizer.
            // Try loading a small tokenizer model first (tinyllama), fall back to full GGUF.
            let tokenizer_path = if candle_engine.is_some() {
                // Check for a small tokenizer model alongside the main model
                let dir = std::path::Path::new(model_path).parent().unwrap_or(std::path::Path::new("."));
                let tiny = dir.join("tinyllama-1.1b.arc-int8");
                if tiny.exists() {
                    tracing::info!("Using TinyLlama tokenizer (lightweight)");
                    tiny.to_string_lossy().to_string()
                } else {
                    model_path.clone()
                }
            } else {
                model_path.clone()
            };

            tracing::info!("Loading model from {}...", tokenizer_path);
            let load_start = Instant::now();
            let load_result = if cli.tokenizer_only {
                tracing::info!("TOKENIZER-ONLY MODE: loading vocab + config, no weights (~30MB)");
                arc_inference::cached_integer_model::load_tokenizer_only(&tokenizer_path)
            } else if tokenizer_path.ends_with(".arc-int8") {
                arc_inference::cached_integer_model::load_cached_model_binary(&tokenizer_path)
            } else if let (Some(start), Some(end)) = (cli.shard_start, cli.shard_end) {
                tracing::info!("SHARD MODE: loading layers [{}, {}) only", start, end);
                arc_inference::cached_integer_model::load_cached_model_shard(&tokenizer_path, start, end)
            } else {
                arc_inference::cached_integer_model::load_cached_model(&tokenizer_path)
            };
            match load_result {
                Ok(model) => {
                    let elapsed = load_start.elapsed();
                    let mb_held: usize = model.layers.iter()
                        .filter(|l| l.is_loaded())
                        .map(|l| l.wq.memory_bytes() + l.wk.memory_bytes() + l.wv.memory_bytes()
                            + l.wo.memory_bytes() + l.w_gate.memory_bytes() + l.w_up.memory_bytes()
                            + l.w_down.memory_bytes())
                        .sum::<usize>() / (1024 * 1024);
                    let layers_held = model.layers.iter().filter(|l| l.is_loaded()).count();
                    tracing::info!(
                        "Model loaded in {:.1}s — {} layers held / {} total, {} MB shard weights, vocab {}",
                        elapsed.as_secs_f64(), layers_held, model.config.n_layers, mb_held,
                        model.config.vocab_size
                    );
                    Some(Arc::new(model))
                }
                Err(e) => {
                    tracing::error!("Failed to load model: {}", e);
                    None
                }
            }
        } else {
            None
        };

    // ── Record boot time for uptime tracking ──────────────────────────
    let boot_time = Instant::now();

    // ── Create channels for P2P transport ↔ consensus ─────────────────
    let (inbound_tx, inbound_rx) = mpsc::channel::<InboundMessage>(1000);
    let (outbound_tx, outbound_rx) = mpsc::channel::<OutboundMessage>(4000);
    let peer_count = Arc::new(AtomicU32::new(0));

    // Deterministic genesis hash (same for all nodes with same genesis config)
    let genesis_hash = Block::genesis().hash;

    // Parse bootstrap peers
    let bootstrap_peers: Vec<SocketAddr> = peers
        .iter()
        .filter_map(|p| p.parse().ok())
        .collect();

    let listen_addr: SocketAddr = format!("0.0.0.0:{}", p2p_port).parse()?;

    // ── Start P2P transport in background ──────────────────────────────
    let peer_count_transport = peer_count.clone();
    let transport_keypair = validator_keypair.clone();
    tokio::spawn(run_transport(
        listen_addr,
        bootstrap_peers,
        validator_address,
        stake,
        genesis_hash,
        outbound_rx,
        inbound_tx,
        peer_count_transport,
        transport_keypair,
        data_dir.clone(),
    ));

    // ── Start benchmark signing pool + indexer (if benchmark mode) ─────
    let benchmark_pool = if cli.benchmark {
        state.start_benchmark_indexer();
        let pool = BenchmarkPool::start(
            bench_sender_start,
            bench_sender_count,
            bench_sign_threads,
            10_000, // txs per batch
        );
        tracing::info!(
            "Benchmark mode ACTIVE — ed25519 signed txs, senders {}-{}, async indexing",
            bench_sender_start,
            bench_sender_start + bench_sender_count - 1
        );
        Some(Arc::new(pool))
    } else {
        None
    };

    // ── Start DAG consensus in background ─────────────────────────────
    // Initialize with ALL known validators from seeds file. This ensures
    // all nodes have the same validator set from boot — critical for
    // deterministic leader selection. Without this, nodes that connect
    // peers at different speeds have different validator counts, causing
    // different leader selection for the same round.
    // If genesis validators are provided, use them. This ensures ALL nodes
    // have the SAME validator set from round 0 — the key to consensus.
    // Without this, nodes discover peers at different times → different
    // validator counts → different epoch freezes → different leaders.
    let peer_vals: Vec<(Hash256, u64)> = genesis_validators.iter()
        .filter(|(addr, _)| *addr != validator_address)
        .cloned()
        .collect();
    let all_vals: Vec<(Hash256, u64)> = {
        let mut v = vec![(validator_address, stake)];
        v.extend(&peer_vals);
        v
    };
    let dag_validators = Arc::new(parking_lot::RwLock::new(all_vals));
    let dag_round = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let dag_committed = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let mut consensus =
        ConsensusManager::new_with_keypair(validator_address, stake, 4 /* num_shards */, cli.benchmark, &peer_vals, validator_keypair);
    consensus.dag_validators = Some(dag_validators.clone());
    consensus.dag_round = Some(dag_round.clone());
    consensus.dag_committed = Some(dag_committed.clone());
    // DAG persistence WAL — survives restarts
    let dag_wal_path = format!("{}/dag-wal", data_dir);
    std::fs::create_dir_all(&dag_wal_path).ok();
    if let Ok(dag_wal) = arc_state::WalWriter::with_segments(&dag_wal_path, 64 * 1024 * 1024) {
        consensus.dag_wal = Some(Arc::new(dag_wal));
        tracing::info!("DAG persistence WAL enabled: {}", dag_wal_path);
    }
    consensus.set_proposer_mode(cli.proposer_mode);
    let state_clone = state.clone();
    let mempool_clone = mempool.clone();
    let pool_clone = benchmark_pool.clone();
    // Run consensus on a dedicated thread with its own tokio runtime.
    // This prevents broadcast/transport/RPC tasks from starving the
    // consensus loop (the root cause of random freezes at ~4000 rounds).
    // If the consensus thread panics, log the error and exit the process —
    // a node without consensus is useless and should restart via systemd.
    std::thread::Builder::new()
        .name("consensus".into())
        .spawn(move || {
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .expect("consensus runtime");
                rt.block_on(async move {
                    consensus
                        .run_consensus_loop(
                            state_clone,
                            mempool_clone,
                            Some(inbound_rx),
                            Some(outbound_tx),
                            pool_clone,
                        )
                        .await;
                });
            }));
            match result {
                Ok(()) => {
                    tracing::error!("Consensus loop exited unexpectedly — shutting down");
                }
                Err(panic_info) => {
                    tracing::error!("CONSENSUS THREAD PANICKED: {:?}", panic_info);
                }
            }
            // Exit the process — a node without consensus must restart
            std::process::exit(1);
        })
        .expect("spawn consensus thread");

    // ── Start ETH JSON-RPC server (MetaMask, Hardhat, Foundry) ──────────
    if eth_rpc_port > 0 {
        let eth_addr = format!("0.0.0.0:{}", eth_rpc_port);
        let eth_node = rpc::build_node_state(
            state.clone(),
            mempool.clone(),
            validator_address,
            stake,
            boot_time,
            peer_count.clone(),
            inference_model.clone(),
            candle_engine.clone(),
            candle_model_id,
        );
        tracing::info!("ETH RPC    : {} (MetaMask/Hardhat/Foundry)", eth_addr);
        tokio::spawn(async move {
            if let Err(e) = rpc::serve_eth(&eth_addr, eth_node).await {
                tracing::error!("ETH RPC server error: {}", e);
            }
        });
    }

    // ── Start RPC server ────────────────────────────────────────────────
    if candle_engine.is_some() {
        tracing::info!("Inference  : ENABLED (candle Q4 float, coherent output)");
    } else if inference_model.is_some() {
        tracing::info!("Inference  : ENABLED (INT8 integer engine)");
    }
    tracing::info!("RPC server listening on {}", rpc_addr);

    // ── Graceful shutdown handler ───────────────────────────────────────
    // On SIGTERM (from systemd stop / rolling upgrade), drain pending state
    // and close connections before exiting. This prevents lost transactions
    // and allows other validators to see a clean disconnect.
    let shutdown_state = state.clone();
    tokio::spawn(async move {
        match tokio::signal::ctrl_c().await {
            Ok(()) => {
                tracing::info!("SIGINT received — initiating graceful shutdown...");
            }
            Err(e) => {
                tracing::warn!("Failed to install SIGINT handler: {}", e);
                return;
            }
        }
        tracing::info!("Flushing WAL and pending state...");
        shutdown_state.sync_wal();
        tracing::info!("Graceful shutdown complete. Exiting.");
        std::process::exit(0);
    });

    // Also handle SIGTERM (systemd sends this)
    #[cfg(unix)]
    {
        let shutdown_state = state.clone();
        tokio::spawn(async move {
            let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                .expect("SIGTERM handler");
            sigterm.recv().await;
            tracing::info!("SIGTERM received — initiating graceful shutdown...");
            shutdown_state.sync_wal();
            tracing::info!("Graceful shutdown complete. Exiting.");
            std::process::exit(0);
        });
    }

    // Build shard_info if this node is a shard holder, then broadcast it
    // to all seed peers so the network's shard registry converges.
    let shard_info_for_broadcast = match (cli.shard_start, cli.shard_end, &inference_model) {
        (Some(start), Some(end), Some(model)) => {
            let total_layers = model.config.n_layers;
            let memory_mb: usize = model.layers.iter()
                .filter(|l| l.is_loaded())
                .map(|l| l.wq.memory_bytes() + l.wk.memory_bytes() + l.wv.memory_bytes()
                    + l.wo.memory_bytes() + l.w_gate.memory_bytes() + l.w_up.memory_bytes()
                    + l.w_down.memory_bytes())
                .sum::<usize>() / (1024 * 1024);
            // Estimate full model size: extrapolate from this shard
            let layers_held = end.saturating_sub(start).max(1);
            let full_model_mb = memory_mb * total_layers / layers_held;
            // Build a stable model id from config
            let model_id_data = format!(
                "arc-{}L-{}d-{}h-{}v",
                model.config.n_layers, model.config.d_model,
                model.config.n_heads, model.config.vocab_size
            );
            let model_id_hash = arc_crypto::hash_bytes(model_id_data.as_bytes());
            // Public socket: prefer external IP if known, fall back to listening port
            let socket_addr = std::env::var("ARC_PUBLIC_SOCKET")
                .unwrap_or_else(|_| format!("{}:{}", rpc_addr.split(':').next().unwrap_or("127.0.0.1"), rpc_addr.split(':').nth(1).unwrap_or("9090")));
            Some(rpc::ShardInfo {
                start_layer: start,
                end_layer: end,
                total_layers,
                model_id: format!("0x{}", hex::encode(&model_id_hash.0)),
                model_name: model_id_data,
                memory_mb,
                full_model_mb,
                socket_addr,
                node_name: validator_seed.clone(),
            })
        }
        _ => None,
    };
    let shard_info = shard_info_for_broadcast.clone();

    // Spawn a background task that announces this node's shard to all seeds
    // AND pulls their shard info back. Runs immediately at startup + every 15s
    // so the network's shard registry converges fast.
    if let Some(si) = shard_info_for_broadcast.clone() {
        // Build the list of peer RPC URLs from the seeds file. The seeds file
        // contains "host:p2p_port" lines; the RPC port is always p2p - 1 in
        // our deployment, but we conservatively try both 9090 (the seed default)
        // and (p2p_port - 1) to handle community nodes on different ports.
        let mut seed_addrs: Vec<String> = Vec::new();
        for p in &peers {
            // p is a SocketAddr string like "1.2.3.4:9091"
            if let Some(host) = p.split(':').next() {
                seed_addrs.push(format!("{}:9090", host));
                if let Some(port_str) = p.split(':').nth(1) {
                    if let Ok(port) = port_str.parse::<u16>() {
                        if port > 1 && port - 1 != 9090 {
                            seed_addrs.push(format!("{}:{}", host, port - 1));
                        }
                    }
                }
            }
        }
        seed_addrs.sort();
        seed_addrs.dedup();
        let seed_addrs_pull = seed_addrs.clone();

        // Background broadcaster: post our shard to every seed AND to our
        // own localhost so the self-entry in the local registry gets its
        // timestamp refreshed every tick. Without the localhost post, the
        // 60s TTL on the registry would prune the self entry even while
        // the node is still live.
        let local_announce_broadcast = format!("http://127.0.0.1:{}/shards/announce", rpc_addr.split(':').nth(1).unwrap_or("9090"));
        tokio::spawn(async move {
            // Brief settle so the local /shards endpoint is up before we ask
            // peers to fetch from us
            tokio::time::sleep(std::time::Duration::from_secs(3)).await;
            let client = match reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(5))
                .build() {
                Ok(c) => c,
                Err(_) => return,
            };
            loop {
                let payload = serde_json::json!({"shard": &si});
                // Refresh our own entry first
                let _ = client.post(&local_announce_broadcast).json(&payload).send().await;
                // Then announce to remote seeds
                for addr in &seed_addrs {
                    let url = format!("http://{}/shards/announce", addr);
                    let _ = client.post(&url).json(&payload).send().await;
                }
                tokio::time::sleep(std::time::Duration::from_secs(15)).await;
            }
        });

        // Background puller: fetch each seed's /shards and re-announce them locally.
        // This converges the registry even when a peer was offline when we first
        // announced. Anyone we reach contributes their full registry to ours.
        let local_announce = format!("http://127.0.0.1:{}/shards/announce", rpc_addr.split(':').nth(1).unwrap_or("9090"));
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_secs(5)).await;
            let client = match reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(5))
                .build() {
                Ok(c) => c,
                Err(_) => return,
            };
            loop {
                // Pull ONLY each seed's own self_shard, not its whole registry.
                // Re-announcing every remote entry we see would resurrect stale
                // entries: a seed's registry can still contain a shard for a
                // node that restarted without --shard-start flags, and pulling
                // + re-announcing it would defeat the 60s TTL and keep the
                // phantom shard alive forever. Each real shard holder already
                // broadcasts its own shard every 15s via the outbound
                // broadcaster above, so trusting only self_shard is both
                // sufficient and safe.
                //
                // IMPORTANT: A peer's self_shard.socket_addr is almost always
                // "0.0.0.0:<port>" because the peer binds to all interfaces and
                // doesn't know its own public IP. Re-announcing that stub
                // locally would make /inference/run_sharded unable to route to
                // the peer (dialing 0.0.0.0 fails). Rewrite the stub to the
                // *seed's actual address* (the URL we just pulled from) before
                // re-announcing — that IS the routable address for that shard
                // holder, and it's what the receiver-side fix for direct
                // /shards/announce broadcasts produces too.
                for addr in &seed_addrs_pull {
                    if let Ok(resp) = client.get(format!("http://{}/shards", addr)).send().await {
                        if let Ok(mut json) = resp.json::<serde_json::Value>().await {
                            if let Some(self_shard) = json.get_mut("self_shard") {
                                if !self_shard.is_null() {
                                    rewrite_pulled_self_shard(self_shard, addr);
                                    let payload = serde_json::json!({"shard": self_shard});
                                    let _ = client.post(&local_announce).json(&payload).send().await;
                                }
                            }
                        }
                    }
                }
                tokio::time::sleep(std::time::Duration::from_secs(20)).await;
            }
        });

        tracing::info!("Shard announcement broadcaster + puller started (15s/20s tick)");
    }

    // ── Community-mode HTTP registration + heartbeat ──────────────────
    // Spawned when --community-mode is set. Outbound-HTTPS only — works
    // behind any NAT/residential firewall. Registers with every seed on
    // startup + every 60s, sends a heartbeat every 15s to keep the
    // registry entry alive. Each seed's TTL is 90s so 5 missed
    // heartbeats before eviction.
    // Auto-enable community mode for observer nodes (stake=0).
    // If you join with no stake, you're a community contributor — no flag needed.
    let community_mode = cli.community_mode || stake == 0;

    if community_mode {
        tracing::info!("╔═══════════════════════════════════════╗");
        tracing::info!("║  COMMUNITY MODE ACTIVE                ║");
        tracing::info!("║  Registering with seed coordinators   ║");
        tracing::info!("║  Your node provides TPS + inference   ║");
        tracing::info!("╚═══════════════════════════════════════╝");
    }

    if community_mode {
        let validator_seed_c = validator_seed.clone();
        let worker_id = format!("0x{}", hex::encode(&validator_address.0));
        let hostname = std::process::Command::new("hostname")
            .output()
            .ok()
            .and_then(|o| String::from_utf8(o.stdout).ok())
            .map(|s| s.trim().to_string())
            .unwrap_or_else(|| "unknown".to_string());
        let platform = format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH);
        let model_name = inference_model
            .as_ref()
            .map(|m| format!("arc-{}L-{}d-{}h-{}v",
                m.config.n_layers, m.config.d_model, m.config.n_heads, m.config.vocab_size));

        // Derive seed RPC endpoints from the peers list (P2P port - 1, usually 9090)
        let mut seed_rpc_addrs: Vec<String> = Vec::new();
        for p in &peers {
            if let Some(host) = p.split(':').next() {
                seed_rpc_addrs.push(format!("{}:9090", host));
            }
        }
        seed_rpc_addrs.sort();
        seed_rpc_addrs.dedup();

        let worker_id_c = worker_id.clone();
        let hostname_c = hostname.clone();
        let platform_c = platform.clone();
        let model_name_c = model_name.clone();
        let seed_rpc_addrs_c = seed_rpc_addrs.clone();
        let rpc_addr_c = rpc_addr.clone();
        // Pre-compute model info for auto-shard registration
        let model_id_hex = inference_model.as_ref().map(|m| {
            let id_data = format!("arc-{}L-{}d-{}h-{}v",
                m.config.n_layers, m.config.d_model, m.config.n_heads, m.config.vocab_size);
            format!("0x{}", hex::encode(arc_crypto::hash_bytes(id_data.as_bytes()).0))
        });
        let total_layers = inference_model.as_ref().map(|m| m.config.n_layers as u32);
        let avail_mem_mb: u64 = inference_model.as_ref()
            .map(|m| (m.config.d_model * m.config.n_layers * 4 / 1024 / 1024) as u64 * 2)
            .unwrap_or(8192)
            .max(4096);

        tokio::spawn(async move {
            // Settle before first POST
            tokio::time::sleep(std::time::Duration::from_secs(5)).await;
            let client = match reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(5))
                .build() {
                Ok(c) => c,
                Err(_) => return,
            };

            // Model info for auto-shard assignment is pre-computed above the spawn.
            let register_payload = serde_json::json!({
                "worker_id": worker_id_c,
                "name": format!("{} ({})", validator_seed_c, hostname_c),
                "capabilities": ["inference"],
                "model": model_name_c,
                "platform": platform_c,
                "model_id": &model_id_hex,
                "total_layers": &total_layers,
                "rpc_addr": &rpc_addr_c,
                "available_memory_mb": avail_mem_mb,
            });
            let heartbeat_payload = serde_json::json!({
                "worker_id": worker_id_c,
                "work_completed": 0,
            });

            // Register once, then heartbeat + re-register periodically.
            // Community gateway runs on port 3001 as a sidecar alongside
            // the main arc-node on 9090. Try both 3001 (gateway) and 9090
            // (if the seed runs the dd0 binary with built-in endpoints).
            let mut ticks: u64 = 0;
            loop {
                for addr in &seed_rpc_addrs_c {
                    // Derive gateway port from RPC addr: replace :9090 with :3001
                    let host = addr.split(':').next().unwrap_or(addr);
                    let gateway_addr = format!("{}:3001", host);
                    // Every 4th tick (60s), do a full re-register to pick up metadata changes
                    if ticks % 4 == 0 {
                        // Try gateway first (port 3001), then arc-node (port 9090)
                        let r = client.post(format!("http://{}/community/register", gateway_addr))
                            .json(&register_payload).send().await;
                        let resp = if let Ok(resp) = r {
                            resp.json::<serde_json::Value>().await.ok()
                        } else {
                            let r2 = client.post(format!("http://{}/community/register", addr))
                                .json(&register_payload).send().await;
                            if let Ok(resp) = r2 { resp.json::<serde_json::Value>().await.ok() } else { None }
                        };
                        // Log shard assignment from coordinator (auto-sharding)
                        if let Some(ref resp) = resp {
                            if let Some(sa) = resp.get("shard_assignment") {
                                if !sa.is_null() {
                                    tracing::info!(
                                        start = sa.get("start_layer").and_then(|v| v.as_u64()).unwrap_or(0),
                                        end = sa.get("end_layer").and_then(|v| v.as_u64()).unwrap_or(0),
                                        total = sa.get("total_layers").and_then(|v| v.as_u64()).unwrap_or(0),
                                        seed = %addr,
                                        "Auto-shard assignment received from coordinator"
                                    );
                                }
                            }
                        }
                    } else {
                        let r = client.post(format!("http://{}/community/heartbeat", gateway_addr))
                            .json(&heartbeat_payload).send().await;
                        if r.is_err() {
                            let _ = client.post(format!("http://{}/community/heartbeat", addr))
                                .json(&heartbeat_payload).send().await;
                        }
                    }
                }
                ticks += 1;
                tokio::time::sleep(std::time::Duration::from_secs(15)).await;
            }
        });
        tracing::info!("Community-mode HTTP registration started (worker_id={})", worker_id);

        // ── Community inference worker loop ──────────────────────────────
        // Continuously long-poll /community/claim_work on all seeds. When
        // a job arrives, run inference locally using the loaded model, then
        // POST the result back to /community/submit_work. This is what
        // makes community nodes provide REAL inference compute.
        if let Some(model) = inference_model.clone() {
            let worker_id_w = worker_id.clone();
            let seed_rpc_addrs_w = seed_rpc_addrs.clone();
            tokio::spawn(async move {
                tokio::time::sleep(std::time::Duration::from_secs(10)).await;
                let client = match reqwest::Client::builder()
                    .timeout(std::time::Duration::from_secs(35)) // 30s claim + 5s overhead
                    .build() {
                    Ok(c) => c,
                    Err(_) => return,
                };
                tracing::info!("Community inference worker started — polling for jobs");
                loop {
                    // Try each seed's gateway for work
                    for addr in &seed_rpc_addrs_w {
                        let host = addr.split(':').next().unwrap_or(addr);
                        let gateway = format!("{}:3001", host);
                        let claim_body = serde_json::json!({
                            "worker_id": worker_id_w,
                            "capabilities": ["inference"],
                        });
                        let resp = match client
                            .post(format!("http://{}/community/claim_work", gateway))
                            .json(&claim_body)
                            .send()
                            .await
                        {
                            Ok(r) => r,
                            Err(_) => continue,
                        };
                        let job: serde_json::Value = match resp.json().await {
                            Ok(j) => j,
                            Err(_) => continue,
                        };
                        if job.get("status").and_then(|s| s.as_str()) != Some("work") {
                            continue; // no_work — try next seed
                        }
                        let job_id = job.get("job_id").and_then(|s| s.as_str()).unwrap_or("").to_string();
                        let input = job.get("input").and_then(|s| s.as_str()).unwrap_or("").to_string();
                        let max_tokens = job.get("max_tokens").and_then(|v| v.as_u64()).unwrap_or(20) as u32;
                        if input.is_empty() || job_id.is_empty() { continue; }

                        tracing::info!("Claimed job {} from {}: {:?} (max_tokens={})",
                            job_id, gateway, &input[..input.len().min(40)], max_tokens);

                        // Run inference locally
                        let start = std::time::Instant::now();
                        let (generated, hash) = model.generate(
                            &{
                                let mut toks = vec![model.config.bos_token];
                                toks.extend(model.encode(&input));
                                toks
                            },
                            max_tokens,
                            &model.config.eos_tokens,
                        );
                        let elapsed_ms = start.elapsed().as_millis() as u64;
                        let output_text = model.decode(&generated);
                        let tokens_gen = generated.len() as u64;
                        let ms_per_tok = if tokens_gen > 0 { elapsed_ms / tokens_gen } else { 0 };

                        tracing::info!("Job {} done: {} tokens in {}ms = {} ms/tok",
                            job_id, tokens_gen, elapsed_ms, ms_per_tok);

                        // Submit result
                        let result_body = serde_json::json!({
                            "job_id": job_id,
                            "worker_id": worker_id_w,
                            "success": true,
                            "output": output_text,
                            "output_hash": format!("0x{}", hex::encode(&hash.0)),
                            "tokens_generated": tokens_gen,
                            "total_ms": elapsed_ms,
                            "ms_per_token": ms_per_tok,
                            "engine": "INT8 integer (community worker)",
                        });
                        let _ = client
                            .post(format!("http://{}/community/submit_work", gateway))
                            .json(&result_body)
                            .timeout(std::time::Duration::from_secs(10))
                            .send()
                            .await;
                        break; // after completing a job, go back to the top and poll again
                    }
                    // Brief sleep between poll rounds to avoid hammering
                    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                }
            });
            tracing::info!("Community inference worker loop spawned");
        }
    }

    rpc::serve(
        &rpc_addr,
        state,
        mempool,
        validator_address,
        stake,
        boot_time,
        peer_count,
        inference_model,
        candle_engine,
        candle_model_id,
        Some(dag_validators),
        Some(dag_round),
        Some(dag_committed),
        shard_info,
    )
    .await?;

    Ok(())
}
