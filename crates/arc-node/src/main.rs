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
    /// RPC listen address
    #[arg(long, default_value = "0.0.0.0:9090")]
    rpc: String,

    /// P2P listen port (QUIC)
    #[arg(long, default_value_t = 9091)]
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

    /// Minimum staked ARC required to run this node
    #[arg(long, default_value_t = 500_000)]
    min_stake: u64,

    /// Validator identity seed (used to derive a unique address).
    /// Different seeds produce different validator addresses.
    /// Default: "arc-validator-0"
    #[arg(long, default_value = "arc-validator-0")]
    validator_seed: String,

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

    // Peers: merge CLI peers with config peers (CLI takes priority if any provided)
    let peers = if !cli.peers.is_empty() {
        cli.peers.clone()
    } else {
        node_cfg.p2p.peers.clone()
    };

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
    tracing::info!("║   Testnet Node v0.1.0                 ║");
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

    let state = Arc::new(StateDB::with_genesis_persistent(&genesis_accounts, &data_dir)
        .expect("Failed to initialize state with WAL persistence"));

    // ── State Sync Protocol (A5) — bootstrap from peer snapshot ─────
    if let Some(peer) = &cli.sync_from {
        tracing::info!("Bootstrapping from peer: {}", peer);

        let sync_mgr = arc_node::state_sync::StateSyncManager::new();
        match sync_mgr.sync_from_peer(peer, &state).await {
            Ok(height) => {
                tracing::info!("State sync complete, height = {}", height);
            }
            Err(e) => {
                tracing::warn!("Chunked sync failed ({}), falling back to monolithic snapshot", e);
                // Fallback: try file-based snapshot
                let snapshot_path = format!("{}/snapshot.lz4", data_dir);
                let snapshot = arc_state::Snapshot::read_from(&snapshot_path)
                    .unwrap_or_else(|e| {
                        tracing::error!("Failed to read snapshot from {}: {}", snapshot_path, e);
                        tracing::error!(
                            "Ensure the peer is running and reachable, or place a snapshot.lz4 \
                             file in the data directory."
                        );
                        std::process::exit(1);
                    });
                tracing::info!(
                    height = snapshot.block_height,
                    accounts = snapshot.accounts.len(),
                    state_root = %snapshot.state_root,
                    "Importing snapshot from file"
                );
                state.import_snapshot(&snapshot, snapshot.state_root)
                    .unwrap_or_else(|e| {
                        tracing::error!("Snapshot verification failed: {}", e);
                        std::process::exit(1);
                    });
                tracing::info!("Snapshot imported and verified, height = {}", state.height());
            }
        }
    }

    let mempool = Arc::new(Mempool::new(10_000_000));

    // ── Record boot time for uptime tracking ──────────────────────────
    let boot_time = Instant::now();

    // ── Create channels for P2P transport ↔ consensus ─────────────────
    let (inbound_tx, inbound_rx) = mpsc::channel::<InboundMessage>(1000);
    let (outbound_tx, outbound_rx) = mpsc::channel::<OutboundMessage>(1000);
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
    // Each node starts single-validator; peers added dynamically via P2P PeerConnected.
    // In benchmark mode this keeps the fast path active (no DAG quorum needed).
    let mut consensus =
        ConsensusManager::new_with_keypair(validator_address, stake, 4 /* num_shards */, cli.benchmark, &[], validator_keypair);
    consensus.set_proposer_mode(cli.proposer_mode);
    let state_clone = state.clone();
    let mempool_clone = mempool.clone();
    let pool_clone = benchmark_pool.clone();
    tokio::spawn(async move {
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
        );
        tracing::info!("ETH RPC    : {} (MetaMask/Hardhat/Foundry)", eth_addr);
        tokio::spawn(async move {
            if let Err(e) = rpc::serve_eth(&eth_addr, eth_node).await {
                tracing::error!("ETH RPC server error: {}", e);
            }
        });
    }

    // ── Start RPC server ────────────────────────────────────────────────
    tracing::info!("RPC server listening on {}", rpc_addr);
    rpc::serve(
        &rpc_addr,
        state,
        mempool,
        validator_address,
        stake,
        boot_time,
        peer_count,
    )
    .await?;

    Ok(())
}
