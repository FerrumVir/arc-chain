use anyhow::Result;
use arc_crypto::{hash_bytes, Hash256};
use arc_mempool::Mempool;
use arc_net::transport::{run_transport, InboundMessage, OutboundMessage};
use arc_node::{benchmark::BenchmarkPool, consensus::ConsensusManager, rpc};
use arc_state::StateDB;
use arc_types::Block;
use clap::Parser;
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
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env().add_directive("arc=info".parse()?))
        .init();

    let cli = Cli::parse();

    // ── Configure rayon thread pool ─────────────────────────────────────
    // In benchmark mode, limit rayon to leave CPU for signing threads
    if cli.benchmark {
        rayon::ThreadPoolBuilder::new()
            .num_threads(cli.bench_rayon_threads)
            .build_global()
            .ok();
    }

    // ── Validate stake ──────────────────────────────────────────────────
    if cli.stake < cli.min_stake {
        eprintln!(
            "Error: stake {} ARC is below the minimum required {} ARC",
            cli.stake, cli.min_stake
        );
        std::process::exit(1);
    }

    // ── Derive validator address from seed ─────────────────────────────
    let validator_address = hash_bytes(cli.validator_seed.as_bytes());

    // ── Determine stake tier for display ───────────────────────────────
    let tier = arc_consensus::StakeTier::from_stake(cli.stake)
        .map(|t| format!("{:?}", t))
        .unwrap_or_else(|| "Below minimum".to_string());

    tracing::info!("╔═══════════════════════════════════════╗");
    tracing::info!("║   ARC Chain — Agent Runtime Chain     ║");
    tracing::info!("║   Testnet Node v0.1.0                 ║");
    tracing::info!("╚═══════════════════════════════════════╝");
    tracing::info!("Validator  : {}", validator_address);
    tracing::info!("Seed       : {}", cli.validator_seed);
    tracing::info!("Stake      : {} ARC ({})", cli.stake, tier);
    tracing::info!("RPC        : {}", cli.rpc);
    tracing::info!("P2P port   : {}", cli.p2p_port);
    tracing::info!("Data dir   : {}", cli.data_dir);
    if !cli.peers.is_empty() {
        tracing::info!("Peers      : {:?}", cli.peers);
    }

    // ── Genesis accounts — prefunded for testing ────────────────────────
    // In benchmark mode, use deterministic ed25519 keypair-derived addresses
    // so signatures can be verified. All nodes derive the same keypairs.
    let genesis_accounts: Vec<(Hash256, u64)> = if cli.benchmark {
        (0..100u8)
            .map(|i| (arc_crypto::benchmark_address(i), 1_000_000_000_000))
            .collect()
    } else {
        (0..100u8)
            .map(|i| (hash_bytes(&[i]), 1_000_000_000_000))
            .collect()
    };

    // TODO: Use `cli.data_dir` for WAL persistence once disk-backed mode is wired.
    let state = Arc::new(StateDB::with_genesis(&genesis_accounts));
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
    let bootstrap_peers: Vec<SocketAddr> = cli
        .peers
        .iter()
        .filter_map(|p| p.parse().ok())
        .collect();

    let listen_addr: SocketAddr = format!("0.0.0.0:{}", cli.p2p_port).parse()?;

    // ── Start P2P transport in background ──────────────────────────────
    let peer_count_transport = peer_count.clone();
    tokio::spawn(run_transport(
        listen_addr,
        bootstrap_peers,
        validator_address,
        cli.stake,
        genesis_hash,
        outbound_rx,
        inbound_tx,
        peer_count_transport,
    ));

    // ── Start benchmark signing pool + indexer (if benchmark mode) ─────
    let benchmark_pool = if cli.benchmark {
        state.start_benchmark_indexer();
        let pool = BenchmarkPool::start(
            cli.bench_sender_start,
            cli.bench_sender_count,
            cli.bench_sign_threads,
            10_000, // txs per batch
        );
        tracing::info!(
            "Benchmark mode ACTIVE — ed25519 signed txs, senders {}-{}, async indexing",
            cli.bench_sender_start,
            cli.bench_sender_start + cli.bench_sender_count - 1
        );
        Some(Arc::new(pool))
    } else {
        None
    };

    // ── Start DAG consensus in background ─────────────────────────────
    // Each node starts single-validator; peers added dynamically via P2P PeerConnected.
    // In benchmark mode this keeps the fast path active (no DAG quorum needed).
    let consensus =
        ConsensusManager::new(validator_address, cli.stake, 4 /* num_shards */, cli.benchmark, &[]);
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

    // ── Start RPC server ────────────────────────────────────────────────
    tracing::info!("RPC server listening on {}", cli.rpc);
    rpc::serve(
        &cli.rpc,
        state,
        mempool,
        validator_address,
        cli.stake,
        boot_time,
        peer_count,
    )
    .await?;

    Ok(())
}
