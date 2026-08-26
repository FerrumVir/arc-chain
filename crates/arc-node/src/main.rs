mod config;
mod validator_identity;

use anyhow::{Context, Result, bail};
use arc_crypto::{Hash256, hash_bytes};
use arc_mempool::Mempool;
use arc_net::transport::{InboundMessage, OutboundMessage, run_transport};
use arc_node::{benchmark::BenchmarkPool, consensus::ConsensusManager, rpc};
use arc_state::StateDB;
use arc_types::Block;
use clap::{CommandFactory, Parser};
use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::AtomicU32;
use std::time::Instant;
use tokio::sync::mpsc;
use tracing_subscriber::EnvFilter;
use zeroize::Zeroize;

#[derive(Parser)]
#[command(name = "arc-node", version, about = "ARC Chain Node")]
struct Cli {
    /// RPC listen address (changed from 9090 to avoid Prometheus default port conflict)
    #[arg(long, default_value = "127.0.0.1:9944")]
    rpc: String,

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

    /// Bootstrap peer addresses (comma-separated host:port)
    #[arg(long, value_delimiter = ',')]
    peers: Vec<String>,

    /// Path to a seeds file (one peer address per line, # comments allowed).
    /// Seeds are merged with --peers. Useful for testnet bootstrap.
    #[arg(long)]
    seeds_file: Option<String>,

    /// Hosts to pull the sharded-inference registry from over HTTP, WITHOUT
    /// joining their P2P network or consensus (comma-separated; a bare host
    /// gets the conventional RPC port 9090).
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

    /// Legacy deterministic seed for stake-zero nodes or disposable devnets.
    /// A production staked node always rejects this secret-bearing interface.
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
    /// Keep this OFF while any validator still runs a pre-v0.7.12 binary:
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

/// Rewrites a pulled peer's `self_shard.socket_addr` in place when it carries
/// a stub (0.0.0.0 / 127.x / [::] / [::1] / empty), replacing the host with the
/// URL we just pulled from and keeping the declared port. When no port is
/// declared, falls back to the pulled URL's port or 9090.
///
/// Pure JSON mutation - no I/O, no async. Unit-testable against static fixtures.
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
    use sha2::{Digest, Sha256};
    use std::io::Read;

