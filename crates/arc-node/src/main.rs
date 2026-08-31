mod config;
mod validator_identity;

use anyhow::{Context, Result, bail, ensure};
use arc_crypto::{Hash256, hash_bytes};
use arc_mempool::Mempool;
use arc_net::transport::{
    InboundMessage, OutboundMessage, run_transport_with_readiness_and_shutdown,
};
use arc_node::{
    benchmark::BenchmarkPool,
    consensus::{ConsensusManager, RecoveryDagRollover},
    recovery_dag_wal::{
        BaselineState as DagBaselineState, CurrentStreamSummary, DagCursor, GenerationInput,
        GenerationPin, GenerationStore, HARD_MAX_RETENTION_ROUND_SPAN,
        RecoveryDagBinding as GenerationDagBinding, RetainedDagRecord, RetainedRecordKind,
        RetentionLimits, StoreAuditStatus, TornSuffix, VerifiedGeneration,
    },
    rpc,
};
use arc_state::StateDB;
use arc_types::Block;
use clap::{CommandFactory, Parser, Subcommand};
use serde::{Deserialize, Serialize};
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::AtomicU32;
use std::time::{Instant, SystemTime, UNIX_EPOCH};
use tokio::sync::mpsc;
use tracing_subscriber::EnvFilter;
use zeroize::{Zeroize, Zeroizing};

#[derive(Parser)]
#[command(name = "arc-node", version, about = "ARC Chain Node")]
struct Cli {
    /// Offline ARCCHKPT creation, approval, verification, and activation.
    #[command(subcommand)]
    operator_command: Option<OperatorCommand>,

    /// RPC listen address (changed from 9090 to avoid Prometheus default port conflict)
    #[arg(long, default_value = "127.0.0.1:9944")]
    rpc: String,

    /// Permission-sealed Unix RPC listener for production reverse proxies.
    /// Conflicts with an explicitly supplied --rpc TCP address.
    #[arg(long, value_name = "PATH", conflicts_with = "rpc")]
    rpc_unix: Option<PathBuf>,

    /// P2P listen port (QUIC) (changed from 9091 to avoid Transmission BitTorrent default)
    #[arg(long, default_value_t = 9945)]
    p2p_port: u16,

    /// Validator stake in ARC. 0 (the default) = observer / community node.
    ///
    /// DEFAULT CHANGED from 5,000,000 to 0, deliberately.
    ///
    /// Voting membership and voting power come only from the approved genesis
    /// or checkpoint. A transport peer's `--stake` claim never joins or changes
    /// the live validator set. Operators must keep this value consistent with
    /// the local validator's approved genesis entry.
    #[arg(long, default_value_t = 0)]
    stake: u64,

    /// Data directory for WAL/snapshots
    #[arg(long, default_value = "./arc-data")]
    data_dir: String,

    /// Private desktop shutdown capability file inside --data-dir.
    ///
    /// Packaged Windows nodes use CREATE_NO_WINDOW and therefore cannot
    /// receive a console control event. The desktop writes an authenticated
    /// sibling request file instead; no network shutdown endpoint is exposed.
    #[arg(long)]
    desktop_shutdown_token_file: Option<PathBuf>,

    /// Bootstrap peer addresses (comma-separated host:port)
    #[arg(long, value_delimiter = ',')]
    peers: Vec<String>,

    /// Path to a seeds file (one peer address per line, # comments allowed).
    /// Seeds are merged with --peers. Useful for testnet bootstrap.
    #[arg(long)]
    seeds_file: Option<String>,

    /// HTTPS origin for a seed/community RPC service. Repeat once per seed
    /// (or provide a comma-separated ARC_COMMUNITY_RPC_URLS value). These
    /// URLs drive worker registration/claims/results and 5-of-6 reward
    /// approvals; they are never derived from P2P peer addresses.
    #[arg(
        long = "community-rpc-url",
        env = "ARC_COMMUNITY_RPC_URLS",
        value_delimiter = ',',
        action = clap::ArgAction::Append,
        value_name = "URL"
    )]
    community_rpc_urls: Vec<String>,

    /// DANGEROUS: allow plaintext HTTP community RPC to a non-loopback host
    /// on a disposable development network. Production remote URLs require
    /// HTTPS. Loopback HTTP is accepted without this flag.
    #[arg(long, default_value_t = false)]
    allow_insecure_community_rpc: bool,

    /// Origins to pull the sharded-inference registry from over HTTP(S),
    /// WITHOUT joining their P2P network or consensus (comma-separated; a
    /// bare host gets port 9090 and is permitted only for loopback/dev HTTP).
    ///
    /// This exists because coordinating inference and joining a chain are
    /// separate concerns that --peers/--seeds-file conflate. Those flags join
    /// P2P and consensus traffic; older protocol versions also trusted a
    /// peer's advertised stake, creating the phantom-validator hazard that v3
    /// now removes. A node still should not join another chain merely to use
    /// its inference pipeline. --shard-hosts is HTTP-only:
    /// GET /shards to learn the pipeline, POST /inference/forward_shard to
    /// use it. No handshake, no stake advertisement, no consensus.
    #[arg(long, value_delimiter = ',')]
    shard_hosts: Vec<String>,

    /// Minimum staked ARC required to run this node
    #[arg(long, default_value_t = 500_000)]
    min_stake: u64,

    /// Mode-0600 Ed25519 JSON keyfile created by `arc keygen`.
    /// Required for every production validator with non-zero stake.
    #[arg(long, env = "ARC_VALIDATOR_KEY_FILE")]
    validator_key_file: Option<String>,

    /// Legacy deterministic seed for an explicitly insecure disposable devnet.
    /// Requires --insecure-dev-validator-seed and numeric-loopback-only RPC,
    /// P2P, and peers; every persistent/community/reward/shard role rejects it.
    #[arg(long, env = "ARC_VALIDATOR_SEED")]
    validator_seed: Option<String>,

    /// DANGEROUS: permit seed-derived staked identities and an incomplete
    /// genesis on a disposable local development network. Never use this on
    /// a production, public, or value-bearing chain.
    #[arg(long, default_value_t = false)]
    insecure_dev_validator_seed: bool,

    /// Public label for this node in the shard registry (shown by GET /shards
    /// on every seed, so treat it as public).
    ///
    /// Defaults to a short hash of public node metadata. It must never derive
    /// from signing material because this value is broadcast to every seed.
    #[arg(long, env = "ARC_NODE_NAME")]
    node_name: Option<String>,

    /// Archive mode - disable all pruning, keep full transaction history.
    /// Use for block explorers and analytics. Requires more disk space.
    /// Regular validators should NOT use this flag.
    #[arg(long, default_value_t = false)]
    archive: bool,

    /// Enable continuous transaction generation (testnet benchmark mode).
    /// Generates transfers between genesis accounts to keep the chain busy.
    #[cfg(feature = "benchmark-tools")]
    #[arg(long, default_value_t = false)]
    benchmark: bool,

    /// Transactions per batch in benchmark mode.
    #[cfg(feature = "benchmark-tools")]
    #[arg(long, default_value_t = 500)]
    bench_batch: usize,

    /// Milliseconds between benchmark batches.
    #[cfg(feature = "benchmark-tools")]
    #[arg(long, default_value_t = 200)]
    bench_interval: u64,

    /// First sender index for benchmark mode (0-49). Use to partition senders
    /// across nodes in multi-node benchmarks to avoid nonce conflicts.
    #[cfg(feature = "benchmark-tools")]
    #[arg(long, default_value_t = 0)]
    bench_sender_start: u8,

    /// Number of senders this node owns in benchmark mode.
    #[cfg(feature = "benchmark-tools")]
    #[arg(long, default_value_t = 50)]
    bench_sender_count: u8,

    /// Number of signing threads in benchmark mode.
    #[cfg(feature = "benchmark-tools")]
    #[arg(long, default_value_t = 4)]
    bench_sign_threads: usize,

    /// Number of rayon threads for batch verification.
    #[cfg(feature = "benchmark-tools")]
    #[arg(long, default_value_t = 6)]
    bench_rayon_threads: usize,

    /// Enable proposer mode (GPU execution pipeline, state diff broadcast).
    /// Proposer nodes execute transactions and broadcast state diffs.
    /// Non-proposer nodes verify diffs without full re-execution.
    #[arg(long, default_value_t = false)]
    proposer_mode: bool,

    /// ETH-compatible JSON-RPC port (default: disabled).
    /// Enables MetaMask, Hardhat, Foundry, and other EVM tooling.
    /// Set to 0 to disable the ETH RPC server.
    #[arg(long, default_value_t = 0)]
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

    /// Quorum-signed ARCCHKPT package to verify and activate in a fresh data
    /// directory before P2P or consensus starts.
    #[arg(long)]
    recovery_checkpoint: Option<String>,

    /// Exact content hash approved out of band for --recovery-checkpoint.
    /// The node never trusts the validator set embedded in an unpinned file.
    #[arg(long)]
    approved_recovery_manifest_hash: Option<String>,

    /// Non-zero recovery epoch expected in the approved ARCCHKPT manifest.
    #[arg(long, default_value_t = 1)]
    recovery_epoch: u64,

    /// Non-zero validator-set generation expected in the ARCCHKPT manifest.
    #[arg(long, default_value_t = 1)]
    validator_set_id: u64,

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

    /// Load the complete deterministic integer model for a stake-zero
    /// community worker without advertising a validator layer shard.
    ///
    /// A normal unsharded GGUF prefers the Candle Q4 backend and keeps only
    /// the integer tokenizer in memory; that is suitable for direct local
    /// inference but cannot produce the cross-platform integer result that
    /// community reward verification recomputes. Using `--shard-range 0:N`
    /// to force integer loading is unsafe for a home worker because it also
    /// announces an overlapping, usually NAT-unreachable validator shard.
    /// This flag is the explicit full-integer, no-shard-advertisement role.
    #[arg(long, default_value_t = false)]
    full_integer_worker: bool,

    /// First layer index to load (inclusive). Pipeline-parallel sharding.
    /// Together with --shard-end, makes this node a SHARD HOLDER for a slice
    /// of the model. Embeddings load only when --shard-start=0; output head
    /// loads only when --shard-end=n_layers.
    /// Example: 80-layer Llama-70B split 8 ways → node 0 uses --shard-start 0
    /// --shard-end 10, node 1 uses --shard-start 10 --shard-end 20, etc.
    ///
    /// Deprecated in favor of repeated --shard-range A:B. Still accepted for
    /// backward compatibility: when set, synthesizes a single range.
    #[arg(long)]
    shard_start: Option<usize>,

    /// Last layer index to load (exclusive). Pipeline-parallel sharding.
    /// Deprecated - use --shard-range.
    #[arg(long)]
    shard_end: Option<usize>,

    /// Layer range this node holds, formatted `START:END` (END exclusive).
    /// Repeatable - every `--shard-range` adds one disjoint slice and the
    /// node announces one ShardInfo per range, so the coordinator can treat
    /// each as an independent replica. Example for 3× replication across
    /// 6 seeds holding 32 Llama-2-7B layers in 6 ranges:
    ///   --shard-range 0:6 --shard-range 11:16 --shard-range 21:26
    #[arg(long = "shard-range", value_name = "START:END")]
    shard_ranges: Vec<String>,

    /// Enable community-mode HTTP registration. When set, the node
    /// registers itself with all seeds via outbound HTTP POST to
    /// /community/register every 60s and sends a heartbeat every 15s.
    /// This makes the node visible on the dashboard and lets it
    /// participate in compute contributions without requiring inbound
    /// connectivity (no port forwarding, no public IP needed).
    /// Recommended for home / residential installs.
    #[arg(long)]
    community_mode: bool,

    /// Allow this validator to issue the v1, job-bound community inference
    /// reward transaction after a worker result is verified.
    ///
    /// Keep this OFF while any validator still runs a pre-v0.8.0 binary:
    /// older nodes cannot decode the new transaction variant. Deploy the new
    /// binary to the entire validator set first, verify one chain tip/state
    /// root, and commit one future
    /// `[chain].community_rewards_v1_activation_height` in the canonical
    /// genesis/checkpoint. This flag only opens local issuance after that
    /// consensus height is reached.
    #[arg(long, default_value_t = false)]
    enable_community_rewards_v1: bool,

    /// One-flag community-node setup. Equivalent to `--stake 0
    /// --community-mode` PLUS auto-discovery of a Llama-2-7B GGUF from
    /// standard paths (./llama2-7b.gguf, $HOME/.arc-models/, /opt/arc/).
    /// Lets `arc-node --community` "just work" for home/residential operators
    /// with no other flags: stake-0 + auto-registers with seeds + serves
    /// local inference if a model is found on disk. If no model is found
    /// the node still runs as a community routing/observer member and
    /// prints clear download instructions with the expected sha256.
    #[arg(long, default_value_t = false)]
    community: bool,

    /// Run as a silent observer: no community registration, no heartbeat, no
    /// work claiming. The node still reads from its peers (GET only) and can
    /// act as a sharded-inference coordinator against them.
    ///
    /// Needed because community mode is auto-enabled whenever stake == 0, and
    /// stake now defaults to 0. Without this flag a plain
    /// `arc-node --seeds-file <public seeds>` would begin POSTing
    /// /community/register and /community/heartbeat to every seed in the file
    /// — writes to someone else's network that the operator never asked for.
    /// This is the flag to use when pointing a local coordinator at a network
    /// you are only allowed to read.
    #[arg(long, default_value_t = false)]
    no_community: bool,

    /// Ask a seed to assign this node a layer range at boot (POST /shards/join).
    ///
    /// OFF by default, and it used to be implicit for any staked node with a
    /// model and no explicit --shard-range. That implicit trigger is how a
    /// joining node injected an off-grid `[0, 8)` shard into a pipeline
    /// already fully covered by the 6-range tiling: the seed inserts the
    /// announcement verbatim with no stub-address check, and v0.7.9 seeds'
    /// pipeline assemblers then abort with
    /// `503 Pipeline gap: expected layer 6 next, got shard [0, 8)` — taking
    /// out sharded inference network-wide. Opt in only when you mean it, and
    /// prefer an explicit on-grid `--shard-range`.
    #[arg(long, default_value_t = false)]
    auto_shard_join: bool,

    /// Number of threads for inference compute (rayon pool width).
    ///
    /// 0 (default) uses rayon's implicit global pool, which is sized from
    /// `RAYON_NUM_THREADS` when that env var is set and from
    /// `available_parallelism()` otherwise. Any non-zero value builds a
    /// dedicated pool that `forward_shard` and local `generate` run inside,
    /// and that pool can be resized at runtime via
    /// `POST /node/threads {"threads": n}` with no restart.
    #[arg(long, default_value_t = 0)]
    threads: usize,

    /// Promote INT8 weights to INT16 storage after load.
    ///
    /// Only meaningful on aarch64, where `matmul_i16_into` dispatches to a
    /// real NEON widening kernel. On x86 this is a pure loss: `enable_i16`
    /// builds every I16Weights via `I16Weights::from_i8`, which has no f32
    /// source and therefore carries no additional precision, and the x86
    /// `dot_i16_i64` is `dot_i16_i64_avx2` → `dot_i16_i64_scalar` (the AVX-512
    /// path was reverted after segfaults). Net effect per layer: double the
    /// weight bytes streamed, identical arithmetic, identical output — plus
    /// the I8 weights are retained alongside the I16 ones, so a 15-layer
    /// holder's resident set grows by several GB.
    ///
    /// Defaults to on for aarch64, off elsewhere. Pass the flag to force it.
    #[arg(long, default_value_t = false)]
    enable_i16: bool,
}

#[derive(Clone, Debug, Subcommand)]
enum OperatorCommand {
    /// Operate the quorum-certified ARCCHKPT recovery protocol.
    Recovery {
        #[command(subcommand)]
        command: RecoveryCommand,
    },
    /// Query one cryptographically pinned, stopped legacy fork without
    /// starting a node, WAL, P2P, consensus, signing, or mutation endpoint.
    Archive {
        #[command(subcommand)]
        command: ArchiveCommand,
    },
}

#[derive(Clone, Debug, Subcommand)]
enum ArchiveCommand {
    /// Serve an immutable ARCCHKPT view on an explicit GET-only transport.
    Serve {
        #[arg(long)]
        archive_manifest: String,
        #[arg(long)]
        complete: String,
        #[arg(long)]
        inventory: String,
        #[arg(long)]
        binding_index: String,
        #[arg(long)]
        binding: String,
        #[arg(long)]
        checkpoint: String,
        /// Finalized, out-of-band archive manifest SHA-256 trust root.
        #[arg(long)]
        expected_archive_manifest_sha256: String,
        /// Finalized, out-of-band COMPLETE.json SHA-256 trust root.
        #[arg(long)]
        expected_complete_sha256: String,
        #[arg(long)]
        node: String,
        /// Explicit development listener; must be loopback. Production uses
        /// --listen-unix so a crashed archive cannot be impersonated locally.
        #[arg(
            long,
            conflicts_with = "listen_unix",
            required_unless_present = "listen_unix"
        )]
        listen: Option<SocketAddr>,
        /// Permission-sealed production origin socket.
        #[arg(
            long,
            value_name = "PATH",
            conflicts_with = "listen",
            required_unless_present = "listen"
        )]
        listen_unix: Option<PathBuf>,
    },
}

#[derive(Clone, Debug, Subcommand)]
enum RecoveryCommand {
    /// Print the content hash and metadata without treating the file as trusted.
    Inspect {
        #[arg(long)]
        checkpoint: String,
    },
    /// Export an unsigned, content-addressed checkpoint from a complete legacy WAL.
    Export {
        #[arg(long)]
        data_dir: String,
        /// Exact-height LZ4 snapshot captured from /sync/snapshot alongside the WAL.
        #[arg(long)]
        snapshot: String,
        #[arg(long)]
        genesis: String,
        /// JSON array of {address, public_key, stake} records for the approved set.
        #[arg(long)]
        validator_public_keys: String,
        /// Canonical archived source set as a JSON array of {address, stake}.
        /// This must be the original eight-validator/40M legacy genesis set;
        /// runtime peer registries are never used as recovery authority.
        #[arg(long)]
        legacy_validator_set: String,
        #[arg(long)]
        output: String,
        #[arg(long)]
        source_consensus_round: u64,
        #[arg(long, default_value_t = 1)]
        recovery_epoch: u64,
        #[arg(long, default_value_t = 1)]
        validator_set_id: u64,
        /// Fixed manifest timestamp; defaults to the current Unix time in milliseconds.
        #[arg(long)]
        created_at_unix_ms: Option<u64>,
        /// Explicitly permit an old WAL that predates genesis.network-hash.
        #[arg(long, default_value_t = false)]
        allow_unbound_legacy_wal: bool,
    },
    /// Validate an unsigned candidate and append one offline validator signature.
    Sign {
        #[arg(long)]
        checkpoint: String,
        #[arg(long)]
        genesis: String,
        #[arg(long)]
        approved_manifest_hash: String,
        #[arg(long)]
        validator_key_file: String,
        #[arg(long)]
        output: String,
        #[arg(long, default_value_t = 1)]
        recovery_epoch: u64,
        #[arg(long, default_value_t = 1)]
        validator_set_id: u64,
    },
    /// Verify content, exact hash pin, network policy, and signature quorum.
    Verify {
        #[arg(long)]
        checkpoint: String,
        #[arg(long)]
        genesis: String,
        #[arg(long)]
        approved_manifest_hash: String,
        #[arg(long, default_value_t = 1)]
        recovery_epoch: u64,
        #[arg(long, default_value_t = 1)]
        validator_set_id: u64,
    },
    /// Verify and atomically activate a checkpoint in a fresh data directory.
    Import {
        #[arg(long)]
        checkpoint: String,
        #[arg(long)]
        data_dir: String,
        #[arg(long)]
        genesis: String,
        #[arg(long)]
        approved_manifest_hash: String,
        #[arg(long, default_value_t = 1)]
        recovery_epoch: u64,
        #[arg(long, default_value_t = 1)]
        validator_set_id: u64,
    },
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RecoveryValidatorFileEntry {
    address: String,
    public_key: String,
    stake: u64,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct LegacyRecoveryValidatorFileEntry {
    address: String,
    stake: u64,
}

fn parse_recovery_hash(label: &str, value: &str) -> Result<Hash256> {
    let bare = value
        .strip_prefix("0x")
        .or_else(|| value.strip_prefix("0X"))
        .unwrap_or(value);
    Hash256::from_hex(bare)
        .map_err(|_| anyhow::anyhow!("{label} must be exactly 32 bytes of hexadecimal"))
}

fn recovery_network_from_genesis(
    genesis_path: &str,
    recovery_epoch: u64,
    validator_set_id: u64,
) -> Result<(
    config::GenesisConfig,
    arc_state::recovery::RecoveryNetworkPolicy,
)> {
    ensure!(recovery_epoch > 0, "recovery_epoch must be non-zero");
    ensure!(validator_set_id > 0, "validator_set_id must be non-zero");
    let genesis = config::load_genesis(genesis_path)
        .context("recovery requires a complete production genesis configuration")?;
    let validators = genesis.validated_validator_set(false)?;
    let genesis_hash = genesis.network_hash(false)?;
    let network = arc_state::recovery::RecoveryNetworkPolicy {
        chain_id: genesis.chain.chain_id.clone(),
        genesis_hash,
        recovery_epoch,
        validator_set_id,
        validators,
        community_rewards_v1_activation_height: genesis
            .chain
            .community_rewards_v1_activation_height,
    };
    Ok((genesis, network))
}

fn load_recovery_validator_file(path: &str) -> Result<Vec<arc_state::recovery::RecoveryValidator>> {
    let bytes = std::fs::read(path)
        .with_context(|| format!("failed to read recovery validator file {path}"))?;
    let records: Vec<RecoveryValidatorFileEntry> = serde_json::from_slice(&bytes)
        .with_context(|| format!("failed to parse recovery validator file {path}"))?;
    ensure!(!records.is_empty(), "recovery validator file is empty");
    records
        .into_iter()
        .enumerate()
        .map(|(index, record)| {
            let address = parse_recovery_hash(
                &format!("validator #{} address", index + 1),
                &record.address,
            )?;
            ensure!(record.stake > 0, "validator {address} has zero stake");
            let public_hex = record
                .public_key
                .strip_prefix("0x")
                .or_else(|| record.public_key.strip_prefix("0X"))
                .unwrap_or(&record.public_key);
            let public_bytes = hex::decode(public_hex)
                .with_context(|| format!("validator {address} public_key is not hexadecimal"))?;
            let public_key: [u8; 32] = public_bytes.try_into().map_err(|bytes: Vec<u8>| {
                anyhow::anyhow!(
                    "validator {address} public_key must be 32 bytes (found {})",
                    bytes.len()
                )
            })?;
            ensure!(
                hash_bytes(&public_key) == address,
                "validator {address} public_key derives to a different address"
            );
            Ok(arc_state::recovery::RecoveryValidator {
                address,
                public_key,
                stake: record.stake,
            })
        })
        .collect()
}

fn load_legacy_recovery_validator_file(path: &str) -> Result<Vec<(Hash256, u64)>> {
    let bytes = std::fs::read(path)
        .with_context(|| format!("failed to read legacy recovery validator file {path}"))?;
    let records: Vec<LegacyRecoveryValidatorFileEntry> = serde_json::from_slice(&bytes)
        .with_context(|| format!("failed to parse legacy recovery validator file {path}"))?;
    let validators = records
        .into_iter()
        .enumerate()
        .map(|(index, record)| {
            Ok((
                parse_recovery_hash(
                    &format!("legacy validator #{} address", index + 1),
                    &record.address,
                )?,
                record.stake,
            ))
        })
        .collect::<Result<Vec<_>>>()?;
    arc_state::recovery::canonicalize_legacy_recovery_validator_set(validators)
        .map_err(anyhow::Error::from)
        .with_context(|| format!("invalid canonical legacy validator file {path}"))
}

fn recovery_trust(
    genesis: &str,
    approved_manifest_hash: &str,
    recovery_epoch: u64,
    validator_set_id: u64,
) -> Result<arc_state::recovery::RecoveryTrustRoot> {
    let (_, network) = recovery_network_from_genesis(genesis, recovery_epoch, validator_set_id)?;
    Ok(arc_state::recovery::RecoveryTrustRoot {
        network,
        approved_manifest_hash: parse_recovery_hash(
            "approved_manifest_hash",
            approved_manifest_hash,
        )?,
    })
}

fn print_recovery_summary(
    checkpoint: &arc_state::recovery::ArcCheckpoint,
    status: &str,
    source_wal: Option<&arc_state::recovery::LegacyWalBoundaryReport>,
) -> Result<()> {
    let transition = checkpoint.manifest.transition_block()?;
    let mut summary = serde_json::json!({
        "status": status,
        "manifest_hash": format!("0x{}", checkpoint.manifest_hash().to_hex()),
        "payload_hash": format!("0x{}", checkpoint.manifest.payload_hash.to_hex()),
        "full_state_root": format!("0x{}", checkpoint.manifest.full_state_root.to_hex()),
        "chain_id": checkpoint.manifest.chain_id,
        "genesis_hash": format!("0x{}", checkpoint.manifest.genesis_hash.to_hex()),
        "source_height": checkpoint.manifest.source_height,
        "source_block_hash": format!("0x{}", checkpoint.manifest.source_block_hash.to_hex()),
        "source_state_root": format!("0x{}", checkpoint.manifest.source_state_root.to_hex()),
        "source_consensus_round": checkpoint.manifest.source_consensus_round,
        "created_at_unix_ms": checkpoint.manifest.created_at_unix_ms,
        "transition_height": transition.header.height,
        "transition_block_hash": format!("0x{}", transition.hash.to_hex()),
        "recovery_domain": format!("0x{}", checkpoint.manifest.recovery_context().domain_hash().to_hex()),
        "recovery_epoch": checkpoint.manifest.recovery_epoch,
        "validator_set_id": checkpoint.manifest.validator_set_id,
        "protocol_version": format!(
            "{}.{}.{}",
            checkpoint.manifest.protocol_version.major,
            checkpoint.manifest.protocol_version.minor,
            checkpoint.manifest.protocol_version.patch,
        ),
        "validator_count": checkpoint.manifest.validators.len(),
        "signature_count": checkpoint.signatures.len(),
        "source_validator_count": checkpoint.payload.validators.len(),
        "source_validator_stake": checkpoint.payload.staking_pool,
        "source_validator_set_hash": format!("0x{}", checkpoint.payload.source_validator_set_hash().to_hex()),
        "community_reward_issuance_policy": arc_state::community_reward_issuance_policy(),
        "community_reward_issuance_policy_hash": format!("0x{}", arc_state::community_reward_issuance_policy_hash().to_hex()),
    });
    if let Some(source_wal) = source_wal {
        let summary = summary
            .as_object_mut()
            .ok_or_else(|| anyhow::anyhow!("recovery summary is not a JSON object"))?;
        summary.insert(
            "source_wal_original_bytes".into(),
            source_wal.source_wal_original_bytes.into(),
        );
        summary.insert(
            "source_wal_accepted_prefix_bytes".into(),
            source_wal.source_wal_accepted_prefix_bytes.into(),
        );
        summary.insert(
            "source_wal_quarantined_tail_bytes".into(),
            source_wal.source_wal_quarantined_tail_bytes.into(),
        );
        summary.insert(
            "source_wal_tail_reason".into(),
            source_wal.source_wal_tail_reason.clone().into(),
        );
    }
    println!("{}", serde_json::to_string_pretty(&summary)?);
    Ok(())
}

const RECOVERY_DAG_BINDING_VERSION: u16 = 2;
const RECOVERY_DAG_BINDING_FILE: &str = "recovery-dag.binding.json";
const RECOVERY_DAG_PIN_SCHEMA: &str = "arc.recovery.dag-generation-pin.v1";

/// Local DAG persistence is useful only after it is bound to the signed
/// checkpoint which created its consensus domain. This small readable file is
/// not a trust root: every field is re-derived from the stored ARCCHKPT at
/// every startup and any mismatch aborts the node.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RecoveryDagBinding {
    format_version: u16,
    manifest_hash: Hash256,
    consensus_domain: arc_consensus::ConsensusDomain,
    /// Domain-separated commitment to the fixed validator identities, public
    /// keys, and stakes in the signed ARCCHKPT manifest. This deliberately
    /// never follows mutable post-transition validator state.
    validator_set_commitment: Hash256,
    source_height: u64,
    transition_height: u64,
    source_consensus_round: u64,
    initial_consensus_round: u64,
}

#[derive(Clone, Debug)]
struct RecoveryDagStartup {
    data_dir: PathBuf,
    wal_dir: PathBuf,
    binding: RecoveryDagBinding,
    archived_legacy_wal: Option<PathBuf>,
}

#[derive(Debug)]
struct RecoveryDagReplay {
    current_round: u64,
    next_commit_round: u64,
    transactions: Vec<arc_types::Transaction>,
    repaired_commit: Option<(Hash256, u64)>,
}

const LIVE_DAG_ROLLOVER_PERCENT: u64 = 90;
const LIVE_DAG_ROLLOVER_ROUND_HEADROOM: u64 = 64;

struct LiveRecoveryDagRollover {
    store: GenerationStore,
    startup: RecoveryDagStartup,
    current: parking_lot::Mutex<VerifiedGeneration>,
}

impl LiveRecoveryDagRollover {
    fn projected_usage(
        generation: &VerifiedGeneration,
        writer: &arc_node::recovery_dag_wal::ActiveLogWriter,
        upcoming: &[RetainedDagRecord],
    ) -> Result<(u64, u64, u64)> {
        let inspection = writer.inspection();
        ensure!(
            writer.generation_pin() == generation.pin
                && inspection.generation_pin == generation.pin,
            "live recovery DAG writer differs from its selected generation"
        );
        let upcoming = writer
            .project_batch_usage(upcoming)
            .map_err(|error| anyhow::anyhow!("project live recovery DAG batch: {error}"))?;
        let record_count = generation
            .manifest
            .retained_records
            .record_count
            .checked_add(inspection.record_count)
            .and_then(|count| count.checked_add(upcoming.appended_records))
            .ok_or_else(|| anyhow::anyhow!("live recovery DAG record count overflows"))?;
        let payload_bytes = generation
            .manifest
            .retained_records
            .payload_bytes
            .checked_add(inspection.payload_bytes)
            .and_then(|bytes| bytes.checked_add(upcoming.appended_payload_bytes))
            .ok_or_else(|| anyhow::anyhow!("live recovery DAG payload size overflows"))?;
        let maximum_round = upcoming
            .maximum_round
            .into_iter()
            .chain(inspection.last_round)
            .max()
            .unwrap_or(generation.manifest.dag_cursor.current_round);
        Ok((record_count, payload_bytes, maximum_round))
    }

    fn projection_fits_hard_limits(
        generation: &VerifiedGeneration,
        projection: (u64, u64, u64),
    ) -> bool {
        let limits = generation.manifest.retained_records.limits;
        projection.0 <= limits.max_records
            && projection.1 <= limits.max_payload_bytes
            && projection.2 >= generation.manifest.dag_cursor.retention_floor_round
            && projection.2 <= generation.manifest.dag_cursor.retention_ceiling_round
    }

    fn projection_needs_rollover(
        generation: &VerifiedGeneration,
        projection: (u64, u64, u64),
    ) -> bool {
        let limits = generation.manifest.retained_records.limits;
        let record_target = limits.max_records.saturating_mul(LIVE_DAG_ROLLOVER_PERCENT) / 100;
        let payload_target = limits
            .max_payload_bytes
            .saturating_mul(LIVE_DAG_ROLLOVER_PERCENT)
            / 100;
        let round_target = generation
            .manifest
            .dag_cursor
            .retention_ceiling_round
            .saturating_sub(LIVE_DAG_ROLLOVER_ROUND_HEADROOM);
        projection.0 >= record_target
            || projection.1 >= payload_target
            || projection.2 >= round_target
    }
}

impl RecoveryDagRollover for LiveRecoveryDagRollover {
    fn prepare_append(
        &self,
        state: &StateDB,
        engine: &arc_consensus::ConsensusEngine,
        writer: arc_node::recovery_dag_wal::ActiveLogWriter,
        upcoming: &[RetainedDagRecord],
    ) -> std::result::Result<arc_node::recovery_dag_wal::ActiveLogWriter, String> {
        let mut current = self.current.lock();
        let before = Self::projected_usage(&current, &writer, upcoming)
            .map_err(|error| format!("{error:#}"))?;
        let fits_before = Self::projection_fits_hard_limits(&current, before);
        let needs_rollover = Self::projection_needs_rollover(&current, before) || !fits_before;
        let canonical_advanced = state.height() > current.manifest.baseline_state.height;
        if !needs_rollover || !canonical_advanced {
            if !fits_before {
                return Err(
                    "recovery DAG capacity exhausted before a newer canonical compaction boundary"
                        .to_string(),
                );
            }
            return Ok(writer);
        }

        let selected_pin = current.pin;
        let expected_active_pin = writer.inspection().pin();
        // Dropping the writer releases the store's advisory lock. No other
        // consensus task can append because the outer writer slot remains
        // exclusively locked until this method returns.
        drop(writer);
        let (records, summary) = stream_recovery_generation_records(
            &self.store,
            &current.manifest.binding,
            selected_pin,
        )
        .map_err(|error| format!("stage live recovery DAG rollover: {error:#}"))?;
        if summary.active_pin != expected_active_pin || summary.active_suffix != TornSuffix::Clean {
            return Err(
                "live recovery DAG changed or tore after its rollover pin was selected".to_string(),
            );
        }
        let successor = compact_replayed_recovery_generation(
            &self.store,
            state,
            &self.startup,
            &current,
            &summary,
            engine,
            &records,
        )
        .map_err(|error| format!("publish live recovery DAG rollover: {error:#}"))?;
        if successor.pin == selected_pin {
            return Err(
                "live recovery DAG threshold requested rollover but produced no successor"
                    .to_string(),
            );
        }
        let successor_writer = self
            .store
            .open_current_active_writer(&successor.manifest.binding, successor.pin)
            .map_err(|error| format!("open live recovery DAG successor writer: {error}"))?;
        let after = Self::projected_usage(&successor, &successor_writer, upcoming)
            .map_err(|error| format!("{error:#}"))?;
        if !Self::projection_fits_hard_limits(&successor, after) {
            return Err(
                "compacted recovery DAG still lacks capacity for the next durable batch"
                    .to_string(),
            );
        }
        *current = successor;
        Ok(successor_writer)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RecoveryDagExternalPin {
    schema: String,
    recovery_manifest_hash: Hash256,
    generation: GenerationPin,
}

fn recovery_dag_pin_path(data_dir: &Path, manifest_hash: Hash256) -> PathBuf {
    data_dir.join(format!("recovery-dag.{}.pin.json", manifest_hash.to_hex()))
}

fn read_recovery_dag_pin(
    data_dir: &Path,
    manifest_hash: Hash256,
) -> Result<Option<RecoveryDagExternalPin>> {
    let path = recovery_dag_pin_path(data_dir, manifest_hash);
    let metadata = match std::fs::symlink_metadata(&path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error)
                .with_context(|| format!("failed to inspect recovery DAG pin {}", path.display()));
        }
    };
    ensure!(
        metadata.file_type().is_file() && !metadata.file_type().is_symlink(),
        "recovery DAG pin {} must be a regular file",
        path.display()
    );
    ensure!(
        metadata.len() <= 64 * 1024,
        "recovery DAG pin {} exceeds 64 KiB",
        path.display()
    );
    let pin: RecoveryDagExternalPin = serde_json::from_slice(
        &std::fs::read(&path)
            .with_context(|| format!("failed to read recovery DAG pin {}", path.display()))?,
    )
    .with_context(|| format!("invalid recovery DAG pin {}", path.display()))?;
    ensure!(
        pin.schema == RECOVERY_DAG_PIN_SCHEMA && pin.recovery_manifest_hash == manifest_hash,
        "recovery DAG pin {} has a foreign schema or checkpoint binding",
        path.display()
    );
    Ok(Some(pin))
}

fn write_recovery_dag_pin_atomically(
    data_dir: &Path,
    manifest_hash: Hash256,
    generation: GenerationPin,
) -> Result<()> {
    let path = recovery_dag_pin_path(data_dir, manifest_hash);
    if let Ok(metadata) = std::fs::symlink_metadata(&path) {
        ensure!(
            metadata.file_type().is_file() && !metadata.file_type().is_symlink(),
            "recovery DAG pin {} must be a regular file",
            path.display()
        );
    }
    let pin = RecoveryDagExternalPin {
        schema: RECOVERY_DAG_PIN_SCHEMA.to_owned(),
        recovery_manifest_hash: manifest_hash,
        generation,
    };
    let bytes = serde_json::to_vec(&pin)?;
    let temporary = data_dir.join(format!(
        ".recovery-dag.{}.pin-{}.tmp",
        manifest_hash.to_hex(),
        uuid::Uuid::new_v4()
    ));
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options
        .open(&temporary)
        .with_context(|| format!("failed to create recovery DAG pin {}", temporary.display()))?;
    if let Err(error) = (|| -> std::io::Result<()> {
        file.write_all(&bytes)?;
        file.write_all(b"\n")?;
        file.sync_all()
    })() {
        let _ = std::fs::remove_file(&temporary);
        return Err(error).context("failed to durably write recovery DAG pin");
    }
    std::fs::rename(&temporary, &path)
        .with_context(|| format!("failed to activate recovery DAG pin {}", path.display()))?;
    sync_directory(data_dir)
}

/// Preserve recovered validator state exactly. Re-seeding the genesis set
/// after post-H+1 WAL replay rewinds legitimate validator changes and creates
/// a different state root on a rolling restart.
fn prepare_replayed_consensus_state(
    state: &StateDB,
    genesis_validators: &[(Hash256, u64)],
) -> Result<()> {
    if state.recovery_context().is_some() {
        let height = state.height();
        let anchor = state
            .get_block(height)
            .ok_or_else(|| anyhow::anyhow!("recovered state has no canonical block at {height}"))?;
        let computed = state.get_state_root();
        ensure!(
            computed == anchor.header.state_root,
            "recovered state root changed after replay: block {} commits {}, replay computes {}",
            height,
            anchor.header.state_root,
            computed
        );
        tracing::info!(
            height,
            state_root = %computed,
            "Recovered validator/state replay rechecked against its canonical block"
        );
    } else if !genesis_validators.is_empty() {
        state.seed_genesis_validators(genesis_validators);
        tracing::info!(
            "Seeded {} genesis validators into StateDB.validators",
            genesis_validators.len()
        );
    }
    Ok(())
}

fn rebuild_replayed_derived_indexes(state: &StateDB) {
    let tier1 = state.rebuild_tier1_pending();
    if tier1 > 0 {
        tracing::info!(
            "Rebuilt {} Tier 1 pending requests from on-disk state",
            tier1
        );
    }
    let bonds = state.rebuild_pending_bond_releases();
    if bonds > 0 {
        tracing::info!(
            "Rebuilt {} pending inference-bond releases from on-disk escrow state",
            bonds
        );
    }
}

fn sync_directory(path: &Path) -> Result<()> {
    #[cfg(unix)]
    File::open(path)
        .with_context(|| format!("failed to open directory {} for fsync", path.display()))?
        .sync_all()
        .with_context(|| format!("failed to fsync directory {}", path.display()))?;
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

/// Process-lifetime advisory lock for one node data directory. The lock file
/// may remain after a crash, but the kernel lock is released automatically;
/// this prevents two self-heal/systemd processes from opening the same state
/// and consensus WALs concurrently without creating a stale-PID blocker.
struct NodeDataDirLock {
    _file: File,
}

fn acquire_node_data_dir_lock(data_dir: &Path) -> Result<NodeDataDirLock> {
    match std::fs::symlink_metadata(data_dir) {
        Ok(metadata) => ensure!(
            metadata.file_type().is_dir() && !metadata.file_type().is_symlink(),
            "node data directory {} must be a real directory",
            data_dir.display()
        ),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            std::fs::create_dir_all(data_dir).with_context(|| {
                format!(
                    "failed to create node data directory {}",
                    data_dir.display()
                )
            })?;
            let parent = data_dir.parent().unwrap_or_else(|| Path::new("."));
            sync_directory(parent)?;
        }
        Err(error) => {
            return Err(error).with_context(|| {
                format!("failed to inspect data directory {}", data_dir.display())
            });
        }
    }

    let path = data_dir.join(".arc-node.lock");
    if let Ok(metadata) = std::fs::symlink_metadata(&path) {
        ensure!(
            metadata.file_type().is_file() && !metadata.file_type().is_symlink(),
            "node data lock {} must be a regular file",
            path.display()
        );
    }
    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options
        .open(&path)
        .with_context(|| format!("failed to open node data lock {}", path.display()))?;
    file.try_lock().with_context(|| {
        format!(
            "node data directory {} is already locked by another ARC process",
            data_dir.display()
        )
    })?;
    file.set_len(0)
        .with_context(|| format!("failed to reset node data lock {}", path.display()))?;
    file.write_all(
        format!("schema=arc.node.data-lock.v1\npid={}\n", std::process::id()).as_bytes(),
    )
    .with_context(|| format!("failed to write node data lock {}", path.display()))?;
    file.sync_all()
        .with_context(|| format!("failed to fsync node data lock {}", path.display()))?;
    sync_directory(data_dir)?;
    Ok(NodeDataDirLock { _file: file })
}

fn write_recovery_dag_binding_atomically(path: &Path, binding: &RecoveryDagBinding) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("recovery DAG binding has no parent directory"))?;
    let tmp = parent.join(format!(".{RECOVERY_DAG_BINDING_FILE}.new"));
    let bytes = serde_json::to_vec_pretty(binding)?;
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&tmp)
        .with_context(|| format!("failed to create recovery DAG binding {}", tmp.display()))?;
    if let Err(error) = (|| -> std::io::Result<()> {
        file.write_all(&bytes)?;
        file.write_all(b"\n")?;
        file.sync_all()
    })() {
        let _ = std::fs::remove_file(&tmp);
        return Err(error).context("failed to durably write recovery DAG binding");
    }
    std::fs::rename(&tmp, path)
        .with_context(|| format!("failed to activate recovery DAG binding {}", path.display()))?;
    sync_directory(parent)
}

fn read_recovery_dag_binding(path: &Path) -> Result<RecoveryDagBinding> {
    let metadata = std::fs::symlink_metadata(path)
        .with_context(|| format!("failed to stat recovery DAG binding {}", path.display()))?;
    ensure!(
        metadata.file_type().is_file() && !metadata.file_type().is_symlink(),
        "recovery DAG binding {} must be a regular file",
        path.display()
    );
    ensure!(
        metadata.len() <= 64 * 1024,
        "recovery DAG binding {} exceeds 64 KiB",
        path.display()
    );
    let bytes = std::fs::read(path)
        .with_context(|| format!("failed to read recovery DAG binding {}", path.display()))?;
    serde_json::from_slice(&bytes)
        .with_context(|| format!("invalid recovery DAG binding {}", path.display()))
}

fn activate_recovery_dag_binding(wal_dir: &Path, binding: &RecoveryDagBinding) -> Result<()> {
    let binding_path = wal_dir.join(RECOVERY_DAG_BINDING_FILE);
    let binding_tmp_path = wal_dir.join(format!(".{RECOVERY_DAG_BINDING_FILE}.new"));
    if binding_path.exists() {
        ensure!(
            !binding_tmp_path.exists(),
            "recovery DAG WAL contains both active and staged binding files"
        );
        let stored = read_recovery_dag_binding(&binding_path)?;
        ensure!(
            stored == *binding,
            "recovery DAG WAL binding differs from the signed active checkpoint"
        );
        return Ok(());
    }

    let entries: Vec<_> = std::fs::read_dir(wal_dir)
        .with_context(|| format!("failed to inspect {}", wal_dir.display()))?
        .collect::<std::io::Result<Vec<_>>>()?;
    if entries.is_empty() {
        return write_recovery_dag_binding_atomically(&binding_path, binding);
    }
    if entries.len() == 1 && entries[0].path() == binding_tmp_path {
        // The binding bytes are fully fsynced before their atomic rename. If
        // power failed in that one window, accept only the exact deterministic
        // bytes re-derived from the active ARCCHKPT, then finish the rename and
        // directory fsync before creating any generation files.
        let staged = read_recovery_dag_binding(&binding_tmp_path)?;
        ensure!(
            staged == *binding,
            "staged recovery DAG binding differs from the signed active checkpoint"
        );
        std::fs::rename(&binding_tmp_path, &binding_path)
            .context("failed to activate the fsynced recovery DAG binding")?;
        return sync_directory(wal_dir);
    }
    bail!(
        "recovery DAG WAL {} contains data without an active binding; refusing replay",
        wal_dir.display()
    )
}

fn archive_legacy_dag_wal(data_dir: &Path, manifest_hash: Hash256) -> Result<Option<PathBuf>> {
    let legacy = data_dir.join("dag-wal");
    let metadata = match std::fs::symlink_metadata(&legacy) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error)
                .with_context(|| format!("failed to inspect legacy DAG WAL {}", legacy.display()));
        }
    };
    ensure!(
        metadata.file_type().is_dir() && !metadata.file_type().is_symlink(),
        "legacy DAG WAL {} must be a real directory before archival",
        legacy.display()
    );
    let archive = data_dir.join(format!("dag-wal.pre-recovery-{}", manifest_hash.to_hex()));
    ensure!(
        !archive.exists(),
        "both legacy DAG WAL {} and archive {} exist; refusing ambiguous recovery startup",
        legacy.display(),
        archive.display()
    );
    std::fs::rename(&legacy, &archive).with_context(|| {
        format!(
            "failed to archive legacy DAG WAL {} as {}",
            legacy.display(),
            archive.display()
        )
    })?;
    sync_directory(data_dir)?;
    Ok(Some(archive))
}

fn checkpoint_validator_set_commitment(
    manifest: &arc_state::recovery::RecoveryManifest,
) -> Result<Hash256> {
    ensure!(
        !manifest.validators.is_empty(),
        "recovery checkpoint validator set is empty"
    );
    let encoded = bincode::serialize(&(manifest.validator_set_id, manifest.validators.as_slice()))
        .context("failed to encode the fixed recovery validator set")?;
    let mut hasher = blake3::Hasher::new_derive_key("ARC-recovery-DAG-validator-set-commitment-v1");
    hasher.update(&(encoded.len() as u64).to_be_bytes());
    hasher.update(&encoded);
    Ok(Hash256(*hasher.finalize().as_bytes()))
}

fn prepare_recovery_dag_startup(
    data_dir: &Path,
    state: &StateDB,
) -> Result<Option<RecoveryDagStartup>> {
    let Some(context) = state.recovery_context() else {
        return Ok(None);
    };
    let manifest_hash = state
        .recovery_manifest_hash()
        .ok_or_else(|| anyhow::anyhow!("recovery state is missing its manifest hash"))?;
    let checkpoint_path = data_dir.join(format!("recovery-{}.arcchkpt", manifest_hash.to_hex()));
    let checkpoint =
        arc_state::recovery::ArcCheckpoint::read_from(&checkpoint_path).with_context(|| {
            format!(
                "failed to reopen active recovery checkpoint {} for DAG binding",
                checkpoint_path.display()
            )
        })?;
    ensure!(
        checkpoint.manifest_hash() == manifest_hash,
        "active recovery checkpoint hash changed before DAG startup"
    );
    ensure!(
        checkpoint.manifest.recovery_context() == context,
        "active recovery checkpoint context differs from replayed state"
    );
    checkpoint
        .verify_content()
        .context("active recovery checkpoint content failed DAG startup validation")?;
    let transition_height = checkpoint
        .manifest
        .source_height
        .checked_add(1)
        .ok_or_else(|| anyhow::anyhow!("recovery transition height overflows u64"))?;
    ensure!(
        state.height() >= transition_height,
        "replayed recovery state height {} precedes transition height {}",
        state.height(),
        transition_height
    );
    let initial_consensus_round = checkpoint
        .manifest
        .source_consensus_round
        .checked_add(1)
        .ok_or_else(|| anyhow::anyhow!("recovery source consensus round overflows u64"))?;
    let binding = RecoveryDagBinding {
        format_version: RECOVERY_DAG_BINDING_VERSION,
        manifest_hash,
        consensus_domain: arc_consensus::ConsensusDomain::new(
            context.domain_hash(),
            context.recovery_epoch,
            context.validator_set_id,
        ),
        validator_set_commitment: checkpoint_validator_set_commitment(&checkpoint.manifest)?,
        source_height: checkpoint.manifest.source_height,
        transition_height,
        source_consensus_round: checkpoint.manifest.source_consensus_round,
        initial_consensus_round,
    };

    let archived_legacy_wal = archive_legacy_dag_wal(data_dir, manifest_hash)?;
    let wal_dir = data_dir.join(format!("dag-wal-recovery-{}", manifest_hash.to_hex()));
    match std::fs::symlink_metadata(&wal_dir) {
        Ok(metadata) => ensure!(
            metadata.file_type().is_dir() && !metadata.file_type().is_symlink(),
            "recovery DAG WAL {} must be a real directory",
            wal_dir.display()
        ),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            std::fs::create_dir(&wal_dir).with_context(|| {
                format!("failed to create recovery DAG WAL {}", wal_dir.display())
            })?;
            sync_directory(data_dir)?;
        }
        Err(error) => {
            return Err(error).with_context(|| {
                format!("failed to inspect recovery DAG WAL {}", wal_dir.display())
            });
        }
    }

    activate_recovery_dag_binding(&wal_dir, &binding)?;

    Ok(Some(RecoveryDagStartup {
        data_dir: data_dir.to_path_buf(),
        wal_dir,
        binding,
        archived_legacy_wal,
    }))
}

fn generation_dag_binding(startup: &RecoveryDagStartup) -> Result<GenerationDagBinding> {
    ensure!(
        startup.binding.manifest_hash != Hash256::ZERO,
        "recovery generation is missing its ARCCHKPT hash"
    );
    ensure!(
        startup.binding.validator_set_commitment != Hash256::ZERO,
        "recovery generation has an empty validator-set commitment"
    );
    Ok(GenerationDagBinding {
        recovery_manifest_hash: startup.binding.manifest_hash,
        recovery_domain: startup.binding.consensus_domain.domain_hash,
        validator_set_commitment: startup.binding.validator_set_commitment,
    })
}

fn canonical_dag_baseline(state: &StateDB) -> Result<DagBaselineState> {
    let height = state.height();
    let block = state
        .get_block(height)
        .ok_or_else(|| anyhow::anyhow!("canonical DAG baseline block {height} is missing"))?;
    let state_root = state.get_state_root();
    ensure!(
        block.header.state_root == state_root,
        "canonical DAG baseline block {} commits {}, live state computes {}",
        block.hash,
        block.header.state_root,
        state_root
    );
    Ok(DagBaselineState {
        height,
        block_hash: block.hash,
        state_root,
    })
}

fn validate_recovery_generation_anchor(
    state: &StateDB,
    startup: &RecoveryDagStartup,
    generation: &VerifiedGeneration,
) -> Result<()> {
    let baseline = &generation.manifest.baseline_state;
    ensure!(
        baseline.height >= startup.binding.transition_height && baseline.height <= state.height(),
        "recovery DAG generation baseline height {} is outside canonical state {}..={} ",
        baseline.height,
        startup.binding.transition_height,
        state.height()
    );
    let block = state.get_block(baseline.height).ok_or_else(|| {
        anyhow::anyhow!(
            "recovery DAG generation baseline block {} is missing",
            baseline.height
        )
    })?;
    ensure!(
        block.hash == baseline.block_hash && block.header.state_root == baseline.state_root,
        "recovery DAG generation baseline differs from canonical block {}",
        baseline.height
    );
    let committed_block_count = baseline
        .height
        .checked_sub(startup.binding.transition_height)
        .ok_or_else(|| anyhow::anyhow!("recovery DAG committed-block count underflows"))?;
    ensure!(
        generation.manifest.dag_cursor.committed_block_count == committed_block_count,
        "recovery DAG generation committed count does not match its canonical baseline"
    );
    let expected_next_round = startup
        .binding
        .initial_consensus_round
        .checked_add(committed_block_count)
        .ok_or_else(|| anyhow::anyhow!("recovery DAG next-commit round overflows"))?;
    ensure!(
        generation.manifest.dag_cursor.next_dag_round == expected_next_round,
        "recovery DAG generation next-commit round is not contiguous with canonical state"
    );
    Ok(())
}

fn recovery_retention_ceiling(floor_round: u64) -> Result<u64> {
    floor_round
        .checked_add(HARD_MAX_RETENTION_ROUND_SPAN)
        .ok_or_else(|| anyhow::anyhow!("recovery DAG retention ceiling overflows u64"))
}

fn initialize_recovery_generation_store(
    state: &StateDB,
    startup: &RecoveryDagStartup,
) -> Result<(GenerationStore, VerifiedGeneration)> {
    let binding = generation_dag_binding(startup)?;
    let store = GenerationStore::new(&startup.wal_dir);
    if startup.wal_dir.join("CURRENT").exists() {
        store
            .recover_interrupted_ancestor_gc(&binding)
            .context("failed to finish interrupted recovery DAG ancestor GC")?;
    }
    let external = read_recovery_dag_pin(&startup.data_dir, startup.binding.manifest_hash)?;

    let mut current = if let Some(external) = external.as_ref() {
        match store.load_current(&binding, Some(external.generation)) {
            Ok(generation) => generation,
            Err(arc_node::recovery_dag_wal::GenerationError::PinMismatch { .. }) => {
                let audit = store
                    .audit(&binding)
                    .context("failed to audit recovery DAG after an external pin mismatch")?;
                match &audit.status {
                    StoreAuditStatus::Clean
                        if audit.current.pin.sequence
                            == external.generation.sequence.saturating_add(1)
                            && audit.current.manifest.previous_generation
                                == Some(external.generation.hash) =>
                    {
                        validate_recovery_generation_anchor(state, startup, &audit.current)?;
                        write_recovery_dag_pin_atomically(
                            &startup.data_dir,
                            startup.binding.manifest_hash,
                            audit.current.pin,
                        )?;
                        audit.current
                    }
                    StoreAuditStatus::PointerBehind { heads }
                        if heads.as_slice() == [external.generation] =>
                    {
                        let successor = store
                            .verify_generation(external.generation.hash, &binding)
                            .context("externally pinned recovery DAG successor is invalid")?;
                        validate_recovery_generation_anchor(state, startup, &successor)?;
                        store
                            .activate_existing_successor(
                                audit.current.pin,
                                external.generation.hash,
                            )
                            .context("failed to activate externally pinned DAG successor")?
                    }
                    _ => bail!(
                        "recovery DAG CURRENT/external pin mismatch is not one exact crash successor"
                    ),
                }
            }
            Err(error) => return Err(error).context("failed to load pinned recovery DAG"),
        }
    } else {
        let generation = if startup.wal_dir.join("CURRENT").exists() {
            // Initial generation publication deliberately precedes the
            // independent pin. Recover that one crash window only when the
            // selected generation is the exact empty transition boundary; no
            // live consensus writer can have opened before the pin existed.
            let generation = store
                .load_current(&binding, None)
                .context("failed to inspect unpinned initial recovery DAG generation")?;
            validate_recovery_generation_anchor(state, startup, &generation)?;
            ensure!(
                generation.pin.sequence == 0
                    && generation.manifest.previous_generation.is_none()
                    && generation.manifest.baseline_state.height
                        == startup.binding.transition_height
                    && generation.manifest.retained_records.record_count == 0,
                "recovery DAG CURRENT exists without a pin and is not the exact empty initial generation"
            );
            let mut observed_records = 0u64;
            let summary = store
                .stream_current_generation_and_active(&binding, generation.pin, |_| {
                    observed_records += 1;
                    Ok(())
                })
                .context("failed to verify unpinned initial recovery DAG active log")?;
            ensure!(
                observed_records == 0
                    && summary.active_record_count == 0
                    && summary.active_suffix == TornSuffix::Clean,
                "unpinned initial recovery DAG generation contains active history"
            );
            generation
        } else {
            let baseline = canonical_dag_baseline(state)?;
            ensure!(
                baseline.height == startup.binding.transition_height,
                "cannot initialize a new recovery DAG generation after canonical state advanced"
            );
            let initial_round = startup.binding.initial_consensus_round;
            store
                .create_initial(
                    GenerationInput {
                        binding: binding.clone(),
                        baseline_state: baseline,
                        dag_cursor: DagCursor {
                            committed_block_count: 0,
                            next_dag_round: initial_round,
                            current_round: initial_round,
                            retention_floor_round: initial_round,
                            retention_ceiling_round: recovery_retention_ceiling(initial_round)?,
                        },
                        retention_limits: RetentionLimits::default(),
                    },
                    std::iter::empty(),
                )
                .context("failed to create initial recovery DAG generation")?
        };
        write_recovery_dag_pin_atomically(
            &startup.data_dir,
            startup.binding.manifest_hash,
            generation.pin,
        )?;
        generation
    };

    validate_recovery_generation_anchor(state, startup, &current)?;
    let audit = store
        .audit(&binding)
        .context("failed to audit immutable recovery DAG generations")?;
    match audit.status {
        StoreAuditStatus::Clean => ensure!(
            audit.current.pin == current.pin,
            "recovery DAG audit CURRENT changed during startup"
        ),
        StoreAuditStatus::PointerBehind { ref heads }
            if heads.len() == 1 && heads[0].sequence == current.pin.sequence.saturating_add(1) =>
        {
            let successor = store
                .verify_generation(heads[0].hash, &binding)
                .context("orphan recovery DAG successor is invalid")?;
            ensure!(
                successor.manifest.previous_generation == Some(current.pin.hash),
                "orphan recovery DAG generation is not the direct crash successor"
            );
            validate_recovery_generation_anchor(state, startup, &successor)?;
            current = store
                .activate_existing_successor(current.pin, successor.pin.hash)
                .context("failed to finish crash-interrupted DAG generation activation")?;
            write_recovery_dag_pin_atomically(
                &startup.data_dir,
                startup.binding.manifest_hash,
                current.pin,
            )?;
        }
        _ => bail!("recovery DAG generation audit found a fork or ambiguous rollback"),
    }
    Ok((store, current))
}

fn stream_recovery_generation_records(
    store: &GenerationStore,
    binding: &GenerationDagBinding,
    generation: GenerationPin,
) -> Result<(Vec<RetainedDagRecord>, CurrentStreamSummary)> {
    let mut records = Vec::new();
    let summary = store
        .stream_current_generation_and_active(binding, generation, |record| {
            records.push(record);
            Ok(())
        })
        .context("failed to stage the bounded recovery DAG generation")?;
    Ok((records, summary))
}

fn stage_recovery_generation_records(
    store: &GenerationStore,
    generation: &VerifiedGeneration,
) -> Result<(Vec<RetainedDagRecord>, CurrentStreamSummary)> {
    let (records, summary) =
        stream_recovery_generation_records(store, &generation.manifest.binding, generation.pin)?;
    if summary.active_suffix == TornSuffix::Clean {
        return Ok((records, summary));
    }

    let evidence = store
        .quarantine_current_active_suffix(
            &generation.manifest.binding,
            generation.pin,
            summary.active_valid_prefix_bytes,
        )
        .context("failed to quarantine the classified torn recovery DAG suffix")?;
    tracing::warn!(
        generation = %generation.pin.hash,
        valid_prefix_bytes = evidence.valid_prefix_bytes,
        quarantined_suffix_bytes = evidence.quarantined_suffix_bytes,
        quarantined_suffix_hash = %evidence.quarantined_suffix_hash,
        quarantine = %evidence.quarantine_path.display(),
        classification = ?evidence.classification,
        "Preserved and removed a torn final recovery DAG active batch"
    );

    let (clean_records, clean_summary) =
        stream_recovery_generation_records(store, &generation.manifest.binding, generation.pin)?;
    ensure!(
        clean_summary.active_suffix == TornSuffix::Clean
            && clean_records == records
            && clean_summary.active_valid_prefix_bytes == summary.active_valid_prefix_bytes,
        "recovery DAG valid prefix changed while its torn suffix was quarantined"
    );
    Ok((clean_records, clean_summary))
}

fn verify_canonical_state_block_for_dag_commit(
    state: &StateDB,
    baseline_height: u64,
    commit_index: u64,
    consensus_domain: &arc_consensus::ConsensusDomain,
    dag_block: &arc_consensus::DagBlock,
    transactions: &std::collections::HashMap<[u8; 32], arc_types::Transaction>,
) -> Result<()> {
    let height = baseline_height
        .checked_add(commit_index)
        .and_then(|height| height.checked_add(1))
        .ok_or_else(|| anyhow::anyhow!("canonical DAG/state binding height overflows u64"))?;
    let block = state.get_block(height).ok_or_else(|| {
        anyhow::anyhow!(
            "DAG commit {} has no canonical state block at height {}",
            dag_block.hash,
            height
        )
    })?;
    ensure!(
        block.header.producer == dag_block.author
            && block.header.timestamp == dag_block.timestamp
            && block.header.height == height
            && block.header.proof_hash == dag_block.state_decision_commitment(consensus_domain),
        "canonical state block {} does not bind DAG leader {} domain/hash/round/author/timestamp/height",
        block.hash,
        dag_block.hash
    );
    let mut expected_state_transactions = Vec::new();
    for hash in &dag_block.transactions {
        ensure!(
            transactions.contains_key(&hash.0),
            "DAG commit {} is missing durable transaction body {}",
            dag_block.hash,
            hash
        );
        if state
            .get_receipt(&hash.0)
            .is_some_and(|receipt| receipt.block_hash == block.hash)
        {
            expected_state_transactions.push(*hash);
        }
    }
    ensure!(
        block.tx_hashes == expected_state_transactions,
        "canonical state block {} transaction list does not match DAG commit {}",
        block.hash,
        dag_block.hash
    );
    Ok(())
}

fn replay_recovery_dag_generation(
    engine: &arc_consensus::ConsensusEngine,
    state: &StateDB,
    startup: &RecoveryDagStartup,
    generation: &VerifiedGeneration,
    records: &[RetainedDagRecord],
) -> Result<RecoveryDagReplay> {
    let installed = engine
        .install_recovery_cursor(startup.binding.source_consensus_round)
        .map_err(|error| anyhow::anyhow!("failed to install signed recovery cursor: {error}"))?;
    ensure!(
        installed == startup.binding.initial_consensus_round,
        "installed recovery cursor differs from signed DAG binding"
    );
    let cursor = &generation.manifest.dag_cursor;
    engine
        .install_recovery_generation_cursor(
            cursor.retention_floor_round,
            cursor.current_round,
            cursor.next_dag_round,
        )
        .map_err(|error| {
            anyhow::anyhow!("failed to install the pinned recovery DAG generation: {error}")
        })?;

    let expected_commits = state
        .height()
        .checked_sub(generation.manifest.baseline_state.height)
        .ok_or_else(|| anyhow::anyhow!("recovery DAG generation baseline exceeds state height"))?;
    let mut commits = 0u64;
    let mut committed_hashes = Vec::new();
    let mut transactions = std::collections::HashMap::<[u8; 32], arc_types::Transaction>::new();
    let mut transaction_payloads = std::collections::HashMap::<[u8; 32], Vec<u8>>::new();
    for record in records {
        match record.kind {
            RetainedRecordKind::TransactionBody => {
                let expected_hash = record.object_hash;
                let transaction: arc_types::Transaction = bincode::deserialize(&record.payload)
                    .with_context(|| format!("invalid retained DAG transaction {expected_hash}"))?;
                ensure!(
                    transaction.hash == expected_hash
                        && record.round >= cursor.retention_floor_round,
                    "retained DAG transaction key/round differs from its generation envelope"
                );
                transaction
                    .verify_signature_in_domain(&startup.binding.consensus_domain.domain_hash)
                    .map_err(|error| {
                        anyhow::anyhow!(
                            "persisted DAG transaction {} failed recovery-domain validation: {}",
                            expected_hash,
                            error
                        )
                    })?;
                if let Some(existing) = transaction_payloads.get(&expected_hash.0) {
                    ensure!(
                        existing == &record.payload,
                        "retained DAG transaction {} has conflicting bodies across rounds",
                        expected_hash
                    );
                } else {
                    transaction_payloads.insert(expected_hash.0, record.payload.clone());
                    transactions.insert(expected_hash.0, transaction);
                }
            }
            RetainedRecordKind::DagBlock => {
                let expected_hash = record.object_hash;
                let block: arc_consensus::DagBlock = bincode::deserialize(&record.payload)
                    .with_context(|| format!("invalid persisted DAG block {expected_hash}"))?;
                ensure!(
                    block.hash == expected_hash && block.round == record.round,
                    "retained DAG block key/round differs from its generation envelope"
                );
                ensure!(
                    block
                        .transactions
                        .iter()
                        .all(|hash| transactions.contains_key(&hash.0)),
                    "persisted DAG block {} is missing one or more durable transaction bodies",
                    expected_hash
                );
                engine.receive_block(&block).map_err(|error| {
                    anyhow::anyhow!(
                        "persisted DAG block {} failed recovery-domain validation: {}",
                        expected_hash,
                        error
                    )
                })?;
                // Active generations keep their immutable startup cursor;
                // reconstruct every later live round only from the same
                // quorum of validated blocks that advanced it originally.
                let _ = engine.advance_round();
            }
            RetainedRecordKind::RoundCursor => {
                ensure!(
                    record.round >= startup.binding.initial_consensus_round
                        && record.round <= engine.current_round(),
                    "retained DAG cursor {} is not justified by replayed quorum blocks (current {})",
                    record.round,
                    engine.current_round()
                );
            }
            RetainedRecordKind::Commit => {
                let hash = record.object_hash;
                engine
                    .restore_recovery_commit_from_local_wal(hash)
                    .map_err(|error| {
                        anyhow::anyhow!("persisted DAG commit {hash} is invalid: {error}")
                    })?;
                commits = commits
                    .checked_add(1)
                    .ok_or_else(|| anyhow::anyhow!("recovery DAG commit count overflows u64"))?;
                committed_hashes.push(hash);
            }
        }
    }
    for (index, hash) in committed_hashes.iter().enumerate() {
        let block = engine.get_block(hash).ok_or_else(|| {
            anyhow::anyhow!("restored DAG commit {hash} disappeared during strict replay")
        })?;
        verify_canonical_state_block_for_dag_commit(
            state,
            generation.manifest.baseline_state.height,
            index as u64,
            &startup.binding.consensus_domain,
            &block,
            &transactions,
        )?;
    }

    // The state WAL fsync deliberately precedes the separate DAG commit
    // cursor fsync. A crash in that narrow window leaves exactly one extra
    // canonical state block. Reconstruct only the deterministic next leader,
    // verify its full two-round certificate and exact state-block binding, then
    // ask the caller to append the missing commit record before networking.
    let repaired_commit = if commits.checked_add(1) == Some(expected_commits) {
        let round = engine.last_committed_round();
        let mut validators: Vec<_> = engine
            .frozen_validator_set()
            .validators
            .iter()
            .map(|validator| validator.address)
            .collect();
        validators.sort_by_key(|address| address.0);
        let leader = validators
            .get(round as usize % validators.len().max(1))
            .copied()
            .ok_or_else(|| anyhow::anyhow!("cannot repair DAG commit with empty validator set"))?;
        let candidates: Vec<_> = engine
            .blocks_in_round(round)
            .into_iter()
            .filter_map(|hash| engine.get_block(&hash))
            .filter(|block| block.author == leader)
            .collect();
        ensure!(
            candidates.len() == 1,
            "cannot uniquely repair DAG commit round {round}: found {} leader blocks",
            candidates.len()
        );
        let candidate = &candidates[0];
        verify_canonical_state_block_for_dag_commit(
            state,
            generation.manifest.baseline_state.height,
            commits,
            &startup.binding.consensus_domain,
            candidate,
            &transactions,
        )?;
        engine
            .restore_recovery_commit_from_local_wal(candidate.hash)
            .map_err(|error| {
                anyhow::anyhow!(
                    "canonical state tail names uncertified DAG commit {}: {error}",
                    candidate.hash
                )
            })?;
        commits += 1;
        Some((candidate.hash, candidate.round))
    } else {
        None
    };
    ensure!(
        commits == expected_commits,
        "recovery DAG/state commit mismatch: generation delta has {}, canonical state has {} post-baseline blocks",
        commits,
        expected_commits
    );
    engine
        .finish_recovery_generation_replay()
        .map_err(|error| anyhow::anyhow!("failed to seal recovery DAG replay: {error}"))?;
    let mut transactions: Vec<_> = transactions.into_values().collect();
    transactions.sort_by_key(|transaction| transaction.hash.0);
    Ok(RecoveryDagReplay {
        current_round: engine.current_round(),
        next_commit_round: engine.last_committed_round(),
        transactions,
        repaired_commit,
    })
}

fn compact_replayed_recovery_generation(
    store: &GenerationStore,
    state: &StateDB,
    startup: &RecoveryDagStartup,
    current: &VerifiedGeneration,
    stream: &CurrentStreamSummary,
    engine: &arc_consensus::ConsensusEngine,
    records: &[RetainedDagRecord],
) -> Result<VerifiedGeneration> {
    ensure!(
        stream.generation_pin == current.pin && stream.active_pin.generation_pin == current.pin,
        "recovery DAG stream pin differs from the generation selected for compaction"
    );
    ensure!(
        stream.active_suffix == TornSuffix::Clean,
        "recovery DAG compaction requires a clean active prefix"
    );

    let baseline = canonical_dag_baseline(state)?;
    let committed_block_count = baseline
        .height
        .checked_sub(startup.binding.transition_height)
        .ok_or_else(|| anyhow::anyhow!("recovery DAG committed-block count underflows"))?;
    let next_dag_round = startup
        .binding
        .initial_consensus_round
        .checked_add(committed_block_count)
        .ok_or_else(|| anyhow::anyhow!("recovery DAG next-commit round overflows"))?;
    ensure!(
        engine.last_committed_round() == next_dag_round,
        "replayed DAG commit cursor {} differs from canonical baseline cursor {}",
        engine.last_committed_round(),
        next_dag_round
    );
    let current_round = engine.current_round();
    ensure!(
        current_round >= next_dag_round,
        "replayed DAG current round precedes its commit cursor"
    );
    let retention_ceiling_round = recovery_retention_ceiling(next_dag_round)?;
    ensure!(
        current_round <= retention_ceiling_round,
        "replayed DAG window exceeds its hard round-span bound"
    );

    // Every commit in the staged delta is now bound into `baseline`. Preserve
    // only bodies/blocks/cursors that can still participate in a future commit,
    // in their original physical order. The exact floor round is the sole
    // parent-compaction boundary accepted by the consensus replay API.
    let retained: Vec<_> = records
        .iter()
        .filter(|record| {
            record.kind != RetainedRecordKind::Commit && record.round >= next_dag_round
        })
        .cloned()
        .collect();
    ensure!(
        retained
            .iter()
            .all(|record| record.round <= retention_ceiling_round),
        "retained recovery DAG record escapes the successor round window"
    );
    let cursor = DagCursor {
        committed_block_count,
        next_dag_round,
        current_round,
        retention_floor_round: next_dag_round,
        retention_ceiling_round,
    };
    let needs_compaction = stream.active_batch_count != 0
        || current.manifest.baseline_state != baseline
        || current.manifest.dag_cursor != cursor
        || current.manifest.retained_records.record_count != retained.len() as u64;
    if !needs_compaction {
        return Ok(current.clone());
    }

    let successor = store
        .append_compacted(
            current.pin,
            stream.active_pin,
            GenerationInput {
                binding: current.manifest.binding.clone(),
                baseline_state: baseline,
                dag_cursor: cursor,
                retention_limits: current.manifest.retained_records.limits,
            },
            retained,
        )
        .context("failed to publish compacted recovery DAG generation")?;
    validate_recovery_generation_anchor(state, startup, &successor)?;
    write_recovery_dag_pin_atomically(
        &startup.data_dir,
        startup.binding.manifest_hash,
        successor.pin,
    )?;
    let gc = store
        .prune_ancestors_keep_current_and_predecessor(&successor.manifest.binding, successor.pin)
        .context("failed to bound recovery DAG generation ancestry")?;
    tracing::info!(
        previous_generation = %current.pin.hash,
        generation = %successor.pin.hash,
        sequence = successor.pin.sequence,
        baseline_height = successor.manifest.baseline_state.height,
        retained_records = successor.manifest.retained_records.record_count,
        pruned_generations = gc.pruned_generations.len(),
        "Published and independently pinned a compacted recovery DAG generation"
    );
    Ok(successor)
}