    let file = std::fs::File::open(path).ok()?;
    let mut reader = std::io::BufReader::with_capacity(1024 * 1024, file);
    let mut hasher = Sha256::new();
    let mut buffer = vec![0_u8; 1024 * 1024];
    loop {
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
const DEFAULT_MODEL_SOURCES: &[&str] =
    &["https://huggingface.co/FerrumVir/llama-2-7b-arc/resolve/main/llama2-7b.gguf"];

/// Resolve a HuggingFace `/resolve/main/` URL to its `/raw/main/` form,
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
fn download_and_verify(url: &str, tmp: &str, expected_sha: &str) -> bool {
    let status = std::process::Command::new("curl")
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
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if !status {
        return false;
    }
    let got = sha256_of(tmp).unwrap_or_default();
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
fn auto_download_model() -> Option<String> {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/root".into());
    let target_dir = format!("{}/.arc-models", home);
    let target = format!("{}/llama2-7b.gguf", target_dir);

    if std::path::Path::new(&target).is_file() {
        match sha256_of(&target) {
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
        if download_and_verify(url, &tmp, TESTNET_MODEL_SHA256) {
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

/// Derive a seed RPC URL ("host:9090") from --peers or --seeds-file.
/// Convention: seeds expose RPC on port 9090 regardless of the P2P port
/// they advertise in seeds files. Returns the first usable host.
fn pick_seed_rpc(cli: &Cli) -> Option<String> {
    for p in &cli.peers {
        if let Some(host) = p.split(':').next()
            && !host.is_empty()
        {
            return Some(format!("{}:9090", host));
        }
    }
    if let Some(path) = &cli.seeds_file
        && let Ok(content) = std::fs::read_to_string(path)
    {
        for raw in content.lines() {
            let line = raw.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            if let Some(host) = line.split(':').next()
                && !host.is_empty()
            {
                return Some(format!("{}:9090", host));
            }
        }
    }
    None
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

/// POST one authenticated community mutation. A new timestamp and CSPRNG
/// nonce are signed for every attempt; callers must invoke this again for a
/// retry or a different coordinator rather than reusing the wire envelope.
async fn post_signed_community<T: serde::Serialize>(
    client: &reqwest::Client,
    target: &str,
    path: &str,
    payload: T,
    keypair: &arc_crypto::KeyPair,
    timeout: std::time::Duration,
) -> Result<reqwest::Response> {
    let signed = rpc::sign_community_request(path, payload, keypair)
        .map_err(anyhow::Error::msg)
        .context("sign community HTTP mutation")?;
    client
        .post(format!("http://{target}{path}"))
        .json(&signed)
        .timeout(timeout)
        .send()
        .await
        .with_context(|| format!("POST authenticated community mutation to {target}{path}"))
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

async fn auto_shard_join(cli: &Cli, model_artifact_id: Hash256) -> Option<(usize, usize)> {
    let seed = pick_seed_rpc(cli)?;
    let url = format!("http://{}/shards/join", seed);
    // Every joiner presents the BLAKE3 commitment of the exact source
    // artifact. Shape metadata cannot distinguish different weights.
    let model_id_hex = hex::encode(model_artifact_id.0);

    let body = serde_json::json!({
        "socket_addr": cli.rpc,
        "node_name": public_node_name(cli),
        "model_id": model_id_hex,
        "model_name": "Llama-2-7B",
        "total_layers": 32u32,
        "available_memory_mb": detect_ram_mb(),
        "gpu_tier": 0u8,
    });

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
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

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env().add_directive("arc=info".parse()?))
        .init();

    let mut cli = Cli::parse();

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
            cli.model = auto_download_model();
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

    // Benchmark settings: CLI > config > default
    let _bench_batch =
        if matches.value_source("bench_batch") == Some(clap::parser::ValueSource::CommandLine) {
            cli.bench_batch
        } else {
            node_cfg.benchmark.batch_size
        };

    let _bench_interval =
        if matches.value_source("bench_interval") == Some(clap::parser::ValueSource::CommandLine) {
            cli.bench_interval
        } else {
            node_cfg.benchmark.interval_ms
        };

    let bench_sender_start = if matches.value_source("bench_sender_start")
        == Some(clap::parser::ValueSource::CommandLine)
    {
        cli.bench_sender_start
    } else {
        node_cfg.benchmark.sender_start
    };

    let bench_sender_count = if matches.value_source("bench_sender_count")
        == Some(clap::parser::ValueSource::CommandLine)
    {
        cli.bench_sender_count
    } else {
        node_cfg.benchmark.sender_count
    };

    let bench_sign_threads = if matches.value_source("bench_sign_threads")
        == Some(clap::parser::ValueSource::CommandLine)
    {
        cli.bench_sign_threads
    } else {
        node_cfg.benchmark.sign_threads
    };

    let bench_rayon_threads = if matches.value_source("bench_rayon_threads")
        == Some(clap::parser::ValueSource::CommandLine)
    {
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
    let chain_participation_enabled = !migration_observer;
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
        match auto_shard_join(
            &cli,
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
    tracing::info!("║   Testnet Node v0.3.0                 ║");
    tracing::info!("╚═══════════════════════════════════════╝");
    tracing::info!("Validator  : {}", validator_address);
    tracing::info!(
        "Identity   : {}",
        match identity_source {
            validator_identity::IdentitySource::Keyfile => "Ed25519 keyfile",
            validator_identity::IdentitySource::InsecureDevelopmentSeed => {
                "INSECURE development seed"
            }
            validator_identity::IdentitySource::StakeZeroSeed => "stake-zero observer identity",
        }
    );
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

    let state = Arc::new({
        let mut db = StateDB::with_genesis_persistent(&genesis_accounts, &data_dir, genesis_hash)
            .context("failed to initialize genesis-bound persistent state")?;
        let reward_activation_height = genesis_cfg
            .as_ref()
            .and_then(|config| config.chain.community_rewards_v1_activation_height);
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
        if cli.archive {
            db.archive_mode = true;
            tracing::info!("Archive mode ENABLED - no pruning, full transaction history retained");
        }
        // Pre-populate StateDB.validators with the genesis validator set so
        // `is_validator()` returns true for the 8 genesis validators on
        // every node, regardless of how far behind the local commit log
        // is. Required for TxBody::FaucetClaim (and any future
        // validator-authorized body) to apply on peers that haven't synced
        // chain history.
        if !genesis_validators.is_empty() {
            db.seed_genesis_validators(&genesis_validators);
            tracing::info!(
                "Seeded {} genesis validators into StateDB.validators",
                genesis_validators.len()
            );
        }
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
    {
        let rebuilt = state.rebuild_tier1_pending();
        if rebuilt > 0 {
            tracing::info!(
                "Rebuilt {} Tier 1 pending requests from on-disk state",
                rebuilt
            );
        }
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

    if let Some(peer) = &sync_peer {
        tracing::info!("Bootstrapping from peer: {}", peer);

        let sync_mgr = arc_node::state_sync::StateSyncManager::new();
        match sync_mgr.sync_from_peer(peer, &state).await {
            Ok(height) => {
                tracing::info!("State sync complete, height = {}", height);
            }
            Err(e) => {
                tracing::warn!(
                    "Sync from peer failed ({}), continuing from genesis state",
                    e
                );
                // Don't crash - the node will start from genesis and catch
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
    let (candle_engine, candle_model_id): (
        Option<Arc<arc_inference::candle_backend::GgufEngine>>,
        Option<arc_crypto::Hash256>,
    ) = if is_shard_holder {
        tracing::info!("Shard holder mode - candle backend SKIPPED to save ~4 GB RAM");
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
            } else {
                arc_inference::cached_integer_model::load_cached_model(model_path)
            };
            match load_result {
                Ok(mut model) => {
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
                    // Multi-range / sharded loaders explicitly drop the I16
                    // quantization at the merge step (cached_integer_model.rs
                    // line 3311) since each sub-load's i16 slices were keyed
                    // on its own subrange and rebuilding from f32 is too
                    // expensive at startup. The single-range loader populates
                    // i16_layers directly. To make the dispatch order (I16 >
                    // block-I8 > Q4 > I8) reach I16 on shard-holder seeds —
                    // and to make `effective_precision_label()` honestly
                    // report "INT16 integer" — promote the in-memory I8
                    // weights to I16 storage format here. This is the
                    // `enable_i16()` path documented at
                    // cached_integer_model.rs:451: same I8-level precision
                    // (no f32 source), but the dispatch now flows through
                    // matmul_i16_into. Real quality improvement requires the
                    // multi-range loader to stitch per-range f32 I16 weights
                    // — separate change.
                    //
                    // GATED as of this change. On aarch64 the promotion is a
                    // real win: `matmul_i16_into` dispatches to
                    // `dot_i16_i64_neon`, an actual widening SIMD kernel. On
                    // x86 it is a pure loss — `from_i8` carries no extra
                    // precision (no f32 source), and `dot_i16_i64` resolves to
                    // `dot_i16_i64_avx2`, which is literally a call to
                    // `dot_i16_i64_scalar` (the AVX-512 path was reverted after
                    // segfaults). So an x86 seed doubled the weight bytes it
                    // streamed per layer, kept the I8 weights resident
                    // alongside the I16 ones, and got byte-identical output out
                    // of the same scalar loop. `--enable-i16` forces it on any
                    // architecture.
                    let want_i16 = cli.enable_i16 || cfg!(target_arch = "aarch64");
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

    // ── Record boot time for uptime tracking ──────────────────────────
    let boot_time = Instant::now();

    // ── Create channels for P2P transport ↔ consensus ─────────────────
    let (inbound_tx, inbound_rx) = mpsc::channel::<InboundMessage>(1000);
    let (outbound_tx, outbound_rx) = mpsc::channel::<OutboundMessage>(4000);
    let peer_count = Arc::new(AtomicU32::new(0));

    if chain_participation_enabled {
        // Parse bootstrap peers only for a node allowed to join this chain.
        let bootstrap_peers: Vec<SocketAddr> =
            peers.iter().filter_map(|p| p.parse().ok()).collect();
        let listen_addr: SocketAddr = format!("0.0.0.0:{}", p2p_port).parse()?;

        // The authenticated handshake commits to the canonical parsed genesis
        // hash, separating chains with different identities/state/validators.
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
    } else {
        drop(outbound_rx);
        drop(inbound_tx);
    }

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
            "Benchmark mode ACTIVE - ed25519 signed txs, senders {}-{}, async indexing",
            bench_sender_start,
            bench_sender_start + bench_sender_count - 1
        );
        Some(Arc::new(pool))
    } else {
        None
    };

    // ── Start DAG consensus in background ─────────────────────────────
    // Initialize the exact approved genesis validator identities and stakes.
    // Seed addresses and transport discovery are connectivity inputs only;
    // they never define or mutate voting membership.
    let peer_vals: Vec<(Hash256, u64)> = genesis_validators
        .iter()
        .filter(|(addr, _)| *addr != validator_address)
        .cloned()
        .collect();
    let all_vals: Vec<(Hash256, u64)> = if chain_participation_enabled {
        let mut v = vec![(validator_address, stake)];
        v.extend(&peer_vals);
        v
    } else {
        Vec::new()
    };
    let dag_validators = Arc::new(parking_lot::RwLock::new(all_vals));
    let dag_round = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let dag_committed = Arc::new(std::sync::atomic::AtomicU64::new(0));

    if chain_participation_enabled {
        let mut consensus = ConsensusManager::new_with_keypair(
            validator_address,
            stake,
            4, /* num_shards */
            cli.benchmark,
            &peer_vals,
            validator_keypair.clone(),
        );
        consensus.dag_validators = Some(dag_validators.clone());
        consensus.dag_round = Some(dag_round.clone());
        consensus.dag_committed = Some(dag_committed.clone());
        // DAG persistence WAL - survives restarts
        let dag_wal_path = format!("{}/dag-wal", data_dir);
        std::fs::create_dir_all(&dag_wal_path).ok();

        // ── v0.7.0: DAG WAL recovery on boot ────────────────────────────────
        //
        // Pre-v0.7 the dag-wal was write-only: every block.proposed and every
        // block.received called wal.append, but nothing ever read those
        // segments back at startup. The seed boots, sees `current_round = 0`,
        // and any block from a peer at round=N gets rejected as "too far
        // ahead" once N > 1M. NYC hit this on the v0.7.0 upgrade attempt:
        // 76 segments of accumulated DAG history sat on disk, ignored.
        //
        // The fix: scan dag-wal/ for the highest block_height we've ever
        // persisted. That's the round we WERE at the moment we crashed/
        // shutdown. Hand it to the local-WAL restore path so the engine
        // resumes from there instead of fighting peers about it.
        //
        // We don't replay block contents — peers will re-deliver any DAG
        // blocks we still need on the normal consensus path. Only locally
        // persisted cursors may restore consensus position; peer hints cannot.
        // Bounded read: scans only the latest segment (≤64 MB), not every
        // segment. NYC's dag-wal is 5 GB+ and growing; reading the whole
        // history at boot would balloon memory and slow startup minutes.
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
        // If the consensus thread panics, log the error and exit the process -
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
                        tracing::error!("Consensus loop exited unexpectedly - shutting down");
                    }
                    Err(panic_info) => {
                        tracing::error!("CONSENSUS THREAD PANICKED: {:?}", panic_info);
                    }
                }
                // Exit the process - a node without consensus must restart
                std::process::exit(1);
            })
            .context("failed to spawn consensus thread")?;
    } else {
        drop(inbound_rx);
        drop(outbound_tx);
        tracing::warn!(
            "Chain P2P/consensus is OFF while genesis validator migration is pending; community HTTP inference remains active"
        );
    }

    // ── Start ETH JSON-RPC server (MetaMask, Hardhat, Foundry) ──────────
    if eth_rpc_port > 0 {
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
    if chain_participation_enabled {
        let validator_task = arc_node::inference_validator::InferenceValidatorTask::new(
            state.clone(),
            mempool.clone(),
            validator_address,
            validator_keypair.clone(),
            candle_engine.clone(),
            inference_model.clone(),
            model_artifact_id,
        );
        tokio::spawn(async move { validator_task.run().await });
        tracing::info!(
            "Tier 1 validator task spawned (candle={}, tokenizer={})",
            candle_engine.is_some(),
            inference_model.is_some()
        );
    } else {
        tracing::info!(
            "Tier 1 on-chain validator task skipped in genesis-migration community observer mode"
        );
    }

    // ── Graceful shutdown handler ───────────────────────────────────────
    // On SIGTERM (from systemd stop / rolling upgrade), drain pending state
    // and close connections before exiting. This prevents lost transactions
    // and allows other validators to see a clean disconnect.
    let shutdown_state = state.clone();
    tokio::spawn(async move {
        match tokio::signal::ctrl_c().await {
            Ok(()) => {
                tracing::info!("SIGINT received - initiating graceful shutdown...");
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
            let Ok(mut sigterm) =
                tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            else {
                tracing::warn!("Failed to install SIGTERM handler");
                return;
            };
            sigterm.recv().await;
            tracing::info!("SIGTERM received - initiating graceful shutdown...");
            shutdown_state.sync_wal();
            tracing::info!("Graceful shutdown complete. Exiting.");
            std::process::exit(0);
        });
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
                let socket_addr = std::env::var("ARC_PUBLIC_SOCKET").unwrap_or_else(|_| {
                    format!(
                        "{}:{}",
                        rpc_addr.split(':').next().unwrap_or("127.0.0.1"),
                        rpc_addr.split(':').nth(1).unwrap_or("9090")
                    )
                });
                ranges
                    .iter()
                    .map(|&(start, end)| rpc::ShardInfo {
                        start_layer: start,
                        end_layer: end,
                        total_layers,
                        model_id: format!("0x{}", hex::encode(artifact_id.0)),
                        model_name: model_display_name.clone(),
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

    // Spawn a background task that announces each held range to every seed
    // AND pulls their shards back. Runs immediately at startup + every 15s
    // so the registry converges fast.
    if !shard_infos_for_broadcast.is_empty() {
        let sis = shard_infos_for_broadcast.clone();
        // Build the list of peer RPC URLs from the seeds file. The seeds file
        // contains "host:p2p_port" lines; the RPC port is always p2p - 1 in
        // our deployment, but we conservatively try both 9090 (the seed default)
        // and (p2p_port - 1) to handle community nodes on different ports.
        let mut seed_addrs: Vec<String> = Vec::new();
        for p in &peers {
            // p is a SocketAddr string like "1.2.3.4:9091"
            if let Some(host) = p.split(':').next() {
                seed_addrs.push(format!("{}:9090", host));
                if let Some(port_str) = p.split(':').nth(1)
                    && let Ok(port) = port_str.parse::<u16>()
                    && port > 1
                    && port - 1 != 9090
                {
                    seed_addrs.push(format!("{}:{}", host, port - 1));
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
        let local_announce_broadcast = format!(
            "http://127.0.0.1:{}/shards/announce",
            rpc_addr.split(':').nth(1).unwrap_or("9090")
        );
        tokio::spawn(async move {
            // Brief settle so the local /shards endpoint is up before we ask
            // peers to fetch from us
            tokio::time::sleep(std::time::Duration::from_secs(3)).await;
            let client = match reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(5))
                .build()
            {
                Ok(c) => c,
                Err(_) => return,
            };
            loop {
                for si in &sis {
                    let payload = serde_json::json!({"shard": si});
                    // Refresh our own entry first
                    let _ = client
                        .post(&local_announce_broadcast)
                        .json(&payload)
                        .send()
                        .await;
                    // Then announce to remote seeds
                    for addr in &seed_addrs {
                        let url = format!("http://{}/shards/announce", addr);
                        let _ = client.post(&url).json(&payload).send().await;
                    }
                }
                tokio::time::sleep(std::time::Duration::from_secs(15)).await;
            }
        });

        // Background puller: fetch each seed's /shards and re-announce them locally.
        // This converges the registry even when a peer was offline when we first
        // announced. Anyone we reach contributes their full registry to ours.
        let local_announce = format!(
            "http://127.0.0.1:{}/shards/announce",
            rpc_addr.split(':').nth(1).unwrap_or("9090")
        );
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_secs(5)).await;
            let client = match reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(5))
                .build()
            {
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
                // re-announcing - that IS the routable address for that shard
                // holder, and it's what the receiver-side fix for direct
                // /shards/announce broadcasts produces too.
                for addr in &seed_addrs_pull {
                    if let Ok(resp) = client.get(format!("http://{}/shards", addr)).send().await
                        && let Ok(mut json) = resp.json::<serde_json::Value>().await
                    {
                        // New peers emit `self_shards: [ShardInfo, ...]`; legacy
                        // peers still emit `self_shard: ShardInfo` - accept both
                        // so a rolling upgrade never loses shard visibility.
                        let mut to_announce: Vec<serde_json::Value> = Vec::new();
                        if let Some(arr) =
                            json.get_mut("self_shards").and_then(|v| v.as_array_mut())
                        {
                            for entry in arr.iter_mut() {
                                if !entry.is_null() {
                                    rewrite_pulled_self_shard(entry, addr);
                                    to_announce.push(entry.clone());
                                }
                            }
                        }
                        if let Some(self_shard) = json.get_mut("self_shard")
                            && !self_shard.is_null()
                        {
                            rewrite_pulled_self_shard(self_shard, addr);
                            to_announce.push(self_shard.clone());
                        }
                        for shard_val in to_announce {
                            let payload = serde_json::json!({"shard": shard_val});
                            let _ = client.post(&local_announce).json(&payload).send().await;
                        }
                    }
                }
                tokio::time::sleep(std::time::Duration::from_secs(20)).await;
            }
        });

        tracing::info!("Shard announcement broadcaster + puller started (15s/20s tick)");
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
    if cli.no_community && (cli.community_mode || stake == 0) {
        tracing::info!(
            "--no-community: observer mode. This node will NOT register, heartbeat or \
             claim work from its peers; it only reads from them."
        );
    }

    if community_mode {
        tracing::info!("╔═══════════════════════════════════════╗");
        tracing::info!("║  COMMUNITY MODE ACTIVE                ║");
        tracing::info!("║  Registering with seed coordinators   ║");
        tracing::info!("║  Capability is verified after load    ║");
        tracing::info!("╚═══════════════════════════════════════╝");
    }

    if community_mode {
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
            if !m.has_all_transformer_layers() {
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
                "Loaded model is partial or tokenizer-only; registering as relay/observer and \
                 disabling the full inference worker"
            );
        }

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
        let model_name_c = worker_model.as_ref().map(|(name, _, _)| name.clone());
        let model_id_c = worker_model
            .as_ref()
            .map(|(_, model_id, _)| model_id.clone());
        let seed_rpc_addrs_c = seed_rpc_addrs.clone();
        let registration_keypair = validator_keypair.clone();

        tokio::spawn(async move {
            // Settle before first POST
            tokio::time::sleep(std::time::Duration::from_secs(5)).await;
            let client = match reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(5))
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
                for addr in &seed_rpc_addrs_c {
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
                tokio::time::sleep(std::time::Duration::from_secs(15)).await;
            }
        });
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
            let seed_rpc_addrs_w = seed_rpc_addrs.clone();
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

            tokio::spawn(async move {
                tokio::time::sleep(std::time::Duration::from_secs(10)).await;
                let client = match reqwest::Client::builder()
                    .timeout(std::time::Duration::from_secs(35)) // 30s claim + 5s overhead
                    .build()
                {
                    Ok(c) => c,
                    Err(_) => return,
                };
                tracing::info!(
                    address = %format!("0x{}", hex::encode(worker_address.0)),
                    "Community inference worker started - polling for jobs"
                );
                loop {
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
                    };
                    let mut claims = tokio::task::JoinSet::new();
                    for addr in &seed_rpc_addrs_w {
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
                        tokio::spawn(async move {
                            while let Some(result) = claims.join_next().await {
                                let Ok(Some((coordinator, job))) = result else {
                                    continue;
                                };
                                let Some(job_id) = job
                                    .get("job_id")
                                    .and_then(|value| value.as_str())
                                    .filter(|value| !value.is_empty())
                                else {
                                    continue;
                                };
                                let decline = rpc::WorkResult {
                                    job_id: job_id.to_string(),
                                    worker_id: decline_worker.clone(),
                                    success: false,
                                    declined: true,
                                    output: String::new(),
                                    output_hash: String::new(),
                                    tokens_generated: 0,
                                    total_ms: 0,
                                    ms_per_token: 0,
                                    engine: String::new(),
                                    error: Some(
                                        "worker already accepted a concurrent coordinator job"
                                            .to_string(),
                                    ),
                                    signed_attestation_hex: None,
                                };
                                match post_signed_community(
                                    &decline_client,
                                    &coordinator,
                                    rpc::COMMUNITY_SUBMIT_WORK_PATH,
                                    decline,
                                    &decline_keypair,
                                    std::time::Duration::from_secs(10),
                                )
                                .await
                                {
                                    Ok(response) if response.status().is_success() => {
                                        tracing::debug!(
                                            job_id,
                                            seed = %coordinator,
                                            "declined concurrent community job without dropping it"
                                        );
                                    }
                                    Ok(response) => {
                                        tracing::warn!(
                                            job_id,
                                            seed = %coordinator,
                                            status = %response.status(),
                                            "coordinator rejected concurrent-job decline"
                                        );
                                    }
                                    Err(error) => {
                                        tracing::warn!(
                                            job_id,
                                            seed = %coordinator,
                                            %error,
                                            "could not decline concurrent community job"
                                        );
                                    }
                                }
                            }
                        });
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
                        let max_tokens =
                            job.get("max_tokens").and_then(|v| v.as_u64()).unwrap_or(20) as u32;
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
                        let assignment_model_matches =
                            assignment_model_id == Some(worker_model_id_hash);
                        if !assignment_model_matches
                            || input.is_empty()
                            || input.len() > 32_768
                            || max_tokens == 0
                            || max_tokens > rpc::INFERENCE_RUN_MAX_TOKENS
                        {
                            let reason = if !assignment_model_matches {
                                "assignment omitted or mismatched the worker's exact model artifact"
                                    .to_string()
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
                                declined: !assignment_model_matches,
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
                        let inference_input = input.clone();
                        let inference = tokio::task::spawn_blocking(move || {
                            let (generated, hash) = inference_model.generate(
                                &{
                                    let mut toks = vec![inference_model.config.bos_token];
                                    toks.extend(inference_model.encode(&inference_input));
                                    toks
                                },
                                max_tokens,
                                &inference_model.config.eos_tokens,
                            );
                            let output_text = inference_model.decode(&generated);
                            (generated, hash, output_text)
                        })
                        .await;
                        let (generated, hash, output_text) = match inference {
                            Ok(result) => result,
                            Err(error) => {
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
                                let _ = post_signed_community(
                                    &client,
                                    &winner,
                                    rpc::COMMUNITY_SUBMIT_WORK_PATH,
                                    failure,
                                    &worker_keypair,
                                    std::time::Duration::from_secs(10),
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
                            let q_url = format!(
                                "http://{}/account/0x{}",
                                winner,
                                hex::encode(worker_address.0)
                            );
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

                        let signed_attestation_hex = match tx.sign(&worker_keypair) {
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
                            engine: "INT8 integer (community worker)".to_string(),
                            error: None,
                            signed_attestation_hex,
                        };

                        let submit_resp = post_signed_community(
                            &client,
                            &winner,
                            rpc::COMMUNITY_SUBMIT_WORK_PATH,
                            result_body,
                            &worker_keypair,
                            // The coordinator independently repeats inference through
                            // a 2-of-3 validator quorum before acknowledging success.
                            // Its verified-dispatch budget is capped at 600 seconds;
                            // allow network/serialization headroom beyond that cap.
                            std::time::Duration::from_secs(660),
                        )
                        .await;

                        // If submit reports invalid_nonce, force a re-query
                        // of the chain on the next loop iteration.
                        match submit_resp {
                            Ok(resp) => {
                                let status_code = resp.status();
                                match resp.json::<serde_json::Value>().await {
                                    Ok(body) => {
                                        if !status_code.is_success() {
                                            tracing::warn!(
                                                job_id,
                                                seed = %winner,
                                                status = %status_code,
                                                response = %body,
                                                "coordinator rejected community result"
                                            );
                                        }
                                        let attestation = body.get("attestation");
                                        if let Some(a) = attestation {
                                            let status = a
                                                .get("status")
                                                .and_then(|s| s.as_str())
                                                .unwrap_or("");
                                            let err = a
                                                .get("error")
                                                .and_then(|s| s.as_str())
                                                .unwrap_or("");
                                            if status == "rejected" && err.contains("InvalidNonce")
                                            {
                                                tracing::warn!(
                                                    "attestation nonce drifted; will re-query chain on next submit"
                                                );
                                                attestation_nonce_initialized.store(
                                                    false,
                                                    std::sync::atomic::Ordering::Relaxed,
                                                );
                                            }
                                        }
                                    }
                                    Err(error) => {
                                        tracing::warn!(
                                            job_id,
                                            seed = %winner,
                                            status = %status_code,
                                            %error,
                                            "coordinator returned an invalid submit response"
                                        );
                                    }
                                }
                            }
                            Err(error) => tracing::warn!(
                                job_id,
                                seed = %winner,
                                %error,
                                "could not submit verified community result"
                            ),
                        }
                    }
                    // Brief sleep between poll rounds to avoid hammering
                    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                }
            });
            tracing::info!("Community inference worker loop spawned");
        }
    }

    // Seed RPC endpoints the coordinator can pull shard topology from when
    // its own registry doesn't cover the model. Convention: seeds serve RPC
    // on 9090 regardless of the P2P port advertised in the seeds file.
    let coordinator_seed_rpcs: Vec<String> = {
        let mut v: Vec<String> = peers
            .iter()
            .filter_map(|p| p.split(':').next())
            .filter(|h| !h.is_empty())
            .map(|h| format!("{}:9090", h))
            .collect();
        // --shard-hosts: registry sources reached over HTTP only, never
        // dialed for P2P. A bare host takes the conventional RPC port; an
        // explicit host:port is honoured so a non-standard RPC port works.
        for h in &cli.shard_hosts {
            let h = h.trim();
            if h.is_empty() {
                continue;
            }
            v.push(if h.contains(':') {
                h.to_string()
            } else {
                format!("{}:9090", h)
            });
        }
        v.sort();
        v.dedup();
        v
    };

    // Inference pool width: explicit --threads wins, then [inference] threads
    // from the config file, else 0 = rayon's global pool (which honours
    // RAYON_NUM_THREADS; see config::InferenceConfig).
    let compute_threads =
        if matches.value_source("threads") == Some(clap::parser::ValueSource::CommandLine) {
            cli.threads
        } else {
            node_cfg.inference.threads
        };

    rpc::serve(
        &rpc_addr,
        state,
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
        compute_threads,
        genesis_chain_identity,
        cli.enable_community_rewards_v1,
    )
    .await?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

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
    fn shipped_placeholder_allows_stake_zero_community_observer_only() {
        let cli =
            Cli::try_parse_from(["arc-node", "--community", "--genesis", "genesis.toml"]).unwrap();
        let genesis: config::GenesisConfig =
            toml::from_str(include_str!("../../../genesis.toml")).unwrap();

        assert_eq!(cli.stake, 0);
        assert!(is_genesis_migration_observer(
            Some(&genesis),
            cli.stake,
            cli.insecure_dev_validator_seed,
        ));
        assert!(genesis.validated_validator_set(false).is_err());
        assert_ne!(
            genesis.migration_observer_network_hash().unwrap(),
            Hash256::ZERO
        );
    }

    #[test]
    fn cli_defaults_keep_unauthenticated_rpc_local_and_eth_disabled() {
        let cli = Cli::try_parse_from(["arc-node"]).unwrap();
        assert_eq!(cli.rpc, "127.0.0.1:9944");
        assert_eq!(cli.eth_rpc_port, 0);
    }

    #[test]
    fn pulled_stub_rewritten_to_seed_host_port() {
        // AMS announces self_shard with socket_addr=0.0.0.0:9090. We pulled
        // from http://136.244.109.1:9090/shards, so the routable addr for AMS
        // IS the URL we pulled from - use it.
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
        // Peer bound to 9090 but we pulled from its port 8545 (hypothetical) -
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