fn run_recovery_operator_command(command: RecoveryCommand) -> Result<()> {
    match command {
        RecoveryCommand::Inspect { checkpoint } => {
            let checkpoint = arc_state::recovery::ArcCheckpoint::read_from(checkpoint)?;
            print_recovery_summary(&checkpoint, "UNTRUSTED_INSPECTION", None)
        }
        RecoveryCommand::Export {
            data_dir,
            snapshot,
            genesis,
            validator_public_keys,
            legacy_validator_set,
            output,
            source_consensus_round,
            recovery_epoch,
            validator_set_id,
            created_at_unix_ms,
            allow_unbound_legacy_wal,
        } => {
            let (genesis, network) =
                recovery_network_from_genesis(&genesis, recovery_epoch, validator_set_id)?;
            let validators = load_recovery_validator_file(&validator_public_keys)?;
            let legacy_validators = load_legacy_recovery_validator_file(&legacy_validator_set)?;
            let mut public_set: Vec<_> = validators
                .iter()
                .map(|validator| (validator.address, validator.stake))
                .collect();
            public_set.sort_by_key(|entry| entry.0.0);
            let mut approved_set = network.validators.clone();
            approved_set.sort_by_key(|entry| entry.0.0);
            ensure!(
                public_set == approved_set,
                "validator public-key file addresses/stakes differ from the complete genesis set"
            );
            let (state, source_wal) = StateDB::load_legacy_recovery_export_source_with_report(
                &data_dir,
                network.genesis_hash,
                allow_unbound_legacy_wal,
                &snapshot,
                &legacy_validators,
            )?;
            let created_at_unix_ms = created_at_unix_ms.unwrap_or(
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .context("host clock is before the Unix epoch")?
                    .as_millis()
                    .try_into()
                    .context("Unix timestamp exceeds u64")?,
            );
            let checkpoint = arc_state::recovery::ArcCheckpoint::export_unsigned(
                &state,
                arc_state::recovery::RecoveryExportSpec {
                    chain_id: genesis.chain.chain_id,
                    genesis_hash: network.genesis_hash,
                    source_consensus_round,
                    recovery_epoch,
                    validator_set_id,
                    validators,
                    community_rewards_v1_activation_height: network
                        .community_rewards_v1_activation_height,
                    created_at_unix_ms,
                },
            )?;
            checkpoint.verify_content()?;
            checkpoint.write_to(&output)?;
            print_recovery_summary(&checkpoint, "EXPORTED_UNSIGNED", Some(&source_wal))
        }
        RecoveryCommand::Sign {
            checkpoint,
            genesis,
            approved_manifest_hash,
            validator_key_file,
            output,
            recovery_epoch,
            validator_set_id,
        } => {
            let mut checkpoint = arc_state::recovery::ArcCheckpoint::read_from(checkpoint)?;
            let trust = recovery_trust(
                &genesis,
                &approved_manifest_hash,
                recovery_epoch,
                validator_set_id,
            )?;
            checkpoint.verify_candidate(&trust)?;
            let keypair = validator_identity::load_ed25519_keyfile(Path::new(&validator_key_file))?;
            checkpoint.add_signature(&keypair)?;
            checkpoint.write_to(&output)?;
            print_recovery_summary(&checkpoint, "SIGNED_CANDIDATE", None)
        }
        RecoveryCommand::Verify {
            checkpoint,
            genesis,
            approved_manifest_hash,
            recovery_epoch,
            validator_set_id,
        } => {
            let checkpoint = arc_state::recovery::ArcCheckpoint::read_from(checkpoint)?;
            let trust = recovery_trust(
                &genesis,
                &approved_manifest_hash,
                recovery_epoch,
                validator_set_id,
            )?;
            checkpoint.verify(&trust)?;
            print_recovery_summary(&checkpoint, "VERIFIED_QUORUM", None)
        }
        RecoveryCommand::Import {
            checkpoint,
            data_dir,
            genesis,
            approved_manifest_hash,
            recovery_epoch,
            validator_set_id,
        } => {
            let (_, network) =
                recovery_network_from_genesis(&genesis, recovery_epoch, validator_set_id)?;
            let approved_manifest_hash =
                parse_recovery_hash("approved_manifest_hash", &approved_manifest_hash)?;
            let state = StateDB::with_genesis_persistent_recovery(
                &[],
                &data_dir,
                network,
                Some(arc_state::recovery::RecoveryImport {
                    checkpoint_path: checkpoint.into(),
                    approved_manifest_hash,
                }),
            )?;
            println!(
                "{}",
                serde_json::to_string_pretty(&serde_json::json!({
                    "status": "ACTIVATED",
                    "height": state.height(),
                    "recovery_epoch": state.recovery_context().map(|context| context.recovery_epoch),
                    "validator_set_id": state.recovery_context().map(|context| context.validator_set_id),
                    "manifest_hash": state.recovery_manifest_hash().map(|hash| format!("0x{}", hash.to_hex())),
                    "state_root": format!("0x{}", state.get_state_root().to_hex()),
                    "data_dir": data_dir,
                }))?
            );
            Ok(())
        }
    }
}

async fn run_operator_command(command: OperatorCommand) -> Result<()> {
    match command {
        OperatorCommand::Recovery { command } => run_recovery_operator_command(command),
        OperatorCommand::Archive {
            command:
                ArchiveCommand::Serve {
                    archive_manifest,
                    complete,
                    inventory,
                    binding_index,
                    binding,
                    checkpoint,
                    expected_archive_manifest_sha256,
                    expected_complete_sha256,
                    node,
                    listen,
                    listen_unix,
                },
        } => {
            let archive_listen = match (listen, listen_unix) {
                (Some(address), None) => {
                    arc_node::legacy_archive::LegacyArchiveListen::Tcp(address)
                }
                #[cfg(unix)]
                (None, Some(path)) => arc_node::legacy_archive::LegacyArchiveListen::Unix(path),
                #[cfg(not(unix))]
                (None, Some(_path)) => anyhow::bail!("--listen-unix requires a Unix host"),
                _ => anyhow::bail!("select exactly one archive listener"),
            };
            arc_node::legacy_archive::serve(
                arc_node::legacy_archive::LegacyArchiveSpec {
                    archive_manifest: archive_manifest.into(),
                    complete: complete.into(),
                    inventory: inventory.into(),
                    binding_index: binding_index.into(),
                    checkpoint: checkpoint.into(),
                    binding: binding.into(),
                    expected_archive_manifest_sha256,
                    expected_complete_sha256,
                    node,
                },
                archive_listen,
            )
            .await
        }
    }
}

/// Return the first candidate whose complete SHA-256 matches the canonical
/// community artifact. A similarly named or same-sized GGUF is not eligible.
fn discover_matching_model(candidates: Vec<String>, expected_sha: &str) -> Option<String> {
    for path in candidates {
        if !std::path::Path::new(&path).is_file() {
            continue;
        }
        match sha256_of(&path) {
            Some(digest) if digest == expected_sha => return Some(path),
            Some(digest) => tracing::warn!(
                path = %path,
                expected_sha,
                actual_sha = %digest,
                "ignoring auto-discovered GGUF with the wrong artifact identity"
            ),
            None => tracing::warn!(
                path = %path,
                "ignoring auto-discovered GGUF because it could not be hashed"
            ),
        }
    }
    None
}

/// Look for the canonical Llama-2-7B Chat GGUF in standard community-node
/// locations. Covers both the seed convention (`llama2-7b.gguf`) and the
/// historical non-chat filename, but accepts bytes only when the full digest
/// matches [`TESTNET_MODEL_SHA256`].
fn auto_discover_model() -> Option<String> {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/root".into());
    let candidates = vec![
        "./llama2-7b.gguf".to_string(),
        "./llama-2-7b.Q4_K_M.gguf".to_string(),
        format!("{}/.arc-models/llama2-7b.gguf", home),
        format!("{}/.arc-models/llama-2-7b.Q4_K_M.gguf", home),
        "/opt/arc/llama2-7b.gguf".to_string(),
        "/var/lib/arc/llama2-7b.gguf".to_string(),
    ];
    discover_matching_model(candidates, TESTNET_MODEL_SHA256)
}

/// Stream a file through SHA-256 in-process. This works identically on Linux,
/// macOS and Windows and keeps memory bounded for multi-gigabyte GGUF files.
fn sha256_of(path: &str) -> Option<String> {
    sha256_of_with_shutdown(path, None)
}

fn sha256_of_with_shutdown(
    path: &str,
    shutdown_requested: Option<&std::sync::atomic::AtomicBool>,
) -> Option<String> {
    use sha2::{Digest, Sha256};
    use std::io::Read;

    let file = std::fs::File::open(path).ok()?;
    let mut reader = std::io::BufReader::with_capacity(1024 * 1024, file);
    let mut hasher = Sha256::new();
    let mut buffer = vec![0_u8; 1024 * 1024];
    loop {
        if shutdown_requested
            .is_some_and(|requested| requested.load(std::sync::atomic::Ordering::SeqCst))
        {
            return None;
        }
        let read = reader.read(&mut buffer).ok()?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Some(hex::encode(hasher.finalize()))
}

/// SHA256 of the canonical testnet Llama-2-7B Q4_K_M GGUF that every seed
/// runs. Community workers must use exactly these Llama-2-7B Chat Q4_K_M
/// bytes. The similarly named non-chat Q4_K_M artifact has a different digest
/// and is not interchangeable. For a migration, change this and every URL in
/// `DEFAULT_MODEL_SOURCES` together.
const TESTNET_MODEL_SHA256: &str =
    "08a5566d61d7cb6b420c3e4387a39e0078e1f2fe5f055f3a03887385304d4bfa";

/// Primary download sources for the canonical testnet GGUF, tried in order.
/// First source whose advertised sha matches TESTNET_MODEL_SHA256 wins.
/// Operators override the list at runtime with ARC_MODEL_URL (comma-
/// separated; e.g. `ARC_MODEL_URL=https://my-mirror/llama2-7b.gguf`).
///
/// HuggingFace is the primary host: free, CDN-backed (CloudFront), supports
/// git-lfs for 4-GB+ files, and works behind any NAT (HTTPS only). Add
/// mirrors by appending URLs here, no recompile needed for ad-hoc mirrors
/// via the env var.
const DEFAULT_MODEL_SOURCES: &[&str] = &[
    "https://huggingface.co/TheBloke/Llama-2-7B-Chat-GGUF/resolve/191239b3e26b2882fb562ffccdd1cf0f65402adb/llama-2-7b-chat.Q4_K_M.gguf",
];

/// Resolve a HuggingFace `/resolve/<immutable-revision>/` URL to its matching
/// `/raw/<immutable-revision>/` form,
/// which returns the git-lfs pointer text (~200 bytes) for large files.
/// The pointer contains the file's real sha256 — letting us fail in 1 KB
/// instead of 4 GB on a misconfigured mirror. Returns None for non-HF URLs
/// or non-LFS files (those will be sha-verified the slow way after download).
fn hf_lfs_pointer_sha(url: &str) -> Option<String> {
    let raw_url = url.replace("/resolve/", "/raw/");
    if raw_url == url {
        return None;
    }
    let out = std::process::Command::new("curl")
        .args(["-sLf", "--max-time", "10", &raw_url])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let body = String::from_utf8(out.stdout).ok()?;
    for line in body.lines() {
        if let Some(sha) = line.strip_prefix("oid sha256:") {
            return Some(sha.trim().to_string());
        }
    }
    None
}

/// Download URL to `tmp` with resume support (`curl -C -` — partial files
/// from prior interrupted runs are continued, not restarted) and verify
/// sha256. Returns true only when the on-disk bytes hash to expected_sha.
/// Cleans up the partial on sha mismatch so a poisoned file can't infect
/// a later retry's resume.
async fn download_and_verify(
    url: &str,
    tmp: &str,
    expected_sha: &str,
    shutdown_requested: &std::sync::atomic::AtomicBool,
) -> bool {
    let mut child = match tokio::process::Command::new("curl")
        .args([
            "-fL",
            "--retry",
            "5",
            "--retry-delay",
            "5",
            "-C",
            "-",
            "-o",
            tmp,
            url,
        ])
        .kill_on_drop(true)
        .spawn()
    {
        Ok(child) => child,
        Err(_) => return false,
    };
    let status = loop {
        if shutdown_requested.load(std::sync::atomic::Ordering::SeqCst) {
            let _ = child.start_kill();
            let _ = child.wait().await;
            tracing::info!("auto-download interrupted by node shutdown request");
            return false;
        }
        match child.try_wait() {
            Ok(Some(status)) => break status.success(),
            Ok(None) => tokio::time::sleep(std::time::Duration::from_millis(200)).await,
            Err(_) => return false,
        }
    };
    if !status {
        return false;
    }
    let got = sha256_of_with_shutdown(tmp, Some(shutdown_requested)).unwrap_or_default();
    if shutdown_requested.load(std::sync::atomic::Ordering::SeqCst) {
        return false;
    }
    if got != expected_sha {
        tracing::warn!(
            "  sha mismatch from {} (got {}); discarding partial",
            url,
            got
        );
        let _ = std::fs::remove_file(tmp);
        return false;
    }
    true
}

/// Download the canonical testnet GGUF to $HOME/.arc-models/llama2-7b.gguf
/// from the first source whose sha matches TESTNET_MODEL_SHA256. Tries the
/// LFS pointer first (fast fail on misconfigured URLs), then the full file
/// with resume support, then sha-verifies. Returns the local path on success.
///
/// Idempotent: returns the existing local path if the file is already
/// present (auto_discover_model() would normally catch that earlier — this
/// guard is kept so direct callers can reuse the function safely).
///
/// Sources come from ARC_MODEL_URL (comma-separated) if set, else from
/// DEFAULT_MODEL_SOURCES. Only invoked when `--community` was explicit, so
/// other run modes never silently fetch multi-GB files.
async fn auto_download_model(shutdown_requested: &std::sync::atomic::AtomicBool) -> Option<String> {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/root".into());
    let target_dir = format!("{}/.arc-models", home);
    let target = format!("{}/llama2-7b.gguf", target_dir);

    if std::path::Path::new(&target).is_file() {
        match sha256_of_with_shutdown(&target, Some(shutdown_requested)) {
            Some(digest) if digest == TESTNET_MODEL_SHA256 => return Some(target),
            Some(digest) => tracing::warn!(
                path = %target,
                expected_sha = TESTNET_MODEL_SHA256,
                actual_sha = %digest,
                "existing default model has the wrong digest; move it aside before retrying auto-download"
            ),
            None => tracing::warn!(
                path = %target,
                "existing default model could not be hashed; move it aside before retrying auto-download"
            ),
        }
        return None;
    }
    if let Err(e) = std::fs::create_dir_all(&target_dir) {
        tracing::warn!("auto-download: cannot create {}: {}", target_dir, e);
        return None;
    }
    let tmp = format!("{}.partial", target);

    let sources: Vec<String> = std::env::var("ARC_MODEL_URL")
        .ok()
        .map(|s| {
            s.split(',')
                .map(|u| u.trim().to_string())
                .filter(|u| !u.is_empty())
                .collect::<Vec<_>>()
        })
        .filter(|v: &Vec<String>| !v.is_empty())
        .unwrap_or_else(|| {
            DEFAULT_MODEL_SOURCES
                .iter()
                .map(|s| s.to_string())
                .collect()
        });

    tracing::info!(
        "--community: searching {} model source(s) for sha {} (~4.08 GB)",
        sources.len(),
        TESTNET_MODEL_SHA256
    );
    tracing::info!("  target: {}", target);

    for url in &sources {
        if shutdown_requested.load(std::sync::atomic::Ordering::SeqCst) {
            return None;
        }
        tracing::info!("--community: trying {}", url);

        // 1) Fast LFS pre-check: HuggingFace `/raw/` URL returns the LFS
        //    pointer (~200 B) containing the file's sha. Skip 4-GB pulls
        //    against URLs whose advertised sha doesn't match.
        if let Some(remote_sha) = hf_lfs_pointer_sha(url) {
            if remote_sha != TESTNET_MODEL_SHA256 {
                tracing::warn!(
                    "  LFS sha {} != expected {} — skipping this source",
                    remote_sha,
                    TESTNET_MODEL_SHA256
                );
                continue;
            }
            tracing::info!("  LFS sha pre-check OK ({})", remote_sha);
        }

        // 2) Download with resume + final sha verification.
        if download_and_verify(url, &tmp, TESTNET_MODEL_SHA256, shutdown_requested).await {
            if let Err(e) = std::fs::rename(&tmp, &target) {
                tracing::warn!("  rename {} -> {} failed: {}", tmp, target, e);
                continue;
            }
            tracing::info!(
                "--community: downloaded + sha-verified model at {} (sha256 {})",
                target,
                TESTNET_MODEL_SHA256
            );
            return Some(target);
        }
    }

    tracing::warn!(
        "auto-download: all {} source(s) failed. To add a mirror set \
         ARC_MODEL_URL=<url1[,url2,...]> and retry; the URL must serve a \
         file whose sha256 == {}.",
        sources.len(),
        TESTNET_MODEL_SHA256
    );
    None
}

/// Best-effort detection of total system RAM in MB. Reads /proc/meminfo on
/// Linux; falls back to 8192 (8 GB) on macOS / non-Linux or on parse failure.
/// Used by validator auto-shard to advertise capacity to the seed.
fn detect_ram_mb() -> u64 {
    if let Ok(meminfo) = std::fs::read_to_string("/proc/meminfo") {
        for line in meminfo.lines() {
            if line.starts_with("MemTotal:")
                && let Some(kb) = line
                    .split_whitespace()
                    .nth(1)
                    .and_then(|s| s.parse::<u64>().ok())
            {
                return kb / 1024;
            }
        }
    }
    8192
}

/// Ask a seed for our shard assignment by POSTing /shards/join. Used by
/// validators that didn't pass an explicit --shard-range; the seed finds
/// the biggest uncovered layer gap in the pipeline and returns a range
/// for this node to hold. Returns `Some((start, end))` on success.
///
/// The exact source-artifact commitment is sent with the join request so the
/// assignment cannot enter a pipeline for same-shape, different-weight bytes.
///
/// The node's public label for the shard registry.
///
/// Deliberately never derived from validator signing material: this string is
/// POSTed to every seed and handed out by GET /shards.
fn public_node_name(cli: &Cli) -> String {
    if let Some(n) = cli.node_name.as_deref() {
        let n = n.trim();
        if !n.is_empty() {
            return n.to_string();
        }
    }
    let hostname = std::env::var("HOSTNAME")
        .or_else(|_| std::env::var("COMPUTERNAME"))
        .unwrap_or_else(|_| "unknown-host".to_string());
    let public_metadata = format!("{}|{}|{}|{}", hostname, cli.rpc, cli.p2p_port, cli.data_dir);
    let digest = arc_crypto::hash_bytes(public_metadata.as_bytes());
    format!("arc-{}", &hex::encode(digest.0)[..8])
}

#[derive(Clone, Copy, Debug)]
struct ValidatorHttpAudience {
    target_validator: Hash256,
    transaction_domain: Option<Hash256>,
}

fn parse_validator_http_audience_hash(value: &str, field: &str) -> Result<Hash256> {
    Hash256::from_hex(value.strip_prefix("0x").unwrap_or(value))
        .map_err(|error| anyhow::anyhow!("invalid {field}: {error}"))
}

/// Resolve the exact validator and recovery domain that an authenticated HTTP
/// mutation targets. Callers may cache this result, but must evict it after a
/// failed request so a recovery transition can never keep using stale domain
/// metadata.
async fn fetch_validator_http_audience(
    client: &reqwest::Client,
    rpc_base: &str,
) -> Result<ValidatorHttpAudience> {
    let info = client
        .get(format!("{rpc_base}/network/info"))
        .timeout(std::time::Duration::from_secs(5))
        .send()
        .await
        .with_context(|| format!("GET validator HTTP audience from {rpc_base}"))?
        .error_for_status()
        .with_context(|| format!("validator {rpc_base} has no audience metadata"))?
        .json::<serde_json::Value>()
        .await
        .context("decode validator HTTP audience")?;
    let target_validator = info
        .get("validator_address")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| anyhow::anyhow!("validator omitted validator_address"))
        .and_then(|value| parse_validator_http_audience_hash(value, "validator_address"))?;
    ensure!(
        target_validator != Hash256::ZERO,
        "validator HTTP audience cannot be the zero address"
    );
    let transaction_domain = match info.get("transaction_domain") {
        Some(serde_json::Value::String(value)) => Some(parse_validator_http_audience_hash(
            value,
            "validator transaction_domain",
        )?),
        Some(serde_json::Value::Null) | None => None,
        Some(_) => {
            return Err(anyhow::anyhow!("validator transaction_domain is malformed"));
        }
    };
    if info
        .get("recovery_active")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false)
        && transaction_domain.is_none()
    {
        bail!("recovery validator omitted its required transaction_domain");
    }
    Ok(ValidatorHttpAudience {
        target_validator,
        transaction_domain,
    })
}

async fn post_signed_shard_announcement(
    client: &reqwest::Client,
    rpc_base: &str,
    shard: &rpc::ShardInfo,
    keypair: &arc_crypto::KeyPair,
    audience: ValidatorHttpAudience,
) -> Result<()> {
    let signed = rpc::sign_validator_shard_announcement(
        shard.clone(),
        keypair,
        audience.target_validator,
        audience.transaction_domain,
    )
    .map_err(anyhow::Error::msg)
    .context("sign validator shard announcement")?;
    client
        .post(format!("{rpc_base}{}", rpc::SHARD_ANNOUNCE_PATH))
        .json(&signed)
        .timeout(std::time::Duration::from_secs(5))
        .send()
        .await
        .with_context(|| format!("POST authenticated shard announcement to {rpc_base}"))?
        .error_for_status()
        .with_context(|| format!("validator {rpc_base} rejected shard announcement"))?;
    Ok(())
}

const SHARD_ANNOUNCEMENT_STARTUP_DELAY_SECS: u64 = 3;
const SHARD_ANNOUNCEMENT_INTERVAL_SECS: u64 = 15;

async fn wait_for_optional_runtime_shutdown(
    receiver: &mut Option<tokio::sync::watch::Receiver<bool>>,
) {
    let Some(receiver) = receiver.as_mut() else {
        std::future::pending::<()>().await;
        return;
    };
    loop {
        if *receiver.borrow_and_update() {
            return;
        }
        if receiver.changed().await.is_err() {
            return;
        }
    }
}

async fn sleep_or_runtime_shutdown(
    receiver: &mut Option<tokio::sync::watch::Receiver<bool>>,
    duration: std::time::Duration,
) -> bool {
    tokio::select! {
        biased;
        _ = wait_for_optional_runtime_shutdown(receiver) => true,
        _ = tokio::time::sleep(duration) => false,
    }
}

/// Keep every locally held range live at each explicitly configured
/// coordinator. The receiver authenticates the holder, destination audience,
/// recovery domain, exact artifact and execution profile before mutating its
/// registry. Read-only topology responses are never imported as a fallback.
#[cfg(test)]
async fn run_signed_shard_announcement_loop(
    shards: Vec<rpc::ShardInfo>,
    targets: Vec<String>,
    keypair: arc_crypto::KeyPair,
) {
    run_signed_shard_announcement_loop_inner(shards, targets, keypair, None).await;
}

async fn run_signed_shard_announcement_loop_with_shutdown(
    shards: Vec<rpc::ShardInfo>,
    targets: Vec<String>,
    keypair: arc_crypto::KeyPair,
    shutdown: tokio::sync::watch::Receiver<bool>,
) {
    run_signed_shard_announcement_loop_inner(shards, targets, keypair, Some(shutdown)).await;
}

async fn run_signed_shard_announcement_loop_inner(
    shards: Vec<rpc::ShardInfo>,
    targets: Vec<String>,
    keypair: arc_crypto::KeyPair,
    mut shutdown: Option<tokio::sync::watch::Receiver<bool>>,
) {
    // Let the local RPC listener start before the first self-announcement.
    if sleep_or_runtime_shutdown(
        &mut shutdown,
        std::time::Duration::from_secs(SHARD_ANNOUNCEMENT_STARTUP_DELAY_SECS),
    )
    .await
    {
        return;
    }
    let client = match reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(5))
        .redirect(reqwest::redirect::Policy::none())
        .build()
    {
        Ok(client) => client,
        Err(error) => {
            tracing::error!(%error, "Cannot build authenticated shard-announcement client");
            return;
        }
    };
    let mut audiences = std::collections::HashMap::<String, ValidatorHttpAudience>::new();
    loop {
        for rpc_base in &targets {
            let audience = if let Some(audience) = audiences.get(rpc_base).copied() {
                audience
            } else {
                match fetch_validator_http_audience(&client, rpc_base).await {
                    Ok(audience) => {
                        audiences.insert(rpc_base.clone(), audience);
                        audience
                    }
                    Err(error) => {
                        tracing::warn!(
                            %rpc_base,
                            %error,
                            "Cannot resolve authenticated shard-announcement audience"
                        );
                        continue;
                    }
                }
            };
            let mut target_succeeded = true;
            for shard in &shards {
                if let Err(error) =
                    post_signed_shard_announcement(&client, rpc_base, shard, &keypair, audience)
                        .await
                {
                    tracing::warn!(
                        %rpc_base,
                        %error,
                        "Authenticated shard announcement failed; evicting cached audience"
                    );
                    target_succeeded = false;
                    break;
                }
            }
            if !target_succeeded {
                audiences.remove(rpc_base);
            }
        }
        if sleep_or_runtime_shutdown(
            &mut shutdown,
            std::time::Duration::from_secs(SHARD_ANNOUNCEMENT_INTERVAL_SECS),
        )
        .await
        {
            return;
        }
    }
}

/// POST one authenticated community mutation. A new timestamp and CSPRNG
/// nonce are signed for every attempt; callers must invoke this again for a
/// retry or a different coordinator rather than reusing the wire envelope.
async fn post_signed_community<T: serde::Serialize>(
    client: &reqwest::Client,
    rpc_base: &str,
    path: &str,
    payload: T,
    keypair: &arc_crypto::KeyPair,
    timeout: std::time::Duration,
) -> Result<reqwest::Response> {
    let info = client
        .get(format!("{rpc_base}/network/info"))
        .timeout(std::time::Duration::from_secs(5))
        .send()
        .await
        .with_context(|| format!("GET community coordinator audience from {rpc_base}"))?
        .error_for_status()
        .with_context(|| format!("community coordinator {rpc_base} has no audience metadata"))?
        .json::<serde_json::Value>()
        .await
        .context("decode community coordinator audience")?;
    let target_coordinator = info
        .get("validator_address")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| anyhow::anyhow!("coordinator omitted validator_address"))
        .and_then(|value| {
            Hash256::from_hex(value)
                .map_err(|error| anyhow::anyhow!("invalid coordinator validator_address: {error}"))
        })?;
    let transaction_domain = match info.get("transaction_domain") {
        Some(serde_json::Value::String(value)) => {
            Some(Hash256::from_hex(value).map_err(|error| {
                anyhow::anyhow!("invalid coordinator transaction_domain: {error}")
            })?)
        }
        Some(serde_json::Value::Null) | None => None,
        Some(_) => {
            return Err(anyhow::anyhow!(
                "coordinator transaction_domain is malformed"
            ));
        }
    };
    if info
        .get("recovery_active")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false)
        && transaction_domain.is_none()
    {
        return Err(anyhow::anyhow!(
            "recovery coordinator omitted its required transaction_domain"
        ));
    }
    let signed = rpc::sign_community_request(
        path,
        payload,
        keypair,
        target_coordinator,
        transaction_domain,
    )
    .map_err(anyhow::Error::msg)
    .context("sign community HTTP mutation")?;
    client
        .post(format!("{rpc_base}{path}"))
        .json(&signed)
        .timeout(timeout)
        .send()
        .await
        .with_context(|| format!("POST authenticated community mutation to {rpc_base}{path}"))
}

async fn decline_community_assignment(
    client: reqwest::Client,
    coordinator: String,
    job: serde_json::Value,
    worker_id: String,
    keypair: arc_crypto::KeyPair,
    reason: &'static str,
) {
    let Some(job_id) = job
        .get("job_id")
        .and_then(serde_json::Value::as_str)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
    else {
        tracing::warn!(seed = %coordinator, "cannot decline community assignment without a job_id");
        return;
    };
    let decline = rpc::WorkResult {
        job_id: job_id.clone(),
        worker_id,
        success: false,
        declined: true,
        output: String::new(),
        output_hash: String::new(),
        tokens_generated: 0,
        total_ms: 0,
        ms_per_token: 0,
        engine: String::new(),
        error: Some(reason.to_string()),
        signed_attestation_hex: None,
    };
    match post_signed_community(
        &client,
        &coordinator,
        rpc::COMMUNITY_SUBMIT_WORK_PATH,
        decline,
        &keypair,
        std::time::Duration::from_secs(10),
    )
    .await
    {
        Ok(response) if response.status().is_success() => {
            tracing::debug!(job_id, seed = %coordinator, %reason, "declined community assignment");
        }
        Ok(response) => {
            tracing::warn!(
                job_id,
                seed = %coordinator,
                status = %response.status(),
                %reason,
                "coordinator rejected community assignment decline"
            );
        }
        Err(error) => {
            tracing::warn!(
                job_id,
                seed = %coordinator,
                %error,
                %reason,
                "could not decline community assignment"
            );
        }
    }
}

const COMMUNITY_SUBMIT_LATE_GRACE_SECS: u64 = 5 * 60;
const COMMUNITY_ASSIGNMENT_CLOCK_SKEW_SECS: u64 = 60;
const COMMUNITY_SUBMIT_BACKOFF_BASE_MS: u64 = 250;
const COMMUNITY_SUBMIT_BACKOFF_CAP_MS: u64 = 30_000;
// Background admission closes at the lifecycle signal. A community assignment
// already owned at that edge can use the complete public request window plus
// its crash/late-submit grace; the managed-service contract then retains 120s
// for task joins and the final WAL durability barrier.
const MANAGED_NODE_STOP_BUDGET_SECS: u64 = 4_420;
const _: () = assert!(
    rpc::PUBLIC_INFERENCE_REQUEST_TIMEOUT_SECS + COMMUNITY_SUBMIT_LATE_GRACE_SECS + 120
        == MANAGED_NODE_STOP_BUDGET_SECS
);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CommunitySubmitResponseDisposition {
    Accepted,
    Retry,
    Rejected,
}

#[derive(Debug)]
enum CommunitySubmitOutcome {
    Accepted { body: String },
    Rejected { body: String },
    DeadlineExceeded,
    LocalError,
}

impl CommunitySubmitOutcome {
    fn response_body(&self) -> Option<&str> {
        match self {
            Self::Accepted { body } | Self::Rejected { body } => Some(body),
            Self::DeadlineExceeded | Self::LocalError => None,
        }
    }
}

fn community_submit_response_disposition(
    status: reqwest::StatusCode,
    body: &str,
) -> CommunitySubmitResponseDisposition {
    if status.is_success() {
        return CommunitySubmitResponseDisposition::Accepted;
    }
    if matches!(
        status,
        reqwest::StatusCode::REQUEST_TIMEOUT
            | reqwest::StatusCode::TOO_MANY_REQUESTS
            | reqwest::StatusCode::BAD_GATEWAY
            | reqwest::StatusCode::SERVICE_UNAVAILABLE
            | reqwest::StatusCode::GATEWAY_TIMEOUT
    ) {
        return CommunitySubmitResponseDisposition::Retry;
    }
    if status == reqwest::StatusCode::CONFLICT {
        let body_lower = body.to_ascii_lowercase();
        let structured_in_progress = serde_json::from_str::<serde_json::Value>(body)
            .ok()
            .and_then(|value| {
                value
                    .get("code")
                    .or_else(|| value.pointer("/error/code"))
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_ascii_lowercase)
            })
            .is_some_and(|code| {
                matches!(
                    code.as_str(),
                    "submit_in_progress" | "submission_in_progress" | "submitting"
                )
            });
        if structured_in_progress
            || body_lower.contains("already being verified")
            || body_lower.contains("submission is in progress")
            || body_lower.contains("submit is in progress")
        {
            return CommunitySubmitResponseDisposition::Retry;
        }
    }
    CommunitySubmitResponseDisposition::Rejected
}

fn community_submit_error_is_network(error: &anyhow::Error) -> bool {
    error
        .chain()
        .filter_map(|cause| cause.downcast_ref::<reqwest::Error>())
        .any(|error| {
            error.is_timeout()
                || error.is_connect()
                || error.is_request()
                || error.is_body()
                || error.status().is_some_and(|status| {
                    matches!(
                        status,
                        reqwest::StatusCode::REQUEST_TIMEOUT
                            | reqwest::StatusCode::TOO_MANY_REQUESTS
                            | reqwest::StatusCode::BAD_GATEWAY
                            | reqwest::StatusCode::SERVICE_UNAVAILABLE
                            | reqwest::StatusCode::GATEWAY_TIMEOUT
                    )
                })
        })
}

fn community_submit_deadline_unix_ms(submitted_at_unix_ms: i64) -> Option<i64> {
    let budget_ms = rpc::PUBLIC_INFERENCE_REQUEST_TIMEOUT_SECS
        .checked_add(COMMUNITY_SUBMIT_LATE_GRACE_SECS)?
        .checked_mul(1_000)?;
    submitted_at_unix_ms.checked_add(i64::try_from(budget_ms).ok()?)
}

/// Validate an authenticated coordinator's wall-clock assignment metadata and
/// return a monotonic duration to the stricter of its exact expiry and the
/// public protocol's 4,000-second ceiling plus late-submit grace.
fn community_submit_window(
    submitted_at_unix_ms: i64,
    expires_at_unix_ms: u64,
    now_unix_ms: i64,
) -> Result<std::time::Duration, String> {
    if submitted_at_unix_ms <= 0 {
        return Err("assignment omitted a positive submitted_at_unix_ms".to_string());
    }
    let expires_at_unix_ms = i64::try_from(expires_at_unix_ms)
        .map_err(|_| "assignment expiry exceeds the Unix timestamp range".to_string())?;
    if expires_at_unix_ms <= submitted_at_unix_ms {
        return Err("assignment expiry is not after its submission time".to_string());
    }
    let allowed_future_ms = i64::try_from(COMMUNITY_ASSIGNMENT_CLOCK_SKEW_SECS * 1_000)
        .expect("clock-skew constant fits i64");
    if submitted_at_unix_ms > now_unix_ms.saturating_add(allowed_future_ms) {
        return Err("assignment submission time is too far in the future".to_string());
    }
    let outer_deadline = community_submit_deadline_unix_ms(submitted_at_unix_ms)
        .ok_or_else(|| "assignment submission deadline overflowed".to_string())?;
    let deadline_unix_ms = expires_at_unix_ms.min(outer_deadline);
    let remaining_ms = deadline_unix_ms
        .checked_sub(now_unix_ms)
        .filter(|remaining| *remaining > 0)
        .ok_or_else(|| "assignment submission window has expired".to_string())?;
    Ok(std::time::Duration::from_millis(
        u64::try_from(remaining_ms).expect("positive i64 milliseconds fit u64"),
    ))
}

fn community_generation_required_positions(prompt_tokens: usize, max_tokens: u32) -> Option<usize> {
    let max_tokens = usize::try_from(max_tokens).ok()?;
    1usize.checked_add(prompt_tokens)?.checked_add(max_tokens)
}

fn community_generation_fits_context(
    prompt_tokens: usize,
    max_tokens: u32,
    max_seq: usize,
) -> bool {
    community_generation_required_positions(prompt_tokens, max_tokens)
        .is_some_and(|required| required <= max_seq)
}

fn community_submit_backoff(attempt: u32, jitter: u64) -> std::time::Duration {
    let exponent = attempt.min(16);
    let ceiling_ms = COMMUNITY_SUBMIT_BACKOFF_BASE_MS
        .saturating_mul(1u64 << exponent)
        .min(COMMUNITY_SUBMIT_BACKOFF_CAP_MS);
    let floor_ms = ceiling_ms / 2;
    let jitter_span = ceiling_ms.saturating_sub(floor_ms).saturating_add(1);
    std::time::Duration::from_millis(floor_ms + jitter % jitter_span)
}

fn community_log_body(body: &str) -> String {
    const MAX_CHARS: usize = 1_024;
    let mut chars = body.chars();
    let prefix: String = chars.by_ref().take(MAX_CHARS).collect();
    if chars.next().is_some() {
        format!("{prefix}…[truncated]")
    } else {
        prefix
    }
}

/// Submit one immutable result to its assigning coordinator. Each attempt
/// invokes `post_signed_community` again, so a timeout-after-server-success is
/// recovered idempotently with a fresh HTTP nonce and signature while the
/// WorkResult, job id, output and signed attestation remain byte-for-byte
/// semantically identical.
async fn submit_community_result_with_retry(
    client: &reqwest::Client,
    coordinator: &str,
    result: &rpc::WorkResult,
    keypair: &arc_crypto::KeyPair,
    deadline: tokio::time::Instant,
) -> CommunitySubmitOutcome {
    let mut attempt = 0u32;
    loop {
        let now = tokio::time::Instant::now();
        let Some(remaining) = deadline.checked_duration_since(now) else {
            tracing::warn!(
                job_id = %result.job_id,
                seed = %coordinator,
                attempts = attempt,
                terminal = "deadline_exceeded",
                "community result submission reached its immutable assignment deadline"
            );
            return CommunitySubmitOutcome::DeadlineExceeded;
        };
        if remaining.is_zero() {
            tracing::warn!(
                job_id = %result.job_id,
                seed = %coordinator,
                attempts = attempt,
                terminal = "deadline_exceeded",
                "community result submission reached its immutable assignment deadline"
            );
            return CommunitySubmitOutcome::DeadlineExceeded;
        }

        attempt = attempt.saturating_add(1);
        let request_timeout = remaining.min(std::time::Duration::from_secs(
            rpc::COMMUNITY_SUBMIT_REQUEST_TIMEOUT_SECS,
        ));
        let attempt_future = async {
            let response = post_signed_community(
                client,
                coordinator,
                rpc::COMMUNITY_SUBMIT_WORK_PATH,
                (*result).clone(),
                keypair,
                request_timeout,
            )
            .await?;
            let status = response.status();
            let body = response
                .text()
                .await
                .context("read community submit response body")?;
            Ok::<_, anyhow::Error>((status, body))
        };

        let retry_reason = match tokio::time::timeout(remaining, attempt_future).await {
            Err(_) => "attempt_timeout".to_string(),
            Ok(Err(error)) if community_submit_error_is_network(&error) => {
                format!("network_error: {error:#}")
            }
            Ok(Err(error)) => {
                tracing::warn!(
                    job_id = %result.job_id,
                    seed = %coordinator,
                    attempts = attempt,
                    terminal = "local_error",
                    %error,
                    "community result submission stopped on a non-network local error"
                );
                return CommunitySubmitOutcome::LocalError;
            }
            Ok(Ok((status, body))) => match community_submit_response_disposition(status, &body) {
                CommunitySubmitResponseDisposition::Accepted => {
                    tracing::info!(
                        job_id = %result.job_id,
                        seed = %coordinator,
                        attempts = attempt,
                        status = %status,
                        terminal = "accepted",
                        "community result submission completed"
                    );
                    return CommunitySubmitOutcome::Accepted { body };
                }
                CommunitySubmitResponseDisposition::Rejected => {
                    tracing::warn!(
                        job_id = %result.job_id,
                        seed = %coordinator,
                        attempts = attempt,
                        status = %status,
                        response = %community_log_body(&body),
                        terminal = "rejected",
                        "community result submission was terminally rejected"
                    );
                    return CommunitySubmitOutcome::Rejected { body };
                }
                CommunitySubmitResponseDisposition::Retry => {
                    format!("retryable_http_{status}")
                }
            },
        };

        let now = tokio::time::Instant::now();
        let Some(remaining) = deadline.checked_duration_since(now) else {
            continue;
        };
        let jitter_uuid = uuid::Uuid::new_v4().as_u128();
        let jitter = (jitter_uuid as u64) ^ (jitter_uuid >> 64) as u64;
        let delay = community_submit_backoff(attempt.saturating_sub(1), jitter).min(remaining);
        tracing::warn!(
            job_id = %result.job_id,
            seed = %coordinator,
            attempts = attempt,
            reason = %retry_reason,
            retry_in_ms = delay.as_millis(),
            "retrying immutable community result with a fresh authenticated envelope"
        );
        tokio::time::sleep(delay).await;
    }
}

/// A stake-zero node may continue providing outbound community inference
/// while the shipped production genesis awaits its public validator keys.
/// It must not treat that placeholder as a consensus validator set.
fn is_genesis_migration_observer(
    genesis: Option<&config::GenesisConfig>,
    stake: u64,
    insecure_dev_mode: bool,
) -> bool {
    stake == 0
        && !insecure_dev_mode
        && genesis.is_some_and(|config| !config.chain.validator_set_complete)
}

/// Consensus/P2P is a staked validator role. A complete production genesis
/// must not turn stake-zero community/full-model workers into unauthenticated
/// validator transport participants merely because migration mode ended.
fn chain_participation_allowed(
    stake: u64,
    migration_observer: bool,
    insecure_dev_validator_seed: bool,
) -> bool {
    !migration_observer && (stake > 0 || insecure_dev_validator_seed)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct NodeRuntimeRoles {
    chain_participation: bool,
    tier1_background_inference: bool,
}

fn node_runtime_roles(chain_participation_enabled: bool, protocol_major: u16) -> NodeRuntimeRoles {
    // Protocol v3 intentionally keeps paid Tier-1 inference dark until its
    // authorization path is complete, and consensus rejects legacy vote and
    // finalize transactions. Do not rebuild restored legacy requests into an
    // unbounded background compute task that cannot produce an admissible v3
    // transaction or fit the managed service's shutdown contract. Chain
    // participation is independent and must remain enabled for v3 validators.
    NodeRuntimeRoles {
        chain_participation: chain_participation_enabled,
        tier1_background_inference: chain_participation_enabled && protocol_major < 3,
    }
}

fn validate_full_integer_worker_role(
    cli: &Cli,
    stake: u64,
    community_rpc_bases: &[String],
) -> Result<()> {
    if !cli.full_integer_worker {
        return Ok(());
    }
    ensure!(
        stake == 0,
        "--full-integer-worker is a stake-zero community role"
    );
    ensure!(
        !cli.no_community && !community_rpc_bases.is_empty(),
        "--full-integer-worker requires community networking and at least one explicit --community-rpc-url"
    );
    ensure!(
        cli.model.is_some(),
        "--full-integer-worker requires --model <exact GGUF path>"
    );
    ensure!(
        !cli.tokenizer_only,
        "--full-integer-worker is incompatible with --tokenizer-only"
    );
    ensure!(
        !cli.enable_i16,
        "--full-integer-worker is pinned to the canonical per-row INT8 reward profile and is incompatible with --enable-i16"
    );
    ensure!(
        cli.shard_ranges.is_empty()
            && cli.shard_start.is_none()
            && cli.shard_end.is_none()
            && !cli.auto_shard_join,
        "--full-integer-worker must not carry shard ranges or --auto-shard-join; it never advertises validator shards"
    );
    Ok(())
}

#[cfg(feature = "benchmark-tools")]
fn benchmark_mode_enabled(cli: &Cli) -> bool {
    cli.benchmark
}

#[cfg(not(feature = "benchmark-tools"))]
fn benchmark_mode_enabled(_cli: &Cli) -> bool {
    false
}

/// Keep deliberately predictable benchmark signers inside one isolated
/// process-local development network. This has no public-network override.
#[cfg(feature = "benchmark-tools")]
fn validate_benchmark_runtime(
    cli: &Cli,
    rpc_addr: &str,
    peers: &[String],
    community_rpc_urls: &[String],
    stake: u64,
) -> Result<()> {
    if !cli.benchmark {
        return Ok(());
    }

    let rpc_socket = rpc_addr.parse::<SocketAddr>().with_context(|| {
        format!("benchmark RPC listen must be a numeric socket address, got {rpc_addr:?}")
    })?;
    ensure!(
        rpc_socket.ip().is_loopback(),
        "benchmark RPC must listen on a numeric loopback address"
    );
    for peer in peers {
        let peer_socket = peer.parse::<SocketAddr>().with_context(|| {
            format!("benchmark P2P peer must be a numeric socket address, got {peer:?}")
        })?;
        ensure!(
            peer_socket.ip().is_loopback(),
            "benchmark P2P peers must use numeric loopback addresses"
        );
    }

    ensure!(
        cli.insecure_dev_validator_seed && cli.genesis.is_none() && stake > 0,
        "--benchmark requires an isolated local devnet: a positive stake, --insecure-dev-validator-seed, and no --genesis"
    );
    ensure!(
        community_rpc_urls.is_empty()
            && cli.shard_hosts.is_empty()
            && !cli.auto_shard_join
            && !cli.enable_community_rewards_v1
            && !cli.community
            && !cli.community_mode,
        "--benchmark forbids community, shard, reward, and auto-join network targets"
    );

    Ok(())
}

fn identity_requires_persistent_role(cli: &Cli, community_rpc_urls: &[String]) -> bool {
    cli.community
        || cli.community_mode
        || cli.full_integer_worker
        || cli.enable_community_rewards_v1
        || !community_rpc_urls.is_empty()
        || !cli.shard_hosts.is_empty()
        || cli.auto_shard_join
        || !cli.shard_ranges.is_empty()
        || cli.shard_start.is_some()
        || cli.shard_end.is_some()
}

/// Validate the identity/network boundary before any signing identity is
/// loaded or derived. Returns whether a fresh process-ephemeral stake-zero
/// observer is permitted.
fn validate_identity_runtime(
    cli: &Cli,
    rpc_addr: &str,
    peers: &[String],
    community_rpc_urls: &[String],
    keyfile_configured: bool,
    seed_configured: bool,
    stake: u64,
) -> Result<bool> {
    let rpc_socket = rpc_addr.parse::<SocketAddr>();
    let peer_sockets = peers
        .iter()
        .map(|peer| {
            peer.parse::<SocketAddr>().with_context(|| {
                format!("identity P2P peer must be a numeric socket address, got {peer:?}")
            })
        })
        .collect::<Result<Vec<_>>>();
    let persistent_role = identity_requires_persistent_role(cli, community_rpc_urls);

    if seed_configured {
        ensure!(
            cli.insecure_dev_validator_seed,
            "every validator seed requires --insecure-dev-validator-seed; use a generated mode-0600 --validator-key-file for persistent or networked identity"
        );
        let rpc_socket = rpc_socket.with_context(|| {
            format!(
                "insecure development seed RPC must be a numeric socket address, got {rpc_addr:?}"
            )
        })?;
        ensure!(
            rpc_socket.ip().is_loopback(),
            "insecure development seed RPC must use a numeric loopback address"
        );
        for peer in peer_sockets? {
            ensure!(
                peer.ip().is_loopback(),
                "insecure development seed P2P peers must use numeric loopback addresses"
            );
        }
        ensure!(
            !persistent_role,
            "insecure development seeds cannot be used for community, reward, shard, auto-join, or external RPC roles; generate a persistent keyfile"
        );
        return Ok(false);
    }

    ensure!(
        !cli.insecure_dev_validator_seed,
        "--insecure-dev-validator-seed requires an explicit --validator-seed or [validator].seed"
    );
    if keyfile_configured || stake > 0 {
        return Ok(false);
    }

    let strictly_loopback = rpc_socket.is_ok_and(|socket| socket.ip().is_loopback())
        && peer_sockets.is_ok_and(|sockets| sockets.iter().all(|socket| socket.ip().is_loopback()));
    ensure!(
        strictly_loopback && !persistent_role,
        "a persistent node identity is required for non-loopback networking and every community/reward/shard role: create an arc-keygen-compatible mode-0600 keyfile, pass --validator-key-file <path>, and preserve it across restarts; ephemeral observer identity is local-only and changes on restart"
    );
    Ok(true)
}

async fn auto_shard_join(
    cli: &Cli,
    rpc_base: &str,
    advertised_socket: &str,
    model_artifact_id: Hash256,
) -> Option<(usize, usize)> {
    if rpc_base.is_empty() {
        tracing::warn!(
            "auto-shard requires an explicit --community-rpc-url; P2P peers are not RPC origins"
        );
        return None;
    }
    let url = format!("{rpc_base}/shards/join");
    // Every joiner presents the BLAKE3 commitment of the exact source
    // artifact. Shape metadata cannot distinguish different weights.
    let model_id_hex = hex::encode(model_artifact_id.0);

    let body = serde_json::json!({
        "socket_addr": advertised_socket,
        "node_name": public_node_name(cli),
        "model_id": model_id_hex,
        "model_name": "Llama-2-7B",
        "execution_profile": arc_inference::cached_integer_model::CANONICAL_REWARD_INFERENCE_PROFILE,
        "total_layers": 32u32,
        "available_memory_mb": detect_ram_mb(),
        "gpu_tier": 0u8,
    });

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .ok()?;
    let resp = match client.post(&url).json(&body).send().await {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!("auto-shard: POST {} failed: {}", url, e);
            return None;
        }
    };
    if !resp.status().is_success() {
        tracing::warn!("auto-shard: {} returned HTTP {}", url, resp.status());
        return None;
    }
    let v: serde_json::Value = match resp.json().await {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!("auto-shard: response parse failed: {}", e);
            return None;
        }
    };
    let start = v.get("assigned_start_layer")?.as_u64()? as usize;
    let end = v.get("assigned_end_layer")?.as_u64()? as usize;
    Some((start, end))
}

fn advertised_shard_rpc_origin(cli: &Cli, rpc_addr: &str) -> Result<String> {
    let raw = match std::env::var("ARC_PUBLIC_SOCKET") {
        Ok(value) if !value.trim().is_empty() => value,
        Ok(_) => anyhow::bail!("ARC_PUBLIC_SOCKET must not be empty"),
        Err(std::env::VarError::NotPresent) if cli.rpc_unix.is_some() => anyhow::bail!(
            "a --rpc-unix shard holder requires ARC_PUBLIC_SOCKET to name its reviewed public gateway"
        ),
        Err(std::env::VarError::NotPresent) => rpc_addr.to_string(),
        Err(error) => return Err(anyhow::anyhow!("cannot read ARC_PUBLIC_SOCKET: {error}")),
    };
    let canonical = rpc::canonical_shard_rpc_origin(&raw)
        .map_err(|error| anyhow::anyhow!("invalid advertised shard RPC origin: {error}"))?;
    if cli.rpc_unix.is_some() {
        rpc::validate_community_rpc_bases(std::slice::from_ref(&canonical), false)
            .map_err(|error| anyhow::anyhow!("unsafe production shard RPC origin: {error}"))?;
    }
    Ok(canonical)
}

/// Process status for the final shutdown durability barrier. Keeping this
/// decision pure makes the fail-closed contract regression-testable without
/// terminating the test process.
fn shutdown_exit_code(wal_result: &Result<(), arc_state::StateError>) -> i32 {
    if wal_result.is_ok() { 0 } else { 1 }
}

const DESKTOP_SHUTDOWN_CONTROL_DIR_NAME: &str = ".arc-desktop-control";
const DESKTOP_SHUTDOWN_TOKEN_FILE_NAME: &str = "token";
const DESKTOP_SHUTDOWN_REQUEST_FILE_NAME: &str = "request";
const DESKTOP_SHUTDOWN_REQUEST_SCHEMA: &str = "arc.desktop.shutdown.v1";
const DESKTOP_SHUTDOWN_FILE_MAX_BYTES: u64 = 256;

struct DesktopShutdownControl {
    request_file: PathBuf,
    expected_token: [u8; 32],
    data_dir: PathBuf,
    executable: PathBuf,
    genesis: PathBuf,
}

#[derive(Clone)]
struct DesktopShutdownReceiptIdentity {
    data_dir: PathBuf,
    expected_token: [u8; 32],
    nonce: [u8; 32],
    executable: PathBuf,
    genesis: PathBuf,
}

impl DesktopShutdownReceiptIdentity {
    fn acknowledge(&self) -> Result<()> {
        arc_crypto::secret_file::acknowledge_desktop_shutdown_receipt(
            &self.data_dir,
            &self.expected_token,
            &self.nonce,
            &self.executable,
            &self.genesis,
        )
        .context("failed to durably acknowledge the desktop shutdown receipt")
    }
}

impl Drop for DesktopShutdownReceiptIdentity {
    fn drop(&mut self) {
        self.expected_token.zeroize();
        self.nonce.zeroize();
    }
}

impl Drop for DesktopShutdownControl {
    fn drop(&mut self) {
        self.expected_token.zeroize();
    }
}

fn prepare_desktop_shutdown_control(
    data_dir: &Path,
    token_file: Option<&Path>,
    genesis_file: Option<&Path>,
) -> Result<Option<DesktopShutdownControl>> {
    let Some(token_file) = token_file else {
        return Ok(None);
    };
    let canonical_data_dir = data_dir.canonicalize().with_context(|| {
        format!(
            "failed to canonicalize node data directory {} for desktop shutdown control",
            data_dir.display()
        )
    })?;
    let control_dir = canonical_data_dir.join(DESKTOP_SHUTDOWN_CONTROL_DIR_NAME);
    arc_crypto::secret_file::validate_private_directory(&control_dir).with_context(|| {
        format!(
            "desktop shutdown control directory is not private: {}",
            control_dir.display()
        )
    })?;
    let expected_token_file = control_dir.join(DESKTOP_SHUTDOWN_TOKEN_FILE_NAME);
    let canonical_token_file = token_file.canonicalize().with_context(|| {
        format!(
            "failed to canonicalize desktop shutdown token {}",
            token_file.display()
        )
    })?;
    ensure!(
        canonical_token_file == expected_token_file,
        "desktop shutdown token must be the exact {} file inside --data-dir",
        DESKTOP_SHUTDOWN_TOKEN_FILE_NAME
    );
    let mut token_file_handle = arc_crypto::secret_file::open_private(&canonical_token_file)
        .with_context(|| {
            format!(
                "failed to open private desktop shutdown token {}",
                canonical_token_file.display()
            )
        })?;
    ensure!(
        token_file_handle.metadata()?.len() <= DESKTOP_SHUTDOWN_FILE_MAX_BYTES,
        "desktop shutdown token exceeds its bounded size"
    );
    let mut token_text = Zeroizing::new(String::new());
    std::io::Read::by_ref(&mut token_file_handle)
        .take(DESKTOP_SHUTDOWN_FILE_MAX_BYTES + 1)
        .read_to_string(&mut token_text)
        .context("failed to read bounded desktop shutdown token")?;
    ensure!(
        token_text.len() as u64 <= DESKTOP_SHUTDOWN_FILE_MAX_BYTES,
        "desktop shutdown token exceeds its bounded size"
    );
    let trimmed = token_text.trim();
    ensure!(
        trimmed.len() == 64,
        "desktop shutdown token must contain exactly 32 hexadecimal bytes"
    );
    let decoded =
        Zeroizing::new(hex::decode(trimmed).context("desktop shutdown token is not hexadecimal")?);
    ensure!(
        decoded.len() == 32,
        "desktop shutdown token must contain exactly 32 bytes"
    );
    let mut expected_token = [0u8; 32];
    expected_token.copy_from_slice(&decoded);
    let genesis = genesis_file
        .ok_or_else(|| anyhow::anyhow!("desktop-managed nodes require an explicit genesis file"))?
        .canonicalize()
        .context("failed to canonicalize desktop-managed genesis file")?;
    let executable = std::env::current_exe()
        .context("failed to resolve the running node executable for shutdown receipt binding")?
        .canonicalize()
        .context("failed to canonicalize the running node executable")?;
    ensure!(
        arc_crypto::secret_file::load_desktop_shutdown_receipt_nonce(
            &canonical_data_dir,
            &expected_token,
            &executable,
            &genesis,
        )
        .context("failed to validate the supervisor's desktop shutdown receipt")?
        .is_some(),
        "desktop-managed node startup requires a prearmed durable shutdown receipt"
    );
    Ok(Some(DesktopShutdownControl {
        request_file: control_dir.join(DESKTOP_SHUTDOWN_REQUEST_FILE_NAME),
        expected_token,
        data_dir: canonical_data_dir,
        executable,
        genesis,
    }))
}

fn constant_time_token_eq(left: &[u8; 32], right: &[u8; 32]) -> bool {
    left.iter()
        .zip(right.iter())
        .fold(0u8, |difference, (left, right)| difference | (left ^ right))
        == 0
}

/// Consume one local request file and authenticate its exact 32-byte token and
/// target process. Every read is through the existing cross-platform private
/// no-follow handle boundary. A valid request for an older PID is stale and is
/// removed; malformed or unauthenticated input remains fail-closed.
fn take_authenticated_desktop_shutdown_request(
    control: &DesktopShutdownControl,
) -> Result<Option<DesktopShutdownReceiptIdentity>> {
    let mut request_file = match arc_crypto::secret_file::open_private(&control.request_file) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error).with_context(|| {
                format!(
                    "failed to open private desktop shutdown request {}",
                    control.request_file.display()
                )
            });
        }
    };
    // Parse only the stable snapshot behind this validated no-follow handle.
    // Once the handle exists, every bounded malformed/stale request is
    // consumed as a one-shot message. That makes a crash-torn legacy request
    // recoverable instead of permanently wedging future graceful stops.
    let parsed = (|| -> Result<Option<DesktopShutdownReceiptIdentity>> {
        ensure!(
            request_file.metadata()?.len() <= DESKTOP_SHUTDOWN_FILE_MAX_BYTES,
            "desktop shutdown request exceeds its bounded size"
        );
        let mut request_text = Zeroizing::new(String::new());
        std::io::Read::by_ref(&mut request_file)
            .take(DESKTOP_SHUTDOWN_FILE_MAX_BYTES + 1)
            .read_to_string(&mut request_text)
            .context("failed to read bounded desktop shutdown request")?;
        ensure!(
            request_text.len() as u64 <= DESKTOP_SHUTDOWN_FILE_MAX_BYTES,
            "desktop shutdown request exceeds its bounded size"
        );
        let mut lines = request_text.lines();
        ensure!(
            lines.next() == Some(DESKTOP_SHUTDOWN_REQUEST_SCHEMA),
            "desktop shutdown request has an invalid schema"
        );
        let target_pid = lines
            .next()
            .and_then(|line| line.strip_prefix("pid="))
            .ok_or_else(|| anyhow::anyhow!("desktop shutdown request omits its target PID"))?
            .parse::<u32>()
            .context("desktop shutdown request target PID is invalid")?;
        let token = lines
            .next()
            .and_then(|line| line.strip_prefix("token="))
            .ok_or_else(|| anyhow::anyhow!("desktop shutdown request omits its token"))?;
        let nonce = lines
            .next()
            .and_then(|line| line.strip_prefix("nonce="))
            .ok_or_else(|| anyhow::anyhow!("desktop shutdown request omits its receipt nonce"))?;
        ensure!(
            lines.next().is_none(),
            "desktop shutdown request contains trailing fields"
        );
        ensure!(
            token.len() == 64,
            "desktop shutdown request token has an invalid length"
        );
        let decoded = Zeroizing::new(
            hex::decode(token).context("desktop shutdown request token is not hex")?,
        );
        ensure!(
            decoded.len() == 32,
            "desktop shutdown request token has an invalid length"
        );
        let mut candidate = [0u8; 32];
        candidate.copy_from_slice(&decoded);
        let authenticated = constant_time_token_eq(&control.expected_token, &candidate);
        candidate.zeroize();
        ensure!(authenticated, "desktop shutdown request token is invalid");
        let nonce_decoded = Zeroizing::new(
            hex::decode(nonce).context("desktop shutdown request nonce is not hex")?,
        );
        ensure!(
            nonce_decoded.len() == 32,
            "desktop shutdown request nonce has an invalid length"
        );
        let mut receipt_nonce = [0u8; 32];
        receipt_nonce.copy_from_slice(&nonce_decoded);
        if target_pid != std::process::id() {
            receipt_nonce.zeroize();
            return Ok(None);
        }
        ensure!(
            arc_crypto::secret_file::validate_desktop_shutdown_receipt(
                &control.data_dir,
                &control.expected_token,
                &receipt_nonce,
                &control.executable,
                &control.genesis,
            )
            .context("desktop shutdown request does not bind a valid durable receipt")?,
            "desktop shutdown request has no armed durable receipt"
        );
        Ok(Some(DesktopShutdownReceiptIdentity {
            data_dir: control.data_dir.clone(),
            expected_token: control.expected_token,
            nonce: receipt_nonce,
            executable: control.executable.clone(),
            genesis: control.genesis.clone(),
        }))
    })();
    let removal =
        arc_crypto::secret_file::remove_private_while_open(&request_file, &control.request_file);
    drop(request_file);
    match removal {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(error).with_context(|| {
                format!(
                    "failed to consume private desktop shutdown request {}",
                    control.request_file.display()
                )
            });
        }
    }
    parsed
}

async fn wait_for_authenticated_desktop_shutdown(
    control: DesktopShutdownControl,
    mut http_shutdown: tokio::sync::watch::Receiver<bool>,
) -> Option<DesktopShutdownReceiptIdentity> {
    let mut poll = tokio::time::interval(std::time::Duration::from_millis(200));
    poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        if *http_shutdown.borrow() {
            return None;
        }
        tokio::select! {
            biased;
            changed = http_shutdown.changed() => {
                if changed.is_err() || *http_shutdown.borrow_and_update() {
                    return None;
                }
            }
            _ = poll.tick() => {
                match take_authenticated_desktop_shutdown_request(&control) {
                    Ok(Some(receipt)) => return Some(receipt),
                    Ok(None) => {}
                    Err(error) => {
                        tracing::warn!(%error, "ignored invalid desktop shutdown request");
                    }
                }
            }
        }
    }
}

/// Close HTTP and background-work admission from one synchronous signal edge.
///
/// HTTP handlers that Axum already accepted are not cancelled by this watch
/// notification; each server drains them before returning. Background loops
/// use the second notification to stop admitting compute immediately. The
/// separate transport/consensus signal remains open until both HTTP servers
/// have drained, preserving the dependencies of already-accepted writes.
fn broadcast_node_shutdown(
    shutdown_requested: &std::sync::atomic::AtomicBool,
    http_shutdown: &tokio::sync::watch::Sender<bool>,
    background_admission_shutdown: &tokio::sync::watch::Sender<bool>,
) {
    shutdown_requested.store(true, std::sync::atomic::Ordering::SeqCst);
    let _ = background_admission_shutdown.send(true);
    let _ = http_shutdown.send(true);
}

fn complete_startup_shutdown_if_requested(
    shutdown_requested: &std::sync::atomic::AtomicBool,
    state: Option<&StateDB>,
    desktop_shutdown_receipt: &std::sync::Mutex<Option<DesktopShutdownReceiptIdentity>>,
) -> Result<bool> {
    if !shutdown_requested.load(std::sync::atomic::Ordering::SeqCst) {
        return Ok(false);
    }
    let authenticated_desktop_shutdown = desktop_shutdown_receipt
        .lock()
        .map_err(|_| anyhow::anyhow!("desktop shutdown receipt lock was poisoned"))?
        .is_some();
    if state.is_none() && authenticated_desktop_shutdown {
        // A recovery receipt can be cleared only after StateDB has opened and
        // replayed. Keep advancing initialization with admission already
        // closed until a state-aware barrier can run.
        return Ok(false);
    }
    if let Some(state) = state {
        state
            .try_sync_wal()
            .context("startup shutdown WAL durability barrier failed")?;
        if let Some(receipt) = desktop_shutdown_receipt
            .lock()
            .map_err(|_| anyhow::anyhow!("desktop shutdown receipt lock was poisoned"))?
            .take()
        {
            // A receipt inherited from a prior crash/nonzero exit is a
            // recovery fence. It may be acknowledged during startup only
            // after StateDB has opened and replayed and its WAL barrier has
            // succeeded. The state=None path below deliberately leaves it
            // armed so an early exit cannot launder a failed prior shutdown.
            receipt.acknowledge()?;
        }
        tracing::info!(
            "shutdown requested during initialization; persistent state is durable and later runtime stages were skipped"
        );
    } else {
        tracing::info!(
            "shutdown requested before persistent state opened; later initialization stages were skipped"
        );
    }
    Ok(true)
}

// Keep one scheduler worker available for lifecycle admission while startup
// performs unavoidable synchronous hashing/recovery/model work on the main
// future. A one-vCPU host must still observe SIGTERM or the authenticated
// desktop request within the managed shutdown budget.
fn p2p_listen_ip(
    p2p_port: u16,
    benchmark_mode: bool,
    insecure_dev_validator_seed: bool,
) -> std::net::Ipv4Addr {
    if p2p_port == 0 || benchmark_mode || insecure_dev_validator_seed {
        std::net::Ipv4Addr::LOCALHOST
    } else {
        std::net::Ipv4Addr::UNSPECIFIED
    }
}

#[tokio::main(flavor = "multi_thread", worker_threads = 2)]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env().add_directive("arc=info".parse()?))
        .init();

    let mut cli = Cli::parse();

    #[cfg(feature = "benchmark-tools")]
    if cli.benchmark && (cli.community || cli.community_mode) {
        bail!("--benchmark cannot be combined with community modes");
    }

    if let Some(command) = cli.operator_command.take() {
        run_operator_command(command).await?;
        return Ok(());
    }

    // Arm lifecycle capture before community auto-download, config work,
    // persistent recovery/replay, or model loading. The signal edge records
    // intent synchronously in an atomic and closes both admission channels;
    // each potentially long startup phase observes that intent before the
    // next phase. This prevents the OS default action from bypassing the WAL
    // barrier during initialization.
    let shutdown_requested = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
    let (background_admission_shutdown_tx, background_admission_shutdown_rx) =
        tokio::sync::watch::channel(false);
    let (runtime_shutdown_tx, runtime_shutdown_rx) = tokio::sync::watch::channel(false);
    let desktop_shutdown_receipt = Arc::new(std::sync::Mutex::new(
        None::<DesktopShutdownReceiptIdentity>,
    ));
    #[cfg(unix)]
    let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .context("failed to install SIGTERM handler before node initialization")?;
    {
        let shutdown_requested = shutdown_requested.clone();
        let shutdown_tx = shutdown_tx.clone();
        let background_admission_shutdown_tx = background_admission_shutdown_tx.clone();
        tokio::spawn(async move {
            match tokio::signal::ctrl_c().await {
                Ok(()) => {
                    tracing::info!(
                        "SIGINT received - stopping HTTP/background admission and draining active work"
                    );
                    broadcast_node_shutdown(
                        &shutdown_requested,
                        &shutdown_tx,
                        &background_admission_shutdown_tx,
                    );
                }
                Err(error) => tracing::warn!(%error, "failed to install SIGINT handler"),
            }
        });
    }
    #[cfg(unix)]
    {
        let shutdown_requested = shutdown_requested.clone();
        let shutdown_tx = shutdown_tx.clone();
        let background_admission_shutdown_tx = background_admission_shutdown_tx.clone();
        tokio::spawn(async move {
            sigterm.recv().await;
            tracing::info!(
                "SIGTERM received - stopping HTTP/background admission and draining active work"
            );
            broadcast_node_shutdown(
                &shutdown_requested,
                &shutdown_tx,
                &background_admission_shutdown_tx,
            );
        });
    }
    // ctrl_c installs its platform handler on first poll. Yield before any
    // synchronous startup phase so both signal listeners are polled and the
    // Unix stream above is already registered on the calling thread.
    tokio::task::yield_now().await;
    tracing::info!("lifecycle signal handlers armed before node initialization");
    let mut runtime_tasks = Vec::<tokio::task::JoinHandle<()>>::new();

    // ── --community: one-flag community-node setup ─────────────────────
    // Forces stake=0 + community_mode=true, and auto-discovers a local
    // GGUF if --model wasn't explicitly set. Lets home/residential
    // operators run `arc-node --community` with zero other flags.
    if cli.community {
        if cli.stake != 0 {
            tracing::info!(
                "--community: overriding --stake {} to 0 (community workers are stake-0)",
                cli.stake
            );
            cli.stake = 0;
        }
        cli.community_mode = true;
    }
    if (cli.community || cli.community_mode || cli.stake == 0) && cli.model.is_none() {
        cli.model = auto_discover_model();
        // If --community was explicit and nothing was found on disk, auto-download
        // the sha-pinned Llama-2-7B Q4_K_M GGUF from HuggingFace. Sha-mismatch or
        // any failure falls through to None and we print the manual instructions.
        if cli.model.is_none() && cli.community {
            cli.model = auto_download_model(&shutdown_requested).await;
        }
        match &cli.model {
            Some(p) => tracing::info!("community mode: model at {}", p),
            None => {
                tracing::warn!(
                    "community mode: no GGUF model found and auto-download did not succeed."
                );
                tracing::warn!("  Place a Llama-2-7B Q4_K_M GGUF (~4.08 GB) at one of:");
                tracing::warn!("    ./llama2-7b.gguf");
                tracing::warn!("    $HOME/.arc-models/llama2-7b.gguf");
                tracing::warn!("    /opt/arc/llama2-7b.gguf");
                tracing::warn!(
                    "  Expected sha256: 08a5566d61d7cb6b420c3e4387a39e0078e1f2fe5f055f3a03887385304d4bfa"
                );
                tracing::warn!(
                    "  Manual download: huggingface-cli download TheBloke/Llama-2-7B-Chat-GGUF llama-2-7b-chat.Q4_K_M.gguf --local-dir $HOME/.arc-models"
                );
                tracing::warn!(
                    "  Then verify the expected SHA-256 and move llama-2-7b-chat.Q4_K_M.gguf to $HOME/.arc-models/llama2-7b.gguf"
                );
                tracing::warn!(
                    "  Continuing in community routing mode (registered with seeds, no local inference)."
                );
            }
        }
    }
    if complete_startup_shutdown_if_requested(&shutdown_requested, None, &desktop_shutdown_receipt)?
    {
        return Ok(());
    }

    // ── Load config file and merge with CLI args ────────────────────────
    // Priority: explicit CLI arg > config file value > hardcoded default.
    // We use clap's ArgMatches to detect which args were explicitly provided.
    let matches = Cli::command().get_matches_from(std::env::args_os());

    let mut node_cfg = if let Some(config_path) = &cli.config {
        let cfg = config::load_config(config_path)?;
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
    #[cfg(unix)]
    let rpc_listener = if let Some(path) = &cli.rpc_unix {
        rpc::RpcListen::Unix(path.clone())
    } else {
        rpc::RpcListen::Tcp(rpc_addr.clone())
    };
    #[cfg(not(unix))]
    let rpc_listener = if cli.rpc_unix.is_some() {
        anyhow::bail!("--rpc-unix requires a Unix host")
    } else {
        rpc::RpcListen::Tcp(rpc_addr.clone())
    };

    let p2p_port =
        if matches.value_source("p2p_port") == Some(clap::parser::ValueSource::CommandLine) {
            cli.p2p_port
        } else {
            node_cfg.p2p.port
        };

    // --community is treated as an explicit CLI override of stake (the post-
    // parse block forces cli.stake = 0 for that flag). Without this short-
    // circuit, matches.value_source() — which only reflects what was on the
    // actual command line, not later mutations — would fall through to
    // node_cfg.validator.stake (5M by default) and silently restore a
    // validator stake on a node the operator explicitly asked to be a
    // stake-0 community worker.
    let stake = if cli.community
        || matches.value_source("stake") == Some(clap::parser::ValueSource::CommandLine)
    {
        cli.stake
    } else {
        node_cfg.validator.stake
    };

    let data_dir =
        if matches.value_source("data_dir") == Some(clap::parser::ValueSource::CommandLine) {
            cli.data_dir.clone()
        } else {
            node_cfg.storage.data_dir.clone()
        };
    let _data_dir_lock = acquire_node_data_dir_lock(Path::new(&data_dir))?;
    let desktop_shutdown_control = prepare_desktop_shutdown_control(
        Path::new(&data_dir),
        cli.desktop_shutdown_token_file.as_deref(),
        cli.genesis.as_deref().map(Path::new),
    )?;
    if let Some(control) = desktop_shutdown_control {
        let shutdown_requested = shutdown_requested.clone();
        let shutdown_tx = shutdown_tx.clone();
        let background_admission_shutdown_tx = background_admission_shutdown_tx.clone();
        let http_shutdown = shutdown_rx.clone();
        let receipt_slot = desktop_shutdown_receipt.clone();
        runtime_tasks.push(tokio::spawn(async move {
            if let Some(receipt) =
                wait_for_authenticated_desktop_shutdown(control, http_shutdown).await
            {
                match receipt_slot.lock() {
                    Ok(mut slot) => *slot = Some(receipt),
                    Err(_) => {
                        tracing::error!("desktop shutdown receipt lock was poisoned");
                        return;
                    }
                }
                tracing::info!(
                    "authenticated local desktop shutdown requested - stopping HTTP/background admission and draining active work"
                );
                broadcast_node_shutdown(
                    &shutdown_requested,
                    &shutdown_tx,
                    &background_admission_shutdown_tx,
                );
            }
        }));
        // The first interval tick is immediate. Poll the watcher once before
        // persistent recovery or model work can monopolize this future.
        tokio::task::yield_now().await;
        tracing::info!("authenticated desktop shutdown watcher armed before persistent state");
    }
    if complete_startup_shutdown_if_requested(&shutdown_requested, None, &desktop_shutdown_receipt)?
    {
        return Ok(());
    }

    let min_stake =
        if matches.value_source("min_stake") == Some(clap::parser::ValueSource::CommandLine) {
            cli.min_stake
        } else {
            node_cfg.validator.min_stake
        };

    // Identity precedence: CLI / environment, then node config. Production
    // staked nodes are allowed only the keyfile path; resolve_identity below
    // rejects every seed-bearing production configuration.
    let validator_key_file = match matches.value_source("validator_key_file") {
        Some(clap::parser::ValueSource::CommandLine)
        | Some(clap::parser::ValueSource::EnvVariable) => cli.validator_key_file.clone(),
        _ => node_cfg.validator.key_file.clone(),
    };
    let mut validator_seed = match matches.value_source("validator_seed") {
        Some(clap::parser::ValueSource::CommandLine)
        | Some(clap::parser::ValueSource::EnvVariable) => cli.validator_seed.clone(),
        _ => node_cfg.validator.seed.clone(),
    };

    let eth_rpc_port =
        if matches.value_source("eth_rpc_port") == Some(clap::parser::ValueSource::CommandLine) {
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

    // Community/reward RPC trust is explicit and independent of P2P. A QUIC
    // peer address says nothing about an HTTP port, TLS certificate, gateway,
    // or reverse-proxy origin, so never synthesize one from `peers`.
    let raw_community_rpc_urls = if !cli.community_rpc_urls.is_empty() {
        cli.community_rpc_urls.clone()
    } else {
        node_cfg.community.rpc_urls.clone()
    };
    let benchmark_mode = benchmark_mode_enabled(&cli);
    #[cfg(feature = "benchmark-tools")]
    validate_benchmark_runtime(&cli, &rpc_addr, &peers, &raw_community_rpc_urls, stake)?;
    let allow_ephemeral_observer = validate_identity_runtime(
        &cli,
        &rpc_addr,
        &peers,
        &raw_community_rpc_urls,
        validator_key_file.is_some(),
        validator_seed.is_some(),
        stake,
    )?;
    let allow_insecure_community_rpc = if matches.value_source("allow_insecure_community_rpc")
        == Some(clap::parser::ValueSource::CommandLine)
    {
        cli.allow_insecure_community_rpc
    } else {
        node_cfg.community.allow_insecure_remote_http
    };
    let community_rpc_bases =
        rpc::validate_community_rpc_bases(&raw_community_rpc_urls, allow_insecure_community_rpc)
            .map_err(anyhow::Error::msg)
            .context("invalid community RPC configuration")?;
    validate_full_integer_worker_role(&cli, stake, &community_rpc_bases)?;
    let mut raw_coordinator_rpc_bases = community_rpc_bases.clone();
    for shard_host in &cli.shard_hosts {
        let shard_host = shard_host.trim();
        if shard_host.is_empty() {
            continue;
        }
        raw_coordinator_rpc_bases.push(if shard_host.contains("://") {
            shard_host.to_string()
        } else if shard_host.contains(':') {
            format!("http://{shard_host}")
        } else {
            format!("http://{shard_host}:9090")
        });
    }
    let coordinator_rpc_bases =
        rpc::validate_community_rpc_bases(&raw_coordinator_rpc_bases, allow_insecure_community_rpc)
            .map_err(anyhow::Error::msg)
            .context("invalid coordinator/shard RPC configuration")?;
    if allow_insecure_community_rpc {
        tracing::warn!(
            "INSECURE DEV OVERRIDE: remote plaintext community RPC is allowed; never use this on a public or value-bearing network"
        );
    }
    if cli.enable_community_rewards_v1
        && community_rpc_bases.len() < arc_types::transaction::COMMUNITY_REWARD_APPROVALS_REQUIRED
    {
        bail!(
            "--enable-community-rewards-v1 requires at least five explicit --community-rpc-url HTTPS origins for five-of-six approval collection"
        );
    }

    // Benchmark settings exist only in explicitly opted-in tool builds.
    #[cfg(feature = "benchmark-tools")]
    let _bench_batch =
        if matches.value_source("bench_batch") == Some(clap::parser::ValueSource::CommandLine) {
            cli.bench_batch
        } else {
            node_cfg.benchmark.batch_size
        };

    #[cfg(feature = "benchmark-tools")]
    let _bench_interval =
        if matches.value_source("bench_interval") == Some(clap::parser::ValueSource::CommandLine) {
            cli.bench_interval
        } else {
            node_cfg.benchmark.interval_ms
        };

    #[cfg(feature = "benchmark-tools")]
    let bench_sender_start = if matches.value_source("bench_sender_start")
        == Some(clap::parser::ValueSource::CommandLine)
    {
        cli.bench_sender_start
    } else {
        node_cfg.benchmark.sender_start
    };

    #[cfg(feature = "benchmark-tools")]
    let bench_sender_count = if matches.value_source("bench_sender_count")
        == Some(clap::parser::ValueSource::CommandLine)
    {
        cli.bench_sender_count
    } else {
        node_cfg.benchmark.sender_count
    };

    #[cfg(feature = "benchmark-tools")]
    let bench_sign_threads = if matches.value_source("bench_sign_threads")
        == Some(clap::parser::ValueSource::CommandLine)
    {
        cli.bench_sign_threads
    } else {
        node_cfg.benchmark.sign_threads
    };

    #[cfg(feature = "benchmark-tools")]
    let bench_rayon_threads = if matches.value_source("bench_rayon_threads")
        == Some(clap::parser::ValueSource::CommandLine)
    {
        cli.bench_rayon_threads
    } else {
        node_cfg.benchmark.rayon_threads
    };

    // ── Configure rayon thread pool ─────────────────────────────────────
    // In benchmark mode, limit rayon to leave CPU for signing threads
    #[cfg(feature = "benchmark-tools")]
    if benchmark_mode {
        rayon::ThreadPoolBuilder::new()
            .num_threads(bench_rayon_threads)
            .build_global()
            .ok();
    }

    // ── Validate stake ──────────────────────────────────────────────────
    // Community-mode workers are stake-0 by definition (no slashing, no
    // consensus role) — they register via /community/register and route
    // inference via the seed dispatch. The min_stake gate is for actual
    // validators, so we bypass it whenever the node is in community-mode-
    // equivalent state (--community, --community-mode, or --stake 0).
    // The existing community_mode auto-derive (line ~1188) already treats
    // stake==0 as community, so this is just the same intent applied
    // earlier in the boot flow.
    let community_mode_intent = cli.community || cli.community_mode || stake == 0;
    if !community_mode_intent && stake < min_stake {
        eprintln!(
            "Error: stake {} ARC is below the minimum required {} ARC",
            stake, min_stake
        );
        std::process::exit(1);
    }

    // ── Fail-closed genesis + validator identity preflight ─────────────
    // This runs before any validator joins P2P, advertises stake, or asks a
    // remote seed for a shard assignment.
    let genesis_cfg = cli
        .genesis
        .as_deref()
        .map(config::load_genesis)
        .transpose()
        .context("validator startup cannot continue with the configured genesis")?;
    let migration_observer =
        is_genesis_migration_observer(genesis_cfg.as_ref(), stake, cli.insecure_dev_validator_seed);
    let (mut genesis_validators, genesis_hash) = match genesis_cfg.as_ref() {
        Some(genesis) if migration_observer => {
            (Vec::new(), genesis.migration_observer_network_hash()?)
        }
        Some(genesis) => (
            genesis.validated_validator_set(cli.insecure_dev_validator_seed)?,
            genesis.network_hash(cli.insecure_dev_validator_seed)?,
        ),
        None => (Vec::new(), Block::genesis().hash),
    };
    let chain_participation_enabled =
        chain_participation_allowed(stake, migration_observer, cli.insecure_dev_validator_seed);
    if migration_observer {
        tracing::warn!("╔══════════════════════════════════════════════════════════════╗");
        tracing::warn!("║  VALIDATOR-SET MIGRATION PENDING: COMMUNITY OBSERVER MODE   ║");
        tracing::warn!("╚══════════════════════════════════════════════════════════════╝");
        tracing::warn!(
            "Stake-zero community inference remains enabled, but chain P2P, consensus, and on-chain voting are disabled until genesis contains the approved public validator addresses and validator_set_complete = true."
        );
    }

    let identity = validator_identity::resolve_identity(
        validator_key_file.as_deref().map(Path::new),
        validator_seed.as_deref(),
        stake,
        cli.insecure_dev_validator_seed,
        allow_ephemeral_observer,
    )
    .context("failed to establish validator signing identity")?;
    let validator_address = identity.keypair.address();
    let identity_source = identity.source;
    let validator_keypair = identity.keypair;

    if stake > 0 {
        match genesis_cfg.as_ref() {
            Some(genesis) => {
                if genesis.chain.validator_set_complete
                    && identity_source != validator_identity::IdentitySource::Keyfile
                {
                    bail!(
                        "a complete/production genesis requires a mode-0600 Ed25519 validator keyfile; insecure seed-derived identities are forbidden even when the dev flag is present"
                    );
                }
                config::verify_staked_identity(&genesis_validators, validator_address, stake)?;
            }
            None if cli.insecure_dev_validator_seed => {
                // Preserve single-node development ergonomics while keeping
                // the bypass unmistakable and impossible without the flag.
                genesis_validators.push((validator_address, stake));
                config::verify_staked_identity(&genesis_validators, validator_address, stake)?;
            }
            None => bail!(
                "staked validators require --genesis <path> with validator_set_complete = true and an entry matching the keyfile's public address and configured stake"
            ),
        }
    }

    // Best-effort removal of copied legacy seed material after derivation.
    if let Some(seed) = validator_seed.as_mut() {
        seed.zeroize();
    }
    if let Some(seed) = cli.validator_seed.as_mut() {
        seed.zeroize();
    }
    if let Some(seed) = node_cfg.validator.seed.as_mut() {
        seed.zeroize();
    }

    if cli.insecure_dev_validator_seed {
        tracing::warn!("╔══════════════════════════════════════════════════════════════╗");
        tracing::warn!("║  INSECURE DISPOSABLE DEVELOPMENT VALIDATOR MODE             ║");
        tracing::warn!("║  NEVER USE THIS MODE ON A PUBLIC OR VALUE-BEARING NETWORK   ║");
        tracing::warn!("╚══════════════════════════════════════════════════════════════╝");
    }

    // Give the already-armed lifecycle listeners one final scheduling edge
    // before artifact hashing can synchronously occupy this startup future.
    tokio::task::yield_now().await;
    if complete_startup_shutdown_if_requested(&shutdown_requested, None, &desktop_shutdown_receipt)?
    {
        return Ok(());
    }

    // Establish one exact model identity before any shard registration or
    // inference-capability advertisement. Failure to open/read --model is a
    // startup error: proceeding with a guessed identity would mix weights.
    let model_artifact = cli
        .model
        .as_deref()
        .map(arc_inference::model_artifact::ModelArtifactCommitment::from_path)
        .transpose()
        .context("cannot establish the exact --model artifact commitment")?;
    let model_artifact_id = model_artifact.as_ref().map(|artifact| artifact.model_id());

    // Ask for a shard only after the signing identity and genesis membership
    // have passed validation, so a misconfigured validator performs no remote
    // mutation before failing startup.
    if cli.auto_shard_join
        && stake > 0
        && cli.model.is_some()
        && cli.shard_ranges.is_empty()
        && cli.shard_start.is_none()
        && cli.shard_end.is_none()
    {
        let advertised_socket = advertised_shard_rpc_origin(&cli, &rpc_addr)?;
        match auto_shard_join(
            &cli,
            coordinator_rpc_bases
                .first()
                .map(String::as_str)
                .unwrap_or(""),
            &advertised_socket,
            model_artifact_id.expect("--model commitment established above"),
        )
        .await
        {
            Some((start, end)) => {
                tracing::info!(
                    "auto-shard: seed assigned this validator layers [{}, {}) — loading shard",
                    start,
                    end
                );
                cli.shard_ranges.push(format!("{}:{}", start, end));
            }
            None => {
                tracing::info!(
                    "auto-shard: no assignment received; loading FULL model. \
                     Pass --shard-range a:b to override, or check seed connectivity."
                );
            }
        }
    }

    // ── Loud warning: local stake must match approved membership ────────
    // Remote peers ignore self-reported stake for consensus membership and
    // voting power. A nonzero value is meaningful only when the local
    // identity and stake are present in the approved genesis/checkpoint.
    if stake > 0 && !peers.is_empty() {
        let public_peers: Vec<&String> = peers
            .iter()
            .filter(|p| {
                let host = p.split(':').next().unwrap_or("");
                !(host.starts_with("127.")
                    || host.starts_with("10.")
                    || host.starts_with("192.168.")
                    || host == "localhost"
                    || host.is_empty())
            })
            .collect();
        if !public_peers.is_empty() {
            tracing::warn!("╔══════════════════════════════════════════════════════════════╗");
            tracing::warn!("║  NONZERO STAKE AGAINST A PUBLIC NETWORK                     ║");
            tracing::warn!("╚══════════════════════════════════════════════════════════════╝");
            tracing::warn!(
                "  --stake {} against {} non-local peer(s).",
                stake,
                public_peers.len()
            );
            tracing::warn!("  This value does not grant validator membership or voting power.");
            tracing::warn!("  Consensus uses only the approved genesis/checkpoint set.");
            tracing::warn!("  A configured validator must use the exact approved stake.");
            tracing::warn!("  If you meant to contribute compute, not consensus: use --community");
            tracing::warn!("  (or --stake 0). Continuing in 5s...");
            std::thread::sleep(std::time::Duration::from_secs(5));
        }
    }

    // ── Determine stake tier for display ───────────────────────────────
    let tier = arc_consensus::StakeTier::from_stake(stake)
        .map(|t| format!("{:?}", t))
        .unwrap_or_else(|| "Below minimum".to_string());

    tracing::info!("╔═══════════════════════════════════════╗");
    tracing::info!("║   ARC Chain - Agent Runtime Chain     ║");
    tracing::info!("║   ARC Node v{:<26}║", env!("CARGO_PKG_VERSION"));
    tracing::info!("╚═══════════════════════════════════════╝");
    tracing::info!("Validator  : {}", validator_address);
    tracing::info!(
        "Identity   : {}",
        match identity_source {
            validator_identity::IdentitySource::Keyfile => "Ed25519 keyfile",
            validator_identity::IdentitySource::InsecureDevelopmentSeed => {
                "INSECURE development seed"
            }
            validator_identity::IdentitySource::EphemeralLoopbackObserver => {
                "ephemeral loopback observer (changes on restart)"
            }
        }
    );
    tracing::info!("Stake      : {} ARC ({})", stake, tier);
    match &rpc_listener {
        rpc::RpcListen::Tcp(address) => tracing::info!("RPC        : {}", address),
        #[cfg(unix)]
        rpc::RpcListen::Unix(path) => tracing::info!("RPC Unix   : {}", path.display()),
    }
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

    // ── Genesis accounts - prefunded for testing ────────────────────────
    // Priority: --genesis file > hardcoded defaults.
    // In benchmark mode (without --genesis), use deterministic ed25519
    // keypair-derived addresses so signatures can be verified.
    // Chain identity as DECLARED by the genesis file. Only a genesis file can
    // name the network, so a node started without --genesis carries None here
    // and GET /network/info reports the name and chain_id as null with a
    // reason rather than inventing one (and never says "mainnet").
    let genesis_chain_identity: Option<rpc::ChainIdentity> =
        genesis_cfg.as_ref().map(|cfg| rpc::ChainIdentity {
            name: cfg.chain.name.clone(),
            chain_id: cfg.chain.chain_id.clone(),
        });

    let genesis_accounts: Vec<(Hash256, u64)> = if let Some(genesis_cfg) = genesis_cfg.as_ref() {
        tracing::info!(
            "Genesis: {} ({} accounts, {} validators)",
            genesis_cfg.chain.name,
            genesis_cfg.accounts.len(),
            genesis_cfg.validators.len(),
        );
        genesis_cfg
            .accounts
            .iter()
            .map(|a| {
                let mut bytes = [0u8; 32];
                hex::decode_to_slice(&a.address, &mut bytes).unwrap_or_else(|e| {
                    eprintln!("Invalid genesis account address '{}': {}", a.address, e);
                    std::process::exit(1);
                });
                (Hash256(bytes), a.balance)
            })
            .collect()
    } else if benchmark_mode {
        #[cfg(feature = "benchmark-tools")]
        {
            // Benchmark mode: deterministic ed25519 keypair-derived addresses.
            // This code and its predictable keys do not exist in default builds.
            (0..100u8)
                .map(|i| (arc_crypto::benchmark_address(i), 1_000_000_000_000))
                .collect()
        }
        #[cfg(not(feature = "benchmark-tools"))]
        {
            bail!("benchmark mode is unavailable in this default build")
        }
    } else {
        // Default: blake3-hashed addresses for testing
        (0..100u8)
            .map(|i| (hash_bytes(&[i]), 1_000_000_000_000))
            .collect()
    };

    if complete_startup_shutdown_if_requested(&shutdown_requested, None, &desktop_shutdown_receipt)?
    {
        return Ok(());
    }
    let state = Arc::new({
        let reward_activation_height = genesis_cfg
            .as_ref()
            .and_then(|config| config.chain.community_rewards_v1_activation_height);
        if cli.recovery_checkpoint.is_some() && genesis_cfg.is_none() {
            bail!(
                "--recovery-checkpoint requires --genesis with the complete approved validator set"
            );
        }
        if cli.recovery_checkpoint.is_none() && cli.approved_recovery_manifest_hash.is_some() {
            bail!(
                "--approved-recovery-manifest-hash is valid only with --recovery-checkpoint; an already-active data directory reads its pinned recovery.active marker"
            );
        }
        let recovery_import = match (
            cli.recovery_checkpoint.as_ref(),
            cli.approved_recovery_manifest_hash.as_deref(),
        ) {
            (Some(path), Some(approved)) => Some(arc_state::recovery::RecoveryImport {
                checkpoint_path: path.into(),
                approved_manifest_hash: Hash256::from_hex(approved).map_err(|_| {
                    anyhow::anyhow!(
                        "--approved-recovery-manifest-hash must be exactly 32 bytes of hex"
                    )
                })?,
            }),
            (Some(_), None) => bail!(
                "--recovery-checkpoint requires --approved-recovery-manifest-hash from the out-of-band GO decision"
            ),
            (None, _) => None,
        };
        let recovery_network = arc_state::recovery::RecoveryNetworkPolicy {
            chain_id: genesis_cfg
                .as_ref()
                .map(|config| config.chain.chain_id.clone())
                .unwrap_or_else(|| "0x415243".to_string()),
            genesis_hash,
            recovery_epoch: cli.recovery_epoch,
            validator_set_id: cli.validator_set_id,
            validators: genesis_validators.clone(),
            community_rewards_v1_activation_height: reward_activation_height,
        };
        let mut db = StateDB::with_genesis_persistent_recovery(
            &genesis_accounts,
            &data_dir,
            recovery_network,
            recovery_import,
        )
        .context("failed to initialize genesis/recovery-bound persistent state")?;
        db.set_community_rewards_v1_activation_height(reward_activation_height);
        match reward_activation_height {
            Some(height) => tracing::info!(
                height,
                "Community reward v1 consensus activation is committed by genesis"
            ),
            None => tracing::info!(
                "Community reward v1 consensus activation is absent; tx 0x25 is disabled"
            ),
        }
        if let Some(context) = db.recovery_context() {
            let manifest_hash = db
                .recovery_manifest_hash()
                .expect("recovery context always has a manifest hash");
            tracing::info!(
                recovery_epoch = context.recovery_epoch,
                validator_set_id = context.validator_set_id,
                checkpoint = %manifest_hash,
                transition_height = db.height(),
                "Quorum-certified ARCCHKPT recovery is active"
            );
        }
        if cli.archive {
            db.archive_mode = true;
            tracing::info!("Archive mode ENABLED - no pruning, full transaction history retained");
        }
        // Legacy state still needs its configured validator seed. Recovered
        // state already contains the exact H+1 validator map plus any later
        // WAL changes; preserve it and fail if replay no longer matches the
        // canonical block root.
        prepare_replayed_consensus_state(&db, &genesis_validators)?;
        db
    });

    // Rebuild the in-memory Tier 1 pending-request index from on-disk
    // escrow state. `tier1_pending` has no WAL op of its own; after a
    // restart it starts empty even though the OPEN/VOTING escrows survive
    // in the account map. Without this, the InferenceValidatorTask wakes
    // up unable to see any outstanding requests and never finalizes them.
    // The index uses `tier1.request_id` storage entries written by
    // InferenceRequest.apply — escrows applied before that storage entry
    // existed (pre-2026-06-04) can't be recovered automatically and stay
    // stuck.
    rebuild_replayed_derived_indexes(&state);
    if complete_startup_shutdown_if_requested(
        &shutdown_requested,
        Some(state.as_ref()),
        &desktop_shutdown_receipt,
    )? {
        return Ok(());
    }

    // ── State Sync Protocol (A5) - bootstrap from peer snapshot ─────
    // Auto-sync: if this node has peers configured and state is fresh (height 0),
    // automatically sync state from the first reachable peer. This allows new
    // nodes to join an existing network without manual --sync-from.
    let sync_peer = if cli.sync_from.is_some() {
        cli.sync_from.clone()
    } else if state.height() == 0 && !peers.is_empty() {
        // Try each peer until one responds
        // Quick check - try first 3 peers with 1s timeout each.
        // Don't block startup for unreachable peers.
        let mut found = None;
        for peer_addr in peers.iter().take(3) {
            let peer_rpc = peer_addr.replace(":9091", ":9090");
            let url = format!("http://{}/health", peer_rpc);
            match reqwest::Client::new()
                .get(&url)
                .timeout(std::time::Duration::from_secs(1))
                .send()
                .await
            {
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

    if state.recovery_context().is_some() && sync_peer.is_some() {
        bail!(
            "legacy peer snapshot sync is forbidden after ARCCHKPT activation; recover only from the pinned local checkpoint plus its verified WAL"
        );
    }

    if let Some(peer) = &sync_peer {
        tracing::info!("Bootstrapping from peer: {}", peer);

        let sync_mgr = arc_node::state_sync::StateSyncManager::new();
        let mut startup_shutdown = Some(shutdown_rx.clone());
        let sync_result = tokio::select! {
            biased;
            _ = wait_for_optional_runtime_shutdown(&mut startup_shutdown) => None,
            result = sync_mgr.sync_from_peer(peer, &state) => Some(result),
        };
        match sync_result {
            None => {
                tracing::info!("State sync cancelled by startup shutdown request");
            }
            Some(Ok(height)) => {
                tracing::info!("State sync complete, height = {}", height);
            }
            Some(Err(e)) => {
                tracing::warn!(
                    "Sync from peer failed ({}), continuing from genesis state",
                    e
                );
                // Don't crash - the node will start from genesis and catch
                // up via DAG consensus. This is fine for testnet.
            }
        }
    }
    if complete_startup_shutdown_if_requested(
        &shutdown_requested,
        Some(state.as_ref()),
        &desktop_shutdown_receipt,
    )? {
        return Ok(());
    }

    let mempool = Arc::new(Mempool::new(10_000_000));

    // ── Initialize candle float backend FIRST (for coherent inference) ──────
    // For GGUF files, load candle FIRST (lightweight Q4), then load tokenizer-only
    // from the same GGUF. This avoids loading 7GB INT8 weights on 8GB nodes.
    //
    // EXCEPTION: shard holders (--shard-start / --shard-end set) do NOT load
    // candle. Candle loads the FULL Q4 model (~4 GB on Llama-7B), and a shard
    // holder never runs /inference/run - only /inference/forward_shard - so
    // that 4 GB is pure waste. On 8 GB VPS this pushes the process into swap
    // and destroys forward_shard latency (observed: 20+ seconds/token on
    // swapping NYC). Disabling candle when the node is a shard-only role
    // keeps the RSS under 4 GB and makes the integer path run at real speed.
    // Parse --shard-range entries ("START:END") into a sorted Vec<(usize, usize)>.
    // Fall back to the deprecated single --shard-start/--shard-end pair so
    // existing launch scripts keep working through the rolling upgrade.
    let mut held_ranges: Vec<(usize, usize)> = Vec::new();
    for raw in &cli.shard_ranges {
        let (s, e) = raw
            .split_once(':')
            .ok_or_else(|| anyhow::anyhow!("--shard-range must be START:END, got {raw:?}"))?;
        let start: usize = s.trim().parse().map_err(|_| {
            anyhow::anyhow!("--shard-range START must be a non-negative integer, got {s:?}")
        })?;
        let end: usize = e.trim().parse().map_err(|_| {
            anyhow::anyhow!("--shard-range END must be a non-negative integer, got {e:?}")
        })?;
        if start >= end {
            return Err(anyhow::anyhow!(
                "--shard-range START ({start}) must be strictly less than END ({end})"
            ));
        }
        held_ranges.push((start, end));
    }
    if held_ranges.is_empty()
        && let (Some(start), Some(end)) = (cli.shard_start, cli.shard_end)
    {
        held_ranges.push((start, end));
    }
    held_ranges.sort();
    for i in 1..held_ranges.len() {
        if held_ranges[i].0 < held_ranges[i - 1].1 {
            return Err(anyhow::anyhow!(
                "--shard-range entries overlap: [{}, {}) and [{}, {})",
                held_ranges[i - 1].0,
                held_ranges[i - 1].1,
                held_ranges[i].0,
                held_ranges[i].1
            ));
        }
    }

    let is_shard_holder = !held_ranges.is_empty();
    ensure!(
        !(is_shard_holder && cli.enable_i16),
        "--enable-i16 is local/nonreward-only and cannot be combined with validator shard ranges; protocol-v3 shard execution is pinned to canonical per-row INT8"
    );
    if complete_startup_shutdown_if_requested(
        &shutdown_requested,
        Some(state.as_ref()),
        &desktop_shutdown_receipt,
    )? {
        return Ok(());
    }
    let (candle_engine, candle_model_id): (
        Option<Arc<arc_inference::candle_backend::GgufEngine>>,
        Option<arc_crypto::Hash256>,
    ) = if is_shard_holder || cli.full_integer_worker {
        if cli.full_integer_worker {
            tracing::info!(
                "Full integer community-worker mode - candle backend SKIPPED; validator shard advertisement remains disabled"
            );
        } else {
            tracing::info!("Shard holder mode - candle backend SKIPPED to save ~4 GB RAM");
        }
        (None, None)
    } else if let Some(model_path) = &cli.model {
        if !model_path.ends_with(".arc-int8") {
            let engine = Arc::new(arc_inference::candle_backend::GgufEngine::new(120_000));
            let artifact = model_artifact
                .as_ref()
                .expect("--model commitment established before model loading");
            match engine.load_gguf_artifact(artifact) {
                Ok(mid) => {
                    tracing::info!("Candle float inference ENABLED (Q4 GGUF)");
                    (Some(engine), Some(mid))
                }
                Err(e) => {
                    tracing::warn!("Candle backend failed: {} - falling back to INT8", e);
                    (None, None)
                }
            }
        } else {
            (None, None) // .arc-int8 files use integer engine only
        }
    } else {
        (None, None)
    };

    // ── Load the integer model or tokenizer from the committed artifact ──
    // Model configuration and tokenization are part of model identity just as
    // much as transformer weights. Never substitute a nearby "small tokenizer"
    // artifact here: doing so would advertise the `--model` content hash while
    // actually tokenizing with unrelated bytes.
    let inference_model: Option<Arc<arc_inference::cached_integer_model::CachedIntegerModel>> =
        if let Some(model_path) = &cli.model {
            tracing::info!("Loading model from {}...", model_path);
            let load_start = Instant::now();
            let load_result = if cli.tokenizer_only || candle_engine.is_some() {
                tracing::info!("TOKENIZER-ONLY MODE: loading vocab + config, no weights (~30MB)");
                arc_inference::cached_integer_model::load_tokenizer_only(model_path)
            } else if model_path.ends_with(".arc-int8") {
                arc_inference::cached_integer_model::load_cached_model_binary(model_path)
            } else if !held_ranges.is_empty() {
                let summary: Vec<String> = held_ranges
                    .iter()
                    .map(|(s, e)| format!("[{s}, {e})"))
                    .collect();
                tracing::info!("SHARD MODE: loading ranges {}", summary.join(", "));
                arc_inference::cached_integer_model::load_cached_model_ranges(
                    model_path,
                    &held_ranges,
                )
            } else if cli.full_integer_worker {
                arc_inference::cached_integer_model::load_cached_model_canonical_i8(model_path)
            } else {
                arc_inference::cached_integer_model::load_cached_model(model_path)
            };
            match load_result {
                Ok(mut model) => {
                    if cli.full_integer_worker {
                        model.enforce_canonical_i8_profile();
                        tracing::info!(
                            profile = arc_inference::cached_integer_model::CANONICAL_REWARD_INFERENCE_PROFILE,
                            "Pinned full community worker to the validator-compatible reward profile"
                        );
                    }
                    if is_shard_holder {
                        model.enforce_canonical_i8_profile();
                        tracing::info!(
                            profile = arc_inference::cached_integer_model::CANONICAL_REWARD_INFERENCE_PROFILE,
                            "Pinned protocol-v3 shard holder to the canonical reward verification profile"
                        );
                    }
                    let elapsed = load_start.elapsed();
                    let mb_held: usize = model
                        .layers
                        .iter()
                        .filter(|l| l.is_loaded())
                        .map(|l| {
                            l.wq.memory_bytes()
                                + l.wk.memory_bytes()
                                + l.wv.memory_bytes()
                                + l.wo.memory_bytes()
                                + l.w_gate.memory_bytes()
                                + l.w_up.memory_bytes()
                                + l.w_down.memory_bytes()
                        })
                        .sum::<usize>()
                        / (1024 * 1024);
                    let layers_held = model.layers.iter().filter(|l| l.is_loaded()).count();
                    // I16 remains a local, nonreward optimization. Validator
                    // shards and full community workers participate in signed
                    // cross-node verification, so their arithmetic identity is
                    // pinned above to canonical I8 and must never be silently
                    // promoted based on host architecture.
                    let want_i16 = !is_shard_holder
                        && !cli.full_integer_worker
                        && (cli.enable_i16 || cfg!(target_arch = "aarch64"));
                    if model.i16_layers.is_none() && layers_held > 0 && want_i16 {
                        model.enable_i16();
                        tracing::info!(
                            "I16 dispatch enabled (promoted from I8); engine label will report \"INT16 integer\""
                        );
                    } else if model.i16_layers.is_none() && layers_held > 0 {
                        tracing::info!(
                            "I16 promotion SKIPPED on {}: from_i8 adds no precision and this \
                             target's i16 dot product is scalar, so promoting would double \
                             per-layer weight bytes for identical output. Pass --enable-i16 to force.",
                            std::env::consts::ARCH
                        );
                    }
                    tracing::info!(
                        "Model loaded in {:.1}s - {} layers held / {} total, {} MB shard weights, vocab {}",
                        elapsed.as_secs_f64(),
                        layers_held,
                        model.config.n_layers,
                        mb_held,
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

    if let Some(artifact) = &model_artifact {
        artifact
            .verify_unchanged()
            .context("--model changed while the inference runtime was loading it")?;
    }
    if cli.full_integer_worker {
        let model = inference_model.as_ref().ok_or_else(|| {
            anyhow::anyhow!("--full-integer-worker failed to load the deterministic integer model")
        })?;
        ensure!(
            model.has_all_transformer_layers(),
            "--full-integer-worker loaded only {}/{} transformer layers",
            model
                .layers
                .iter()
                .filter(|layer| layer.is_loaded())
                .count(),
            model.config.n_layers
        );
        ensure!(
            candle_engine.is_none() && held_ranges.is_empty() && model.has_canonical_i8_profile(),
            "--full-integer-worker invariant failed: worker must use the canonical per-row INT8 profile and must not advertise shard ranges"
        );
    }
    if complete_startup_shutdown_if_requested(
        &shutdown_requested,
        Some(state.as_ref()),
        &desktop_shutdown_receipt,
    )? {
        return Ok(());
    }

    // ── Record boot time for uptime tracking ──────────────────────────
    let boot_time = Instant::now();

    let mut consensus_thread = None::<std::thread::JoinHandle<()>>;

    // ── Create channels for P2P transport ↔ consensus ─────────────────
    let (inbound_tx, inbound_rx) = mpsc::channel::<InboundMessage>(1000);
    let (outbound_tx, outbound_rx) = mpsc::channel::<OutboundMessage>(4000);
    let peer_count = Arc::new(AtomicU32::new(0));

    // ── Start benchmark signing pool + indexer (if benchmark mode) ─────
    #[cfg(feature = "benchmark-tools")]
    let benchmark_pool = if benchmark_mode {
        state.start_benchmark_indexer();
        let pool = BenchmarkPool::start(
            bench_sender_start,
            bench_sender_count,
            bench_sign_threads,
            10_000, // txs per batch
        );
        tracing::info!(
            "Benchmark mode ACTIVE - ed25519 signed txs, senders {}-{}, async indexing",
            bench_sender_start,
            bench_sender_start + bench_sender_count - 1
        );
        Some(Arc::new(pool))
    } else {
        None
    };
    #[cfg(not(feature = "benchmark-tools"))]
    let benchmark_pool: Option<Arc<BenchmarkPool>> = None;

    // ── Start DAG consensus in background ─────────────────────────────
    // Initialize the exact approved genesis validator identities and stakes.
    // Seed addresses and transport discovery are connectivity inputs only;
    // they never define or mutate voting membership.
    let peer_vals: Vec<(Hash256, u64)> = genesis_validators
        .iter()
        .filter(|(addr, _)| *addr != validator_address)
        .cloned()
        .collect();
    let runtime_roles = node_runtime_roles(
        chain_participation_enabled,
        state.active_protocol_version().major,
    );
    let all_vals: Vec<(Hash256, u64)> = if runtime_roles.chain_participation {
        let mut v = vec![(validator_address, stake)];
        v.extend(&peer_vals);
        v
    } else {
        Vec::new()
    };
    let dag_validators = Arc::new(parking_lot::RwLock::new(all_vals));
    let dag_round = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let dag_committed = Arc::new(std::sync::atomic::AtomicU64::new(0));

    if runtime_roles.chain_participation {
        let recovery_dag_startup = prepare_recovery_dag_startup(Path::new(&data_dir), &state)?;
        let mut consensus = ConsensusManager::new_with_keypair(
            validator_address,
            stake,
            4, /* num_shards */
            benchmark_mode,
            &peer_vals,
            validator_keypair.clone(),
        );
        if let Some(context) = state.recovery_context() {
            let domain = arc_consensus::ConsensusDomain::new(
                context.domain_hash(),
                context.recovery_epoch,
                context.validator_set_id,
            );
            if let Some(startup) = recovery_dag_startup.as_ref() {
                ensure!(
                    startup.binding.consensus_domain == domain,
                    "recovery DAG binding differs from the active consensus domain"
                );
            }
            consensus
                .engine
                .install_consensus_domain(domain)
                .map_err(|error| {
                    anyhow::anyhow!("failed to install recovery consensus domain: {error}")
                })?;
        }
        consensus.dag_validators = Some(dag_validators.clone());
        consensus.dag_round = Some(dag_round.clone());
        consensus.dag_committed = Some(dag_committed.clone());
        if let Some(startup) = recovery_dag_startup.as_ref() {
            // Protocol v3 never opens the legacy segmented WAL. Select an
            // independently pinned content-addressed generation, stage its
            // bounded immutable+active stream, quarantine only a classified
            // torn final active batch, and replay every block/commit before
            // networking starts.
            let (store, selected_generation) =
                initialize_recovery_generation_store(state.as_ref(), startup)?;
            let (staged_records, staged_summary) =
                stage_recovery_generation_records(&store, &selected_generation)?;
            let RecoveryDagReplay {
                current_round: recovered_round,
                next_commit_round: recovered_committed,
                transactions: recovered_transactions,
                repaired_commit,
            } = replay_recovery_dag_generation(
                consensus.engine.as_ref(),
                state.as_ref(),
                startup,
                &selected_generation,
                &staged_records,
            )?;
            // Certified-but-not-yet-committed replay blocks may become
            // executable on the first consensus tick. Their exact bodies must
            // be installed directly as availability preimages; the live
            // mempool drain order is not a recovery guarantee.
            consensus.install_recovered_preimages(recovered_transactions.clone());
            let mut restored_transactions = 0usize;
            for transaction in recovered_transactions {
                if state.get_receipt(&transaction.hash.0).is_none()
                    && state
                        .validate_v3_transaction_admission(&transaction)
                        .is_ok()
                {
                    mempool.insert(transaction).map_err(|error| {
                        anyhow::anyhow!(
                            "failed to restore a recovery-domain DAG transaction into the mempool: {error}"
                        )
                    })?;
                    restored_transactions += 1;
                }
            }
            dag_round.store(recovered_round, std::sync::atomic::Ordering::SeqCst);
            dag_committed.store(recovered_committed, std::sync::atomic::Ordering::SeqCst);

            // Advance the immutable baseline to the exact canonical state
            // boundary and remove commit records now transitively bound by its
            // block hash/root. Publication selects generation+empty active log
            // atomically; the independent pin is advanced only afterwards.
            let active_generation = compact_replayed_recovery_generation(
                &store,
                state.as_ref(),
                startup,
                &selected_generation,
                &staged_summary,
                consensus.engine.as_ref(),
                &staged_records,
            )?;
            let active_writer = store
                .open_current_active_writer(
                    &active_generation.manifest.binding,
                    active_generation.pin,
                )
                .context("failed to open the pinned recovery DAG active writer")?;
            consensus.recovery_dag_writer =
                Some(Arc::new(parking_lot::Mutex::new(Some(active_writer))));
            consensus.recovery_dag_rollover = Some(Arc::new(LiveRecoveryDagRollover {
                store,
                startup: startup.clone(),
                current: parking_lot::Mutex::new(active_generation.clone()),
            }));
            if let Some((hash, round)) = repaired_commit {
                tracing::warn!(
                    %hash,
                    round,
                    generation = %active_generation.pin.hash,
                    "Bound the repaired state/DAG commit crash window into a new generation baseline"
                );
            }
            tracing::info!(
                recovered_round,
                recovered_committed,
                manifest_hash = %startup.binding.manifest_hash,
                validator_set_commitment = %startup.binding.validator_set_commitment,
                generation = %active_generation.pin.hash,
                archived_legacy_wal = ?startup.archived_legacy_wal,
                restored_transactions,
                "Recovery DAG cursor and bounded writer are pinned to the signed ARCCHKPT domain"
            );
        } else {
            // Preserve the segmented WAL compatibility path only for legacy
            // pre-recovery networks. Protocol-v3 startup above cannot reach it.
            let dag_wal_path = Path::new(&data_dir).join("dag-wal");
            std::fs::create_dir_all(&dag_wal_path).with_context(|| {
                format!(
                    "failed to create legacy DAG WAL directory {}",
                    dag_wal_path.display()
                )
            })?;
            let recovered_round = arc_state::latest_block_height_in_wal_dir(&dag_wal_path);
            if recovered_round > 0 {
                // The highest WAL round does not prove that any earlier leader was
                // committed. Preserve the commit cursor until an exact local commit
                // record or quorum-certified checkpoint recovery path is available.
                let recovered_committed = 0;
                consensus
                    .engine
                    .restore_round_from_local_wal(recovered_round, recovered_committed);
                tracing::info!(
                    recovered_round,
                    recovered_committed,
                    "DAG WAL round restored from local disk; commit cursor remains fail-closed pending certified recovery"
                );
            } else {
                tracing::info!("DAG WAL is empty - starting fresh from round 0");
            }

            match arc_state::WalWriter::with_segments(&dag_wal_path, 64 * 1024 * 1024) {
                Ok(dag_wal) => {
                    consensus.dag_wal = Some(Arc::new(dag_wal));
                    tracing::info!(
                        "Legacy DAG persistence WAL enabled: {}",
                        dag_wal_path.display()
                    );
                }
                Err(error) => tracing::warn!(
                    error = %error,
                    path = %dag_wal_path.display(),
                    "Legacy DAG persistence WAL is unavailable"
                ),
            }
        }

        // Recovery replay and durable-writer setup are complete before any
        // network input can arrive. Bind the authenticated QUIC endpoint and
        // require an explicit readiness result before the consensus thread is
        // allowed to start.
        let bootstrap_peers: Vec<SocketAddr> =
            peers.iter().filter_map(|peer| peer.parse().ok()).collect();
        let listen_ip = p2p_listen_ip(p2p_port, benchmark_mode, cli.insecure_dev_validator_seed);
        let listen_addr = SocketAddr::from((listen_ip, p2p_port));
        let mut allowed_validator_addresses = std::collections::HashSet::new();
        allowed_validator_addresses.insert(validator_address.0);
        allowed_validator_addresses.extend(genesis_validators.iter().map(|(address, _)| address.0));
        let (startup_tx, startup_rx) = tokio::sync::oneshot::channel();
        let transport_shutdown = runtime_shutdown_rx.clone();
        let transport_task = tokio::spawn(run_transport_with_readiness_and_shutdown(
            listen_addr,
            bootstrap_peers,
            validator_address,
            stake,
            genesis_hash,
            Arc::new(allowed_validator_addresses),
            outbound_rx,
            inbound_tx,
            peer_count.clone(),
            validator_keypair.clone(),
            data_dir.clone(),
            startup_tx,
            transport_shutdown,
        ));
        let bound_addr =
            match tokio::time::timeout(std::time::Duration::from_secs(15), startup_rx).await {
                Ok(Ok(Ok(bound_addr))) => bound_addr,
                Ok(Ok(Err(error))) => {
                    transport_task.abort();
                    bail!("authenticated validator transport failed startup: {error}");
                }
                Ok(Err(_)) => {
                    transport_task.abort();
                    bail!("authenticated validator transport exited before reporting readiness");
                }
                Err(_) => {
                    transport_task.abort();
                    bail!("authenticated validator transport readiness timed out after 15 seconds");
                }
            };
        tracing::info!(
            bound = %bound_addr,
            "Authenticated validator transport is ready before consensus startup"
        );
        let transport_exit_shutdown = runtime_shutdown_rx.clone();
        runtime_tasks.push(tokio::spawn(async move {
            match transport_task.await {
                Ok(()) if *transport_exit_shutdown.borrow() => {
                    tracing::info!("Authenticated validator transport joined for shutdown");
                    return;
                }
                Ok(()) => {
                    tracing::error!(
                        "Authenticated validator transport exited after readiness; terminating node"
                    );
                }
                Err(error) => {
                    tracing::error!(
                        error = %error,
                        "Authenticated validator transport task failed after readiness; terminating node"
                    );
                }
            }
            std::process::exit(1);
        }));

        consensus.set_proposer_mode(cli.proposer_mode);
        let state_clone = state.clone();
        let mempool_clone = mempool.clone();
        let pool_clone = benchmark_pool.clone();
        let consensus_shutdown = runtime_shutdown_rx.clone();
        let consensus_exit_shutdown = runtime_shutdown_rx.clone();
        // Run consensus on a dedicated thread with its own tokio runtime.
        // This prevents broadcast/transport/RPC tasks from starving the
        // consensus loop (the root cause of random freezes at ~4000 rounds).
        // If the consensus thread panics, log the error and exit the process -
        // a node without consensus is useless and should restart via systemd.
        consensus_thread = Some(
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
                                .run_consensus_loop_with_shutdown(
                                    state_clone,
                                    mempool_clone,
                                    Some(inbound_rx),
                                    Some(outbound_tx),
                                    pool_clone,
                                    consensus_shutdown,
                                )
                                .await;
                        });
                    }));
                    match result {
                        Ok(()) if *consensus_exit_shutdown.borrow() => {
                            tracing::info!("Consensus thread joined for shutdown");
                            return;
                        }
                        Ok(()) => {
                            tracing::error!("Consensus loop exited unexpectedly - shutting down");
                        }
                        Err(panic_info) => {
                            tracing::error!("CONSENSUS THREAD PANICKED: {:?}", panic_info);
                        }
                    }
                    // Exit the process - a node without consensus must restart
                    std::process::exit(1);
                })
                .context("failed to spawn consensus thread")?,
        );
    } else {
        drop(inbound_rx);
        drop(outbound_tx);
        drop(inbound_tx);
        drop(outbound_rx);
        tracing::warn!(
            "Chain P2P/consensus is OFF while genesis validator migration is pending; community HTTP inference remains active"
        );
    }

    // ── Start ETH JSON-RPC server (MetaMask, Hardhat, Foundry) ──────────
    let eth_server_task = if eth_rpc_port > 0 {
        // Keep this unauthenticated compatibility surface local by default.
        // Operators that need remote access should publish it through an
        // authenticated, rate-limited reverse proxy rather than exposing the
        // raw JSON-RPC listener directly.
        let eth_addr = format!("127.0.0.1:{}", eth_rpc_port);
        let mut eth_node = rpc::build_node_state(
            state.clone(),
            mempool.clone(),
            validator_address,
            Some(Arc::new(validator_keypair.clone())),
            stake,
            boot_time,
            peer_count.clone(),
            inference_model.clone(),
            candle_engine.clone(),
            candle_model_id,
            model_artifact_id,
        );
        // Same declared identity on the ETH port's state.
        eth_node.chain_identity = genesis_chain_identity.clone();
        tracing::info!("ETH RPC    : {} (MetaMask/Hardhat/Foundry)", eth_addr);
        let eth_shutdown = shutdown_rx.clone();
        Some(tokio::spawn(async move {
            rpc::serve_eth(&eth_addr, eth_node, Some(eth_shutdown)).await
        }))
    } else {
        None
    };

    // ── Start RPC server ────────────────────────────────────────────────
    if candle_engine.is_some() {
        tracing::info!("Inference  : ENABLED (candle Q4 float, coherent output)");
    } else if inference_model.is_some() {
        tracing::info!("Inference  : ENABLED (INT8 integer engine)");
    }
    match &rpc_listener {
        rpc::RpcListen::Tcp(address) => tracing::info!("RPC server listening on {}", address),
        #[cfg(unix)]
        rpc::RpcListen::Unix(path) => {
            tracing::info!(
                "RPC server listening on sealed Unix socket {}",
                path.display()
            )
        }
    }

    // ── Spawn Tier 1 on-chain inference validator task ──────────────────
    // The task polls StateDB for open InferenceRequest escrows, checks
    // committee membership (deterministic VRF over the validator set),
    // runs candle inference locally for requests selecting it, and
    // submits InferenceVote / InferenceFinalize txs. See
    // `arc-chain-docs/TIER1_ONCHAIN_INFERENCE_PLAN.md`.
    //
    // Safe to spawn without a model: missing engine/tokenizer/exact artifact
    // identity makes this validator abstain. Synthetic fallback votes are
    // forbidden because they would claim execution of bytes never loaded.
    if runtime_roles.tier1_background_inference {
        let validator_task = arc_node::inference_validator::InferenceValidatorTask::new(
            state.clone(),
            mempool.clone(),
            validator_address,
            validator_keypair.clone(),
            candle_engine.clone(),
            inference_model.clone(),
            model_artifact_id,
        );
        let validator_shutdown = background_admission_shutdown_rx.clone();
        runtime_tasks.push(tokio::spawn(async move {
            validator_task.run_with_shutdown(validator_shutdown).await;
        }));
        tracing::info!(
            "Tier 1 validator task spawned (candle={}, tokenizer={})",
            candle_engine.is_some(),
            inference_model.is_some()
        );
    } else if !runtime_roles.chain_participation {
        tracing::info!(
            "Tier 1 on-chain validator task skipped in genesis-migration community observer mode"
        );
    } else {
        tracing::info!(
            protocol_major = state.active_protocol_version().major,
            "Tier 1 background validator disabled while paid inference is dark for this protocol"
        );
    }

    // Build one ShardInfo per held range if this node is a shard holder, then
    // broadcast each so the network's shard registry records every replica
    // slot this node contributes (supports nodes that hold multiple disjoint
    // layer ranges for 3× replication).
    let shard_ranges_are_loaded = inference_model.as_ref().is_some_and(|model| {
        held_ranges.iter().all(|(start, end)| {
            *end <= model.layers.len()
                && model.layers[*start..*end]
                    .iter()
                    .all(arc_inference::cached_integer_model::CachedLayer::is_loaded)
        })
    });
    if !held_ranges.is_empty() && !shard_ranges_are_loaded {
        tracing::warn!(
            "Configured shard ranges are not fully loaded; this node will not announce or serve them"
        );
    }
    let shard_infos_for_broadcast: Vec<rpc::ShardInfo> =
        match (&held_ranges, &inference_model, model_artifact_id) {
            (ranges, Some(model), Some(artifact_id))
                if !ranges.is_empty() && shard_ranges_are_loaded =>
            {
                let total_layers = model.config.n_layers;
                let layers_held_total: usize =
                    ranges.iter().map(|(s, e)| e.saturating_sub(*s)).sum();
                let memory_mb_total: usize = model
                    .layers
                    .iter()
                    .filter(|l| l.is_loaded())
                    .map(|l| {
                        l.wq.memory_bytes()
                            + l.wk.memory_bytes()
                            + l.wv.memory_bytes()
                            + l.wo.memory_bytes()
                            + l.w_gate.memory_bytes()
                            + l.w_up.memory_bytes()
                            + l.w_down.memory_bytes()
                    })
                    .sum::<usize>()
                    / (1024 * 1024);
                let per_layer_mb = memory_mb_total / layers_held_total.max(1);
                let full_model_mb = per_layer_mb * total_layers;
                let model_display_name = format!(
                    "arc-{}L-{}d-{}h-{}v",
                    model.config.n_layers,
                    model.config.d_model,
                    model.config.n_heads,
                    model.config.vocab_size
                );
                let socket_addr = advertised_shard_rpc_origin(&cli, &rpc_addr)?;
                ranges
                    .iter()
                    .map(|&(start, end)| rpc::ShardInfo {
                        start_layer: start,
                        end_layer: end,
                        total_layers,
                        model_id: format!("0x{}", hex::encode(artifact_id.0)),
                        model_name: model_display_name.clone(),
                        execution_profile: model.effective_precision_label().to_string(),
                        memory_mb: per_layer_mb * (end - start),
                        full_model_mb,
                        socket_addr: socket_addr.clone(),
                        // NOT validator_seed: this ShardInfo is POSTed to every seed
                        // every 15s and served publicly by GET /shards, and the
                        // desktop's seed is the wallet's BIP-39 phrase.
                        node_name: public_node_name(&cli),
                    })
                    .collect()
            }
            _ => Vec::new(),
        };
    let shard_infos = shard_infos_for_broadcast.clone();

    // Announce each held range directly to every explicit validator HTTP
    // origin. Every mutation is signed for the exact destination validator
    // and recovery domain; no raw or re-signed third-party announcement is
    // emitted. Direct holder announcements are the only topology authority.
    if !shard_infos_for_broadcast.is_empty() {
        let sis = shard_infos_for_broadcast.clone();
        // RPC origins are explicit TLS/gateway configuration. Never infer an
        // HTTP port from a P2P bootstrap address.
        let announcement_targets = coordinator_rpc_bases.clone();
        if announcement_targets.is_empty() {
            tracing::info!(
                "No remote shard announcement origin configured; local shard TTL is refreshed in-process"
            );
        } else {
            let shard_announcement_keypair = validator_keypair.clone();
            let shard_announcement_shutdown = background_admission_shutdown_rx.clone();
            runtime_tasks.push(tokio::spawn(
                run_signed_shard_announcement_loop_with_shutdown(
                    sis,
                    announcement_targets,
                    shard_announcement_keypair,
                    shard_announcement_shutdown,
                ),
            ));

            tracing::info!("Signed direct-holder shard broadcaster started (15s tick)");
        }
    }

    // ── Community-mode HTTP registration + heartbeat ──────────────────
    // Spawned when --community-mode is set. Outbound HTTP only - works
    // behind any NAT/residential firewall. Registers with every seed on
    // startup + every 60s, sends a heartbeat every 15s to keep the
    // registry entry alive. Signatures authenticate mutations but do not
    // encrypt them; production deployments must supply TLS at a reverse proxy.
    // Each seed's TTL is 90s so 5 missed
    // heartbeats before eviction.
    // Auto-enable community mode for observer nodes (stake=0).
    // If you join with no stake, you're a community contributor - no flag needed.
    //
    // `--no-community` opts out. That escape hatch exists because --stake now
    // defaults to 0: without it, a bare `arc-node --seeds-file <public seeds>`
    // would start POSTing /community/register and /community/heartbeat to
    // every seed listed. Read-only coordinators need to be able to say no.
    let community_mode = (cli.community_mode || stake == 0) && !cli.no_community;
    let community_networking = community_mode && !community_rpc_bases.is_empty();
    if cli.no_community && (cli.community_mode || stake == 0) {
        tracing::info!(
            "--no-community: observer mode. This node will NOT register, heartbeat or \
             claim work from its peers; it only reads from them."
        );
    }

    if community_mode && !community_networking {
        tracing::warn!(
            "Community mode has no coordinator RPC origin. Configure one or more explicit --community-rpc-url https://... values; P2P seeds are intentionally not converted into HTTP targets."
        );
    }

    if community_networking {
        tracing::info!("╔═══════════════════════════════════════╗");
        tracing::info!("║  COMMUNITY MODE ACTIVE                ║");
        tracing::info!("║  Registering with seed coordinators   ║");
        tracing::info!("║  Capability is verified after load    ║");
        tracing::info!("╚═══════════════════════════════════════╝");
    }

    if community_networking {
        let public_node_name_c = public_node_name(&cli);
        let worker_id = format!("0x{}", hex::encode(validator_address.0));
        let hostname = std::process::Command::new("hostname")
            .output()
            .ok()
            .and_then(|o| String::from_utf8(o.stdout).ok())
            .map(|s| s.trim().to_string())
            .unwrap_or_else(|| "unknown".to_string());
        let platform = format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH);
        let worker_model = inference_model.as_ref().and_then(|m| {
            if !m.has_all_transformer_layers() || !m.has_canonical_i8_profile() {
                return None;
            }
            let artifact_id = model_artifact_id?;
            let model_name = format!(
                "arc-{}L-{}d-{}h-{}v",
                m.config.n_layers, m.config.d_model, m.config.n_heads, m.config.vocab_size
            );
            let model_id = format!("0x{}", hex::encode(artifact_id.0));
            Some((model_name, model_id, artifact_id))
        });
        if inference_model.is_some() && worker_model.is_none() {
            tracing::warn!(
                "Loaded model is partial, tokenizer-only, or non-canonical; registering as \
                 relay/observer and disabling reward-bearing community inference"
            );
        }

        let community_rpc_targets = community_rpc_bases.clone();

        let worker_id_c = worker_id.clone();
        let hostname_c = hostname.clone();
        let platform_c = platform.clone();
        let model_name_c = worker_model.as_ref().map(|(name, _, _)| name.clone());
        let model_id_c = worker_model
            .as_ref()
            .map(|(_, model_id, _)| model_id.clone());
        let execution_profile_c = worker_model.as_ref().map(|_| {
            arc_inference::cached_integer_model::CANONICAL_REWARD_INFERENCE_PROFILE.to_string()
        });
        let community_rpc_targets_c = community_rpc_targets.clone();
        let registration_keypair = validator_keypair.clone();
        let mut registration_shutdown = Some(background_admission_shutdown_rx.clone());

        runtime_tasks.push(tokio::spawn(async move {
            // Settle before first POST
            if sleep_or_runtime_shutdown(
                &mut registration_shutdown,
                std::time::Duration::from_secs(5),
            )
            .await
            {
                return;
            }
            let client = match reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(5))
                .redirect(reqwest::redirect::Policy::none())
                .build()
            {
                Ok(c) => c,
                Err(_) => return,
            };

            // Advertise "inference" ONLY when a model is actually loaded.
            //
            // This was hardcoded to ["inference"] regardless. All three live
            // registered workers reported `model: null` yet still counted as
            // live inference workers on the seed side (which checks
            // capabilities alone), so the router dispatched real jobs into a
            // black hole and blocked in dispatch_to_community_worker for the
            // full community_dispatch_timeout — 60 s at the desktop's 16-token
            // default — before falling back to local.
            let capabilities: Vec<String> = if model_name_c.is_some() && model_id_c.is_some() {
                vec!["inference".to_string()]
            } else {
                vec!["relay".to_string()]
            };
            let register_payload = rpc::CommunityRegisterRequest {
                worker_id: worker_id_c.clone(),
                name: format!("{} ({})", public_node_name_c, hostname_c),
                capabilities,
                model: model_name_c,
                model_id: model_id_c,
                execution_profile: execution_profile_c,
                platform: platform_c,
            };
            let heartbeat_payload = rpc::CommunityHeartbeatRequest {
                worker_id: worker_id_c,
                work_completed: None,
            };

            // Register once, then heartbeat + re-register periodically.
            //
            // V3 intentionally contacts arc-node's authenticated RPC only.
            // Retrying the old unsigned :3001 gateway would downgrade proof
            // of possession and make rollout failures look like success.
            //
            // Seeds are contacted CONCURRENTLY. Serially, one unreachable
            // seed's 5 s timeout delayed every seed after it, and with six
            // seeds a full round could exceed the 15 s tick.
            let mut ticks: u64 = 0;
            loop {
                let register_tick = ticks.is_multiple_of(4);
                let mut set = tokio::task::JoinSet::new();
                for addr in &community_rpc_targets_c {
                    let client = client.clone();
                    let addr = addr.clone();
                    let register_payload = register_payload.clone();
                    let heartbeat_payload = heartbeat_payload.clone();
                    let keypair = registration_keypair.clone();
                    set.spawn(async move {
                        let response = if register_tick {
                            post_signed_community(
                                &client,
                                &addr,
                                rpc::COMMUNITY_REGISTER_PATH,
                                register_payload,
                                &keypair,
                                std::time::Duration::from_secs(5),
                            )
                            .await
                        } else {
                            post_signed_community(
                                &client,
                                &addr,
                                rpc::COMMUNITY_HEARTBEAT_PATH,
                                heartbeat_payload,
                                &keypair,
                                std::time::Duration::from_secs(5),
                            )
                            .await
                        };
                        match response {
                            Ok(response) if response.status().is_success() => {}
                            Ok(response) => {
                                tracing::warn!(
                                    seed = %addr,
                                    status = %response.status(),
                                    "coordinator rejected authenticated community presence"
                                );
                            }
                            Err(error) => {
                                tracing::debug!(seed = %addr, %error, "community presence POST failed");
                            }
                        }
                    });
                }
                while set.join_next().await.is_some() {}
                ticks += 1;
                if sleep_or_runtime_shutdown(
                    &mut registration_shutdown,
                    std::time::Duration::from_secs(15),
                )
                .await
                {
                    return;
                }
            }
        }));
        tracing::info!(
            "Community-mode HTTP registration started (worker_id={})",
            worker_id
        );

        // ── Community inference worker loop ──────────────────────────────
        // Continuously long-poll /community/claim_work on all seeds. When
        // a job arrives, run inference locally using the loaded model, then
        // POST the result back to /community/submit_work. This is what
        // makes community nodes provide REAL inference compute.
        if let (Some(model), Some((_, worker_model_id, worker_model_id_hash))) =
            (inference_model.clone(), worker_model.clone())
        {
            let worker_id_w = worker_id.clone();
            let community_rpc_targets_w = community_rpc_targets.clone();
            // Worker keypair + address for signing InferenceAttestation txs
            // (v0.7.0). On every successful submit we build a signed
            // tx with `from = worker_address` so on-chain rewards accrue
            // to the worker who actually did the work.
            let worker_keypair = validator_keypair.clone();
            let worker_address = validator_address;
            // Per-process attestation nonce. Initialized on first submit
            // by querying the chain for the worker's current account
            // nonce (see init-from-chain block inside the loop), then
            // incremented locally per attestation. If a future submit
            // is rejected with InvalidNonce we re-query and reset.
            let attestation_nonce = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
            let attestation_nonce_initialized =
                std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
            let mut worker_shutdown = Some(background_admission_shutdown_rx.clone());

            runtime_tasks.push(tokio::spawn(async move {
                if sleep_or_runtime_shutdown(
                    &mut worker_shutdown,
                    std::time::Duration::from_secs(10),
                )
                .await
                {
                    return;
                }
                let client = match reqwest::Client::builder()
                    .timeout(std::time::Duration::from_secs(35)) // 30s claim + 5s overhead
                    .redirect(reqwest::redirect::Policy::none())
                    .build()
                {
                    Ok(c) => c,
                    Err(_) => return,
                };
                tracing::info!(
                    address = %format!("0x{}", hex::encode(worker_address.0)),
                    "Community inference worker started - polling for jobs"
                );
                let mut decline_tasks = tokio::task::JoinSet::new();
                loop {
                    while let Some(result) = decline_tasks.try_join_next() {
                        if let Err(error) = result {
                            tracing::warn!(%error, "community decline task failed");
                        }
                    }
                    if worker_shutdown
                        .as_ref()
                        .is_some_and(|receiver| *receiver.borrow())
                    {
                        break;
                    }
                    // Long-poll EVERY seed at once and take the first to hand
                    // us work.
                    //
                    // This used to be a sequential `for addr in seeds`, each
                    // awaiting a long-poll of up to COMMUNITY_CLAIM_TIMEOUT_SECS
                    // (30 s) on :9090 and then again on :3001. With six seeds
                    // the worst-case revisit interval for any one seed was
                    // ~6 minutes, against a dispatcher that gives up after at
                    // most 60 s — so a job queued on seed 5 expired before the
                    // worker got back around to asking. Every live seed
                    // reported total_work_completed = 0 despite three
                    // registered workers.
                    //
                    // Now: one in-flight claim per seed, first responder wins.
                    // The remaining polls are drained in the background. If
                    // two independent coordinators assign work at the same
                    // instant, every extra assignment is explicitly declined
                    // so its dispatcher falls back immediately instead of
                    // silently losing the dequeued job when a request future
                    // is canceled.
                    let claim_body = rpc::ClaimWorkRequest {
                        worker_id: worker_id_w.clone(),
                        capabilities: vec!["inference".to_string()],
                        model_id: worker_model_id.clone(),
                        execution_profile:
                            arc_inference::cached_integer_model::CANONICAL_REWARD_INFERENCE_PROFILE
                                .to_string(),
                    };
                    let mut claims = tokio::task::JoinSet::new();
                    for addr in &community_rpc_targets_w {
                        let client = client.clone();
                        let body = claim_body.clone();
                        let target = addr.clone();
                        let keypair = worker_keypair.clone();
                        claims.spawn(async move {
                            let response = post_signed_community(
                                &client,
                                &target,
                                rpc::COMMUNITY_CLAIM_WORK_PATH,
                                body,
                                &keypair,
                                std::time::Duration::from_secs(35),
                            )
                            .await
                            .ok()?;
                            if !response.status().is_success() {
                                return None;
                            }
                            let job: serde_json::Value = response.json().await.ok()?;
                            if job.get("status").and_then(|s| s.as_str()) == Some("work") {
                                Some((target, job))
                            } else {
                                None
                            }
                        });
                    }

                    let mut claimed: Option<(String, serde_json::Value)> = None;
                    while let Some(res) = claims.join_next().await {
                        if let Ok(Some(hit)) = res {
                            claimed = Some(hit);
                            break;
                        }
                    }
                    // A request can already have consumed a remote queue item
                    // by the time its future is canceled. Keep the remaining
                    // polls alive and explicitly decline any additional jobs;
                    // the seed then releases its worker slot and immediately
                    // falls back to local inference.
                    if claimed.is_some() {
                        let decline_client = client.clone();
                        let decline_worker = worker_id_w.clone();
                        let decline_keypair = worker_keypair.clone();
                        decline_tasks.spawn(async move {
                            while let Some(result) = claims.join_next().await {
                                let Ok(Some((coordinator, job))) = result else {
                                    continue;
                                };
                                decline_community_assignment(
                                    decline_client.clone(),
                                    coordinator,
                                    job,
                                    decline_worker.clone(),
                                    decline_keypair.clone(),
                                    "worker already accepted a concurrent coordinator job",
                                )
                                .await;
                            }
                        });
                    }

                    // A long-poll can return a job at the same instant the
                    // lifecycle signal closes admission. Decline that claimed
                    // item and drain the remaining claim futures; never begin
                    // a new blocking inference after shutdown was requested.
                    if worker_shutdown
                        .as_ref()
                        .is_some_and(|receiver| *receiver.borrow())
                    {
                        if let Some((coordinator, job)) = claimed.take() {
                            decline_tasks.spawn(decline_community_assignment(
                                client.clone(),
                                coordinator,
                                job,
                                worker_id_w.clone(),
                                worker_keypair.clone(),
                                "worker is shutting down before compute admission",
                            ));
                        }
                        break;
                    }

                    {
                        let Some((winner, job)) = claimed else {
                            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                            continue;
                        };

                        let job_id = job
                            .get("job_id")
                            .and_then(|s| s.as_str())
                            .unwrap_or("")
                            .to_string();
                        let input = job
                            .get("input")
                            .and_then(|s| s.as_str())
                            .unwrap_or("")
                            .to_string();
                        let assignment_max_tokens = job
                            .get("max_tokens")
                            .and_then(serde_json::Value::as_u64)
                            .and_then(|value| u32::try_from(value).ok());
                        let max_tokens = assignment_max_tokens.unwrap_or(0);
                        if job_id.is_empty() {
                            tracing::warn!(
                                seed = %winner,
                                "coordinator returned a community assignment without a job_id"
                            );
                            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                            continue;
                        }
                        let assignment_model_id = job
                            .get("model_id")
                            .and_then(|value| value.as_str())
                            .and_then(|value| {
                                let bare = value
                                    .strip_prefix("0x")
                                    .or_else(|| value.strip_prefix("0X"))
                                    .unwrap_or(value);
                                Hash256::from_hex(bare).ok()
                            });
                        let assignment_transaction_domain = job
                            .get("transaction_domain")
                            .and_then(|value| value.as_str())
                            .and_then(|value| {
                                let bare = value
                                    .strip_prefix("0x")
                                    .or_else(|| value.strip_prefix("0X"))
                                    .unwrap_or(value);
                                Hash256::from_hex(bare).ok()
                            });
                        let transaction_domain_required = job
                            .get("transaction_domain_required")
                            .and_then(|value| value.as_bool())
                            .unwrap_or(false);
                        let transaction_domain_is_malformed = job
                            .get("transaction_domain")
                            .is_some_and(|value| !value.is_null())
                            && assignment_transaction_domain.is_none();
                        let required_transaction_domain_is_missing =
                            transaction_domain_required && assignment_transaction_domain.is_none();
                        let assignment_model_matches =
                            assignment_model_id == Some(worker_model_id_hash);
                        let assignment_execution_profile_matches = job
                            .get("execution_profile")
                            .and_then(|value| value.as_str())
                            == Some(
                                arc_inference::cached_integer_model::CANONICAL_REWARD_INFERENCE_PROFILE,
                            );
                        let deadline_anchor = tokio::time::Instant::now();
                        let now_unix_ms = chrono::Utc::now().timestamp_millis();
                        let assignment_submit_window = job
                            .get("submitted_at_unix_ms")
                            .and_then(serde_json::Value::as_i64)
                            .ok_or_else(|| {
                                "assignment omitted an integer submitted_at_unix_ms".to_string()
                            })
                            .and_then(|submitted_at_unix_ms| {
                                job.get("expires_at_unix_ms")
                                    .and_then(serde_json::Value::as_u64)
                                    .ok_or_else(|| {
                                        "assignment omitted an integer expires_at_unix_ms"
                                            .to_string()
                                    })
                                    .and_then(|expires_at_unix_ms| {
                                        community_submit_window(
                                            submitted_at_unix_ms,
                                            expires_at_unix_ms,
                                            now_unix_ms,
                                        )
                                    })
                            });
                        if !assignment_model_matches
                            || !assignment_execution_profile_matches
                            || transaction_domain_is_malformed
                            || required_transaction_domain_is_missing
                            || assignment_submit_window.is_err()
                            || input.is_empty()
                            || input.len() > 32_768
                            || assignment_max_tokens.is_none()
                            || max_tokens == 0
                            || max_tokens > rpc::INFERENCE_RUN_MAX_TOKENS
                        {
                            let reason = if !assignment_model_matches {
                                "assignment omitted or mismatched the worker's exact model artifact"
                                    .to_string()
                            } else if !assignment_execution_profile_matches {
                                "assignment omitted or mismatched the canonical INT8 execution profile"
                                    .to_string()
                            } else if transaction_domain_is_malformed {
                                "assignment carried a malformed recovery transaction domain"
                                    .to_string()
                            } else if required_transaction_domain_is_missing {
                                "assignment requires recovery-domain signing but omitted the transaction domain"
                                    .to_string()
                            } else if let Err(error) = &assignment_submit_window {
                                error.clone()
                            } else {
                                format!(
                                    "invalid assignment bounds (input_bytes={}, max_tokens={})",
                                    input.len(),
                                    max_tokens
                                )
                            };
                            tracing::warn!(
                                job_id,
                                seed = %winner,
                                reason,
                                "declining malformed community assignment"
                            );
                            let failure = rpc::WorkResult {
                                job_id: job_id.clone(),
                                worker_id: worker_id_w.clone(),
                                success: false,
                                // A coordinator identity mismatch is not a
                                // model failure by this worker.
                                declined: !assignment_model_matches
                                    || !assignment_execution_profile_matches,
                                output: String::new(),
                                output_hash: String::new(),
                                tokens_generated: 0,
                                total_ms: 0,
                                ms_per_token: 0,
                                engine: String::new(),
                                error: Some(reason),
                                signed_attestation_hex: None,
                            };
                            let _ = post_signed_community(
                                &client,
                                &winner,
                                rpc::COMMUNITY_SUBMIT_WORK_PATH,
                                failure,
                                &worker_keypair,
                                std::time::Duration::from_secs(10),
                            )
                            .await;
                            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                            continue;
                        }
                        let submission_deadline = deadline_anchor
                            .checked_add(assignment_submit_window.expect(
                                "the malformed-assignment branch rejects invalid deadlines",
                            ))
                            .expect("the protocol submission window fits Tokio's Instant");

                        let input_preview: String = input.chars().take(40).collect();

                        tracing::info!(
                            "Claimed job {} from {}: {:?} (max_tokens={})",
                            job_id,
                            winner,
                            input_preview,
                            max_tokens
                        );

                        // Model generation is CPU-heavy and synchronous. Running it
                        // directly inside this Tokio task starved registration,
                        // heartbeat, claim, and submit I/O on small community hosts.
                        // Move the complete encode/generate/decode path to Tokio's
                        // blocking pool so networking remains responsive.
                        let start = std::time::Instant::now();
                        let inference_model = model.clone();
                        // CachedIntegerModel owns the one configured BOS
                        // forward. Passing a caller-prefixed BOS here made the
                        // worker use a different prompt than ordinary sharded
                        // inference and its independent shard verifier.
                        let inference_tokens = inference_model.encode(&input);
                        let generation_error = if inference_tokens.is_empty() {
                            Some("assigned prompt encoded to zero tokens".to_string())
                        } else {
                            inference_model
                                .preflight_generation(inference_tokens.len(), max_tokens)
                                .err()
                                .map(|error| error.to_string())
                        };
                        if let Some(error) = generation_error {
                            let error = format!(
                                "{error}; worker_context_helper_admitted={}",
                                community_generation_fits_context(
                                    inference_tokens.len(),
                                    max_tokens,
                                    inference_model.config.max_seq,
                                )
                            );
                            tracing::warn!(
                                job_id,
                                seed = %winner,
                                %error,
                                "declining community assignment before blocking compute"
                            );
                            let failure = rpc::WorkResult {
                                job_id: job_id.clone(),
                                worker_id: worker_id_w.clone(),
                                success: false,
                                declined: true,
                                output: String::new(),
                                output_hash: String::new(),
                                tokens_generated: 0,
                                total_ms: 0,
                                ms_per_token: 0,
                                engine: String::new(),
                                error: Some(format!("invalid generation context: {error}")),
                                signed_attestation_hex: None,
                            };
                            let _ = submit_community_result_with_retry(
                                &client,
                                &winner,
                                &failure,
                                &worker_keypair,
                                submission_deadline,
                            )
                            .await;
                            continue;
                        }
                        let inference = tokio::task::spawn_blocking(move || {
                            // The fallible model API is authoritative at this
                            // untrusted boundary. It performs checked context
                            // admission immediately before allocating KV state;
                            // even a tokenizer-expanded prompt can only become
                            // a typed worker failure, never an indexing panic.
                            let (generated, hash) = inference_model
                                .try_generate(
                                    &inference_tokens,
                                    max_tokens,
                                    &inference_model.config.eos_tokens,
                                )
                                .map_err(|error| {
                                    let helper_admitted = community_generation_fits_context(
                                        inference_tokens.len(),
                                        max_tokens,
                                        inference_model.config.max_seq,
                                    );
                                    format!(
                                        "{error}; worker_context_helper_admitted={helper_admitted}"
                                    )
                                })?;
                            let output_text = inference_model.decode(&generated);
                            Ok::<_, String>((generated, hash, output_text))
                        })
                        .await;
                        let (generated, hash, output_text) = match inference {
                            Ok(Ok(result)) => result,
                            Ok(Err(error)) => {
                                tracing::error!(
                                    job_id,
                                    seed = %winner,
                                    %error,
                                    "community inference worker task failed"
                                );
                                let failure = rpc::WorkResult {
                                    job_id: job_id.clone(),
                                    worker_id: worker_id_w.clone(),
                                    success: false,
                                    declined: false,
                                    output: String::new(),
                                    output_hash: String::new(),
                                    tokens_generated: 0,
                                    total_ms: 0,
                                    ms_per_token: 0,
                                    engine: String::new(),
                                    error: Some(format!("local inference task failed: {error}")),
                                    signed_attestation_hex: None,
                                };
                                let _ = submit_community_result_with_retry(
                                    &client,
                                    &winner,
                                    &failure,
                                    &worker_keypair,
                                    submission_deadline,
                                )
                                .await;
                                continue;
                            }
                            Err(error) => {
                                tracing::error!(
                                    job_id,
                                    seed = %winner,
                                    %error,
                                    "community inference worker blocking task aborted"
                                );
                                let failure = rpc::WorkResult {
                                    job_id: job_id.clone(),
                                    worker_id: worker_id_w.clone(),
                                    success: false,
                                    declined: false,
                                    output: String::new(),
                                    output_hash: String::new(),
                                    tokens_generated: 0,
                                    total_ms: 0,
                                    ms_per_token: 0,
                                    engine: String::new(),
                                    error: Some(format!("local inference task aborted: {error}")),
                                    signed_attestation_hex: None,
                                };
                                let _ = submit_community_result_with_retry(
                                    &client,
                                    &winner,
                                    &failure,
                                    &worker_keypair,
                                    submission_deadline,
                                )
                                .await;
                                continue;
                            }
                        };
                        let elapsed_ms = start.elapsed().as_millis() as u64;
                        let tokens_gen = generated.len() as u64;
                        let ms_per_tok = elapsed_ms.checked_div(tokens_gen).unwrap_or(0);

                        tracing::info!(
                            "Job {} done: {} tokens in {}ms = {} ms/tok",
                            job_id,
                            tokens_gen,
                            elapsed_ms,
                            ms_per_tok
                        );

                        // ── Build + sign the InferenceAttestation tx ──
                        // First time only: query the chain for the worker's
                        // current nonce and seed our local counter from
                        // there. Subsequent attestations increment locally.
                        if !attestation_nonce_initialized.load(std::sync::atomic::Ordering::Relaxed)
                        {
                            let q_url =
                                format!("{}/account/0x{}", winner, hex::encode(worker_address.0));
                            if let Ok(resp) = client.get(&q_url).send().await
                                && let Ok(v) = resp.json::<serde_json::Value>().await
                            {
                                let n = v.get("nonce").and_then(|x| x.as_u64()).unwrap_or(0);
                                attestation_nonce.store(n, std::sync::atomic::Ordering::Relaxed);
                                tracing::info!(
                                    starting_nonce = n,
                                    "worker attestation nonce initialized from chain"
                                );
                            }
                            attestation_nonce_initialized
                                .store(true, std::sync::atomic::Ordering::Relaxed);
                        }

                        // Build the InferenceAttestation body. Mirrors what
                        // /inference/run does on the local-served path.
                        let input_hash = arc_crypto::hash_bytes(input.as_bytes());

                        let nonce =
                            attestation_nonce.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        let mut tx = arc_types::Transaction {
                            tx_type: arc_types::TxType::InferenceAttestation,
                            from: worker_address,
                            nonce,
                            body: arc_types::TxBody::InferenceAttestation(
                                arc_types::transaction::InferenceAttestationBody {
                                    model_id: worker_model_id_hash,
                                    input_hash,
                                    output_hash: hash,
                                    challenge_period: 100,
                                    // Bond = 0 for testnet community attestations.
                                    // Production tokenomics will require a non-zero
                                    // bond — that lands with the inference-pool
                                    // reward distribution in a later release.
                                    bond: 0,
                                    beneficiary: None,
                                },
                            ),
                            fee: 0,
                            gas_limit: 0,
                            hash: arc_crypto::Hash256::ZERO,
                            signature: arc_crypto::Signature::null(),
                            sig_verified: false,
                        };

                        let signed_attestation = match assignment_transaction_domain {
                            Some(domain) => tx.sign_in_domain(&worker_keypair, &domain),
                            None => tx.sign(&worker_keypair),
                        };
                        let signed_attestation_hex = match signed_attestation {
                            Ok(()) => bincode::serialize(&tx)
                                .ok()
                                .map(|b| format!("0x{}", hex::encode(b))),
                            Err(e) => {
                                tracing::warn!("attestation sign failed: {:?}", e);
                                None
                            }
                        };

                        let result_body = rpc::WorkResult {
                            job_id: job_id.clone(),
                            worker_id: format!("0x{}", hex::encode(worker_address.0)),
                            success: true,
                            declined: false,
                            output: output_text,
                            output_hash: format!("0x{}", hex::encode(hash.0)),
                            tokens_generated: tokens_gen,
                            total_ms: elapsed_ms,
                            ms_per_token: ms_per_tok,
                            engine: arc_inference::cached_integer_model::CANONICAL_REWARD_INFERENCE_PROFILE
                                .to_string(),
                            error: None,
                            signed_attestation_hex,
                        };

                        let submit_outcome = submit_community_result_with_retry(
                            &client,
                            &winner,
                            &result_body,
                            &worker_keypair,
                            submission_deadline,
                        )
                        .await;

                        // If a terminal response reports invalid_nonce, force
                        // a chain re-query before building the next immutable
                        // attestation. A network timeout is deliberately not
                        // treated as rejection: the server may have succeeded.
                        if let Some(body) = submit_outcome.response_body()
                            && let Ok(body) = serde_json::from_str::<serde_json::Value>(body)
                            && let Some(attestation) = body.get("attestation")
                        {
                            let status = attestation
                                .get("status")
                                .and_then(serde_json::Value::as_str)
                                .unwrap_or("");
                            let error = attestation
                                .get("error")
                                .and_then(serde_json::Value::as_str)
                                .unwrap_or("");
                            if status == "rejected" && error.contains("InvalidNonce") {
                                tracing::warn!(
                                    "attestation nonce drifted; will re-query chain on next submit"
                                );
                                attestation_nonce_initialized
                                    .store(false, std::sync::atomic::Ordering::Relaxed);
                            }
                        }
                    }
                    // Brief sleep between poll rounds to avoid hammering
                    if sleep_or_runtime_shutdown(
                        &mut worker_shutdown,
                        std::time::Duration::from_millis(500),
                    )
                    .await
                    {
                        break;
                    }
                }
                while let Some(result) = decline_tasks.join_next().await {
                    if let Err(error) = result {
                        tracing::warn!(%error, "community decline task failed during shutdown");
                    }
                }
                tracing::info!("Community inference worker stopped at the lifecycle barrier");
            }));
            tracing::info!("Community inference worker loop spawned");
        }
    }

    // Explicit HTTP(S) registry origins only. Consensus/P2P bootstrap
    // addresses never become RPC destinations by port-number convention.
    let coordinator_seed_rpcs = coordinator_rpc_bases;

    // Inference pool width: explicit --threads wins, then [inference] threads
    // from the config file, else 0 = rayon's global pool (which honours
    // RAYON_NUM_THREADS; see config::InferenceConfig).
    let compute_threads =
        if matches.value_source("threads") == Some(clap::parser::ValueSource::CommandLine) {
            cli.threads
        } else {
            node_cfg.inference.threads
        };

    let rpc_result = rpc::serve(
        rpc_listener,
        state.clone(),
        mempool,
        validator_address,
        Some(Arc::new(validator_keypair)),
        stake,
        boot_time,
        peer_count,
        inference_model,
        candle_engine,
        candle_model_id,
        model_artifact_id,
        Some(dag_validators),
        Some(dag_round),
        Some(dag_committed),
        shard_infos,
        coordinator_seed_rpcs,
        community_rpc_bases,
        compute_threads,
        genesis_chain_identity,
        cli.enable_community_rewards_v1,
        Some(shutdown_rx),
    )
    .await;

    let eth_result = if let Some(task) = eth_server_task {
        match task.await {
            Ok(result) => result,
            Err(error) => Err(anyhow::anyhow!("ETH RPC server task join failed: {error}")),
        }
    } else {
        Ok(())
    };

    if shutdown_requested.load(std::sync::atomic::Ordering::SeqCst) {
        // Both HTTP servers have now drained accepted handlers. Close the
        // transport/consensus barrier only at this point, then join every
        // runtime/background task and the dedicated consensus OS thread. Only
        // that completed join set proves there can be no writer racing the
        // final StateDB WAL fsync.
        let _ = runtime_shutdown_tx.send(true);
        let mut runtime_join_error = None::<String>;
        for task in runtime_tasks {
            if let Err(error) = task.await
                && runtime_join_error.is_none()
            {
                runtime_join_error = Some(format!("node runtime task failed to join: {error}"));
            }
        }
        if let Some(thread) = consensus_thread {
            match tokio::task::spawn_blocking(move || thread.join()).await {
                Ok(Ok(())) => {}
                Ok(Err(_)) => {
                    runtime_join_error.get_or_insert_with(|| {
                        "consensus thread panicked during shutdown".to_string()
                    });
                }
                Err(error) => {
                    runtime_join_error.get_or_insert_with(|| {
                        format!("consensus thread join task failed during shutdown: {error}")
                    });
                }
            }
        }

        // A failed fsync must never be logged or returned as a clean exit. Run
        // it even when a prior join reported failure so the best available
        // durable boundary is still established before systemd restarts us.
        let wal_result = state.try_sync_wal();
        if shutdown_exit_code(&wal_result) != 0 {
            let error = wal_result.expect_err("nonzero shutdown status requires WAL failure");
            tracing::error!(%error, "shutdown WAL durability barrier failed");
            return Err(anyhow::anyhow!(
                "shutdown WAL durability barrier failed: {error}"
            ));
        }
        rpc_result?;
        eth_result?;
        if let Some(error) = runtime_join_error {
            return Err(anyhow::anyhow!(error));
        }
        if let Some(receipt) = desktop_shutdown_receipt
            .lock()
            .map_err(|_| anyhow::anyhow!("desktop shutdown receipt lock was poisoned"))?
            .take()
        {
            // This is the node's durable acknowledgement to its supervisor.
            // A nonzero return, panic, forced kill, join failure, or WAL fsync
            // failure reaches no removal path and leaves the receipt armed.
            receipt.acknowledge()?;
        }
        tracing::info!(
            "RPC handlers drained, node writers joined, WAL durability barrier completed, and the desktop receipt was acknowledged; shutdown is clean"
        );
        return Ok(());
    }

    rpc_result?;
    eth_result?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use arc_consensus::{ConsensusEngine, DagBlock, STAKE_ARC, Validator, ValidatorSet};
    use serde_json::json;

    #[test]
    fn shutdown_exit_status_fails_closed_on_wal_barrier_error() {
        assert_eq!(shutdown_exit_code(&Ok(())), 0);
        assert_eq!(
            shutdown_exit_code(&Err(arc_state::StateError::PersistenceError(
                "fsync failed".to_string()
            ))),
            1
        );
    }

    #[test]
    fn shutdown_broadcast_is_two_phase_and_matches_the_managed_stop_budget() {
        let requested = std::sync::atomic::AtomicBool::new(false);
        let (http_tx, http_rx) = tokio::sync::watch::channel(false);
        let (background_tx, background_rx) = tokio::sync::watch::channel(false);
        let (runtime_tx, runtime_rx) = tokio::sync::watch::channel(false);

        broadcast_node_shutdown(&requested, &http_tx, &background_tx);

        assert!(requested.load(std::sync::atomic::Ordering::SeqCst));
        assert!(
            *http_rx.borrow(),
            "HTTP admission did not close at the signal edge"
        );
        assert!(
            *background_rx.borrow(),
            "background admission did not close at the signal edge"
        );
        assert!(
            !*runtime_rx.borrow(),
            "transport/consensus stopped before accepted HTTP handlers drained"
        );

        runtime_tx.send(true).unwrap();
        assert!(*runtime_rx.borrow());
        assert_eq!(
            rpc::PUBLIC_INFERENCE_REQUEST_TIMEOUT_SECS + COMMUNITY_SUBMIT_LATE_GRACE_SECS + 120,
            MANAGED_NODE_STOP_BUDGET_SECS
        );
        assert_eq!(MANAGED_NODE_STOP_BUDGET_SECS, 4_420);
    }

    #[test]
    fn explicit_ephemeral_p2p_port_is_loopback_only_for_community_canaries() {
        let p2p_port = 0;
        let benchmark_mode = false;
        let insecure_dev_validator_seed = false;
        let listen_ip = p2p_listen_ip(p2p_port, benchmark_mode, insecure_dev_validator_seed);
        let socket = std::net::UdpSocket::bind(SocketAddr::from((listen_ip, p2p_port))).unwrap();
        assert!(socket.local_addr().unwrap().ip().is_loopback());

        let public_default = p2p_listen_ip(9945, benchmark_mode, insecure_dev_validator_seed);
        assert!(public_default.is_unspecified());
    }

    fn write_test_desktop_shutdown_token(data_dir: &Path, token: &str) -> PathBuf {
        std::fs::create_dir_all(data_dir).unwrap();
        let control_dir = data_dir.join(DESKTOP_SHUTDOWN_CONTROL_DIR_NAME);
        arc_crypto::secret_file::secure_private_directory(&control_dir).unwrap();
        let path = control_dir.join(DESKTOP_SHUTDOWN_TOKEN_FILE_NAME);
        let mut file = arc_crypto::secret_file::create_new_private(&path).unwrap();
        file.write_all(format!("{token}\n").as_bytes()).unwrap();
        file.sync_all().unwrap();
        drop(file);
        path
    }

    fn write_test_desktop_shutdown_request(path: &Path, pid: u32, token: &str, nonce: &[u8; 32]) {
        let mut file = arc_crypto::secret_file::create_new_private(path).unwrap();
        file.write_all(
            format!(
                "{DESKTOP_SHUTDOWN_REQUEST_SCHEMA}\npid={pid}\ntoken={token}\nnonce={}\n",
                hex::encode(nonce)
            )
            .as_bytes(),
        )
        .unwrap();
        file.sync_all().unwrap();
    }

    #[tokio::test]
    async fn desktop_shutdown_file_requires_the_exact_private_capability() {
        let data_dir = std::env::temp_dir().join(format!(
            "arc-desktop-shutdown-test-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let token = "42".repeat(32);
        let token_file = write_test_desktop_shutdown_token(&data_dir, &token);
        let genesis = data_dir.join("genesis.toml");
        std::fs::write(&genesis, b"chain_id = \"receipt-test\"\n").unwrap();
        let executable = std::env::current_exe().unwrap();
        let token_bytes = [0x42u8; 32];
        let arm = arc_crypto::secret_file::arm_desktop_shutdown_receipt(
            &data_dir,
            &token_bytes,
            &executable,
            &genesis,
        )
        .unwrap();
        let control =
            prepare_desktop_shutdown_control(&data_dir, Some(&token_file), Some(&genesis))
                .unwrap()
                .unwrap();
        assert!(constant_time_token_eq(
            &control.expected_token,
            &control.expected_token
        ));
        let mut different = control.expected_token;
        different[31] ^= 1;
        assert!(!constant_time_token_eq(&control.expected_token, &different));

        write_test_desktop_shutdown_request(
            &control.request_file,
            std::process::id(),
            &"43".repeat(32),
            &arm.nonce,
        );
        assert!(take_authenticated_desktop_shutdown_request(&control).is_err());
        assert!(!control.request_file.exists());

        // A crash-torn/malformed legacy request is consumed after its stable
        // private-handle snapshot, allowing the next atomic publication.
        let mut torn = arc_crypto::secret_file::create_new_private(&control.request_file).unwrap();
        torn.write_all(b"arc.desktop.shutdown.v1\npid=").unwrap();
        torn.sync_all().unwrap();
        drop(torn);
        assert!(take_authenticated_desktop_shutdown_request(&control).is_err());
        assert!(!control.request_file.exists());

        write_test_desktop_shutdown_request(
            &control.request_file,
            std::process::id().wrapping_add(1),
            &token,
            &arm.nonce,
        );
        assert!(
            take_authenticated_desktop_shutdown_request(&control)
                .unwrap()
                .is_none()
        );
        assert!(!control.request_file.exists());

        write_test_desktop_shutdown_request(
            &control.request_file,
            std::process::id(),
            &token,
            &arm.nonce,
        );
        let (_http_tx, http_rx) = tokio::sync::watch::channel(false);
        assert!(
            tokio::time::timeout(
                std::time::Duration::from_secs(1),
                wait_for_authenticated_desktop_shutdown(control, http_rx),
            )
            .await
            .expect("authenticated shutdown watcher timed out")
            .is_some()
        );

        let outside = data_dir.with_extension("outside-token");
        std::fs::write(&outside, format!("{token}\n")).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(&outside, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
        assert!(
            prepare_desktop_shutdown_control(&data_dir, Some(&outside), Some(&genesis)).is_err(),
            "a token outside the locked data directory became a shutdown capability"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(&token_file, std::fs::Permissions::from_mode(0o644)).unwrap();
            assert!(
                prepare_desktop_shutdown_control(&data_dir, Some(&token_file), Some(&genesis))
                    .is_err(),
                "a group/world-readable shutdown token was accepted"
            );
        }
        std::fs::remove_file(outside).unwrap();
        std::fs::remove_dir_all(data_dir).unwrap();
    }

    #[test]
    fn startup_shutdown_gate_skips_later_stages_and_syncs_open_state() {
        let requested = std::sync::atomic::AtomicBool::new(false);
        let receipt = std::sync::Mutex::new(None);
        assert!(!complete_startup_shutdown_if_requested(&requested, None, &receipt).unwrap());
        requested.store(true, std::sync::atomic::Ordering::SeqCst);
        assert!(complete_startup_shutdown_if_requested(&requested, None, &receipt).unwrap());

        let state = StateDB::new();
        assert!(
            complete_startup_shutdown_if_requested(&requested, Some(&state), &receipt).unwrap()
        );
    }

    #[tokio::test]
    async fn late_background_work_is_rejected_while_preexisting_work_drains() {
        let requested = std::sync::atomic::AtomicBool::new(false);
        let (http_tx, http_rx) = tokio::sync::watch::channel(false);
        let (background_tx, background_rx) = tokio::sync::watch::channel(false);
        let (runtime_tx, runtime_rx) = tokio::sync::watch::channel(false);
        let (late_work_tx, late_work_rx) = tokio::sync::oneshot::channel::<()>();
        let (existing_started_tx, existing_started_rx) = tokio::sync::oneshot::channel::<()>();
        let (existing_finish_tx, existing_finish_rx) = tokio::sync::oneshot::channel::<()>();
        let late_work_admitted = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let admitted = late_work_admitted.clone();

        let mut background_task = tokio::spawn(async move {
            let mut children = tokio::task::JoinSet::new();
            children.spawn(async move {
                let _ = existing_started_tx.send(());
                let _ = existing_finish_rx.await;
            });
            let mut shutdown = Some(background_rx);
            tokio::select! {
                biased;
                _ = wait_for_optional_runtime_shutdown(&mut shutdown) => {}
                _ = late_work_rx => {
                    admitted.store(true, std::sync::atomic::Ordering::SeqCst);
                }
            }
            while children.join_next().await.is_some() {}
        });
        existing_started_rx.await.unwrap();

        // This work appears after the lifecycle edge, modeling a claim/tick
        // becoming ready near the end of a long accepted-handler drain.
        broadcast_node_shutdown(&requested, &http_tx, &background_tx);
        let _ = late_work_tx.send(());
        tokio::task::yield_now().await;
        assert!(!late_work_admitted.load(std::sync::atomic::Ordering::SeqCst));
        assert!(*http_rx.borrow());
        assert!(!*runtime_rx.borrow());
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(25), &mut background_task,)
                .await
                .is_err(),
            "preexisting background work was abandoned instead of drained"
        );

        existing_finish_tx.send(()).unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(1), background_task)
            .await
            .expect("preexisting background work did not join")
            .unwrap();
        assert!(
            !*runtime_rx.borrow(),
            "transport/consensus closed before the modeled HTTP drain completed"
        );
        runtime_tx.send(true).unwrap();
        assert!(*runtime_rx.borrow());
    }

    #[test]
    fn protocol_v3_keeps_consensus_but_disables_the_legacy_tier1_worker() {
        assert_eq!(
            node_runtime_roles(true, 2),
            NodeRuntimeRoles {
                chain_participation: true,
                tier1_background_inference: true,
            }
        );
        assert_eq!(
            node_runtime_roles(true, 3),
            NodeRuntimeRoles {
                chain_participation: true,
                tier1_background_inference: false,
            },
            "protocol v3 must keep P2P/consensus live while paid Tier-1 inference is dark"
        );
        assert_eq!(
            node_runtime_roles(false, 2),
            NodeRuntimeRoles {
                chain_participation: false,
                tier1_background_inference: false,
            }
        );
    }

    #[tokio::test]
    async fn signed_holder_populates_empty_coordinator_before_first_refresh_interval() {
        let holder_key = arc_crypto::KeyPair::generate_ed25519();
        let coordinator_key = arc_crypto::KeyPair::generate_ed25519();
        let model_id = hash_bytes(b"signed-holder-startup-model");
        let state = Arc::new(StateDB::new());
        state.seed_genesis_validators(&[
            (
                holder_key.address(),
                arc_state::StateDB::MIN_VALIDATOR_STAKE,
            ),
            (
                coordinator_key.address(),
                arc_state::StateDB::MIN_VALIDATOR_STAKE,
            ),
        ]);

        // The coordinator authenticates the announced execution destination
        // independently from the signed POST, so expose the holder's exact
        // validator identity at its declared shard origin.
        let holder_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let holder_addr = holder_listener.local_addr().unwrap();
        let holder_origin = format!("http://{holder_addr}");
        let holder_address = holder_key.address();
        let holder_app = axum::Router::new()
            .route(
                "/network/info",
                axum::routing::get(move || async move {
                    axum::Json(json!({
                        "validator_address": format!("0x{}", holder_address.to_hex()),
                        "transaction_domain": serde_json::Value::Null,
                        "recovery_active": false,
                    }))
                }),
            )
            .route("/health", axum::routing::get(|| async { "ok" }));
        let holder_server = tokio::spawn(async move {
            axum::serve(holder_listener, holder_app).await.unwrap();
        });

        let reserved = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let coordinator_addr = reserved.local_addr().unwrap();
        drop(reserved);
        let coordinator_origin = format!("http://{coordinator_addr}");
        let (coordinator_shutdown_tx, coordinator_shutdown_rx) = tokio::sync::watch::channel(false);
        let coordinator_server = tokio::spawn({
            let coordinator_addr = coordinator_addr.to_string();
            let coordinator_key = coordinator_key.clone();
            let state = state.clone();
            let holder_origin = holder_origin.clone();
            async move {
                rpc::serve(
                    rpc::RpcListen::Tcp(coordinator_addr),
                    state,
                    Arc::new(Mempool::new(1_000)),
                    coordinator_key.address(),
                    Some(Arc::new(coordinator_key)),
                    arc_state::StateDB::MIN_VALIDATOR_STAKE,
                    Instant::now(),
                    Arc::new(AtomicU32::new(0)),
                    None,
                    None,
                    None,
                    Some(model_id),
                    None,
                    None,
                    None,
                    Vec::new(),
                    Vec::new(),
                    vec![holder_origin],
                    0,
                    None,
                    false,
                    Some(coordinator_shutdown_rx),
                )
                .await
                .unwrap();
            }
        });
        let client = reqwest::Client::new();
        let mut ready = false;
        for _ in 0..100 {
            if client
                .get(format!("{coordinator_origin}/health"))
                .send()
                .await
                .is_ok_and(|response| response.status().is_success())
            {
                ready = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert!(ready, "coordinator RPC did not start");

        let startup_shard = rpc::ShardInfo {
            start_layer: 0,
            end_layer: 1,
            total_layers: 1,
            model_id: format!("0x{}", model_id.to_hex()),
            model_name: "signed-holder-startup-model".to_string(),
            execution_profile:
                arc_inference::cached_integer_model::CANONICAL_REWARD_INFERENCE_PROFILE.to_string(),
            memory_mb: 1,
            full_model_mb: 1,
            socket_addr: holder_origin,
            node_name: "signed-holder".to_string(),
        };
        let broadcaster = tokio::spawn(run_signed_shard_announcement_loop(
            vec![startup_shard],
            vec![coordinator_origin.clone()],
            holder_key,
        ));

        let topology = tokio::time::timeout(
            std::time::Duration::from_secs(SHARD_ANNOUNCEMENT_INTERVAL_SECS),
            async {
                loop {
                    if let Ok(response) = client
                        .get(format!("{coordinator_origin}/shards"))
                        .send()
                        .await
                        && let Ok(body) = response.json::<serde_json::Value>().await
                        && body["shard_count"] == 1
                    {
                        break body;
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(25)).await;
                }
            },
        )
        .await
        .expect("signed holder did not populate coordinator before the first 15-second refresh");
        assert_eq!(
            topology["shards"][0]["execution_profile"],
            arc_inference::cached_integer_model::CANONICAL_REWARD_INFERENCE_PROFILE
        );
        assert_eq!(
            topology["shards"][0]["model_id"],
            format!("0x{}", model_id.to_hex())
        );

        broadcaster.abort();
        coordinator_shutdown_tx.send(true).unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(2), coordinator_server)
            .await
            .expect("coordinator did not drain after its shutdown signal")
            .unwrap();
        holder_server.abort();
    }

    #[test]
    fn validator_http_audience_hash_accepts_one_prefix_and_rejects_malformed_mixed_prefixes() {
        let expected = hash_bytes(b"validator-http-audience");
        let bare = expected.to_hex();
        let prefixed = format!("0x{bare}");
        assert_eq!(
            parse_validator_http_audience_hash(&bare, "validator_address").unwrap(),
            expected
        );
        assert_eq!(
            parse_validator_http_audience_hash(&prefixed, "validator transaction_domain").unwrap(),
            expected
        );

        for malformed in [
            format!("0x{prefixed}"),
            format!("0X{bare}"),
            format!("0x0X{bare}"),
            format!(" {prefixed}"),
            format!("0x{}", &bare[..bare.len() - 1]),
        ] {
            assert!(
                parse_validator_http_audience_hash(&malformed, "validator_address").is_err(),
                "malformed or mixed-prefix audience was accepted: {malformed}"
            );
        }
    }

    #[cfg(not(feature = "benchmark-tools"))]
    #[test]
    fn default_cli_omits_benchmark_mutation_mode() {
        assert!(Cli::try_parse_from(["arc-node", "--benchmark"]).is_err());
    }

    #[cfg(feature = "benchmark-tools")]
    fn isolated_benchmark_cli() -> Cli {
        Cli::try_parse_from([
            "arc-node",
            "--benchmark",
            "--stake",
            "500000",
            "--insecure-dev-validator-seed",
            "--validator-seed",
            "isolated-benchmark-only",
        ])
        .unwrap()
    }

    #[cfg(feature = "benchmark-tools")]
    #[test]
    fn benchmark_runtime_accepts_only_numeric_loopback_devnet() {
        let cli = isolated_benchmark_cli();
        validate_benchmark_runtime(
            &cli,
            "127.0.0.1:9944",
            &["127.42.0.9:9945".to_string(), "[::1]:9946".to_string()],
            &[],
            500_000,
        )
        .unwrap();

        for rpc in ["localhost:9944", "0.0.0.0:9944", "140.82.16.112:9944"] {
            assert!(
                validate_benchmark_runtime(&cli, rpc, &[], &[], 500_000).is_err(),
                "unsafe benchmark RPC was accepted: {rpc}"
            );
        }
        for peer in ["localhost:9945", "0.0.0.0:9945", "140.82.16.112:9945"] {
            assert!(
                validate_benchmark_runtime(
                    &cli,
                    "127.0.0.1:9944",
                    &[peer.to_string()],
                    &[],
                    500_000,
                )
                .is_err(),
                "unsafe benchmark P2P peer was accepted: {peer}"
            );
        }
        assert!(
            validate_benchmark_runtime(
                &cli,
                "127.0.0.1:9944",
                &[],
                &["http://127.0.0.1:9090".to_string()],
                500_000,
            )
            .is_err(),
            "benchmark mode accepted an external RPC role"
        );

        let community_cli = Cli::try_parse_from([
            "arc-node",
            "--benchmark",
            "--stake",
            "500000",
            "--insecure-dev-validator-seed",
            "--validator-seed",
            "isolated-benchmark-only",
            "--community-mode",
        ])
        .unwrap();
        assert!(
            validate_benchmark_runtime(&community_cli, "127.0.0.1:9944", &[], &[], 500_000,)
                .is_err(),
            "benchmark mode accepted a community runtime"
        );
    }

    #[test]
    fn ephemeral_identity_is_only_available_to_strict_loopback_observers() {
        let observer = Cli::try_parse_from(["arc-node", "--stake", "0"]).unwrap();
        assert!(
            validate_identity_runtime(
                &observer,
                "127.0.0.1:9944",
                &["127.9.8.7:9945".to_string(), "[::1]:9946".to_string()],
                &[],
                false,
                false,
                0,
            )
            .unwrap()
        );

        for unsafe_rpc in ["localhost:9944", "0.0.0.0:9944", "140.82.16.112:9944"] {
            assert!(
                validate_identity_runtime(&observer, unsafe_rpc, &[], &[], false, false, 0,)
                    .is_err(),
                "ephemeral identity accepted unsafe RPC {unsafe_rpc}"
            );
        }
        for unsafe_peer in ["localhost:9945", "0.0.0.0:9945", "140.82.16.112:9945"] {
            assert!(
                validate_identity_runtime(
                    &observer,
                    "127.0.0.1:9944",
                    &[unsafe_peer.to_string()],
                    &[],
                    false,
                    false,
                    0,
                )
                .is_err(),
                "ephemeral identity accepted unsafe peer {unsafe_peer}"
            );
        }

        let community = Cli::try_parse_from(["arc-node", "--community-mode"]).unwrap();
        let error = validate_identity_runtime(
            &community,
            "127.0.0.1:9944",
            &[],
            &["https://validator.example".to_string()],
            false,
            false,
            0,
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("persistent node identity"), "{error}");
        assert!(error.contains("preserve it across restarts"), "{error}");

        assert!(
            !validate_identity_runtime(
                &community,
                "0.0.0.0:9944",
                &["validator.example:9945".to_string()],
                &["https://validator.example".to_string()],
                true,
                false,
                0,
            )
            .unwrap(),
            "a configured persistent keyfile should disable ephemeral identity"
        );
    }

    #[test]
    fn insecure_seed_runtime_has_no_public_or_role_override() {
        let seed_cli = Cli::try_parse_from([
            "arc-node",
            "--stake",
            "0",
            "--validator-seed",
            "disposable-local-only",
            "--insecure-dev-validator-seed",
        ])
        .unwrap();
        assert!(
            !validate_identity_runtime(
                &seed_cli,
                "[::1]:9944",
                &["127.0.0.1:9945".to_string()],
                &[],
                false,
                true,
                0,
            )
            .unwrap()
        );

        for (rpc, peers) in [
            ("localhost:9944", Vec::new()),
            ("0.0.0.0:9944", Vec::new()),
            ("127.0.0.1:9944", vec!["validator.example:9945".to_string()]),
        ] {
            assert!(
                validate_identity_runtime(&seed_cli, rpc, &peers, &[], false, true, 0).is_err(),
                "insecure seed accepted unsafe runtime RPC={rpc} peers={peers:?}"
            );
        }

        let community_seed = Cli::try_parse_from([
            "arc-node",
            "--community-mode",
            "--validator-seed",
            "disposable-local-only",
            "--insecure-dev-validator-seed",
        ])
        .unwrap();
        assert!(
            validate_identity_runtime(&community_seed, "127.0.0.1:9944", &[], &[], false, true, 0,)
                .is_err(),
            "insecure seed accepted a community mutation role"
        );

        let missing_flag = Cli::try_parse_from([
            "arc-node",
            "--stake",
            "0",
            "--validator-seed",
            "disposable-local-only",
        ])
        .unwrap();
        assert!(
            validate_identity_runtime(&missing_flag, "127.0.0.1:9944", &[], &[], false, true, 0,)
                .is_err()
        );
    }

    #[test]
    fn community_context_helper_accepts_exact_boundary_and_rejects_plus_one() {
        // try_generate owns the one internal BOS position; the worker passes
        // only the raw one-token prompt.
        assert_eq!(community_generation_required_positions(1, 3), Some(5));
        assert!(community_generation_fits_context(1, 3, 5));
        assert!(!community_generation_fits_context(1, 4, 5));
        assert_eq!(community_generation_required_positions(usize::MAX, 0), None);
    }

    #[test]
    fn community_submit_classifier_retries_only_transient_responses() {
        for status in [
            reqwest::StatusCode::REQUEST_TIMEOUT,
            reqwest::StatusCode::TOO_MANY_REQUESTS,
            reqwest::StatusCode::BAD_GATEWAY,
            reqwest::StatusCode::SERVICE_UNAVAILABLE,
            reqwest::StatusCode::GATEWAY_TIMEOUT,
        ] {
            assert_eq!(
                community_submit_response_disposition(status, "transient"),
                CommunitySubmitResponseDisposition::Retry
            );
        }
        assert_eq!(
            community_submit_response_disposition(
                reqwest::StatusCode::CONFLICT,
                "an authenticated submit for this job is already being verified",
            ),
            CommunitySubmitResponseDisposition::Retry
        );
        assert_eq!(
            community_submit_response_disposition(
                reqwest::StatusCode::CONFLICT,
                r#"{"error":{"code":"submission_in_progress"}}"#,
            ),
            CommunitySubmitResponseDisposition::Retry
        );

        for status in [
            reqwest::StatusCode::BAD_REQUEST,
            reqwest::StatusCode::NOT_FOUND,
            reqwest::StatusCode::GONE,
            reqwest::StatusCode::UNPROCESSABLE_ENTITY,
        ] {
            assert_eq!(
                community_submit_response_disposition(status, "expired/not-found/invalid"),
                CommunitySubmitResponseDisposition::Rejected
            );
        }
        assert_eq!(
            community_submit_response_disposition(
                reqwest::StatusCode::CONFLICT,
                "job already submitted with different output semantics",
            ),
            CommunitySubmitResponseDisposition::Rejected
        );
        assert_eq!(
            community_submit_response_disposition(reqwest::StatusCode::OK, "not-json"),
            CommunitySubmitResponseDisposition::Accepted
        );
    }

    #[test]
    fn community_submit_window_uses_exact_expiry_and_public_outer_cap() {
        let submitted = 1_700_000_000_000i64;
        let now = submitted + 10_000;
        let exact_expiry = u64::try_from(submitted + 45_000).unwrap();
        assert_eq!(
            community_submit_window(submitted, exact_expiry, now).unwrap(),
            std::time::Duration::from_secs(35)
        );

        let beyond_public_cap = u64::try_from(submitted + 10_000_000).unwrap();
        assert_eq!(
            community_submit_window(submitted, beyond_public_cap, now).unwrap(),
            std::time::Duration::from_millis(
                (rpc::PUBLIC_INFERENCE_REQUEST_TIMEOUT_SECS + COMMUNITY_SUBMIT_LATE_GRACE_SECS)
                    * 1_000
                    - 10_000,
            )
        );
        assert!(community_submit_window(submitted, exact_expiry, submitted + 45_000).is_err());
        assert!(
            community_submit_window(
                submitted + (COMMUNITY_ASSIGNMENT_CLOCK_SKEW_SECS as i64 + 1) * 1_000,
                exact_expiry + 120_000,
                submitted,
            )
            .is_err()
        );
    }

    #[test]
    fn community_submit_backoff_is_exponential_jittered_and_capped() {
        let first_low = community_submit_backoff(0, 0);
        let first_high = community_submit_backoff(0, u64::MAX);
        assert!(first_low >= std::time::Duration::from_millis(125));
        assert!(first_high <= std::time::Duration::from_millis(250));

        let capped_low = community_submit_backoff(31, 0);
        let capped_high = community_submit_backoff(31, u64::MAX);
        assert!(capped_low >= std::time::Duration::from_millis(15_000));
        assert!(capped_high <= std::time::Duration::from_millis(30_000));
    }

    fn recovery_test_binding(domain: arc_consensus::ConsensusDomain) -> RecoveryDagBinding {
        RecoveryDagBinding {
            format_version: RECOVERY_DAG_BINDING_VERSION,
            manifest_hash: hash_bytes(b"manifest"),
            consensus_domain: domain,
            validator_set_commitment: hash_bytes(b"fixed-checkpoint-validator-set"),
            source_height: 900,
            transition_height: 901,
            source_consensus_round: 100,
            initial_consensus_round: 101,
        }
    }

    fn recovery_test_generation_binding(binding: &RecoveryDagBinding) -> GenerationDagBinding {
        GenerationDagBinding {
            recovery_manifest_hash: binding.manifest_hash,
            recovery_domain: binding.consensus_domain.domain_hash,
            validator_set_commitment: binding.validator_set_commitment,
        }
    }

    fn recovery_test_block(
        author: Hash256,
        round: u64,
        parents: Vec<Hash256>,
        domain: &arc_consensus::ConsensusDomain,
    ) -> DagBlock {
        let transactions = Vec::new();
        let ordering_commitment = DagBlock::compute_ordering_commitment(&transactions);
        let mut block = DagBlock {
            author,
            round,
            parents,
            transactions,
            timestamp: round,
            hash: Hash256::ZERO,
            signature: Vec::new(),
            ordering_commitment,
        };
        block.hash = block.compute_hash_in_domain(domain);
        block
    }

    fn recovery_test_engine(
        validators: &[Hash256],
        domain: &arc_consensus::ConsensusDomain,
    ) -> ConsensusEngine {
        let set = ValidatorSet::new(
            validators
                .iter()
                .enumerate()
                .map(|(index, address)| Validator::new(*address, STAKE_ARC, index as u16).unwrap())
                .collect(),
            1,
        );
        let engine = ConsensusEngine::new(set, validators[0]);
        engine.install_consensus_domain(domain.clone()).unwrap();
        engine
    }

    #[test]
    fn sha256_streaming_matches_known_vector() {
        let test_dir =
            std::env::temp_dir().join(format!("arc-model-hash-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&test_dir).unwrap();
        let file = test_dir.join("model.gguf");
        std::fs::write(&file, b"abc").unwrap();

        assert_eq!(
            sha256_of(file.to_str().unwrap()).as_deref(),
            Some("ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad")
        );

        std::fs::remove_dir_all(&test_dir).unwrap();
    }

    #[test]
    fn automatic_model_discovery_skips_same_named_wrong_bytes() {
        let test_dir =
            std::env::temp_dir().join(format!("arc-model-discovery-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&test_dir).unwrap();
        let wrong = test_dir.join("llama2-7b.gguf");
        let exact = test_dir.join("llama-2-7b-chat.Q4_K_M.gguf");
        std::fs::write(&wrong, b"same size is not enough").unwrap();
        std::fs::write(&exact, b"canonical test artifact").unwrap();
        let exact_digest = sha256_of(exact.to_str().unwrap()).unwrap();

        let selected = discover_matching_model(
            vec![
                wrong.to_string_lossy().into_owned(),
                exact.to_string_lossy().into_owned(),
            ],
            &exact_digest,
        );
        assert_eq!(selected.as_deref(), exact.to_str());

        std::fs::remove_dir_all(&test_dir).unwrap();
    }

    #[test]
    fn automatic_model_source_is_immutable_and_digest_bound() {
        assert_eq!(DEFAULT_MODEL_SOURCES.len(), 1);
        assert!(
            DEFAULT_MODEL_SOURCES[0].contains("/resolve/191239b3e26b2882fb562ffccdd1cf0f65402adb/")
        );
        assert!(!DEFAULT_MODEL_SOURCES[0].contains("/resolve/main/"));
        assert_eq!(
            TESTNET_MODEL_SHA256,
            "08a5566d61d7cb6b420c3e4387a39e0078e1f2fe5f055f3a03887385304d4bfa"
        );
    }

    #[test]
    fn shipped_complete_genesis_keeps_stake_zero_community_worker_off_chain_transport() {
        let cli =
            Cli::try_parse_from(["arc-node", "--community", "--genesis", "genesis.toml"]).unwrap();
        let genesis: config::GenesisConfig =
            toml::from_str(include_str!("../../../genesis.toml")).unwrap();

        assert_eq!(cli.stake, 0);
        assert!(genesis.chain.validator_set_complete);
        assert!(!is_genesis_migration_observer(
            Some(&genesis),
            cli.stake,
            cli.insecure_dev_validator_seed,
        ));
        assert_eq!(genesis.validated_validator_set(false).unwrap().len(), 6);
        assert_ne!(genesis.network_hash(false).unwrap(), Hash256::ZERO);
        assert!(
            !chain_participation_allowed(cli.stake, false, cli.insecure_dev_validator_seed),
            "a stake-zero community worker must not start chain P2P or consensus"
        );
    }

    #[test]
    fn complete_genesis_keeps_stake_zero_community_worker_off_consensus_transport() {
        let cli = Cli::try_parse_from([
            "arc-node",
            "--community",
            "--community-rpc-url",
            "https://validator.example",
        ])
        .unwrap();
        let mut genesis: config::GenesisConfig =
            toml::from_str(include_str!("../../../genesis.toml")).unwrap();
        genesis.chain.validator_set_complete = true;
        let migration_observer = is_genesis_migration_observer(
            Some(&genesis),
            cli.stake,
            cli.insecure_dev_validator_seed,
        );
        assert!(
            !migration_observer,
            "the validator set is declared complete"
        );
        assert_eq!(cli.stake, 0);
        assert!(
            !chain_participation_allowed(
                cli.stake,
                migration_observer,
                cli.insecure_dev_validator_seed,
            ),
            "stake-zero workers must not start QUIC, gossip, or consensus"
        );
        assert!(
            !cli.no_community && !cli.community_rpc_urls.is_empty(),
            "outbound authenticated community HTTP remains enabled"
        );
        assert!(chain_participation_allowed(1, false, false));
        assert!(chain_participation_allowed(0, false, true));
    }

    #[test]
    fn cli_defaults_keep_unauthenticated_rpc_local_and_eth_disabled() {
        let cli = Cli::try_parse_from(["arc-node"]).unwrap();
        assert_eq!(cli.rpc, "127.0.0.1:9944");
        assert!(cli.rpc_unix.is_none());
        assert_eq!(cli.eth_rpc_port, 0);
        assert!(cli.community_rpc_urls.is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn production_rpc_unix_listener_is_exclusive_and_archive_selects_exactly_one_transport() {
        let cli = Cli::try_parse_from(["arc-node", "--rpc-unix", "/run/arc/rpc.sock"])
            .expect("Unix RPC listener should not require the default TCP listener");
        assert_eq!(
            cli.rpc_unix.as_deref(),
            Some(std::path::Path::new("/run/arc/rpc.sock"))
        );
        assert!(
            Cli::try_parse_from([
                "arc-node",
                "--rpc",
                "127.0.0.1:9944",
                "--rpc-unix",
                "/run/arc/rpc.sock",
            ])
            .is_err(),
            "explicit TCP and Unix RPC listeners must conflict"
        );

        let required = [
            "archive",
            "serve",
            "--archive-manifest",
            "/sealed/ARCHIVE-MANIFEST.json",
            "--complete",
            "/sealed/COMPLETE.json",
            "--inventory",
            "/sealed/legacy-nyc.inventory",
            "--binding-index",
            "/sealed/binding.files.sha256",
            "--binding",
            "/sealed/binding.json",
            "--checkpoint",
            "/sealed/candidate.arcchkpt",
            "--expected-archive-manifest-sha256",
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "--expected-complete-sha256",
            "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
            "--node",
            "nyc",
        ];
        let mut unix_args = vec!["arc-node"];
        unix_args.extend(required);
        unix_args.extend(["--listen-unix", "/run/arc-archive/rpc.sock"]);
        assert!(Cli::try_parse_from(unix_args).is_ok());

        let mut missing_args = vec!["arc-node"];
        missing_args.extend(required);
        assert!(Cli::try_parse_from(missing_args).is_err());

        let mut both_args = vec!["arc-node"];
        both_args.extend(required);
        both_args.extend([
            "--listen",
            "127.0.0.1:9950",
            "--listen-unix",
            "/run/arc-archive/rpc.sock",
        ]);
        assert!(Cli::try_parse_from(both_args).is_err());
    }

    #[test]
    fn community_rpc_url_is_repeatable_and_separate_from_p2p() {
        let cli = Cli::try_parse_from([
            "arc-node",
            "--peers",
            "seed-p2p.example:9945",
            "--community-rpc-url",
            "https://seed-a.example",
            "--community-rpc-url",
            "https://seed-b.example:9443",
        ])
        .unwrap();
        assert_eq!(cli.peers, ["seed-p2p.example:9945"]);
        assert_eq!(
            cli.community_rpc_urls,
            [
                "https://seed-a.example".to_string(),
                "https://seed-b.example:9443".to_string(),
            ]
        );
    }

    #[test]
    fn full_integer_worker_is_explicit_stake_zero_and_never_a_shard_holder() {
        let valid = Cli::try_parse_from([
            "arc-node",
            "--stake",
            "0",
            "--model",
            "/models/llama2-7b.gguf",
            "--full-integer-worker",
            "--community-rpc-url",
            "https://seed.example",
        ])
        .unwrap();
        let origins = vec!["https://seed.example".to_string()];
        validate_full_integer_worker_role(&valid, 0, &origins).unwrap();
        assert!(valid.shard_ranges.is_empty());
        assert!(valid.shard_start.is_none());
        assert!(valid.shard_end.is_none());

        assert!(validate_full_integer_worker_role(&valid, 1, &origins).is_err());
        assert!(validate_full_integer_worker_role(&valid, 0, &[]).is_err());

        for incompatible in [
            "--tokenizer-only",
            "--enable-i16",
            "--no-community",
            "--auto-shard-join",
        ] {
            let cli = Cli::try_parse_from([
                "arc-node",
                "--stake",
                "0",
                "--model",
                "/models/llama2-7b.gguf",
                "--full-integer-worker",
                "--community-rpc-url",
                "https://seed.example",
                incompatible,
            ])
            .unwrap();
            assert!(
                validate_full_integer_worker_role(&cli, 0, &origins).is_err(),
                "{incompatible} must be rejected"
            );
        }

        let ranged = Cli::try_parse_from([
            "arc-node",
            "--stake",
            "0",
            "--model",
            "/models/llama2-7b.gguf",
            "--full-integer-worker",
            "--community-rpc-url",
            "https://seed.example",
            "--shard-range",
            "0:32",
        ])
        .unwrap();
        assert!(validate_full_integer_worker_role(&ranged, 0, &origins).is_err());
    }

    #[test]
    fn recovery_operator_subcommands_have_stable_required_inputs() {
        let cli = Cli::try_parse_from([
            "arc-node",
            "recovery",
            "verify",
            "--checkpoint",
            "candidate.arcchkpt",
            "--genesis",
            "genesis.toml",
            "--approved-manifest-hash",
            &"11".repeat(32),
        ])
        .unwrap();
        let Some(OperatorCommand::Recovery {
            command:
                RecoveryCommand::Verify {
                    recovery_epoch,
                    validator_set_id,
                    ..
                },
        }) = cli.operator_command
        else {
            panic!("recovery verify command was not parsed")
        };
        assert_eq!(recovery_epoch, 1);
        assert_eq!(validator_set_id, 1);

        assert!(
            Cli::try_parse_from([
                "arc-node",
                "recovery",
                "import",
                "--checkpoint",
                "candidate.arcchkpt",
            ])
            .is_err(),
            "activation must require data-dir, genesis, and the exact manifest pin"
        );
    }

    #[test]
    fn recovery_dag_binding_archives_legacy_history_and_refuses_ambiguity() {
        let data_dir = std::env::temp_dir().join(format!(
            "arc-recovery-dag-binding-test-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir(&data_dir).unwrap();
        let legacy = data_dir.join("dag-wal");
        std::fs::create_dir(&legacy).unwrap();
        std::fs::write(legacy.join("wal-00000000.bin"), b"legacy-history").unwrap();
        let manifest_hash = hash_bytes(b"manifest");

        let archive = archive_legacy_dag_wal(&data_dir, manifest_hash)
            .unwrap()
            .expect("legacy WAL should be archived");
        assert!(!legacy.exists());
        assert_eq!(
            std::fs::read(archive.join("wal-00000000.bin")).unwrap(),
            b"legacy-history"
        );

        std::fs::create_dir(&legacy).unwrap();
        assert!(archive_legacy_dag_wal(&data_dir, manifest_hash).is_err());
        std::fs::remove_dir_all(&data_dir).unwrap();
    }

    #[test]
    fn node_data_directory_lock_rejects_concurrent_owner_and_survives_stale_file() {
        let data_dir =
            std::env::temp_dir().join(format!("arc-node-data-lock-test-{}", uuid::Uuid::new_v4()));
        let first = acquire_node_data_dir_lock(&data_dir).unwrap();
        let error = acquire_node_data_dir_lock(&data_dir)
            .err()
            .expect("a second process handle must not share one data directory");
        assert!(error.to_string().contains("already locked"), "{error}");

        drop(first);
        let second = acquire_node_data_dir_lock(&data_dir)
            .expect("the persistent lock file must be reusable after owner exit");
        drop(second);
        std::fs::remove_dir_all(&data_dir).unwrap();
    }

    #[test]
    fn recovery_dag_binding_round_trip_is_exact_and_tamper_visible() {
        let data_dir = std::env::temp_dir().join(format!(
            "arc-recovery-dag-file-test-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir(&data_dir).unwrap();
        let path = data_dir.join(RECOVERY_DAG_BINDING_FILE);
        let binding = recovery_test_binding(arc_consensus::ConsensusDomain::new(
            hash_bytes(b"domain"),
            7,
            11,
        ));
        write_recovery_dag_binding_atomically(&path, &binding).unwrap();
        assert_eq!(read_recovery_dag_binding(&path).unwrap(), binding);

        let mut tampered = binding.clone();
        tampered.validator_set_commitment = hash_bytes(b"mutable-live-validator-set");
        std::fs::write(&path, serde_json::to_vec_pretty(&tampered).unwrap()).unwrap();
        assert_ne!(read_recovery_dag_binding(&path).unwrap(), binding);

        let staged_dir = data_dir.join("staged-crash");
        std::fs::create_dir(&staged_dir).unwrap();
        let staged_path = staged_dir.join(format!(".{RECOVERY_DAG_BINDING_FILE}.new"));
        std::fs::write(&staged_path, serde_json::to_vec_pretty(&binding).unwrap()).unwrap();
        activate_recovery_dag_binding(&staged_dir, &binding).unwrap();
        assert!(!staged_path.exists());
        assert_eq!(
            read_recovery_dag_binding(&staged_dir.join(RECOVERY_DAG_BINDING_FILE)).unwrap(),
            binding
        );

        let mismatched_dir = data_dir.join("mismatched-staged-crash");
        std::fs::create_dir(&mismatched_dir).unwrap();
        std::fs::write(
            mismatched_dir.join(format!(".{RECOVERY_DAG_BINDING_FILE}.new")),
            serde_json::to_vec_pretty(&tampered).unwrap(),
        )
        .unwrap();
        assert!(activate_recovery_dag_binding(&mismatched_dir, &binding).is_err());
        std::fs::remove_dir_all(&data_dir).unwrap();
    }

    #[test]
    fn recovery_dag_startup_quarantines_only_a_torn_final_active_batch() {
        let data_dir = std::env::temp_dir().join(format!(
            "arc-recovery-dag-torn-startup-test-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir(&data_dir).unwrap();
        let store = GenerationStore::new(data_dir.join("store"));
        let local_binding = recovery_test_binding(arc_consensus::ConsensusDomain::new(
            hash_bytes(b"domain"),
            7,
            11,
        ));
        let generation = store
            .create_initial(
                GenerationInput {
                    binding: recovery_test_generation_binding(&local_binding),
                    baseline_state: DagBaselineState {
                        height: 901,
                        block_hash: hash_bytes(b"block"),
                        state_root: hash_bytes(b"root"),
                    },
                    dag_cursor: DagCursor {
                        committed_block_count: 0,
                        next_dag_round: 101,
                        current_round: 101,
                        retention_floor_round: 101,
                        retention_ceiling_round: recovery_retention_ceiling(101).unwrap(),
                    },
                    retention_limits: RetentionLimits::default(),
                },
                std::iter::empty(),
            )
            .unwrap();
        let active_path = store.active_log_path(generation.pin);
        let valid_prefix_bytes = std::fs::metadata(&active_path).unwrap().len();
        let mut active = OpenOptions::new().append(true).open(&active_path).unwrap();
        active.write_all(&[0, 1]).unwrap();
        active.sync_all().unwrap();
        drop(active);

        let (records, summary) = stage_recovery_generation_records(&store, &generation).unwrap();
        assert!(records.is_empty());
        assert_eq!(summary.active_suffix, TornSuffix::Clean);
        assert_eq!(
            std::fs::metadata(&active_path).unwrap().len(),
            valid_prefix_bytes
        );
        assert!(
            std::fs::read_dir(store.root())
                .unwrap()
                .filter_map(|entry| entry.ok())
                .any(|entry| entry.file_name().to_string_lossy().contains(".torn-")),
            "the exact torn suffix must remain quarantined beside the generation store"
        );
        std::fs::remove_dir_all(&data_dir).unwrap();
    }

    #[test]
    fn recovery_dag_startup_repairs_exact_initial_and_successor_pin_crash_windows() {
        let data_dir = std::env::temp_dir().join(format!(
            "arc-recovery-dag-pin-crash-test-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir(&data_dir).unwrap();
        let state = StateDB::with_genesis(&[(hash_bytes(b"funded"), 100)]);
        state
            .execute_block_adaptive_at(&[], hash_bytes(b"producer"), 1)
            .unwrap();
        let mut local_binding = recovery_test_binding(arc_consensus::ConsensusDomain::new(
            hash_bytes(b"domain"),
            7,
            11,
        ));
        local_binding.source_height = 0;
        local_binding.transition_height = 1;
        let startup = RecoveryDagStartup {
            data_dir: data_dir.clone(),
            wal_dir: data_dir.join("generation-store"),
            binding: local_binding,
            archived_legacy_wal: None,
        };
        let store = GenerationStore::new(&startup.wal_dir);
        let baseline = canonical_dag_baseline(&state).unwrap();
        let input = GenerationInput {
            binding: recovery_test_generation_binding(&startup.binding),
            baseline_state: baseline,
            dag_cursor: DagCursor {
                committed_block_count: 0,
                next_dag_round: startup.binding.initial_consensus_round,
                current_round: startup.binding.initial_consensus_round,
                retention_floor_round: startup.binding.initial_consensus_round,
                retention_ceiling_round: recovery_retention_ceiling(
                    startup.binding.initial_consensus_round,
                )
                .unwrap(),
            },
            retention_limits: RetentionLimits::default(),
        };

        // Simulate process death after initial CURRENT publication but before
        // the independent pin rename.
        let initial = store
            .create_initial(input.clone(), std::iter::empty())
            .unwrap();
        assert!(
            read_recovery_dag_pin(&data_dir, startup.binding.manifest_hash)
                .unwrap()
                .is_none()
        );
        let (store, resumed_initial) =
            initialize_recovery_generation_store(&state, &startup).unwrap();
        assert_eq!(resumed_initial.pin, initial.pin);
        assert_eq!(
            read_recovery_dag_pin(&data_dir, startup.binding.manifest_hash)
                .unwrap()
                .unwrap()
                .generation,
            initial.pin
        );

        // Simulate death after successor CURRENT rename but before advancing
        // the independent pin. Only that exact direct successor is accepted.
        let successor = store
            .append(initial.pin, input, std::iter::empty())
            .unwrap();
        assert_eq!(
            read_recovery_dag_pin(&data_dir, startup.binding.manifest_hash)
                .unwrap()
                .unwrap()
                .generation,
            initial.pin
        );
        let (_, resumed_successor) =
            initialize_recovery_generation_store(&state, &startup).unwrap();
        assert_eq!(resumed_successor.pin, successor.pin);
        assert_eq!(
            read_recovery_dag_pin(&data_dir, startup.binding.manifest_hash)
                .unwrap()
                .unwrap()
                .generation,
            successor.pin
        );
        std::fs::remove_dir_all(&data_dir).unwrap();
    }

    #[test]
    fn live_rollover_projection_counts_shared_500_transaction_bodies_once_per_round() {
        let data_dir = std::env::temp_dir().join(format!(
            "arc-recovery-dag-shared-body-projection-test-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir(&data_dir).unwrap();
        let local_binding = recovery_test_binding(arc_consensus::ConsensusDomain::new(
            hash_bytes(b"shared-body-projection-domain"),
            7,
            11,
        ));
        let store = GenerationStore::new(data_dir.join("generation-store"));
        let generation = store
            .create_initial(
                GenerationInput {
                    binding: recovery_test_generation_binding(&local_binding),
                    baseline_state: DagBaselineState {
                        height: 1,
                        block_hash: hash_bytes(b"shared-body-baseline-block"),
                        state_root: hash_bytes(b"shared-body-baseline-root"),
                    },
                    dag_cursor: DagCursor {
                        committed_block_count: 0,
                        next_dag_round: 100,
                        current_round: 100,
                        retention_floor_round: 100,
                        retention_ceiling_round: 4_196,
                    },
                    retention_limits: RetentionLimits::default(),
                },
                std::iter::empty(),
            )
            .unwrap();
        let bodies: Vec<_> = (0..500u64)
            .map(|index| {
                RetainedDagRecord::transaction(
                    100,
                    hash_bytes(format!("shared-transaction-{index}").as_bytes()),
                    index.to_be_bytes().to_vec(),
                )
            })
            .collect();
        let mut writer = store
            .open_current_active_writer(&generation.manifest.binding, generation.pin)
            .unwrap();

        for validator in 0..6u64 {
            let mut batch = bodies.clone();
            batch.push(RetainedDagRecord::dag_block(
                100,
                hash_bytes(format!("validator-block-{validator}").as_bytes()),
                validator.to_be_bytes().to_vec(),
            ));
            let exact = writer.project_batch_usage(&batch).unwrap();
            assert_eq!(exact.appended_records, if validator == 0 { 501 } else { 1 });
            assert_eq!(
                exact.idempotently_omitted_records,
                if validator == 0 { 0 } else { 500 }
            );
            let projected =
                LiveRecoveryDagRollover::projected_usage(&generation, &writer, &batch).unwrap();
            assert_eq!(projected.0, 501 + validator);
            writer
                .append_batch(&batch, arc_node::recovery_dag_wal::ActiveDurability::Fsync)
                .unwrap();
        }
        assert_eq!(writer.inspection().record_count, 506);
        assert!(
            !LiveRecoveryDagRollover::projection_needs_rollover(
                &generation,
                LiveRecoveryDagRollover::projected_usage(&generation, &writer, &[]).unwrap(),
            ),
            "six 500-tx validator blocks must consume 506 records, not 3,006"
        );
        drop(writer);
        drop(store);
        std::fs::remove_dir_all(data_dir).unwrap();
    }

    #[test]
    fn recovery_dag_restart_replays_compacts_and_repairs_certified_commit() {
        let data_dir = std::env::temp_dir().join(format!(
            "arc-recovery-dag-replay-test-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir(&data_dir).unwrap();
        let domain = arc_consensus::ConsensusDomain::new(hash_bytes(b"domain"), 7, 11);
        let mut binding = recovery_test_binding(domain.clone());
        binding.source_height = 0;
        binding.transition_height = 0;
        let validators: Vec<Hash256> = (0..4).map(|index| hash_bytes(&[index as u8])).collect();
        let bootstrap = binding.initial_consensus_round;

        let transaction_key = arc_crypto::KeyPair::generate_ed25519();
        let mut transaction = arc_types::Transaction::new_transfer(
            transaction_key.address(),
            hash_bytes(b"recipient"),
            7,
            0,
        );
        transaction
            .sign_in_domain(&transaction_key, &domain.domain_hash)
            .unwrap();

        let mut round_0: Vec<_> = validators
            .iter()
            .map(|author| recovery_test_block(*author, bootstrap, Vec::new(), &domain))
            .collect();
        round_0[0].transactions = vec![transaction.hash];
        round_0[0].ordering_commitment =
            DagBlock::compute_ordering_commitment(&round_0[0].transactions);
        round_0[0].hash = round_0[0].compute_hash_in_domain(&domain);
        let parents_0: Vec<_> = round_0.iter().map(|block| block.hash).collect();
        let round_1: Vec<_> = validators
            .iter()
            .map(|author| recovery_test_block(*author, bootstrap + 1, parents_0.clone(), &domain))
            .collect();
        let parents_1: Vec<_> = round_1.iter().map(|block| block.hash).collect();
        let round_2: Vec<_> = validators
            .iter()
            .map(|author| recovery_test_block(*author, bootstrap + 2, parents_1.clone(), &domain))
            .collect();

        let mut sorted_validators = validators.clone();
        sorted_validators.sort_by_key(|address| address.0);
        let leader = sorted_validators[bootstrap as usize % sorted_validators.len()];
        let leader_hash = round_0
            .iter()
            .find(|block| block.author == leader)
            .unwrap()
            .hash;
        let leader_block = round_0
            .iter()
            .find(|block| block.hash == leader_hash)
            .unwrap();
        let state = StateDB::with_genesis(&[(transaction_key.address(), 100)]);
        let canonical_transactions = if leader_block.transactions.is_empty() {
            Vec::new()
        } else {
            vec![transaction.clone()]
        };
        state
            .execute_block_adaptive_at_with_proof(
                &canonical_transactions,
                leader,
                bootstrap,
                leader_block.state_decision_commitment(&domain),
            )
            .unwrap();
        let generation_records: Vec<_> = std::iter::once(RetainedDagRecord::transaction(
            bootstrap,
            transaction.hash,
            bincode::serialize(&transaction).unwrap(),
        ))
        .chain(round_0.iter().chain(&round_1).chain(&round_2).map(|block| {
            RetainedDagRecord::dag_block(
                block.round,
                block.hash,
                bincode::serialize(block).unwrap(),
            )
        }))
        .chain(std::iter::once(RetainedDagRecord::commit(
            bootstrap,
            leader_hash,
        )))
        .collect();
        let startup = RecoveryDagStartup {
            data_dir: data_dir.clone(),
            wal_dir: data_dir.join("generation-store"),
            binding,
            archived_legacy_wal: None,
        };
        let store = GenerationStore::new(&startup.wal_dir);
        let genesis = state.get_block(0).unwrap();
        let generation = store
            .create_initial(
                GenerationInput {
                    binding: recovery_test_generation_binding(&startup.binding),
                    baseline_state: DagBaselineState {
                        height: 0,
                        block_hash: genesis.hash,
                        state_root: hash_bytes(b"test-transition-root"),
                    },
                    dag_cursor: DagCursor {
                        committed_block_count: 0,
                        next_dag_round: bootstrap,
                        // The immutable cursor predates the later staged
                        // blocks; replay must re-derive +3 from their quorum.
                        current_round: bootstrap,
                        retention_floor_round: bootstrap,
                        retention_ceiling_round: recovery_retention_ceiling(bootstrap).unwrap(),
                    },
                    // Four validators across three rounds plus one body and
                    // one commit produce fourteen records. A sixteen-record
                    // test limit crosses the 90% live-rollover watermark and
                    // exercises rollover without a production-sized fixture.
                    retention_limits: RetentionLimits {
                        max_records: 16,
                        max_payload_bytes: 16 * 1024 * 1024,
                    },
                },
                generation_records.clone(),
            )
            .unwrap();
        let (staged, _) = stage_recovery_generation_records(&store, &generation).unwrap();
        assert_eq!(staged, generation_records);

        for _ in 0..2 {
            let engine = recovery_test_engine(&validators, &domain);
            let replay =
                replay_recovery_dag_generation(&engine, &state, &startup, &generation, &staged)
                    .unwrap();
            assert_eq!(replay.current_round, bootstrap + 3);
            assert_eq!(replay.next_commit_round, bootstrap + 1);
            assert_eq!(
                replay
                    .transactions
                    .iter()
                    .map(|transaction| transaction.hash)
                    .collect::<Vec<_>>(),
                vec![transaction.hash]
            );
            assert!(replay.repaired_commit.is_none());
            assert_eq!(engine.committed_blocks(), vec![leader_hash]);
        }

        let compact_engine = recovery_test_engine(&validators, &domain);
        replay_recovery_dag_generation(&compact_engine, &state, &startup, &generation, &staged)
            .unwrap();
        let active_writer = store
            .open_current_active_writer(&generation.manifest.binding, generation.pin)
            .unwrap();
        let live_rollover = LiveRecoveryDagRollover {
            store: GenerationStore::new(&startup.wal_dir),
            startup: startup.clone(),
            current: parking_lot::Mutex::new(generation.clone()),
        };
        let successor_writer = live_rollover
            .prepare_append(&state, &compact_engine, active_writer, &[])
            .unwrap();
        drop(successor_writer);
        let compacted = live_rollover.current.lock().clone();
        assert_eq!(compacted.pin.sequence, 1);
        assert_eq!(compacted.manifest.baseline_state.height, 1);
        assert_eq!(
            read_recovery_dag_pin(&data_dir, startup.binding.manifest_hash)
                .unwrap()
                .unwrap()
                .generation,
            compacted.pin
        );
        let (compacted_records, compacted_summary) =
            stage_recovery_generation_records(&store, &compacted).unwrap();
        assert_eq!(compacted_summary.active_record_count, 0);
        assert!(compacted_records.iter().all(
            |record| record.kind != RetainedRecordKind::Commit && record.round > bootstrap
        ));
        let restart_engine = recovery_test_engine(&validators, &domain);
        let replay = replay_recovery_dag_generation(
            &restart_engine,
            &state,
            &startup,
            &compacted,
            &compacted_records,
        )
        .unwrap();
        assert_eq!(replay.next_commit_round, bootstrap + 1);
        assert!(replay.repaired_commit.is_none());

        let repair_dir = std::env::temp_dir().join(format!(
            "arc-recovery-dag-repair-test-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir(&repair_dir).unwrap();
        let repair_startup = RecoveryDagStartup {
            data_dir: repair_dir.clone(),
            wal_dir: repair_dir.join("generation-store"),
            binding: startup.binding.clone(),
            archived_legacy_wal: None,
        };
        let repair_store = GenerationStore::new(&repair_startup.wal_dir);
        let repair_records: Vec<_> = generation_records
            .iter()
            .filter(|record| record.kind != RetainedRecordKind::Commit)
            .cloned()
            .collect();
        let repair_generation = repair_store
            .create_initial(
                GenerationInput {
                    binding: recovery_test_generation_binding(&repair_startup.binding),
                    baseline_state: generation.manifest.baseline_state.clone(),
                    dag_cursor: generation.manifest.dag_cursor.clone(),
                    retention_limits: RetentionLimits::default(),
                },
                repair_records.clone(),
            )
            .unwrap();
        let repair_engine = recovery_test_engine(&validators, &domain);
        let replay = replay_recovery_dag_generation(
            &repair_engine,
            &state,
            &repair_startup,
            &repair_generation,
            &repair_records,
        )
        .unwrap();
        assert_eq!(replay.next_commit_round, bootstrap + 1);
        assert_eq!(replay.repaired_commit, Some((leader_hash, bootstrap)));
        assert_eq!(repair_engine.committed_blocks(), vec![leader_hash]);

        let wrong_domain = arc_consensus::ConsensusDomain::new(hash_bytes(b"wrong"), 7, 11);
        let engine = recovery_test_engine(&validators, &wrong_domain);
        assert!(
            replay_recovery_dag_generation(&engine, &state, &startup, &generation, &staged,)
                .is_err()
        );
        std::fs::remove_dir_all(&repair_dir).unwrap();
        std::fs::remove_dir_all(&data_dir).unwrap();
    }

    #[test]
    fn legacy_recovery_validator_file_accepts_only_canonical_eight_positive_stakes() {
        let path = std::env::temp_dir().join(format!(
            "arc-legacy-validator-set-{}.json",
            uuid::Uuid::new_v4()
        ));
        let mut records: Vec<_> = (0..8)
            .map(|index| {
                json!({
                    "address": hash_bytes(format!("legacy-{index}").as_bytes()).to_hex(),
                    "stake": 5_000_000,
                })
            })
            .collect();
        std::fs::write(&path, serde_json::to_vec(&records).unwrap()).unwrap();
        let parsed = load_legacy_recovery_validator_file(path.to_str().unwrap()).unwrap();
        assert_eq!(parsed.len(), 8);
        assert_eq!(parsed.iter().map(|entry| entry.1).sum::<u64>(), 40_000_000);

        records.extend((0..10).map(|index| {
            json!({
                "address": hash_bytes(format!("phantom-peer-{index}").as_bytes()).to_hex(),
                "stake": 0,
            })
        }));
        std::fs::write(&path, serde_json::to_vec(&records).unwrap()).unwrap();
        let error = load_legacy_recovery_validator_file(path.to_str().unwrap()).unwrap_err();
        assert!(format!("{error:#}").contains("exactly 8 validators"));
        std::fs::remove_file(path).unwrap();
    }
}
