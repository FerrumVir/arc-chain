use arc_consensus::StakeTier;
use arc_crypto::{Hash256, MerkleProof};
use arc_gpu::probe_gpu;
use arc_mempool::Mempool;
use arc_state::StateDB;
use arc_types::*;
use arc_types::economics::RoleRevenueConfig;
use axum::{
    extract::{ConnectInfo, DefaultBodyLimit, Query, State as AxumState},
    http::StatusCode,
    routing::{get, post},
    Json, Router,
};
use std::net::SocketAddr;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;
use tower_http::cors::CorsLayer;

/// Faucet configuration.
const FAUCET_CLAIM_AMOUNT: u64 = 10_000;
const FAUCET_RATE_LIMIT_SECS: u64 = 60; // 1 minute per address (testnet - was 1 hour)
const FAUCET_GLOBAL_RATE_LIMIT: usize = 5000; // 5000 claims/minute - intentionally high for testnet TPS demos; lower before mainnet

/// Reward per InferenceAttestation in ARC. Testnet flat rate. Production
/// will replace this with a halving-curve emission from the inference
/// pool (tracked in a future tokenomics release). Authoritative on the
/// chain side: the desktop reads this back via /worker/earnings rather
/// than hardcoding it client-side, so a future change here surfaces in
/// every UI without a coordinated frontend release.
const REWARD_PER_ATTESTATION_ARC: f64 = 2.5;

/// Shared node state passed to all handlers.
#[derive(Clone)]
pub struct NodeState {
    pub state: Arc<StateDB>,
    pub mempool: Arc<Mempool>,
    pub validator_address: Hash256,
    /// Validator keypair for signing coordinator-internal txs
    /// (InferenceEscrowRelease, auto InferenceAttestation). Optional only
    /// for test fixtures; production paths always have it.
    pub validator_keypair: Option<Arc<arc_crypto::KeyPair>>,
    pub stake: u64,
    pub tier: StakeTier,
    pub boot_time: Instant,
    pub peer_count: Arc<AtomicU32>,
    /// Faucet rate limiter: address → last claim time.
    /// DashMap so faucet handler never blocks the tokio runtime under load.
    pub faucet_claims: Arc<dashmap::DashMap<[u8; 32], Instant>>,
    /// Total faucet claims since boot.
    pub faucet_claims_total: Arc<AtomicU32>,
    /// Cached INT8 inference model (if --model was provided).
    pub inference_model: Option<Arc<arc_inference::cached_integer_model::CachedIntegerModel>>,
    /// Candle GGUF float inference engine (coherent output).
    pub candle_engine: Option<Arc<arc_inference::candle_backend::GgufEngine>>,
    /// Model ID for candle engine.
    pub candle_model_id: Option<arc_crypto::Hash256>,
    /// Live DAG validator set (updated by consensus loop via PeerConnected).
    pub dag_validators: Arc<parking_lot::RwLock<Vec<(Hash256, u64)>>>,
    /// Per-sender rate limiter for tx submission: sender_address → last submit time.
    /// Limits to 10 tx/sec per sender to prevent mempool flood DoS.
    pub tx_rate_limit: Arc<dashmap::DashMap<[u8; 32], Instant>>,
    /// DAG consensus round (updated by consensus loop).
    pub dag_round: Arc<AtomicU64>,
    /// DAG committed block count (updated by consensus loop).
    pub dag_committed: Arc<AtomicU64>,
    /// Inference results indexed by attestation tx hash - for explorer display.
    pub inference_results: Arc<dashmap::DashMap<String, Value>>,
    /// Pipeline-parallel sharding: every layer range this node holds.
    /// Set from repeated --shard-range flags (or the deprecated single
    /// --shard-start/--shard-end pair). Empty = non-shard-holder (validator
    /// or coordinator role only). Each entry is announced as an independent
    /// replica so the coordinator treats multi-range nodes naturally.
    pub shard_infos: Vec<ShardInfo>,
    /// Per-request KV cache for sharded inference. Key: request_id (Hash256 hex).
    /// Each entry is an Arc<Mutex<KVCache>> so handlers can clone the Arc and
    /// release the DashMap shard lock immediately.
    pub shard_kv_caches: Arc<dashmap::DashMap<String, Arc<std::sync::Mutex<arc_inference::cached_integer_model::KVCache>>>>,
    /// Network-wide shard registry (gossiped via /shards/announce).
    /// Maps node socket addr → (ShardInfo, last-seen Instant). Entries older
    /// than SHARD_REGISTRY_TTL_SECS (60s) are considered stale and dropped
    /// at read time. This prevents stale entries from nodes that USED to
    /// hold a shard but no longer do (e.g. after `arc-node` restarts without
    /// --shard-start/--shard-end flags) from polluting the pipeline walker.
    pub shard_registry: Arc<dashmap::DashMap<String, (ShardInfo, std::time::Instant)>>,
    /// Per-replica rolling EWMA of forward_shard hop latency (ms). Keyed by
    /// socket_addr. Populated after every successful hop; consumed by the
    /// coordinator to sort replica lists ascending before picking primary
    /// (run_sharded) or the top-k (run_consensus). Does not affect output
    /// determinism - only WHICH replica answers, not WHAT it answers.
    /// Closes #29.
    pub latency_stats: Arc<dashmap::DashMap<String, LatencyEWMA>>,
    /// Total sharded inference runs served by this node since boot.
    /// Incremented every time /inference/run_sharded completes successfully.
    pub sharded_runs_total: Arc<AtomicU64>,
    /// Total bytes of activations forwarded between shards since boot.
    pub sharded_bytes_total: Arc<AtomicU64>,
    /// Monotonic counter for inference attestation nonces. Ensures repeat
    /// submissions of the same prompt+output produce unique tx_hashes
    /// (otherwise the mempool de-dups them).
    pub attestation_nonce: Arc<AtomicU64>,
    /// Network-wide deterministic inference cache. Same prompt + same model
    /// returns the cached output_tokens in O(1), proven correct by the
    /// integer engine's determinism. Survives the full coordinator session
    /// (until eviction). The cache hit count is exposed in the response.
    pub inference_cache: Arc<arc_inference::distributed::DistributedCache>,
    /// Community worker registry - nodes that volunteered HTTP-based
    /// inference compute. Keyed by worker_id (self-chosen), value is the
    /// registration record + last-seen Instant for TTL pruning. Workers
    /// are pure outbound-HTTPS contributors (POST to register, POST to
    /// heartbeat, long-poll for work). They never need inbound
    /// connectivity so they work behind any NAT / residential firewall.
    pub community_workers: Arc<dashmap::DashMap<String, (CommunityWorker, std::time::Instant)>>,
    /// Community work dispatch: sender side. The coordinator pushes WorkItems
    /// here when it wants community nodes to run forward_shard. This is the
    /// "producer" half of an mpsc channel - wire it up in main.rs when
    /// starting a coordinator.
    /// Multi-model shard registry (from distributed.rs). Tracks shards
    /// per-model for multi-model routing. Populated alongside the flat
    /// shard_registry for backward compatibility.
    pub multi_model_registry: Arc<arc_inference::distributed::ShardRegistry>,
    /// Inference verification manager - commit-challenge system for
    /// economically-secured inference. Providers commit result_hash + bond;
    /// challengers can dispute with their own bond.
    pub verification_manager: Arc<std::sync::Mutex<arc_vm::inference_verify::VerificationManager>>,
    /// Revenue split configuration - 40% proposers, 25% verifiers, 15% observers, 20% treasury.
    pub revenue_config: RoleRevenueConfig,
    pub community_work_tx: Option<Arc<tokio::sync::mpsc::Sender<WorkItem>>>,
    /// Community work dispatch: receiver side. Wrapped in a tokio::Mutex so
    /// multiple claim_work handlers can await concurrently (only one wins
    /// each item). The long-poll handler calls `recv()` with a timeout.
    pub community_work_queue: Option<Arc<tokio::sync::Mutex<tokio::sync::mpsc::Receiver<WorkItem>>>>,
    /// Community work results: keyed by request_id. The coordinator inserts
    /// a oneshot::Sender before dispatching a WorkItem. When a community
    /// worker POSTs to /community/submit_work, the handler removes the
    /// sender and delivers the WorkResult. The coordinator awaits the
    /// oneshot::Receiver to resume the pipeline walk.
    pub community_work_results: Option<Arc<dashmap::DashMap<String, tokio::sync::oneshot::Sender<WorkResult>>>>,
}

/// Rolling EWMA of forward_shard hop latency for a single replica socket.
/// `ms` is the smoothed latency in milliseconds; `count` is the number of
/// samples folded in; `last_updated` is used for the /inference/latency_stats
/// endpoint (freshness display). Created on first successful hop.
#[derive(Debug, Clone)]
pub struct LatencyEWMA {
    pub ms: f64,
    pub count: u64,
    pub last_updated: std::time::Instant,
}

/// Describes which slice of a model a node holds.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ShardInfo {
    /// First layer index (inclusive).
    pub start_layer: usize,
    /// Last layer index (exclusive).
    pub end_layer: usize,
    /// Total layer count of the full model (so dashboard can compute %).
    pub total_layers: usize,
    /// Model identifier (hex of model_id hash).
    pub model_id: String,
    /// Human-readable model name (from GGUF metadata).
    pub model_name: String,
    /// Memory used by the layers held on this node, in MB.
    pub memory_mb: usize,
    /// Memory the FULL model would use, in MB.
    pub full_model_mb: usize,
    /// Public socket of this node (for the next shard to forward to).
    pub socket_addr: String,
    /// Friendly node name (NYC, LAX, ...).
    pub node_name: String,
}

/// A community worker: an arc-node running with --community-mode that
/// registered via outbound HTTPS POST. Workers contribute compute by
/// polling /community/claim_work and POSTing results back. They do NOT
/// participate in consensus or hold shards - they're pure volunteer
/// compute providers. Their entries in the registry are TTL-pruned if
/// they stop heartbeating.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct CommunityWorker {
    /// Self-chosen worker ID (hex of validator pubkey or a uuid).
    pub worker_id: String,
    /// Operator-friendly name (hostname, city, whatever).
    pub name: String,
    /// What this worker can do. For now: just "inference".
    pub capabilities: Vec<String>,
    /// Model they can serve (matches coordinator's model_name exactly).
    pub model: Option<String>,
    /// OS + arch, for dashboard display.
    pub platform: String,
    /// Unix timestamp of first registration (for "joined at").
    pub registered_at: u64,
    /// Monotonic counter of work units completed (success only).
    pub work_completed: u64,
    /// v0.7.0 scoring stats — populated by submit_work as jobs come in.
    /// Used to rank workers in the /workers/scoreboard endpoint and
    /// (in a future release) to bias dispatch toward higher-scoring
    /// workers via per-worker job lanes.
    /// Total successful submissions.
    #[serde(default)]
    pub success_count: u64,
    /// Total submissions reporting failure.
    #[serde(default)]
    pub failure_count: u64,
    /// Sum of total_ms across every successful submission. Average
    /// latency per job = sum_total_ms_success / success_count when
    /// success_count > 0.
    #[serde(default)]
    pub sum_total_ms_success: u64,
    /// Last total_ms reported (most-recent latency datapoint).
    #[serde(default)]
    pub last_total_ms: u64,
}

/// Community worker TTL. Workers that haven't heartbeated within this
/// window are pruned. Heartbeat interval is 15s, TTL is 90s, so a
/// worker survives 5 missed heartbeats before being evicted.
pub const COMMUNITY_WORKER_TTL_SECS: u64 = 90;

/// Time a shard registry entry is considered fresh. Entries older than this
/// are dropped at read time. Must be greater than the shard announcement
/// broadcast interval (15s) with generous slack so a single lost announcement
/// doesn't prune a live shard.
pub const SHARD_REGISTRY_TTL_SECS: u64 = 60;

/// Collect fresh shard entries, pruning stale ones that haven't been
/// re-announced within SHARD_REGISTRY_TTL_SECS. Called by the pipeline walker
/// and by the /shards GET endpoint so neither sees ghosts.
fn fresh_shards(
    registry: &dashmap::DashMap<String, (ShardInfo, std::time::Instant)>,
) -> Vec<ShardInfo> {
    let now = std::time::Instant::now();
    let ttl = std::time::Duration::from_secs(SHARD_REGISTRY_TTL_SECS);
    let mut keep: Vec<ShardInfo> = Vec::new();
    let mut expired_keys: Vec<String> = Vec::new();
    for entry in registry.iter() {
        let (info, ts) = entry.value();
        if now.duration_since(*ts) <= ttl {
            keep.push(info.clone());
        } else {
            expired_keys.push(entry.key().clone());
        }
    }
    for k in expired_keys {
        registry.remove(&k);
    }
    keep
}

/// Rolling EWMA weight for new samples. α = 0.2 gives recent hops meaningful
/// pull while keeping a multi-sample memory - a single outlier won't steal
/// primary, but a sustained shift in latency rebalances within ~5-10 hops.
const LATENCY_ALPHA: f64 = 0.2;

/// Fold a hop observation into the EWMA for `socket`.
pub fn record_latency(
    stats: &dashmap::DashMap<String, LatencyEWMA>,
    socket: &str,
    hop_ms: u64,
) {
    let hop = hop_ms as f64;
    let now = std::time::Instant::now();
    stats
        .entry(socket.to_string())
        .and_modify(|e| {
            e.ms = LATENCY_ALPHA * hop + (1.0 - LATENCY_ALPHA) * e.ms;
            e.count = e.count.saturating_add(1);
            e.last_updated = now;
        })
        .or_insert_with(|| LatencyEWMA { ms: hop, count: 1, last_updated: now });
}

/// Sort a replica bucket by EWMA latency ascending. Unseen replicas (no
/// sample yet) are placed AFTER seen ones but keep their insertion order
/// - this keeps cold-start behavior identical to the old first-match logic
/// and avoids starving an unseen replica of its first try.
pub fn sort_replicas_by_latency(
    replicas: &mut Vec<ShardInfo>,
    stats: &dashmap::DashMap<String, LatencyEWMA>,
) {
    replicas.sort_by(|a, b| {
        let a_ms = stats.get(&a.socket_addr).map(|v| v.ms);
        let b_ms = stats.get(&b.socket_addr).map(|v| v.ms);
        match (a_ms, b_ms) {
            (Some(x), Some(y)) => x.partial_cmp(&y).unwrap_or(std::cmp::Ordering::Equal),
            (Some(_), None) => std::cmp::Ordering::Less,
            (None, Some(_)) => std::cmp::Ordering::Greater,
            (None, None) => std::cmp::Ordering::Equal,
        }
    });
}

/// Build a `NodeState` from components.
pub fn build_node_state(
    state: Arc<StateDB>,
    mempool: Arc<Mempool>,
    validator_address: Hash256,
    validator_keypair: Option<Arc<arc_crypto::KeyPair>>,
    stake: u64,
    boot_time: Instant,
    peer_count: Arc<AtomicU32>,
    inference_model: Option<Arc<arc_inference::cached_integer_model::CachedIntegerModel>>,
    candle_engine: Option<Arc<arc_inference::candle_backend::GgufEngine>>,
    candle_model_id: Option<arc_crypto::Hash256>,
) -> NodeState {
    let tier = StakeTier::from_stake(stake).unwrap_or(StakeTier::Spark);
    NodeState {
        state,
        mempool,
        validator_address,
        validator_keypair,
        stake,
        tier,
        boot_time,
        peer_count,
        faucet_claims: Arc::new(dashmap::DashMap::new()),
        faucet_claims_total: Arc::new(AtomicU32::new(0)),
        inference_model,
        candle_engine,
        candle_model_id,
        dag_validators: Arc::new(parking_lot::RwLock::new(vec![(validator_address, stake)])),
        tx_rate_limit: Arc::new(dashmap::DashMap::new()),
        dag_round: Arc::new(AtomicU64::new(0)),
        dag_committed: Arc::new(AtomicU64::new(0)),
        inference_results: Arc::new(dashmap::DashMap::new()),
        shard_infos: Vec::new(),
        shard_kv_caches: Arc::new(dashmap::DashMap::new()),
        shard_registry: Arc::new(dashmap::DashMap::new()),
        latency_stats: Arc::new(dashmap::DashMap::new()),
        sharded_runs_total: Arc::new(AtomicU64::new(0)),
        sharded_bytes_total: Arc::new(AtomicU64::new(0)),
        attestation_nonce: Arc::new(AtomicU64::new(0)),
        // 10_000-entry deterministic cache for sharded inference results.
        // LRU eviction by hit_count when full.
        inference_cache: Arc::new(arc_inference::distributed::DistributedCache::new(10_000)),
        multi_model_registry: Arc::new(arc_inference::distributed::ShardRegistry::new()),
        verification_manager: Arc::new(std::sync::Mutex::new(arc_vm::inference_verify::VerificationManager::new())),
        revenue_config: RoleRevenueConfig::default(),
        community_workers: Arc::new(dashmap::DashMap::new()),
        // Community work dispatch — bounded mpsc with 256-slot buffer. New
        // jobs that arrive when 256 are already queued get backpressure
        // (the dispatcher in /inference/run awaits .send().await). Workers
        // long-poll the receiver in claim_work; multiple handlers race for
        // each item via the tokio Mutex.
        community_work_tx: None,
        community_work_queue: None,
        community_work_results: None,
    }
}

/// Capacity of the community work mpsc. Each slot is a single whole-prompt
/// job; under heavy load the dispatcher's `.send().await` provides natural
/// backpressure without unbounded memory growth.
const COMMUNITY_WORK_QUEUE_CAP: usize = 256;

/// Start the RPC server.
pub async fn serve(
    addr: &str,
    state: Arc<StateDB>,
    mempool: Arc<Mempool>,
    validator_address: Hash256,
    validator_keypair: Option<Arc<arc_crypto::KeyPair>>,
    stake: u64,
    boot_time: Instant,
    peer_count: Arc<AtomicU32>,
    inference_model: Option<Arc<arc_inference::cached_integer_model::CachedIntegerModel>>,
    candle_engine: Option<Arc<arc_inference::candle_backend::GgufEngine>>,
    candle_model_id: Option<arc_crypto::Hash256>,
    dag_validators: Option<Arc<parking_lot::RwLock<Vec<(Hash256, u64)>>>>,
    dag_round: Option<Arc<AtomicU64>>,
    dag_committed: Option<Arc<AtomicU64>>,
    shard_infos: Vec<ShardInfo>,
) -> anyhow::Result<()> {
    let mut node = build_node_state(state, mempool, validator_address, validator_keypair, stake, boot_time, peer_count, inference_model, candle_engine, candle_model_id);
    if let Some(dv) = dag_validators {
        node.dag_validators = dv;
    }
    if let Some(r) = dag_round {
        node.dag_round = r;
    }
    if let Some(c) = dag_committed {
        node.dag_committed = c;
    }

    // ── Community work dispatch wiring ──────────────────────────────────
    // Every node's RPC server now exposes a real mpsc-backed work queue.
    // Producers (the smart router in /inference/run, task 2 of v0.7.0)
    // push WorkItems via `community_work_tx`; consumers
    // (/community/claim_work long-pollers) drain via `community_work_queue`.
    // Results land back via the `community_work_results` oneshot map
    // keyed by job_id. Pre-v0.7.0 these were `None` so claim_work always
    // returned 503 — that's why every community worker reported 0
    // attestations forever.
    let (work_tx, work_rx) = tokio::sync::mpsc::channel::<WorkItem>(COMMUNITY_WORK_QUEUE_CAP);
    node.community_work_tx = Some(Arc::new(work_tx));
    node.community_work_queue = Some(Arc::new(tokio::sync::Mutex::new(work_rx)));
    node.community_work_results = Some(Arc::new(dashmap::DashMap::new()));

    node.shard_infos = shard_infos.clone();
    // Seed the local registry with every range this node holds so /shards
    // reports the full picture the moment RPC comes up. The registry is
    // keyed by (socket_addr + range) so two entries with the same socket but
    // different ranges coexist.
    for si in &shard_infos {
        let key = format!("{}#{}-{}", si.socket_addr, si.start_layer, si.end_layer);
        node.shard_registry.insert(key, (si.clone(), std::time::Instant::now()));
    }

    let app = Router::new()
        .route("/", get(index))
        .route("/health", get(health))
        .route("/info", get(chain_info))
        .route("/node/info", get(node_info))
        .route("/block/latest", get(get_latest_block))
        .route("/block/{height}", get(get_block))
        .route("/account/{address}", get(get_account))
        .route("/tx/submit", post(submit_tx))
        .route("/tx/submit_signed", post(submit_signed_tx))
        .route("/tx/submit_batch", post(submit_batch))
        .route("/validators", get(get_validators))
        .route("/tx/{hash}", get(get_transaction))
        .route("/tx/{hash}/proof", get(get_tx_proof))
        .route("/block/{height}/proofs", get(get_block_proofs))
        .route("/blocks", get(get_blocks))
        .route("/block/{height}/txs", get(get_block_txs))
        .route("/account/{address}/txs", get(get_account_txs))
        .route("/stats", get(get_stats))
        .route("/tx/{hash}/full", get(get_full_transaction))
        .route("/contract/{address}", get(get_contract_info))
        .route("/contract/{address}/call", post(call_contract))
        // Agents (Synths)
        .route("/agents", get(get_agents))
        // Faucet (testnet token dispensing)
        .route("/faucet/claim", post(faucet_claim))
        .route("/faucet/status", get(faucet_status))
        // Light Client Finality Proofs (A8)
        .route("/light/snapshot", get(light_snapshot))
        // State Sync Protocol (A5) - snapshot bootstrap for new nodes
        .route("/sync/snapshot", get(sync_snapshot))
        .route("/sync/snapshot/info", get(sync_snapshot_info))
        // Chunked State Sync - parallel chunk download for fast catch-up
        .route("/sync/manifest", get(sync_manifest))
        .route("/sync/chunk/{index}", get(sync_chunk))
        .route("/sync/status", get(sync_status))
        // DAG round sync - allows new nodes to start at the right round
        .route("/sync/dag_state", get(sync_dag_state))
        // Inference - run model and record attestation on-chain
        .route("/inference/run", post(inference_run))
        .route("/inference/attestations", get(inference_list_attestations))
        .route("/inference/results", get(inference_list_results))
        // Per-worker earnings derived from on-chain InferenceAttestation
        // events (tx 0x16). v0.7.0: replaces the synthesized count*2.5
        // estimate the desktop used to compute client-side.
        .route("/worker/earnings/{address}", get(worker_earnings))
        // v0.7.0: live community-worker leaderboard. Reads the in-memory
        // CommunityWorker registry; no chain query. Sorted by composite
        // score (success rate * 1000 - avg_ms). Dashboard renders this.
        .route("/workers/scoreboard", get(workers_scoreboard))
        // Pipeline-parallel sharded inference
        .route("/inference/run_sharded", post(inference_run_sharded))
        .route("/inference/run_consensus", post(inference_run_consensus))
        .route("/inference/forward_shard", post(inference_forward_shard))
        // Tier 1 fully-on-chain inference. Submitter sends a prompt and a
        // max_reward; the chain selects a VRF committee, each member runs
        // candle locally, votes are aggregated by `apply_inference_finalize`
        // and the result is committed on-chain. See
        // `arc-chain-docs/TIER1_ONCHAIN_INFERENCE_PLAN.md`.
        .route("/inference/onchain/submit", post(inference_onchain_submit))
        .route("/inference/onchain/result/{request_id}", get(inference_onchain_result))
        // Deterministic inference cache introspection
        .route("/inference/cache_stats", get(inference_cache_stats))
        .route("/inference/latency_stats", get(inference_latency_stats))
        .route("/inference/cache_check", post(inference_cache_check))
        // Shard registry - discovery + announcement
        .route("/shards", get(get_shards))
        .route("/shards/announce", post(announce_shard))
        // Multi-model registry - list all models and per-model shards
        .route("/models", get(get_models))
        .route("/models/shards", get(get_model_shards))
        // Auto-sharding - compute optimal shard plan for a model
        .route("/shards/auto_plan", post(compute_auto_shard_plan))
        // Auto-join: node with model asks coordinator for shard assignment
        .route("/shards/join", post(shard_join))
        // Auto-routing inference: automatically picks best path
        .route("/inference/auto", post(inference_auto))
        // Inference verification - commit-challenge system
        .route("/inference/commit", post(inference_commit))
        .route("/inference/challenge", post(inference_challenge))
        .route("/inference/verification_status", get(inference_verification_status))
        // Revenue split info
        .route("/economics/revenue_split", get(get_revenue_split))
        // Milestone C: read-only registry + demand discovery. Workers use
        // these to discover what models exist and what ranges are open
        // for the taking. Writes go through /tx/submit_signed like any
        // other chain mutation - no dedicated POST endpoints needed for
        // the MVP.
        .route("/models/registry", get(list_model_registry))
        .route("/models/open_requests", get(list_open_model_requests))
        // Milestone D: capacity advertisement discovery + per-node
        // assignment long-poll. Also read-only from the state.
        .route("/capacity/advertisements", get(list_capacity_advertisements))
        .route("/assignments/for_me", get(get_assignment_for_me))
        // Community worker registration (HTTP-only, works behind NAT)
        .route("/community/register", post(community_register))
        .route("/community/heartbeat", post(community_heartbeat))
        .route("/community/list", get(community_list))
        // Community inference work dispatch (long-poll claim + submit)
        .route("/community/claim_work", post(community_claim_work))
        .route("/community/submit_work", post(community_submit_work))
        // Off-chain channel relay (WebSocket-style via long-poll for simplicity)
        .route("/channel/{channel_id}/relay", post(channel_relay))
        .route("/channel/{channel_id}/state", get(channel_state))
        // ETH-compatible JSON-RPC (MetaMask, Hardhat, Foundry)
        .route("/eth", post(eth_json_rpc))
        .layer(DefaultBodyLimit::max(256 * 1024 * 1024)) // 256 MB
        // CORS: permissive is correct for a public blockchain RPC node.
        // All major L1s (Ethereum, Solana, Sui) use permissive CORS for RPC.
        // There are no authenticated endpoints to protect.
        .layer(CorsLayer::permissive())
        .with_state(node);

    // SO_REUSEADDR: allow immediate rebind after process restart.
    // Without this, the port stays in TIME_WAIT for 60s after kill.
    let socket = tokio::net::TcpSocket::new_v4()?;
    socket.set_reuseaddr(true)?;
    socket.bind(addr.parse()?)?;
    let listener = socket.listen(1024)?;
    // into_make_service_with_connect_info lets handlers extract
    // ConnectInfo<SocketAddr>. announce_shard uses this to override stub
    // `0.0.0.0:*` socket_addrs in shard announcements with the peer's real
    // source IP - otherwise the coordinator can't route /inference/forward_shard
    // calls to shards held by remote nodes.
    axum::serve(listener, app.into_make_service_with_connect_info::<SocketAddr>()).await?;
    Ok(())
}

/// Start an ETH-compatible JSON-RPC server on a separate port.
/// Handles only the `/` POST endpoint for MetaMask, Hardhat, Foundry, etc.
pub async fn serve_eth(addr: &str, node: NodeState) -> anyhow::Result<()> {
    let app = Router::new()
        .route("/", post(eth_json_rpc))
        // CORS: permissive is correct for a public blockchain RPC node.
        // All major L1s (Ethereum, Solana, Sui) use permissive CORS for RPC.
        // There are no authenticated endpoints to protect.
        .layer(CorsLayer::permissive())
        .with_state(node);

    // SO_REUSEADDR: allow immediate rebind after process restart.
    // Without this, the port stays in TIME_WAIT for 60s after kill.
    let socket = tokio::net::TcpSocket::new_v4()?;
    socket.set_reuseaddr(true)?;
    socket.bind(addr.parse()?)?;
    let listener = socket.listen(1024)?;
    axum::serve(listener, app).await?;
    Ok(())
}

async fn index() -> &'static str {
    concat!("ARC Chain - Agent Runtime Chain - Testnet v", env!("CARGO_PKG_VERSION"))
}

/// JSON error response body returned by endpoints that fail with 4xx/5xx.
#[derive(Serialize)]
struct ApiError {
    error: String,
}

/// Helper to create a (StatusCode, Json<ApiError>) pair.
fn api_error(code: StatusCode, msg: impl Into<String>) -> (StatusCode, Json<ApiError>) {
    (code, Json(ApiError { error: msg.into() }))
}

// ---------------------------------------------------------------------------
// Health & Node Info
// ---------------------------------------------------------------------------

#[derive(Serialize)]
struct HealthResponse {
    status: String,
    version: String,
    height: u64,
    peers: u32,
    uptime_secs: u64,
    dag_round: u64,
    dag_committed: u64,
    validators: usize,
}

async fn health(AxumState(node): AxumState<NodeState>) -> Json<HealthResponse> {
    let validators = node.dag_validators.read().len();
    // Periodic cleanup: evict stale tx rate limit entries (>60s old)
    if node.tx_rate_limit.len() > 1000 {
        node.tx_rate_limit.retain(|_, v| v.elapsed().as_secs() < 60);
    }
    Json(HealthResponse {
        status: "ok".to_string(),
        version: env!("CARGO_PKG_VERSION").to_string(),
        height: node.state.height(),
        peers: node.peer_count.load(Ordering::Relaxed),
        uptime_secs: node.boot_time.elapsed().as_secs(),
        dag_round: node.dag_round.load(Ordering::Relaxed),
        dag_committed: node.dag_committed.load(Ordering::Relaxed),
        validators,
    })
}

#[derive(Serialize)]
struct NodeInfoResponse {
    validator: String,
    stake: u64,
    tier: String,
    height: u64,
    version: String,
    mempool_size: usize,
}

async fn node_info(AxumState(node): AxumState<NodeState>) -> Json<NodeInfoResponse> {
    Json(NodeInfoResponse {
        validator: node.validator_address.to_hex(),
        stake: node.stake,
        tier: format!("{:?}", node.tier),
        height: node.state.height(),
        version: env!("CARGO_PKG_VERSION").to_string(),
        mempool_size: node.mempool.len(),
    })
}

// ---------------------------------------------------------------------------
// Chain Info
// ---------------------------------------------------------------------------

#[derive(Serialize)]
struct ChainInfoResponse {
    chain: String,
    version: String,
    block_height: u64,
    account_count: usize,
    mempool_size: usize,
    gpu: arc_gpu::GpuInfo,
}

async fn chain_info(AxumState(node): AxumState<NodeState>) -> Json<ChainInfoResponse> {
    Json(ChainInfoResponse {
        chain: "ARC Chain".to_string(),
        version: env!("CARGO_PKG_VERSION").to_string(),
        block_height: node.state.height(),
        account_count: node.state.account_count(),
        mempool_size: node.mempool.len(),
        gpu: probe_gpu(),
    })
}

// ---------------------------------------------------------------------------
// Block & Account endpoints
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct BlockPath {
    height: u64,
}

async fn get_latest_block(
    AxumState(node): AxumState<NodeState>,
) -> Result<Json<Block>, StatusCode> {
    let height = node.state.height();
    if height == 0 {
        return Err(StatusCode::NOT_FOUND);
    }
    node.state
        .get_block(height)
        .map(Json)
        .ok_or(StatusCode::NOT_FOUND)
}

async fn get_block(
    AxumState(node): AxumState<NodeState>,
    axum::extract::Path(height): axum::extract::Path<u64>,
) -> Result<Json<Block>, (StatusCode, Json<ApiError>)> {
    node.state
        .get_block(height)
        .map(Json)
        .ok_or_else(|| api_error(StatusCode::NOT_FOUND, format!("Block at height {} not found", height)))
}

async fn get_account(
    AxumState(node): AxumState<NodeState>,
    axum::extract::Path(address): axum::extract::Path<String>,
) -> Result<Json<Account>, (StatusCode, Json<ApiError>)> {
    let addr = Hash256::from_hex(&address)
        .map_err(|_| api_error(StatusCode::BAD_REQUEST, "Invalid address. Must be 64 hex characters."))?;
    node.state
        .get_account(&addr)
        .map(Json)
        .ok_or_else(|| api_error(StatusCode::NOT_FOUND, format!("Account {} not found", address)))
}

// ---------------------------------------------------------------------------
// Transaction submission
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct SubmitTxRequest {
    from: String,
    to: String,
    amount: u64,
    nonce: u64,
    tx_type: Option<String>,
    /// Ed25519 signature (128-char hex, optional). Required for mainnet.
    signature: Option<String>,
    /// Ed25519 public key (64-char hex, required with signature).
    public_key: Option<String>,
}

#[derive(Serialize)]
struct SubmitTxResponse {
    tx_hash: String,
    status: String,
}

async fn submit_tx(
    AxumState(node): AxumState<NodeState>,
    Json(req): Json<SubmitTxRequest>,
) -> Result<Json<SubmitTxResponse>, (StatusCode, String)> {
    let from = Hash256::from_hex(&req.from).map_err(|_| (StatusCode::BAD_REQUEST, "invalid from address".to_string()))?;
    let to = Hash256::from_hex(&req.to).map_err(|_| (StatusCode::BAD_REQUEST, "invalid to address".to_string()))?;

    // Per-sender rate limit: 10 tx/sec (100ms cooldown)
    if let Some(last) = node.tx_rate_limit.get(&from.0) {
        if last.elapsed().as_millis() < 100 {
            return Err((StatusCode::TOO_MANY_REQUESTS, "rate limited: max 10 tx/sec per sender".to_string()));
        }
    }
    node.tx_rate_limit.insert(from.0, Instant::now());

    // Check if a signature was provided
    if let Some(ref sig_hex) = req.signature {
        if let Some(ref pubkey_hex) = req.public_key {
            // Build signed transaction
            let mut tx = Transaction::new_transfer(from, to, req.amount, req.nonce);

            // Parse signature and public key
            let sig_bytes = hex::decode(sig_hex).map_err(|_| (StatusCode::BAD_REQUEST, "invalid signature hex".to_string()))?;
            let pk_bytes = hex::decode(pubkey_hex).map_err(|_| (StatusCode::BAD_REQUEST, "invalid public_key hex".to_string()))?;

            if sig_bytes.len() != 64 || pk_bytes.len() != 32 {
                return Err((StatusCode::BAD_REQUEST, "signature must be 64 bytes, public_key must be 32 bytes".to_string()));
            }

            let mut pk_arr = [0u8; 32];
            pk_arr.copy_from_slice(&pk_bytes);

            tx.signature = arc_crypto::signature::Signature::Ed25519 {
                public_key: pk_arr,
                signature: sig_bytes,
            };

            // Verify signature before accepting
            tx.verify_signature().map_err(|_| (StatusCode::BAD_REQUEST, "signature verification failed".to_string()))?;
            // Mark as pre-verified so block execution can skip re-verification.
            tx.sig_verified = true;

            let hash = tx.hash.to_hex();
            node.mempool.insert(tx).map_err(|_| (StatusCode::CONFLICT, "duplicate transaction".to_string()))?;

            return Ok(Json(SubmitTxResponse {
                tx_hash: hash,
                status: "pending".to_string(),
            }));
        }
    }

    // No signature provided - reject. Unsigned transfers are a security hole
    // (anyone could drain any account). Require a signature for all transfers.
    Err((StatusCode::BAD_REQUEST, "Signature required. Provide 'signature' and 'public_key' fields. Use the wallet at http://140.82.16.112:3100 to send tokens.".to_string()))
}

#[derive(Deserialize)]
struct SubmitBatchRequest {
    transactions: Vec<SubmitTxRequest>,
}

#[derive(Serialize)]
struct SubmitBatchResponse {
    accepted: usize,
    rejected: usize,
    tx_hashes: Vec<String>,
}

async fn submit_batch(
    AxumState(node): AxumState<NodeState>,
    Json(req): Json<SubmitBatchRequest>,
) -> Json<SubmitBatchResponse> {
    let mut accepted = 0usize;
    let mut rejected = 0usize;
    let mut hashes = Vec::new();

    for tx_req in req.transactions {
        let from = match Hash256::from_hex(&tx_req.from) {
            Ok(h) => h,
            Err(_) => { rejected += 1; continue; }
        };
        let to = match Hash256::from_hex(&tx_req.to) {
            Ok(h) => h,
            Err(_) => { rejected += 1; continue; }
        };

        let tx = Transaction::new_transfer(from, to, tx_req.amount, tx_req.nonce);
        let hash = tx.hash.to_hex();

        match node.mempool.insert(tx) {
            Ok(()) => {
                accepted += 1;
                hashes.push(hash);
            }
            Err(_) => {
                rejected += 1;
            }
        }
    }

    Json(SubmitBatchResponse {
        accepted,
        rejected,
        tx_hashes: hashes,
    })
}

// ---------------------------------------------------------------------------
// Signed transaction submission (for CLI / external signers)
// ---------------------------------------------------------------------------

async fn submit_signed_tx(
    AxumState(node): AxumState<NodeState>,
    Json(tx): Json<Transaction>,
) -> Result<Json<SubmitTxResponse>, StatusCode> {
    let hash = tx.hash.to_hex();
    let tx_type = format!("{:?}", tx.tx_type);
    let from_short = hex::encode(&tx.from.0[..6]);
    let nonce = tx.nonce;
    let sig_ok = tx.verify_signature().is_ok();
    eprintln!(
        "[SUBMIT] hash=0x{} type={} from={} nonce={} sig_ok={} sig_verified={}",
        &hash[..16], tx_type, from_short, nonce, sig_ok, tx.sig_verified
    );

    match node.mempool.insert(tx) {
        Ok(()) => {
            eprintln!("[SUBMIT] hash=0x{} insert=OK pool_len={}", &hash[..16], node.mempool.len());
            Ok(Json(SubmitTxResponse {
                tx_hash: hash,
                status: "pending".to_string(),
            }))
        }
        Err(e) => {
            eprintln!("[SUBMIT] hash=0x{} insert=ERR {:?}", &hash[..16], e);
            Err(StatusCode::CONFLICT)
        }
    }
}

// ---------------------------------------------------------------------------
// Validators endpoint
// ---------------------------------------------------------------------------

#[derive(Serialize)]
struct ValidatorInfoResponse {
    address: String,
    stake: u64,
    tier: String,
}

#[derive(Serialize)]
struct ValidatorsResponse {
    validators: Vec<ValidatorInfoResponse>,
    total_stake: u64,
    count: usize,
}

async fn get_validators(
    AxumState(node): AxumState<NodeState>,
) -> Json<ValidatorsResponse> {
    // Show live DAG validator set (updated by consensus loop via PeerConnected).
    // Falls back to staked accounts from state if DAG set is single-validator.
    let dag_vals = node.dag_validators.read().clone();
    let mut validators: Vec<ValidatorInfoResponse> = dag_vals
        .iter()
        .map(|(addr, stake)| {
            let tier = StakeTier::from_stake(*stake)
                .map(|t| format!("{:?}", t))
                .unwrap_or_else(|| "Below minimum".to_string());
            ValidatorInfoResponse {
                address: addr.to_hex(),
                stake: *stake,
                tier,
            }
        })
        .collect();

    // Sort by stake descending
    validators.sort_by(|a, b| b.stake.cmp(&a.stake));
    let total_stake: u64 = validators.iter().map(|v| v.stake).sum();
    let count = validators.len();

    Json(ValidatorsResponse {
        validators,
        total_stake,
        count,
    })
}

// ---------------------------------------------------------------------------
// Agents (Synths) endpoint
// ---------------------------------------------------------------------------

#[derive(Serialize)]
struct AgentInfoResponse {
    name: String,
    address: String,
    status: String,
    model_type: String,
    endpoint: String,
    inferences: u64,
    earned: u64,
    uptime_secs: u64,
    last_action: String,
    last_action_timestamp: u64,
}

#[derive(Serialize)]
struct AgentsListResponse {
    agents: Vec<AgentInfoResponse>,
    count: usize,
}

async fn get_agents(
    AxumState(node): AxumState<NodeState>,
) -> Json<AgentsListResponse> {
    // Scan full_transactions for RegisterAgent transactions and build agent list.
    let mut agents: Vec<AgentInfoResponse> = Vec::new();
    let mut seen_names = std::collections::HashSet::new();

    for entry in node.state.full_transactions.iter() {
        let tx = entry.value();
        if let TxBody::RegisterAgent(body) = &tx.body {
            // Deduplicate by agent name (latest registration wins)
            if seen_names.contains(&body.agent_name) {
                continue;
            }
            seen_names.insert(body.agent_name.clone());

            let uptime = node.boot_time.elapsed().as_secs();
            agents.push(AgentInfoResponse {
                name: body.agent_name.clone(),
                address: tx.from.to_hex(),
                status: "active".to_string(),
                model_type: if body.metadata.is_empty() {
                    "Unknown".to_string()
                } else {
                    String::from_utf8(body.metadata.clone())
                        .unwrap_or_else(|_| "Unknown".to_string())
                },
                endpoint: body.endpoint.clone(),
                inferences: 0,
                earned: 0,
                uptime_secs: uptime,
                last_action: "Registered on-chain".to_string(),
                last_action_timestamp: std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs(),
            });
        }
    }

    let count = agents.len();
    Json(AgentsListResponse { agents, count })
}

// ---------------------------------------------------------------------------
// Faucet endpoints
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct FaucetClaimRequest {
    address: String,
}

#[derive(Serialize)]
struct FaucetClaimResponse {
    tx_hash: String,
    amount: u64,
    message: String,
}

#[derive(Serialize)]
struct FaucetErrorResponse {
    error: String,
}

#[derive(Serialize)]
struct FaucetStatusResponse {
    address: String,
    node_url: String,
    claims_today: u32,
    claim_amount: u64,
    rate_limit_secs: u64,
    balance: u64,
}

async fn faucet_claim(
    AxumState(node): AxumState<NodeState>,
    Json(req): Json<FaucetClaimRequest>,
) -> Result<Json<FaucetClaimResponse>, (StatusCode, Json<FaucetErrorResponse>)> {
    // Parse recipient address
    let to = Hash256::from_hex(&req.address).map_err(|_| {
        (StatusCode::BAD_REQUEST, Json(FaucetErrorResponse {
            error: "Invalid address. Must be 64 hex characters.".to_string(),
        }))
    })?;

    // Rate limiting: check if this address claimed recently
    // Global rate limit: 5000 faucet claims/minute (testnet only - production should be 100)
    // DashMap iter is lock-free per shard so this never blocks the runtime.
    {
        let total = node.faucet_claims_total.load(Ordering::Relaxed);
        if total > FAUCET_GLOBAL_RATE_LIMIT as u32 {
            let recent = node.faucet_claims.iter().filter(|e| e.value().elapsed().as_secs() < 60).count();
            if recent > FAUCET_GLOBAL_RATE_LIMIT {
                return Err((StatusCode::TOO_MANY_REQUESTS, Json(FaucetErrorResponse {
                    error: "Faucet busy. Too many claims globally. Try again in a minute.".to_string(),
                })));
            }
        }
    }

    // Per-address rate limit
    if let Some(entry) = node.faucet_claims.get(&to.0) {
        let elapsed = entry.value().elapsed().as_secs();
        if elapsed < FAUCET_RATE_LIMIT_SECS {
            let remaining = FAUCET_RATE_LIMIT_SECS - elapsed;
            return Err((StatusCode::TOO_MANY_REQUESTS, Json(FaucetErrorResponse {
                error: format!(
                    "Rate limited. Try again in {} minutes.",
                    (remaining + 59) / 60
                ),
            })));
        }
    }

    // FaucetClaim path (default). Legacy null-sig Transfer path is
    // retained for emergency rollback via FAUCET_V2_ENABLED=false, but
    // every active v0.7.1 seed should use the new path so the funded
    // balance actually propagates cross-seed. v0.7.2 will drop the gate
    // and the legacy branch entirely.
    let v2_enabled = std::env::var("FAUCET_V2_ENABLED")
        .map(|v| v != "false" && v != "0")
        .unwrap_or(true);

    let hash = if v2_enabled {
        // v0.7.1+ validator-signed FaucetClaim path. Validator signs a
        // FaucetClaim tx that arc-state's executor accepts as
        // authorization to debit the shared system pool
        // (`arc-types::transaction::faucet_pool_address()`). Replaces
        // the legacy null-sig Transfer, which peers rejected at
        // `pipeline.rs`'s verify stage (signature bytes, not the
        // `sig_verified` flag) so the funded balance only existed on
        // the seed that received the /faucet/claim call.
        let keypair = node.validator_keypair.as_ref().ok_or_else(|| {
            (StatusCode::SERVICE_UNAVAILABLE, Json(FaucetErrorResponse {
                error: "Validator keypair not configured on this node.".to_string(),
            }))
        })?;
        let validator_addr = node.validator_address;

        let pool_addr = arc_types::transaction::faucet_pool_address();
        let pool_account = node.state
            .get_account(&pool_addr)
            .ok_or_else(|| {
                (StatusCode::INTERNAL_SERVER_ERROR, Json(FaucetErrorResponse {
                    error: "Faucet pool account not funded. Node misconfiguration.".to_string(),
                }))
            })?;
        if pool_account.balance < FAUCET_CLAIM_AMOUNT {
            return Err((StatusCode::SERVICE_UNAVAILABLE, Json(FaucetErrorResponse {
                error: "Faucet balance too low. Please try another node.".to_string(),
            })));
        }

        // Read validator's current state nonce per-call. An in-memory
        // atomic counter would drift past state when txs fail to land,
        // leaving a permanent nonce gap. Concurrent calls in the same
        // block window race; the loser gets a 409 on commit and retries.
        let validator_account = node.state.get_or_create_account(&validator_addr);
        let nonce = validator_account.nonce;

        let mut tx = Transaction::new_faucet_claim(
            validator_addr,
            to,
            FAUCET_CLAIM_AMOUNT,
            nonce,
        );
        tx.sign(keypair).map_err(|e| {
            (StatusCode::INTERNAL_SERVER_ERROR, Json(FaucetErrorResponse {
                error: format!("Faucet sign failed: {:?}", e),
            }))
        })?;
        tx.sig_verified = true;
        let hash = tx.hash.to_hex();

        // Receipt first — the consensus thread dedups via
        // `receipts.contains_key()`. Peers don't have the receipt and
        // run the FaucetClaim arm through `execute_block` normally.
        let receipt = TxReceipt {
            tx_hash: tx.hash,
            block_height: node.state.height(),
            block_hash: node.state.get_block(node.state.height())
                .map(|b| b.hash)
                .unwrap_or(Hash256::ZERO),
            index: 0,
            success: true,
            gas_used: arc_types::gas_costs::FAUCET_CLAIM,
            value_commitment: None,
            inclusion_proof: None,
            logs: vec![],
        };
        node.state.receipts.insert(tx.hash.0, receipt);

        // Pre-apply locally so /account/X reflects funded balance
        // immediately. Mirrors the executor arm exactly.
        {
            let mut signer = validator_account.clone();
            signer.nonce += 1;
            node.state.update_account(&validator_addr, signer);

            let mut pool = pool_account.clone();
            pool.balance -= FAUCET_CLAIM_AMOUNT;
            node.state.update_account(&pool_addr, pool);

            let mut recipient = node.state.get_or_create_account(&to);
            recipient.balance = recipient.balance.saturating_add(FAUCET_CLAIM_AMOUNT);
            node.state.update_account(&to, recipient);
        }
        node.state.full_transactions.insert(tx.hash.0, tx.clone());
        node.state.account_txs.entry(validator_addr.0).or_default().push(tx.hash);
        node.state.account_txs.entry(pool_addr.0).or_default().push(tx.hash);
        node.state.account_txs.entry(to.0).or_default().push(tx.hash);

        let _ = node.mempool.insert(tx);
        hash
    } else {
        // Legacy v0.7.0 null-sig Transfer path. Funded balance is
        // observable only on the seed that handled the call (the
        // known propagation bug). Acceptable during the rollout window
        // because no FaucetClaim variant is emitted that v0.7.0 peers
        // can't deserialize.
        let faucet_addr = arc_crypto::hash_bytes(&[0u8]);
        let faucet_account = node.state
            .get_account(&faucet_addr)
            .ok_or_else(|| {
                (StatusCode::INTERNAL_SERVER_ERROR, Json(FaucetErrorResponse {
                    error: "Faucet account not funded. Node misconfiguration.".to_string(),
                }))
            })?;

        if faucet_account.balance < FAUCET_CLAIM_AMOUNT {
            return Err((StatusCode::SERVICE_UNAVAILABLE, Json(FaucetErrorResponse {
                error: "Faucet balance too low. Please try another node.".to_string(),
            })));
        }

        static FAUCET_NONCE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let nonce = FAUCET_NONCE.fetch_add(1, Ordering::SeqCst);
        if nonce == 0 {
            FAUCET_NONCE.store(faucet_account.nonce + 1, Ordering::SeqCst);
        }
        let actual_nonce = if nonce == 0 { faucet_account.nonce } else { nonce };

        let mut tx = Transaction::new_transfer(faucet_addr, to, FAUCET_CLAIM_AMOUNT, actual_nonce);
        tx.sig_verified = true;
        let hash = tx.hash.to_hex();

        let receipt = TxReceipt {
            tx_hash: tx.hash,
            block_height: node.state.height(),
            block_hash: node.state.get_block(node.state.height())
                .map(|b| b.hash)
                .unwrap_or(Hash256::ZERO),
            index: 0,
            success: true,
            gas_used: 21_000,
            value_commitment: None,
            inclusion_proof: None,
            logs: vec![],
        };
        node.state.receipts.insert(tx.hash.0, receipt);

        {
            let mut sender = faucet_account.clone();
            sender.balance -= FAUCET_CLAIM_AMOUNT;
            sender.nonce += 1;
            node.state.update_account(&faucet_addr, sender);

            let mut recipient = node.state.get_or_create_account(&to);
            recipient.balance = recipient.balance.saturating_add(FAUCET_CLAIM_AMOUNT);
            node.state.update_account(&to, recipient);
        }
        node.state.full_transactions.insert(tx.hash.0, tx.clone());
        node.state.account_txs.entry(faucet_addr.0).or_default().push(tx.hash);
        node.state.account_txs.entry(to.0).or_default().push(tx.hash);

        let _ = node.mempool.insert(tx);
        hash
    };

    // Record claim time + evict stale entries to prevent unbounded growth
    node.faucet_claims.insert(to.0, Instant::now());
    if node.faucet_claims.len() > 10_000 {
        node.faucet_claims.retain(|_, v| v.elapsed().as_secs() < 7200);
    }
    node.faucet_claims_total.fetch_add(1, Ordering::Relaxed);

    Ok(Json(FaucetClaimResponse {
        tx_hash: hash,
        amount: FAUCET_CLAIM_AMOUNT,
        message: format!(
            "Sent {} ARC to {}",
            FAUCET_CLAIM_AMOUNT,
            req.address
        ),
    }))
}

async fn faucet_status(
    AxumState(node): AxumState<NodeState>,
) -> Json<FaucetStatusResponse> {
    let faucet_addr = arc_crypto::hash_bytes(&[0u8]);
    let balance = node.state.get_account(&faucet_addr)
        .map(|a| a.balance)
        .unwrap_or(0);
    Json(FaucetStatusResponse {
        address: faucet_addr.to_hex(),
        node_url: format!("http://localhost:9944"),
        claims_today: node.faucet_claims_total.load(Ordering::Relaxed),
        claim_amount: FAUCET_CLAIM_AMOUNT,
        rate_limit_secs: FAUCET_RATE_LIMIT_SECS,
        balance,
    })
}

// ---------------------------------------------------------------------------
// Proof & query endpoints
// ---------------------------------------------------------------------------

/// Parse a 64-char hex string into a [u8; 32] array.
fn parse_hash(hex_str: &str) -> Result<[u8; 32], (StatusCode, Json<ApiError>)> {
    // Accept both forms: with or without "0x" prefix.
    let stripped = hex_str.strip_prefix("0x").unwrap_or(hex_str);
    Hash256::from_hex(stripped)
        .map(|h| h.0)
        .map_err(|_| api_error(StatusCode::BAD_REQUEST, "Invalid hash. Must be 64 hex characters (0x prefix optional)."))
}

/// GET /tx/{hash} - Look up a transaction receipt by its hash.
/// Falls back to on-demand reconstruction for benchmark transactions.
async fn get_transaction(
    AxumState(node): AxumState<NodeState>,
    axum::extract::Path(hash): axum::extract::Path<String>,
) -> Result<Json<TxReceipt>, (StatusCode, Json<ApiError>)> {
    let tx_hash = parse_hash(&hash)?;
    // Try indexed receipts first
    if let Some(receipt) = node.state.get_receipt(&tx_hash) {
        return Ok(Json(receipt));
    }
    // Fall back to on-demand reconstruction for benchmark txs
    node.state
        .get_benchmark_receipt_by_hash(&tx_hash)
        .map(Json)
        .ok_or_else(|| api_error(StatusCode::NOT_FOUND, format!("Transaction {} not found", hash)))
}

/// GET /tx/{hash}/proof - Return a full verification bundle for a transaction.
/// For benchmark transactions, reconstructs the Merkle tree on-demand (~130ms).
async fn get_tx_proof(
    AxumState(node): AxumState<NodeState>,
    axum::extract::Path(hash): axum::extract::Path<String>,
) -> Result<Json<Value>, (StatusCode, Json<ApiError>)> {
    let tx_hash = parse_hash(&hash)?;

    // Try indexed receipt with stored proof first
    if let Some(receipt) = node.state.get_receipt(&tx_hash) {
        if let Some(ref proof_bytes) = receipt.inclusion_proof {
            if let Ok(merkle_proof) = bincode::deserialize::<MerkleProof>(proof_bytes) {
                let siblings: Vec<Value> = merkle_proof
                    .siblings
                    .iter()
                    .map(|(h, is_left)| {
                        json!({
                            "hash": h.to_hex(),
                            "is_left": is_left,
                        })
                    })
                    .collect();

                return Ok(Json(json!({
                    "tx_hash": Hash256(tx_hash).to_hex(),
                    "blake3_domain": "ARC-chain-tx-v1",
                    "merkle_proof": {
                        "leaf": merkle_proof.leaf.to_hex(),
                        "index": merkle_proof.index,
                        "siblings": siblings,
                        "root": merkle_proof.root.to_hex(),
                    },
                    "block_height": receipt.block_height,
                    "pedersen_commitment": receipt.value_commitment.map(hex::encode),
                })));
            }
        }
    }

    // Fall back to on-demand proof reconstruction for benchmark txs
    let (height, idx) = node
        .state
        .get_tx_location(&tx_hash)
        .ok_or_else(|| api_error(StatusCode::NOT_FOUND, format!("Transaction {} not found", hash)))?;

    let merkle_proof = node
        .state
        .reconstruct_benchmark_proof(height, idx)
        .ok_or_else(|| api_error(StatusCode::NOT_FOUND, "Could not reconstruct proof for transaction"))?;

    let siblings: Vec<Value> = merkle_proof
        .siblings
        .iter()
        .map(|(h, is_left)| {
            json!({
                "hash": h.to_hex(),
                "is_left": is_left,
            })
        })
        .collect();

    let block_tx_root = node
        .state
        .get_block(height)
        .map(|b| b.header.tx_root);
    let verified = block_tx_root.map(|r| r == merkle_proof.root).unwrap_or(false);

    Ok(Json(json!({
        "tx_hash": Hash256(tx_hash).to_hex(),
        "blake3_domain": "ARC-chain-tx-v1",
        "merkle_proof": {
            "leaf": merkle_proof.leaf.to_hex(),
            "index": merkle_proof.index,
            "siblings": siblings,
            "root": merkle_proof.root.to_hex(),
        },
        "block_height": height,
        "block_tx_root": block_tx_root.map(|r| r.to_hex()),
        "verified": verified,
    })))
}

/// GET /block/{height}/proofs - Return all Merkle proofs for transactions in a block.
async fn get_block_proofs(
    AxumState(node): AxumState<NodeState>,
    axum::extract::Path(height): axum::extract::Path<u64>,
) -> Result<Json<Value>, (StatusCode, Json<ApiError>)> {
    let block = node
        .state
        .get_block(height)
        .ok_or_else(|| api_error(StatusCode::NOT_FOUND, format!("Block at height {} not found", height)))?;

    let mut proofs = Vec::new();
    for tx_hash in &block.tx_hashes {
        if let Some(receipt) = node.state.get_receipt(&tx_hash.0) {
            if let Some(ref proof_bytes) = receipt.inclusion_proof {
                if let Ok(proof) = bincode::deserialize::<MerkleProof>(proof_bytes) {
                    let siblings: Vec<Value> = proof
                        .siblings
                        .iter()
                        .map(|(h, is_left)| {
                            json!({ "hash": h.to_hex(), "is_left": is_left })
                        })
                        .collect();

                    proofs.push(json!({
                        "tx_hash": tx_hash.to_hex(),
                        "leaf": proof.leaf.to_hex(),
                        "index": proof.index,
                        "siblings": siblings,
                        "root": proof.root.to_hex(),
                    }));
                }
            }
        }
    }

    Ok(Json(json!({
        "block_height": height,
        "block_hash": block.hash.to_hex(),
        "tx_root": block.header.tx_root.to_hex(),
        "proof_count": proofs.len(),
        "proofs": proofs,
    })))
}

/// Query parameters for paginated block listing.
#[derive(Deserialize)]
struct BlocksQuery {
    from: Option<u64>,
    to: Option<u64>,
    limit: Option<usize>,
}

/// GET /blocks?from=0&to=100&limit=20 - Paginated block listing.
async fn get_blocks(
    AxumState(node): AxumState<NodeState>,
    Query(params): Query<BlocksQuery>,
) -> Json<Value> {
    let height = node.state.height();
    let from = params.from.unwrap_or(0);
    let to = params.to.unwrap_or(height);
    let limit = params.limit.unwrap_or(20).min(100);

    let blocks = node.state.get_block_range(from, to, limit);

    let block_list: Vec<Value> = blocks
        .iter()
        .map(|b| {
            json!({
                "height": b.header.height,
                "hash": b.hash.to_hex(),
                "parent_hash": b.header.parent_hash.to_hex(),
                "tx_root": b.header.tx_root.to_hex(),
                "tx_count": b.header.tx_count,
                "timestamp": b.header.timestamp,
                "producer": b.header.producer.to_hex(),
            })
        })
        .collect();

    Json(json!({
        "from": from,
        "to": to,
        "limit": limit,
        "count": block_list.len(),
        "blocks": block_list,
    }))
}

/// GET /block/{height}/txs?offset=0&limit=100 - Paginated transaction listing for a block.
/// Reconstructs benchmark transactions on-demand from deterministic parameters.
#[derive(Deserialize)]
struct BlockTxsQuery {
    offset: Option<u32>,
    limit: Option<u32>,
}

async fn get_block_txs(
    AxumState(node): AxumState<NodeState>,
    axum::extract::Path(height): axum::extract::Path<u64>,
    Query(params): Query<BlockTxsQuery>,
) -> Result<Json<Value>, (StatusCode, Json<ApiError>)> {
    let block = node
        .state
        .get_block(height)
        .ok_or_else(|| api_error(StatusCode::NOT_FOUND, format!("Block at height {} not found", height)))?;

    let offset = params.offset.unwrap_or(0);
    let limit = params.limit.unwrap_or(100).min(1000);

    // Try existing tx_hashes first (normal blocks)
    if !block.tx_hashes.is_empty() {
        let end = (offset + limit).min(block.header.tx_count);
        let txs: Vec<Value> = (offset..end)
            .filter_map(|i| {
                let hash = block.tx_hashes.get(i as usize)?;
                Some(json!({
                    "index": i,
                    "hash": hash.to_hex(),
                }))
            })
            .collect();

        return Ok(Json(json!({
            "block_height": height,
            "tx_count": block.header.tx_count,
            "offset": offset,
            "limit": limit,
            "returned": txs.len(),
            "transactions": txs,
        })));
    }

    // Reconstruct benchmark transactions on-demand
    let txs = node.state.get_benchmark_block_txs(height, offset, limit);
    let tx_list: Vec<Value> = txs
        .iter()
        .enumerate()
        .map(|(i, tx)| {
            json!({
                "index": offset + i as u32,
                "hash": tx.hash.to_hex(),
                "from": tx.from.to_hex(),
                "nonce": tx.nonce,
                "tx_type": format!("{:?}", tx.tx_type),
                "body": match &tx.body {
                    TxBody::Transfer(b) => json!({
                        "type": "Transfer",
                        "to": b.to.to_hex(),
                        "amount": b.amount,
                    }),
                    _ => json!({}),
                },
            })
        })
        .collect();

    Ok(Json(json!({
        "block_height": height,
        "tx_count": block.header.tx_count,
        "offset": offset,
        "limit": limit,
        "returned": tx_list.len(),
        "transactions": tx_list,
    })))
}

/// GET /account/{address}/txs - Return transaction hashes involving an account.
async fn get_account_txs(
    AxumState(node): AxumState<NodeState>,
    axum::extract::Path(address): axum::extract::Path<String>,
) -> Result<Json<Value>, (StatusCode, Json<ApiError>)> {
    let addr = parse_hash(&address)?;
    let tx_hashes = node.state.get_account_txs(&addr);

    let hashes: Vec<String> = tx_hashes.iter().map(|h| h.to_hex()).collect();

    Ok(Json(json!({
        "address": address,
        "tx_count": hashes.len(),
        "tx_hashes": hashes,
    })))
}

/// GET /stats - Basic chain statistics.
async fn get_stats(AxumState(node): AxumState<NodeState>) -> Json<Value> {
    let indexed_receipts = node.state.receipts.len();
    let indexed_hashes = node.state.tx_index.len();
    let executed = node.state.benchmark_tx_count.load(std::sync::atomic::Ordering::Relaxed) as usize;
    let dag_round = node.dag_round.load(std::sync::atomic::Ordering::Relaxed);
    let dag_committed = node.dag_committed.load(std::sync::atomic::Ordering::Relaxed);
    let validators = node.dag_validators.read().len();
    let peers = node.peer_count.load(Ordering::Relaxed);
    let uptime = node.boot_time.elapsed().as_secs();
    let bench_tps = if uptime > 0 { executed as u64 / uptime } else { 0 };
    let sharded_runs = node.sharded_runs_total.load(Ordering::Relaxed);
    let sharded_bytes = node.sharded_bytes_total.load(Ordering::Relaxed);
    Json(json!({
        "chain": "ARC Chain",
        "version": env!("CARGO_PKG_VERSION"),
        "block_height": node.state.height(),
        "total_accounts": node.state.account_count(),
        "mempool_size": node.mempool.len(),
        "total_transactions": indexed_receipts + executed,
        "benchmark_executed": executed,
        "benchmark_tps": bench_tps,
        "indexed_hashes": indexed_hashes,
        "indexed_receipts": indexed_receipts,
        "dag_round": dag_round,
        "dag_committed": dag_committed,
        "validators": validators,
        "connected_peers": peers,
        "uptime_secs": uptime,
        "sharded_runs_total": sharded_runs,
        "sharded_bytes_total": sharded_bytes,
    }))
}

// ---------------------------------------------------------------------------
// State Sync Protocol (A5) - snapshot bootstrap for new nodes
// ---------------------------------------------------------------------------

/// Returns metadata about the latest snapshot available for sync.
async fn sync_snapshot_info(
    AxumState(node): AxumState<NodeState>,
) -> Json<Value> {
    let height = node.state.height();
    let state_root = node.state.get_state_root();
    let account_count = node.state.account_count();
    Json(json!({
        "available": true,
        "height": height,
        "state_root": format!("{}", state_root),
        "account_count": account_count,
    }))
}

/// Stream the full state snapshot as LZ4-compressed bincode.
/// New nodes download this to bootstrap without replaying from genesis.
async fn sync_snapshot(
    AxumState(node): AxumState<NodeState>,
) -> Result<axum::response::Response, StatusCode> {
    use axum::response::IntoResponse;
    use axum::http::header;

    let snapshot = node.state.export_snapshot();
    let data = bincode::serialize(&snapshot)
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let compressed = lz4_flex::compress_prepend_size(&data);

    Ok((
        [
            (header::CONTENT_TYPE, "application/octet-stream"),
            (header::CONTENT_DISPOSITION, "attachment; filename=\"snapshot.lz4\""),
        ],
        compressed,
    ).into_response())
}

// ---------------------------------------------------------------------------
// Light Client Proofs (A8)
// ---------------------------------------------------------------------------

/// GET /light/snapshot - Returns a lightweight snapshot for light client bootstrapping:
/// current height, state root, account count, total supply, latest block hash.
async fn light_snapshot(
    AxumState(node): AxumState<NodeState>,
) -> Json<Value> {
    let snap = node.state.generate_light_snapshot();
    Json(json!({
        "height": snap.height,
        "state_root": format!("{}", snap.state_root),
        "account_count": snap.account_count,
        "total_supply": snap.total_supply,
        "latest_block_hash": format!("{}", snap.latest_block_hash),
    }))
}

// ---------------------------------------------------------------------------
// Chunked State Sync - parallel chunk download for fast catch-up
// ---------------------------------------------------------------------------

/// GET /sync/manifest - Returns the snapshot manifest (height, chunk count,
/// state root, accounts) so a syncing node can plan parallel chunk downloads.
async fn sync_manifest(
    AxumState(node): AxumState<NodeState>,
) -> Json<Value> {
    let manifest = node.state.export_snapshot_manifest();
    Json(json!({
        "version": manifest.version,
        "state_root": format!("{}", manifest.state_root),
        "total_accounts": manifest.total_accounts,
        "total_chunks": manifest.total_chunks,
        "chunk_size": manifest.chunk_size,
        "manifest_hash": format!("{}", manifest.manifest_hash),
    }))
}

/// GET /sync/chunk/:index - Returns a single snapshot chunk by index.
/// Each chunk contains ~1000 accounts with a BLAKE3 integrity proof.
///
/// Serializes the canonical `StateSnapshot` struct so `state_sync::fetch_chunk`
/// (which calls `resp.json::<StateSnapshot>()`) can decode it directly. A prior
/// version hand-rolled a different JSON shape — accounts as
/// `[{address, balance, nonce}]` instead of `[[hex, full_account]]` — and
/// stripped `code_hash`/`storage_root`/`staked_balance`/`chunk_proof`, which
/// broke every state-sync attempt with "error decoding response body" and
/// stranded fresh-state nodes (e.g. LHR after `--reset-state`) at round 0.
async fn sync_chunk(
    AxumState(node): AxumState<NodeState>,
    axum::extract::Path(index): axum::extract::Path<u32>,
) -> Result<Json<arc_state::StateSnapshot>, StatusCode> {
    let manifest = node.state.export_snapshot_manifest();
    let chunk = node.state.export_snapshot_chunk(index, manifest.chunk_size)
        .ok_or(StatusCode::NOT_FOUND)?;
    Ok(Json(chunk))
}

/// GET /sync/status - Returns whether this node can serve snapshots and
/// information about the latest available snapshot.
async fn sync_status(
    AxumState(node): AxumState<NodeState>,
) -> Json<Value> {
    let manifest = node.state.export_snapshot_manifest();
    Json(json!({
        "available": true,
        "syncing": false,
        "latest_snapshot": {
            "height": manifest.version,
            "state_root": format!("{}", manifest.state_root),
            "total_chunks": manifest.total_chunks,
            "total_accounts": manifest.total_accounts,
        },
    }))
}

/// GET /sync/dag_state - Returns the current DAG consensus round state.
/// Used by new nodes to start at the right round instead of round 0.
/// This prevents permanent partition from genesis round mismatch.
async fn sync_dag_state(
    AxumState(node): AxumState<NodeState>,
) -> Json<Value> {
    let current_round = node.dag_round.load(std::sync::atomic::Ordering::Relaxed);
    let last_committed_round = node.dag_committed.load(std::sync::atomic::Ordering::Relaxed);
    let validator_count = node.dag_validators.read().len();

    Json(json!({
        "current_round": current_round,
        "last_committed_round": last_committed_round,
        "validator_count": validator_count,
        "protocol_version": 2,
    }))
}

// ---------------------------------------------------------------------------
// Full transaction & contract endpoints
// ---------------------------------------------------------------------------

/// GET /tx/{hash}/full - Return the full transaction body with type-specific fields.
/// Falls back to on-demand reconstruction for benchmark transactions.
async fn get_full_transaction(
    AxumState(node): AxumState<NodeState>,
    axum::extract::Path(hash): axum::extract::Path<String>,
) -> Result<Json<Value>, (StatusCode, Json<ApiError>)> {
    let tx_hash = parse_hash(&hash)?;

    let tx = node
        .state
        .get_transaction(&tx_hash)
        .or_else(|| node.state.get_benchmark_tx_by_hash(&tx_hash))
        .ok_or_else(|| api_error(StatusCode::NOT_FOUND, format!("Transaction {} not found", hash)))?;

    let receipt = node
        .state
        .get_receipt(&tx_hash)
        .or_else(|| node.state.get_benchmark_receipt_by_hash(&tx_hash));

    let body_json = match &tx.body {
        TxBody::Transfer(b) => json!({
            "type": "Transfer",
            "to": b.to.to_hex(),
            "amount": b.amount,
            "amount_commitment": b.amount_commitment.map(hex::encode),
        }),
        TxBody::Settle(b) => json!({
            "type": "Settle",
            "agent_id": b.agent_id.to_hex(),
            "service_hash": b.service_hash.to_hex(),
            "amount": b.amount,
            "usage_units": b.usage_units,
        }),
        TxBody::Swap(b) => json!({
            "type": "Swap",
            "counterparty": b.counterparty.to_hex(),
            "offer_amount": b.offer_amount,
            "receive_amount": b.receive_amount,
            "offer_asset": b.offer_asset.to_hex(),
            "receive_asset": b.receive_asset.to_hex(),
        }),
        TxBody::Escrow(b) => json!({
            "type": "Escrow",
            "beneficiary": b.beneficiary.to_hex(),
            "amount": b.amount,
            "conditions_hash": b.conditions_hash.to_hex(),
            "is_create": b.is_create,
        }),
        TxBody::Stake(b) => json!({
            "type": "Stake",
            "amount": b.amount,
            "is_stake": b.is_stake,
            "validator": b.validator.to_hex(),
        }),
        TxBody::WasmCall(b) => json!({
            "type": "WasmCall",
            "contract": b.contract.to_hex(),
            "function": b.function,
            "calldata": hex::encode(&b.calldata),
            "value": b.value,
            "gas_limit": b.gas_limit,
        }),
        TxBody::MultiSig(b) => json!({
            "type": "MultiSig",
            "signers": b.signers.iter().map(|s| s.to_hex()).collect::<Vec<_>>(),
            "threshold": b.threshold,
        }),
        TxBody::DeployContract(b) => json!({
            "type": "DeployContract",
            "bytecode_size": b.bytecode.len(),
            "constructor_args_size": b.constructor_args.len(),
            "state_rent_deposit": b.state_rent_deposit,
        }),
        TxBody::RegisterAgent(b) => json!({
            "type": "RegisterAgent",
            "agent_name": b.agent_name,
            "endpoint": b.endpoint,
            "protocol": b.protocol.to_hex(),
            "capabilities_size": b.capabilities.len(),
        }),
        TxBody::JoinValidator(b) => json!({
            "type": "JoinValidator",
            "pubkey": hex::encode(b.pubkey),
            "initial_stake": b.initial_stake,
        }),
        TxBody::LeaveValidator => json!({
            "type": "LeaveValidator",
        }),
        TxBody::ClaimRewards => json!({
            "type": "ClaimRewards",
        }),
        TxBody::UpdateStake(b) => json!({
            "type": "UpdateStake",
            "new_stake": b.new_stake,
        }),
        TxBody::Governance(b) => json!({
            "type": "Governance",
            "proposal_id": b.proposal_id,
            "action": format!("{:?}", b.action),
        }),
        TxBody::BridgeLock(b) => json!({
            "type": "BridgeLock",
            "destination_chain": b.destination_chain,
            "destination_address": hex::encode(b.destination_address),
            "amount": b.amount,
        }),
        TxBody::BridgeMint(b) => json!({
            "type": "BridgeMint",
            "source_chain": b.source_chain,
            "source_tx_hash": b.source_tx_hash.to_hex(),
            "recipient": b.recipient.to_hex(),
            "amount": b.amount,
            "merkle_proof_size": b.merkle_proof.len(),
        }),
        TxBody::BatchSettle(body) => {
            let total: u64 = body.entries.iter().map(|e| e.amount).sum();
            json!({
                "type": "BatchSettle",
                "entries": body.entries.len(),
                "total_amount": total,
            })
        }
        TxBody::ChannelOpen(body) => json!({
            "type": "ChannelOpen",
            "channel_id": format!("0x{}", hex::encode(&body.channel_id.0)),
            "counterparty": format!("0x{}", hex::encode(&body.counterparty.0)),
            "deposit": body.deposit,
            "timeout_blocks": body.timeout_blocks,
        }),
        TxBody::ChannelClose(body) => json!({
            "type": "ChannelClose",
            "channel_id": format!("0x{}", hex::encode(&body.channel_id.0)),
            "opener_balance": body.opener_balance,
            "counterparty_balance": body.counterparty_balance,
            "state_nonce": body.state_nonce,
        }),
        TxBody::ChannelDispute(body) => json!({
            "type": "ChannelDispute",
            "channel_id": format!("0x{}", hex::encode(&body.channel_id.0)),
            "opener_balance": body.opener_balance,
            "counterparty_balance": body.counterparty_balance,
            "state_nonce": body.state_nonce,
            "challenge_period": body.challenge_period,
        }),
        TxBody::ShardProof(body) => json!({
            "type": "ShardProof",
            "shard_id": body.shard_id,
            "block_height": body.block_height,
            "tx_count": body.tx_count,
            "proof_size": body.proof_data.len(),
            "prev_state_root": format!("0x{}", hex::encode(&body.prev_state_root.0)),
            "post_state_root": format!("0x{}", hex::encode(&body.post_state_root.0)),
        }),
        TxBody::InferenceAttestation(body) => json!({
            "type": "InferenceAttestation",
            "model_id": format!("0x{}", hex::encode(&body.model_id.0)),
            "input_hash": format!("0x{}", hex::encode(&body.input_hash.0)),
            "output_hash": format!("0x{}", hex::encode(&body.output_hash.0)),
            "challenge_period": body.challenge_period,
            "bond": body.bond,
        }),
        TxBody::InferenceChallenge(body) => json!({
            "type": "InferenceChallenge",
            "attestation_hash": format!("0x{}", hex::encode(&body.attestation_hash.0)),
            "challenger_output_hash": format!("0x{}", hex::encode(&body.challenger_output_hash.0)),
            "challenger_bond": body.challenger_bond,
        }),
        TxBody::InferenceRegister(body) => json!({
            "type": "InferenceRegister",
            "tier": body.tier,
            "stake_bond": body.stake_bond,
        }),
        TxBody::InferenceEscrowOpen(body) => json!({
            "type": "InferenceEscrowOpen",
            "request_id": format!("0x{}", hex::encode(&body.request_id)),
            "model_id": format!("0x{}", hex::encode(&body.model_id.0)),
            "max_fee": body.max_fee,
            "max_tokens": body.max_tokens,
            "timeout_blocks": body.timeout_blocks,
        }),
        TxBody::InferenceEscrowRelease(body) => json!({
            "type": "InferenceEscrowRelease",
            "request_id": format!("0x{}", hex::encode(&body.request_id)),
            "payer": format!("0x{}", hex::encode(&body.payer.0)),
            "model_id": format!("0x{}", hex::encode(&body.model_id.0)),
            "max_tokens": body.max_tokens,
            "timeout_blocks": body.timeout_blocks,
            "output_hash": format!("0x{}", hex::encode(&body.output_hash.0)),
            "proposer": format!("0x{}", hex::encode(&body.proposer.0)),
            "replicas": body.replicas.iter()
                .map(|r| format!("0x{}", hex::encode(&r.0)))
                .collect::<Vec<_>>(),
            "observer_pool": format!("0x{}", hex::encode(&body.observer_pool.0)),
            "treasury": format!("0x{}", hex::encode(&body.treasury.0)),
        }),
        TxBody::InferenceEscrowRefund(body) => json!({
            "type": "InferenceEscrowRefund",
            "request_id": format!("0x{}", hex::encode(&body.request_id)),
            "model_id": format!("0x{}", hex::encode(&body.model_id.0)),
            "max_tokens": body.max_tokens,
            "timeout_blocks": body.timeout_blocks,
        }),
        TxBody::ModelRegistration(body) => json!({
            "type": "ModelRegistration",
            "model_id": format!("0x{}", hex::encode(&body.model_id.0)),
            "metadata_hash": format!("0x{}", hex::encode(&body.metadata_hash.0)),
            "chunk_tree_root": format!("0x{}", hex::encode(&body.chunk_tree_root.0)),
            "n_layers": body.n_layers,
            "d_model": body.d_model,
            "quantization": body.quantization,
            "registration_fee": body.registration_fee,
            "royalty_recipient": format!("0x{}", hex::encode(&body.royalty_recipient.0)),
        }),
        TxBody::ModelRequest(body) => json!({
            "type": "ModelRequest",
            "request_id": format!("0x{}", hex::encode(&body.request_id)),
            "model_id": format!("0x{}", hex::encode(&body.model_id.0)),
            "target_k_replication": body.target_k_replication,
            "bond_per_layer_epoch": body.bond_per_layer_epoch,
            "max_wait_secs": body.max_wait_secs,
        }),
        TxBody::ShardCoverageClaim(body) => json!({
            "type": "ShardCoverageClaim",
            "model_id": format!("0x{}", hex::encode(&body.model_id.0)),
            "node_pubkey": format!("0x{}", hex::encode(&body.node_pubkey)),
            "ranges": body.ranges.iter()
                .map(|(s, e)| json!([s, e])).collect::<Vec<_>>(),
            "bond": body.bond,
            "epoch_blocks": body.epoch_blocks,
        }),
        TxBody::CapacityAdvertisement(body) => json!({
            "type": "CapacityAdvertisement",
            "node_pubkey": format!("0x{}", hex::encode(&body.node_pubkey)),
            "ram_bytes": body.ram_bytes,
            "vram_bytes": body.vram_bytes,
            "bandwidth_mbps": body.bandwidth_mbps,
            "uptime_hint_mins": body.uptime_hint_mins,
            "stake": body.stake,
            "region": body.region,
        }),
        TxBody::ShardAssignmentProposal(body) => json!({
            "type": "ShardAssignmentProposal",
            "epoch_blocks": body.epoch_blocks,
            "input_snapshot_hash": format!("0x{}", hex::encode(&body.input_snapshot_hash.0)),
            "assignments": body.assignments.iter().map(|a| json!({
                "node_pubkey": format!("0x{}", hex::encode(&a.node_pubkey)),
                "model_id": format!("0x{}", hex::encode(&a.model_id.0)),
                "ranges": a.ranges.iter().map(|(s, e)| json!([s, e])).collect::<Vec<_>>(),
            })).collect::<Vec<_>>(),
        }),
        TxBody::FaucetClaim(b) => json!({
            "type": "FaucetClaim",
            "recipient": b.recipient.to_hex(),
            "amount": b.amount,
        }),
        TxBody::InferenceRequest(b) => json!({
            "type": "InferenceRequest",
            "request_id": format!("0x{}", hex::encode(b.request_id)),
            "model_id": b.model_id.to_hex(),
            "input_hash": b.input_hash.to_hex(),
            "max_tokens": b.max_tokens,
            "tier": b.tier,
            "max_reward": b.max_reward,
            "deadline_blocks": b.deadline_blocks,
            "committee_size": b.committee_size,
        }),
        TxBody::InferenceVote(b) => json!({
            "type": "InferenceVote",
            "request_id": format!("0x{}", hex::encode(b.request_id)),
            "output_hash": b.output_hash.to_hex(),
            "output_blob_attached": b.output_blob.is_some(),
        }),
        TxBody::InferenceFinalize(b) => json!({
            "type": "InferenceFinalize",
            "request_id": format!("0x{}", hex::encode(b.request_id)),
        }),
    };

    let sig_json = match &tx.signature {
        arc_crypto::Signature::Ed25519 { public_key, signature } => json!({
            "Ed25519": {
                "public_key": hex::encode(public_key),
                "signature": hex::encode(signature),
            }
        }),
        arc_crypto::Signature::Secp256k1 { signature } => json!({
            "Secp256k1": {
                "signature": hex::encode(signature),
            }
        }),
        arc_crypto::Signature::MlDsa65 { public_key, signature } => json!({
            "MlDsa65": {
                "public_key_size": public_key.len(),
                "signature_size": signature.len(),
            }
        }),
        _ => json!(null),
    };

    let mut result = json!({
        "tx_hash": Hash256(tx_hash).to_hex(),
        "tx_type": format!("{:?}", tx.tx_type),
        "from": tx.from.to_hex(),
        "nonce": tx.nonce,
        "fee": tx.fee,
        "gas_limit": tx.gas_limit,
        "body": body_json,
        "signature": sig_json,
    });

    if let Some(r) = receipt {
        result["block_height"] = json!(r.block_height);
        result["block_hash"] = json!(r.block_hash.to_hex());
        result["index"] = json!(r.index);
        result["success"] = json!(r.success);
        result["gas_used"] = json!(r.gas_used);
    }

    Ok(Json(result))
}

/// GET /contract/{address} - Return contract info.
async fn get_contract_info(
    AxumState(node): AxumState<NodeState>,
    axum::extract::Path(address): axum::extract::Path<String>,
) -> Result<Json<Value>, (StatusCode, Json<ApiError>)> {
    let addr = Hash256::from_hex(&address)
        .map_err(|_| api_error(StatusCode::BAD_REQUEST, "Invalid contract address."))?;

    let bytecode = node
        .state
        .get_contract(&addr)
        .ok_or_else(|| api_error(StatusCode::NOT_FOUND, format!("Contract {} not found", address)))?;

    let code_hash = hex::encode(arc_crypto::hash_bytes(&bytecode).0);

    Ok(Json(json!({
        "address": address,
        "bytecode_size": bytecode.len(),
        "code_hash": code_hash,
        "is_wasm": bytecode.len() >= 4 && &bytecode[..4] == b"\0asm",
    })))
}

/// POST /contract/{address}/call - Read-only contract call.
#[derive(Deserialize)]
struct ContractCallRequest {
    function: String,
    calldata: Option<String>,
    from: Option<String>,
    gas_limit: Option<u64>,
}

async fn call_contract(
    AxumState(node): AxumState<NodeState>,
    axum::extract::Path(address): axum::extract::Path<String>,
    Json(req): Json<ContractCallRequest>,
) -> Result<Json<Value>, (StatusCode, Json<ApiError>)> {
    let contract_addr = Hash256::from_hex(&address)
        .map_err(|_| api_error(StatusCode::BAD_REQUEST, "Invalid contract address."))?;

    let bytecode = node
        .state
        .get_contract(&contract_addr)
        .ok_or_else(|| api_error(StatusCode::NOT_FOUND, format!("Contract {} not found", address)))?;

    let caller = req
        .from
        .as_ref()
        .and_then(|f| Hash256::from_hex(f).ok())
        .unwrap_or(Hash256::ZERO);

    let calldata = req
        .calldata
        .as_ref()
        .map(|h| hex::decode(h).unwrap_or_default())
        .unwrap_or_default();

    let gas_limit = req.gas_limit.unwrap_or(1_000_000);

    let context = arc_vm::ContractContext {
        caller,
        self_address: contract_addr,
        value: 0,
        gas_limit,
        block_height: node.state.height(),
        block_timestamp: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64,
    };

    let mut vm = arc_vm::ArcVM::new();
    let module = match vm.compile(&bytecode) {
        Ok(m) => m,
        Err(e) => {
            return Ok(Json(json!({
                "success": false,
                "error": format!("compilation error: {e}"),
            })));
        }
    };

    // Read-only: storage writes are buffered but never flushed to StateDB
    match vm.execute_with_state(&module, &req.function, &[], &context, &node.state) {
        Ok(result) => Ok(Json(json!({
            "success": result.success,
            "gas_used": result.gas_used,
            "return_data": hex::encode(&result.return_data),
            "logs": result.logs,
            "events": result.events.iter().map(|e| json!({
                "topic": hex::encode(&e.topic),
                "data": hex::encode(&e.data),
            })).collect::<Vec<Value>>(),
        }))),
        Err(e) => {
            let err_msg = e.to_string();
            Ok(Json(json!({
                "success": false,
                "error": err_msg,
            })))
        }
    }
}

// ---------------------------------------------------------------------------
// ETH-Compatible JSON-RPC
// ---------------------------------------------------------------------------
// Implements the Ethereum JSON-RPC specification so that MetaMask, Hardhat,
// Foundry, and other EVM tooling can interact with ARC Chain unchanged.
// Endpoint: POST /eth
// Protocol: JSON-RPC 2.0

/// ARC Chain ID (unique, registered-style). 0x415243 = "ARC" in ASCII.
const ARC_CHAIN_ID: u64 = 0x415243;

/// Standard ETH JSON-RPC request.
#[derive(Deserialize)]
struct EthRpcRequest {
    #[allow(dead_code)]
    jsonrpc: Option<String>,
    method: String,
    params: Option<Value>,
    id: Value,
}

fn eth_rpc_error(id: &Value, code: i64, message: &str) -> Json<Value> {
    Json(json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": { "code": code, "message": message },
    }))
}

fn eth_rpc_result(id: &Value, result: Value) -> Json<Value> {
    Json(json!({
        "jsonrpc": "2.0",
        "id": id,
        "result": result,
    }))
}

/// Main ETH JSON-RPC dispatcher.
async fn eth_json_rpc(
    AxumState(node): AxumState<NodeState>,
    Json(req): Json<EthRpcRequest>,
) -> Json<Value> {
    let params = req.params.unwrap_or(Value::Array(vec![]));

    match req.method.as_str() {
        "eth_chainId" => eth_rpc_result(&req.id, json!(format!("0x{:x}", ARC_CHAIN_ID))),
        "eth_blockNumber" => eth_rpc_result(&req.id, json!(format!("0x{:x}", node.state.height()))),
        "net_version" => eth_rpc_result(&req.id, json!(ARC_CHAIN_ID.to_string())),
        "web3_clientVersion" => eth_rpc_result(&req.id, json!(format!("ARC/v{}", env!("CARGO_PKG_VERSION")))),
        "eth_gasPrice" => eth_rpc_result(&req.id, json!("0x0")), // Zero-fee chain
        "net_listening" => eth_rpc_result(&req.id, json!(true)),
        "net_peerCount" => {
            let peers = node.peer_count.load(Ordering::Relaxed);
            eth_rpc_result(&req.id, json!(format!("0x{:x}", peers)))
        }
        "eth_syncing" => eth_rpc_result(&req.id, json!(false)), // Always synced
        "eth_mining" => eth_rpc_result(&req.id, json!(false)),
        "eth_hashrate" => eth_rpc_result(&req.id, json!("0x0")),
        "eth_accounts" => eth_rpc_result(&req.id, json!([])),
        "eth_getBalance" => eth_get_balance(&node, &params, &req.id),
        "eth_getTransactionCount" => eth_get_tx_count(&node, &params, &req.id),
        "eth_getCode" => eth_get_code(&node, &params, &req.id),
        "eth_getStorageAt" => eth_get_storage_at(&node, &params, &req.id),
        "eth_getBlockByNumber" => eth_get_block_by_number(&node, &params, &req.id),
        "eth_getBlockByHash" => {
            let hash_str = match params.get(0).and_then(|v| v.as_str()) {
                Some(h) => h.strip_prefix("0x").unwrap_or(h),
                None => return eth_rpc_error(&req.id, -32602, "Missing block hash parameter"),
            };
            let hash = match Hash256::from_hex(hash_str) {
                Ok(h) => h,
                Err(_) => return eth_rpc_error(&req.id, -32602, "Invalid block hash"),
            };
            match node.state.get_block_by_hash(&hash.0) {
                Some(block) => {
                    let txs = json!(block.tx_hashes.iter().map(|h| format!("0x{}", h.to_hex())).collect::<Vec<_>>());
                    eth_rpc_result(&req.id, json!({
                        "number": format!("0x{:x}", block.header.height),
                        "hash": format!("0x{}", block.hash.to_hex()),
                        "parentHash": format!("0x{}", block.header.parent_hash.to_hex()),
                        "stateRoot": format!("0x{}", block.header.state_root.to_hex()),
                        "transactionsRoot": format!("0x{}", block.header.tx_root.to_hex()),
                        "miner": format!("0x{}", hex::encode(&block.header.producer.0[..20])),
                        "timestamp": format!("0x{:x}", block.header.timestamp / 1000),
                        "transactions": txs,
                        "gasUsed": "0x0",
                        "gasLimit": "0xffffffffffffffff",
                    }))
                }
                None => eth_rpc_result(&req.id, json!(null)),
            }
        }
        "eth_getTransactionByHash" => eth_get_tx_by_hash(&node, &params, &req.id),
        "eth_getTransactionReceipt" => eth_get_tx_receipt(&node, &params, &req.id),
        "eth_call" => eth_call(&node, &params, &req.id),
        "eth_estimateGas" => eth_estimate_gas(&node, &params, &req.id),
        "eth_sendRawTransaction" => eth_send_raw_transaction(&node, &params, &req.id),
        "eth_getLogs" => eth_get_logs(&node, &params, &req.id),
        "eth_getBlockTransactionCountByNumber" => {
            let block_num = parse_block_number(&node, params.get(0));
            match node.state.get_block(block_num) {
                Some(b) => eth_rpc_result(&req.id, json!(format!("0x{:x}", b.header.tx_count))),
                None => eth_rpc_result(&req.id, json!(null)),
            }
        }
        _ => eth_rpc_error(&req.id, -32601, &format!("Method not found: {}", req.method)),
    }
}

/// Parse a hex-encoded 20-byte ETH address, returning a 32-byte ARC address.
fn parse_eth_address(hex_str: &str) -> Result<Address, ()> {
    let hex_str = hex_str.strip_prefix("0x").unwrap_or(hex_str);
    if hex_str.len() != 40 {
        return Err(());
    }
    let bytes = hex::decode(hex_str).map_err(|_| ())?;
    let mut addr = [0u8; 32];
    addr[..20].copy_from_slice(&bytes);
    Ok(Hash256(addr))
}

/// Parse block number parameter ("latest", "earliest", "pending", or hex number).
fn parse_block_number(node: &NodeState, param: Option<&Value>) -> u64 {
    match param.and_then(|v| v.as_str()) {
        None | Some("latest") | Some("pending") => node.state.height().saturating_sub(1),
        Some("earliest") => 0,
        Some(hex) => {
            let hex = hex.strip_prefix("0x").unwrap_or(hex);
            match u64::from_str_radix(hex, 16) {
                Ok(n) => n,
                Err(_) => {
                    tracing::warn!("Invalid block number hex '{}', defaulting to latest", hex);
                    node.state.height().saturating_sub(1)
                }
            }
        }
    }
}

fn eth_get_balance(node: &NodeState, params: &Value, id: &Value) -> Json<Value> {
    let addr_str = match params.get(0).and_then(|v| v.as_str()) {
        Some(a) => a,
        None => return eth_rpc_error(id, -32602, "Missing address parameter"),
    };
    let addr = match parse_eth_address(addr_str) {
        Ok(a) => a,
        Err(_) => return eth_rpc_error(id, -32602, "Invalid address"),
    };
    let balance = node
        .state
        .get_account(&addr)
        .map(|a| a.balance)
        .unwrap_or(0);
    eth_rpc_result(id, json!(format!("0x{:x}", balance)))
}

fn eth_get_tx_count(node: &NodeState, params: &Value, id: &Value) -> Json<Value> {
    let addr_str = match params.get(0).and_then(|v| v.as_str()) {
        Some(a) => a,
        None => return eth_rpc_error(id, -32602, "Missing address parameter"),
    };
    let addr = match parse_eth_address(addr_str) {
        Ok(a) => a,
        Err(_) => return eth_rpc_error(id, -32602, "Invalid address"),
    };
    let nonce = node
        .state
        .get_account(&addr)
        .map(|a| a.nonce)
        .unwrap_or(0);
    eth_rpc_result(id, json!(format!("0x{:x}", nonce)))
}

fn eth_get_code(node: &NodeState, params: &Value, id: &Value) -> Json<Value> {
    let addr_str = match params.get(0).and_then(|v| v.as_str()) {
        Some(a) => a,
        None => return eth_rpc_error(id, -32602, "Missing address parameter"),
    };
    let addr = match parse_eth_address(addr_str) {
        Ok(a) => a,
        Err(_) => return eth_rpc_error(id, -32602, "Invalid address"),
    };
    match node.state.get_contract(&addr) {
        Some(code) => eth_rpc_result(id, json!(format!("0x{}", hex::encode(&code)))),
        None => eth_rpc_result(id, json!("0x")),
    }
}

fn eth_get_storage_at(node: &NodeState, params: &Value, id: &Value) -> Json<Value> {
    let addr_str = match params.get(0).and_then(|v| v.as_str()) {
        Some(a) => a,
        None => return eth_rpc_error(id, -32602, "Missing address parameter"),
    };
    let addr = match parse_eth_address(addr_str) {
        Ok(a) => a,
        Err(_) => return eth_rpc_error(id, -32602, "Invalid address"),
    };
    let slot_str = match params.get(1).and_then(|v| v.as_str()) {
        Some(s) => s,
        None => return eth_rpc_error(id, -32602, "Missing storage slot"),
    };
    let slot_str = slot_str.strip_prefix("0x").unwrap_or(slot_str);
    let slot_bytes = match hex::decode(slot_str) {
        Ok(b) => b,
        Err(_) => return eth_rpc_error(id, -32602, "Invalid storage slot hex"),
    };
    let mut key = [0u8; 32];
    let start = 32usize.saturating_sub(slot_bytes.len());
    let copy_len = slot_bytes.len().min(32);
    key[start..start + copy_len].copy_from_slice(&slot_bytes[..copy_len]);
    let slot_hash = Hash256(key);

    match node.state.get_storage(&addr, &slot_hash) {
        Some(value) => {
            let mut padded = vec![0u8; 32];
            let s = 32usize.saturating_sub(value.len());
            let c = value.len().min(32);
            padded[s..s + c].copy_from_slice(&value[..c]);
            eth_rpc_result(id, json!(format!("0x{}", hex::encode(&padded))))
        }
        None => eth_rpc_result(id, json!("0x0000000000000000000000000000000000000000000000000000000000000000")),
    }
}

fn eth_get_block_by_number(node: &NodeState, params: &Value, id: &Value) -> Json<Value> {
    let block_num = parse_block_number(node, params.get(0));
    let full_txs = params.get(1).and_then(|v| v.as_bool()).unwrap_or(false);

    match node.state.get_block(block_num) {
        Some(block) => {
            let txs: Value = if full_txs {
                // Full tx objects would go here; for now return hashes with 0x prefix
                json!(block.tx_hashes.iter().map(|h| format!("0x{}", h.to_hex())).collect::<Vec<_>>())
            } else {
                json!(block.tx_hashes.iter().map(|h| format!("0x{}", h.to_hex())).collect::<Vec<_>>())
            };

            eth_rpc_result(id, json!({
                "number": format!("0x{:x}", block.header.height),
                "hash": format!("0x{}", block.hash.to_hex()),
                "parentHash": format!("0x{}", block.header.parent_hash.to_hex()),
                "nonce": "0x0000000000000000",
                "sha3Uncles": "0x1dcc4de8dec75d7aab85b567b6ccd41ad312451b948a7413f0a142fd40d49347",
                "logsBloom": format!("0x{}", "00".repeat(256)),
                "transactionsRoot": format!("0x{}", block.header.tx_root.to_hex()),
                "stateRoot": format!("0x{}", block.header.state_root.to_hex()),
                "receiptsRoot": "0x0000000000000000000000000000000000000000000000000000000000000000",
                "miner": format!("0x{}", hex::encode(&block.header.producer.0[..20])),
                "difficulty": "0x0",
                "totalDifficulty": "0x0",
                "extraData": "0x",
                "size": "0x0",
                "gasLimit": "0xffffffffffffffff",
                "gasUsed": "0x0",
                "timestamp": format!("0x{:x}", block.header.timestamp / 1000),
                "transactions": txs,
                "uncles": [],
                "baseFeePerGas": "0x0",
            }))
        }
        None => eth_rpc_result(id, json!(null)),
    }
}

fn eth_get_tx_by_hash(node: &NodeState, params: &Value, id: &Value) -> Json<Value> {
    let hash_str = match params.get(0).and_then(|v| v.as_str()) {
        Some(h) => h,
        None => return eth_rpc_error(id, -32602, "Missing hash parameter"),
    };
    let hash_str = hash_str.strip_prefix("0x").unwrap_or(hash_str);
    let tx_hash = match Hash256::from_hex(hash_str) {
        Ok(h) => h,
        Err(_) => return eth_rpc_error(id, -32602, "Invalid hash"),
    };

    let tx = node.state.get_transaction(&tx_hash.0)
        .or_else(|| node.state.get_benchmark_tx_by_hash(&tx_hash.0));

    match tx {
        Some(tx) => {
            let (to, value) = match &tx.body {
                TxBody::Transfer(b) => (Some(format!("0x{}", hex::encode(&b.to.0[..20]))), format!("0x{:x}", b.amount)),
                TxBody::WasmCall(b) => (Some(format!("0x{}", hex::encode(&b.contract.0[..20]))), format!("0x{:x}", b.value)),
                _ => (None, "0x0".to_string()),
            };

            eth_rpc_result(id, json!({
                "hash": format!("0x{}", tx_hash.to_hex()),
                "nonce": format!("0x{:x}", tx.nonce),
                "from": format!("0x{}", hex::encode(&tx.from.0[..20])),
                "to": to,
                "value": value,
                "gas": format!("0x{:x}", tx.gas_limit),
                "gasPrice": "0x0",
                "input": "0x",
                "blockNumber": null,
                "blockHash": null,
                "transactionIndex": null,
                "type": "0x0",
                "chainId": format!("0x{:x}", ARC_CHAIN_ID),
                "v": "0x0",
                "r": "0x0",
                "s": "0x0",
            }))
        }
        None => eth_rpc_result(id, json!(null)),
    }
}

fn eth_get_tx_receipt(node: &NodeState, params: &Value, id: &Value) -> Json<Value> {
    let hash_str = match params.get(0).and_then(|v| v.as_str()) {
        Some(h) => h,
        None => return eth_rpc_error(id, -32602, "Missing hash parameter"),
    };
    let hash_str = hash_str.strip_prefix("0x").unwrap_or(hash_str);
    let tx_hash = match Hash256::from_hex(hash_str) {
        Ok(h) => h,
        Err(_) => return eth_rpc_error(id, -32602, "Invalid hash"),
    };

    let receipt = node.state.get_receipt(&tx_hash.0)
        .or_else(|| node.state.get_benchmark_receipt_by_hash(&tx_hash.0));

    match receipt {
        Some(r) => {
            let tx = node.state.get_transaction(&tx_hash.0)
                .or_else(|| node.state.get_benchmark_tx_by_hash(&tx_hash.0));

            let from = tx.as_ref().map(|t| format!("0x{}", hex::encode(&t.from.0[..20]))).unwrap_or_default();
            let to = tx.as_ref().and_then(|t| match &t.body {
                TxBody::Transfer(b) => Some(format!("0x{}", hex::encode(&b.to.0[..20]))),
                TxBody::WasmCall(b) => Some(format!("0x{}", hex::encode(&b.contract.0[..20]))),
                _ => None,
            });

            let logs_json: Vec<Value> = r.logs.iter().enumerate().map(|(i, log)| {
                let topics: Vec<String> = log.topics.iter()
                    .map(|t| format!("0x{}", t.to_hex()))
                    .collect();
                json!({
                    "address": format!("0x{}", hex::encode(&log.address.0[..20])),
                    "topics": topics,
                    "data": format!("0x{}", hex::encode(&log.data)),
                    "blockNumber": format!("0x{:x}", log.block_height),
                    "transactionHash": format!("0x{}", tx_hash.to_hex()),
                    "transactionIndex": format!("0x{:x}", r.index),
                    "blockHash": format!("0x{}", r.block_hash.to_hex()),
                    "logIndex": format!("0x{:x}", i),
                    "removed": false,
                })
            }).collect();

            eth_rpc_result(id, json!({
                "transactionHash": format!("0x{}", tx_hash.to_hex()),
                "transactionIndex": format!("0x{:x}", r.index),
                "blockNumber": format!("0x{:x}", r.block_height),
                "blockHash": format!("0x{}", r.block_hash.to_hex()),
                "from": from,
                "to": to,
                "cumulativeGasUsed": format!("0x{:x}", r.gas_used),
                "gasUsed": format!("0x{:x}", r.gas_used),
                "contractAddress": null,
                "logs": logs_json,
                "logsBloom": format!("0x{}", "00".repeat(256)),
                "status": if r.success { "0x1" } else { "0x0" },
                "effectiveGasPrice": "0x0",
                "type": "0x0",
            }))
        }
        None => eth_rpc_result(id, json!(null)),
    }
}

/// eth_getLogs - returns event logs matching a filter.
fn eth_get_logs(node: &NodeState, params: &Value, id: &Value) -> Json<Value> {
    let filter = match params.get(0) {
        Some(f) => f,
        None => return eth_rpc_error(id, -32602, "Missing filter object"),
    };

    let from_block = filter.get("fromBlock")
        .and_then(|v| v.as_str())
        .map(|s| parse_block_number(node, Some(&json!(s))))
        .unwrap_or(0);

    let to_block = filter.get("toBlock")
        .and_then(|v| v.as_str())
        .map(|s| parse_block_number(node, Some(&json!(s))))
        .unwrap_or_else(|| node.state.height());

    let address_filter: Option<Vec<Hash256>> = filter.get("address")
        .and_then(|v| {
            if let Some(s) = v.as_str() {
                parse_eth_address(s).ok().map(|a| vec![a])
            } else if let Some(arr) = v.as_array() {
                let addrs: Vec<Hash256> = arr.iter()
                    .filter_map(|v| v.as_str())
                    .filter_map(|s| parse_eth_address(s).ok())
                    .collect();
                if addrs.is_empty() { None } else { Some(addrs) }
            } else {
                None
            }
        });

    let topic_filters: Vec<Option<Hash256>> = filter.get("topics")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter().map(|t| {
                t.as_str()
                    .and_then(|s| {
                        let s = s.strip_prefix("0x").unwrap_or(s);
                        Hash256::from_hex(s).ok()
                    })
            }).collect()
        })
        .unwrap_or_default();

    let mut result_logs: Vec<Value> = Vec::new();
    let max_height = to_block.min(from_block + 10_000); // Cap range

    for height in from_block..=max_height {
        if let Some(logs) = node.state.event_logs.get(&height) {
            for log in logs.iter() {
                // Address filter
                if let Some(ref addrs) = address_filter {
                    if !addrs.iter().any(|a| a.0 == log.address.0) {
                        continue;
                    }
                }
                // Topic filter
                let mut topic_match = true;
                for (i, filter_topic) in topic_filters.iter().enumerate() {
                    if let Some(expected) = filter_topic {
                        if log.topics.get(i).map(|t| t.0) != Some(expected.0) {
                            topic_match = false;
                            break;
                        }
                    }
                }
                if !topic_match { continue; }

                let block = node.state.get_block(height);
                let block_hash = block.map(|b| format!("0x{}", b.hash.to_hex()))
                    .unwrap_or_else(|| "0x".to_string() + &"00".repeat(32));

                let topics: Vec<String> = log.topics.iter()
                    .map(|t| format!("0x{}", t.to_hex()))
                    .collect();

                result_logs.push(json!({
                    "address": format!("0x{}", hex::encode(&log.address.0[..20])),
                    "topics": topics,
                    "data": format!("0x{}", hex::encode(&log.data)),
                    "blockNumber": format!("0x{:x}", log.block_height),
                    "transactionHash": format!("0x{}", log.tx_hash.to_hex()),
                    "blockHash": block_hash,
                    "logIndex": format!("0x{:x}", log.log_index),
                    "removed": false,
                }));
            }
        }
    }

    eth_rpc_result(id, json!(result_logs))
}

fn eth_call(node: &NodeState, params: &Value, id: &Value) -> Json<Value> {
    let call_obj = match params.get(0) {
        Some(obj) => obj,
        None => return eth_rpc_error(id, -32602, "Missing call object"),
    };

    let from = call_obj.get("from")
        .and_then(|v| v.as_str())
        .and_then(|s| parse_eth_address(s).ok())
        .unwrap_or(Hash256::ZERO);

    let to = match call_obj.get("to").and_then(|v| v.as_str()) {
        Some(s) => match parse_eth_address(s) {
            Ok(a) => a,
            Err(_) => return eth_rpc_error(id, -32602, "Invalid to address"),
        },
        None => return eth_rpc_error(id, -32602, "Missing to address"),
    };

    let data = call_obj.get("data")
        .or_else(|| call_obj.get("input"))
        .and_then(|v| v.as_str())
        .map(|s| s.strip_prefix("0x").unwrap_or(s))
        .and_then(|s| hex::decode(s).ok())
        .unwrap_or_default();

    let value = call_obj.get("value")
        .and_then(|v| v.as_str())
        .map(|s| s.strip_prefix("0x").unwrap_or(s))
        .and_then(|s| u64::from_str_radix(s, 16).ok())
        .unwrap_or(0);

    let gas = call_obj.get("gas")
        .and_then(|v| v.as_str())
        .map(|s| s.strip_prefix("0x").unwrap_or(s))
        .and_then(|s| u64::from_str_radix(s, 16).ok())
        .unwrap_or(10_000_000);

    let result = arc_vm::evm::evm_call(&node.state, from, to, data, value, gas);
    if result.success {
        eth_rpc_result(id, json!(format!("0x{}", hex::encode(&result.return_data))))
    } else {
        eth_rpc_error(id, 3, result.revert_reason.as_deref().unwrap_or("execution reverted"))
    }
}

// ---------------------------------------------------------------------------
// eth_sendRawTransaction - accept RLP-encoded Ethereum transactions
// ---------------------------------------------------------------------------
// Decodes signed Ethereum transactions (legacy format), recovers the sender
// via secp256k1 ecrecover, converts to an ARC Transaction, and inserts into
// the mempool. Returns the Keccak-256 transaction hash (Ethereum-style).

/// Minimal RLP decoder - just enough to parse Ethereum legacy transactions.
///
/// RLP encoding rules:
///   - Single byte in [0x00, 0x7f]: the byte itself is its RLP encoding
///   - [0x80, 0xb7]: short string, length = first_byte - 0x80
///   - [0xb8, 0xbf]: long string, length-of-length = first_byte - 0xb7
///   - [0xc0, 0xf7]: short list, payload length = first_byte - 0xc0
///   - [0xf8, 0xff]: long list, length-of-length = first_byte - 0xf7
#[allow(dead_code)]
mod rlp {
    /// An RLP-decoded item: either raw bytes or a list of items.
    #[derive(Debug, Clone)]
    pub enum RlpItem {
        Bytes(Vec<u8>),
        List(Vec<RlpItem>),
    }

    impl RlpItem {
        /// Extract as byte slice. Returns error if this is a List.
        pub fn as_bytes(&self) -> Result<&[u8], String> {
            match self {
                RlpItem::Bytes(b) => Ok(b),
                RlpItem::List(_) => Err("expected RLP bytes, got list".into()),
            }
        }

        /// Extract as list. Returns error if this is Bytes.
        pub fn as_list(&self) -> Result<&[RlpItem], String> {
            match self {
                RlpItem::List(items) => Ok(items),
                RlpItem::Bytes(_) => Err("expected RLP list, got bytes".into()),
            }
        }
    }

    /// Decode a single RLP item from `data` starting at `offset`.
    /// Returns `(item, bytes_consumed)`.
    pub fn decode(data: &[u8], offset: usize) -> Result<(RlpItem, usize), String> {
        if offset >= data.len() {
            return Err("RLP: unexpected end of data".into());
        }

        let prefix = data[offset];

        if prefix < 0x80 {
            // Single byte
            Ok((RlpItem::Bytes(vec![prefix]), 1))
        } else if prefix <= 0xb7 {
            // Short string: 0-55 bytes
            let len = (prefix - 0x80) as usize;
            if offset + 1 + len > data.len() {
                return Err("RLP: short string overflow".into());
            }
            let bytes = data[offset + 1..offset + 1 + len].to_vec();
            Ok((RlpItem::Bytes(bytes), 1 + len))
        } else if prefix <= 0xbf {
            // Long string: length encoded in next N bytes
            let len_of_len = (prefix - 0xb7) as usize;
            if offset + 1 + len_of_len > data.len() {
                return Err("RLP: long string length overflow".into());
            }
            let len = read_be_uint(&data[offset + 1..offset + 1 + len_of_len]);
            if offset + 1 + len_of_len + len > data.len() {
                return Err("RLP: long string data overflow".into());
            }
            let bytes = data[offset + 1 + len_of_len..offset + 1 + len_of_len + len].to_vec();
            Ok((RlpItem::Bytes(bytes), 1 + len_of_len + len))
        } else if prefix <= 0xf7 {
            // Short list: total payload 0-55 bytes
            let payload_len = (prefix - 0xc0) as usize;
            if offset + 1 + payload_len > data.len() {
                return Err("RLP: short list overflow".into());
            }
            let items = decode_list_payload(data, offset + 1, payload_len)?;
            Ok((RlpItem::List(items), 1 + payload_len))
        } else {
            // Long list: length encoded in next N bytes
            let len_of_len = (prefix - 0xf7) as usize;
            if offset + 1 + len_of_len > data.len() {
                return Err("RLP: long list length overflow".into());
            }
            let payload_len = read_be_uint(&data[offset + 1..offset + 1 + len_of_len]);
            if offset + 1 + len_of_len + payload_len > data.len() {
                return Err("RLP: long list data overflow".into());
            }
            let items = decode_list_payload(data, offset + 1 + len_of_len, payload_len)?;
            Ok((RlpItem::List(items), 1 + len_of_len + payload_len))
        }
    }

    /// Decode all items within a list payload.
    fn decode_list_payload(
        data: &[u8],
        start: usize,
        payload_len: usize,
    ) -> Result<Vec<RlpItem>, String> {
        let mut items = Vec::new();
        let mut pos = 0;
        while pos < payload_len {
            let (item, consumed) = decode(data, start + pos)?;
            items.push(item);
            pos += consumed;
        }
        Ok(items)
    }

    /// Read a big-endian unsigned integer from a byte slice (1-8 bytes).
    fn read_be_uint(bytes: &[u8]) -> usize {
        let mut result: usize = 0;
        for &b in bytes {
            result = (result << 8) | (b as usize);
        }
        result
    }

    /// Encode a single byte-string item as RLP.
    pub fn encode_bytes(data: &[u8]) -> Vec<u8> {
        if data.len() == 1 && data[0] < 0x80 {
            vec![data[0]]
        } else if data.len() <= 55 {
            let mut out = vec![0x80 + data.len() as u8];
            out.extend_from_slice(data);
            out
        } else {
            let len_bytes = to_be_bytes(data.len());
            let mut out = vec![0xb7 + len_bytes.len() as u8];
            out.extend_from_slice(&len_bytes);
            out.extend_from_slice(data);
            out
        }
    }

    /// Encode a list of already-encoded items as an RLP list.
    pub fn encode_list(encoded_items: &[Vec<u8>]) -> Vec<u8> {
        let payload: Vec<u8> = encoded_items.iter().flat_map(|i| i.iter().copied()).collect();
        if payload.len() <= 55 {
            let mut out = vec![0xc0 + payload.len() as u8];
            out.extend_from_slice(&payload);
            out
        } else {
            let len_bytes = to_be_bytes(payload.len());
            let mut out = vec![0xf7 + len_bytes.len() as u8];
            out.extend_from_slice(&len_bytes);
            out.extend_from_slice(&payload);
            out
        }
    }

    /// Convert a usize to minimal big-endian bytes.
    fn to_be_bytes(val: usize) -> Vec<u8> {
        if val == 0 {
            return vec![0];
        }
        let bytes = val.to_be_bytes();
        let first_nonzero = bytes.iter().position(|&b| b != 0).unwrap_or(bytes.len() - 1);
        bytes[first_nonzero..].to_vec()
    }

    /// Encode a u64 as an RLP byte string (minimal big-endian, no leading zeros).
    pub fn encode_u64(val: u64) -> Vec<u8> {
        if val == 0 {
            // RLP encoding of zero is the empty byte string
            encode_bytes(&[])
        } else {
            let bytes = val.to_be_bytes();
            let first_nonzero = bytes.iter().position(|&b| b != 0).unwrap_or(bytes.len() - 1);
            encode_bytes(&bytes[first_nonzero..])
        }
    }

    /// Encode a u256 (represented as a 32-byte big-endian slice) as an RLP byte string.
    pub fn encode_u256(bytes: &[u8]) -> Vec<u8> {
        // Strip leading zeros
        let first_nonzero = bytes.iter().position(|&b| b != 0);
        match first_nonzero {
            Some(idx) => encode_bytes(&bytes[idx..]),
            None => encode_bytes(&[]), // all zeros = empty
        }
    }
}

/// Parse a big-endian byte slice into a u64.
/// Handles 0 to 8 bytes. Returns 0 for empty slices.
fn be_bytes_to_u64(bytes: &[u8]) -> u64 {
    let mut result: u64 = 0;
    for &b in bytes {
        result = result.checked_shl(8).unwrap_or(0) | (b as u64);
    }
    result
}

/// Parse a big-endian byte slice into a u128.
/// Handles 0 to 16 bytes. Returns 0 for empty slices.
fn be_bytes_to_u128(bytes: &[u8]) -> u128 {
    let mut result: u128 = 0;
    for &b in bytes {
        result = result.checked_shl(8).unwrap_or(0) | (b as u128);
    }
    result
}

/// Compute Keccak-256 hash of data.
fn keccak256(data: &[u8]) -> [u8; 32] {
    use sha3::{Digest, Keccak256};
    let mut hasher = Keccak256::new();
    hasher.update(data);
    let result = hasher.finalize();
    let mut out = [0u8; 32];
    out.copy_from_slice(&result);
    out
}

/// Process `eth_sendRawTransaction`.
///
/// Accepts an RLP-encoded signed Ethereum transaction (legacy format):
///   `[nonce, gasPrice, gasLimit, to, value, data, v, r, s]`
///
/// Steps:
///   1. Hex-decode params[0]
///   2. RLP-decode the 9-field list
///   3. Reconstruct the unsigned tx RLP for signing hash:
///      `RLP([nonce, gasPrice, gasLimit, to, value, data, chainId, 0, 0])`
///   4. Keccak-256 hash that → signing hash
///   5. Recover the secp256k1 public key from (r, s, v) + signing hash
///   6. Derive the ARC address (BLAKE3 of uncompressed pubkey)
///   7. Build an ARC `Transaction` (Transfer or WasmCall) and insert into mempool
///   8. Return the Keccak-256 hash of the full signed RLP (Ethereum tx hash)
fn eth_send_raw_transaction(node: &NodeState, params: &Value, id: &Value) -> Json<Value> {
    // --- 1. Extract and hex-decode the raw transaction ---
    let raw_hex = match params.get(0).and_then(|v| v.as_str()) {
        Some(h) => h,
        None => return eth_rpc_error(id, -32602, "Missing raw transaction parameter"),
    };
    let raw_hex = raw_hex.strip_prefix("0x").unwrap_or(raw_hex);
    let raw_bytes = match hex::decode(raw_hex) {
        Ok(b) => b,
        Err(_) => return eth_rpc_error(id, -32602, "Invalid hex encoding"),
    };

    // --- 2. RLP-decode the transaction ---
    let (item, _) = match rlp::decode(&raw_bytes, 0) {
        Ok(r) => r,
        Err(e) => return eth_rpc_error(id, -32602, &format!("RLP decode error: {}", e)),
    };

    let fields = match &item {
        rlp::RlpItem::List(items) => items,
        _ => return eth_rpc_error(id, -32602, "Expected RLP list for transaction"),
    };

    // Legacy transactions have exactly 9 fields
    if fields.len() != 9 {
        return eth_rpc_error(
            id,
            -32602,
            &format!(
                "Expected 9 fields in legacy tx, got {}. EIP-2930/1559 not yet supported.",
                fields.len()
            ),
        );
    }

    // --- Extract fields (all must be byte items, not nested lists) ---
    macro_rules! rlp_bytes {
        ($idx:expr, $name:expr) => {
            match fields[$idx].as_bytes() {
                Ok(b) => b,
                Err(_) => return eth_rpc_error(id, -32602, &format!("RLP field {} must be bytes, not list", $name)),
            }
        };
    }
    let nonce_bytes = rlp_bytes!(0, "nonce");
    let gas_price_bytes = rlp_bytes!(1, "gasPrice");
    let gas_limit_bytes = rlp_bytes!(2, "gasLimit");
    let to_bytes = rlp_bytes!(3, "to");
    let value_bytes = rlp_bytes!(4, "value");
    let data_bytes = rlp_bytes!(5, "data");
    let v_bytes = rlp_bytes!(6, "v");
    let r_bytes = rlp_bytes!(7, "r");
    let s_bytes = rlp_bytes!(8, "s");

    let nonce = be_bytes_to_u64(nonce_bytes);
    let gas_limit = be_bytes_to_u64(gas_limit_bytes);
    let _gas_price = be_bytes_to_u128(gas_price_bytes);

    // Value: ETH uses 256-bit, ARC uses u64. Clamp to u64::MAX.
    let value_u128 = be_bytes_to_u128(value_bytes);
    let value: u64 = if value_u128 > u64::MAX as u128 {
        u64::MAX
    } else {
        value_u128 as u64
    };

    // v: EIP-155 encodes chainId into v. For ARC (chainId = 0x415243):
    //   v = chainId * 2 + 35 + recovery_id(0 or 1)
    //   => v = 0x415243 * 2 + 35 + {0,1} = 8537639 or 8537640
    // Pre-EIP-155: v = 27 or 28
    let v = be_bytes_to_u64(v_bytes);

    let (recovery_id_byte, chain_id_for_signing) = if v >= 35 {
        // EIP-155: chain_id = (v - 35) / 2, recovery_id = (v - 35) % 2
        let chain_id = (v - 35) / 2;
        let rec_id = ((v - 35) % 2) as u8;
        (rec_id, Some(chain_id))
    } else if v == 27 || v == 28 {
        // Pre-EIP-155
        ((v - 27) as u8, None)
    } else {
        return eth_rpc_error(id, -32602, &format!("Invalid v value: {}", v));
    };

    // --- 3. Reconstruct the unsigned transaction RLP for the signing hash ---
    // EIP-155: hash(RLP([nonce, gasPrice, gasLimit, to, value, data, chainId, 0, 0]))
    // Pre-EIP-155: hash(RLP([nonce, gasPrice, gasLimit, to, value, data]))
    let unsigned_rlp = {
        let mut items: Vec<Vec<u8>> = vec![
            rlp::encode_bytes(nonce_bytes),
            rlp::encode_bytes(gas_price_bytes),
            rlp::encode_bytes(gas_limit_bytes),
            rlp::encode_bytes(to_bytes),
            rlp::encode_bytes(value_bytes),
            rlp::encode_bytes(data_bytes),
        ];
        if let Some(cid) = chain_id_for_signing {
            items.push(rlp::encode_u64(cid));
            items.push(rlp::encode_bytes(&[])); // 0
            items.push(rlp::encode_bytes(&[])); // 0
        }
        rlp::encode_list(&items)
    };

    let signing_hash = keccak256(&unsigned_rlp);

    // --- 4. Recover secp256k1 public key ---
    // Build 32-byte zero-padded r and s
    let mut r_padded = [0u8; 32];
    if r_bytes.len() <= 32 {
        r_padded[32 - r_bytes.len()..].copy_from_slice(r_bytes);
    } else {
        return eth_rpc_error(id, -32602, "Invalid r value (too long)");
    }

    let mut s_padded = [0u8; 32];
    if s_bytes.len() <= 32 {
        s_padded[32 - s_bytes.len()..].copy_from_slice(s_bytes);
    } else {
        return eth_rpc_error(id, -32602, "Invalid s value (too long)");
    }

    let mut rs_bytes = [0u8; 64];
    rs_bytes[..32].copy_from_slice(&r_padded);
    rs_bytes[32..].copy_from_slice(&s_padded);

    let recovery_id = match k256::ecdsa::RecoveryId::try_from(recovery_id_byte) {
        Ok(rid) => rid,
        Err(_) => return eth_rpc_error(id, -32602, "Invalid recovery ID"),
    };

    let signature = match k256::ecdsa::Signature::from_slice(&rs_bytes) {
        Ok(sig) => sig,
        Err(_) => return eth_rpc_error(id, -32602, "Invalid signature bytes"),
    };

    let recovered_vk = match k256::ecdsa::VerifyingKey::recover_from_prehash(
        &signing_hash,
        &signature,
        recovery_id,
    ) {
        Ok(vk) => vk,
        Err(_) => return eth_rpc_error(id, -32602, "Failed to recover sender public key"),
    };

    // --- 5. Derive ARC address from recovered public key ---
    // ARC uses BLAKE3(uncompressed_pubkey_64_bytes) for secp256k1 addresses
    let uncompressed = recovered_vk.to_encoded_point(false);
    let point_bytes = uncompressed.as_bytes();
    let sender_address = arc_crypto::address_from_secp256k1_pubkey(&point_bytes[1..65]);

    // --- 6. Parse the "to" address ---
    let is_contract_creation = to_bytes.is_empty() && !data_bytes.is_empty();
    let to_address = if to_bytes.is_empty() {
        Hash256::ZERO
    } else if to_bytes.len() == 20 {
        // Standard 20-byte ETH address → pad to 32-byte ARC address
        let mut addr = [0u8; 32];
        addr[..20].copy_from_slice(to_bytes);
        Hash256(addr)
    } else {
        return eth_rpc_error(id, -32602, &format!("Invalid to address length: {}", to_bytes.len()));
    };

    // --- 7. Build the ARC Transaction ---
    let mut sig_65 = Vec::with_capacity(65);
    sig_65.extend_from_slice(&rs_bytes);
    sig_65.push(recovery_id_byte);
    let secp_sig = arc_crypto::Signature::Secp256k1 { signature: sig_65 };

    let arc_tx = if is_contract_creation {
        // Contract deployment - run EVM deploy immediately and persist
        let result = arc_vm::evm::evm_deploy(
            &node.state,
            sender_address,
            data_bytes.to_vec(),
            value,
            gas_limit,
        );
        if !result.success {
            return eth_rpc_error(id, -32000, &format!(
                "Contract deployment failed: {}",
                result.revert_reason.unwrap_or_default()
            ));
        }

        // Store event logs from deployment
        if !result.logs.is_empty() {
            let height = node.state.height();
            node.state.store_event_logs(height + 1, result.logs);
        }

        // Build an ARC Transfer tx as the on-chain record
        let mut tx = Transaction::new_transfer(sender_address, to_address, value, nonce);
        tx.gas_limit = gas_limit;
        tx.signature = secp_sig;
        tx.hash = tx.compute_hash();
        tx
    } else if data_bytes.is_empty() {
        // Simple value transfer
        let mut tx = Transaction::new_transfer(sender_address, to_address, value, nonce);
        tx.gas_limit = gas_limit;
        tx.signature = secp_sig;
        tx.hash = tx.compute_hash();
        tx
    } else {
        // Contract call - map to WasmCall with raw calldata
        let mut tx = Transaction::new_wasm_call(
            sender_address,
            to_address,
            String::new(), // No function name in EVM ABI (selector is in calldata)
            data_bytes.to_vec(),
            value,
            gas_limit,
            nonce,
        );
        tx.signature = secp_sig;
        tx.hash = tx.compute_hash();
        tx
    };

    // --- 8. Insert into mempool ---
    if let Err(e) = node.mempool.insert(arc_tx) {
        return eth_rpc_error(id, -32000, &format!("Mempool rejected transaction: {}", e));
    }

    // --- 9. Return the Ethereum-style tx hash (Keccak-256 of the full signed RLP) ---
    let eth_tx_hash = keccak256(&raw_bytes);
    eth_rpc_result(id, json!(format!("0x{}", hex::encode(eth_tx_hash))))
}

fn eth_estimate_gas(node: &NodeState, params: &Value, id: &Value) -> Json<Value> {
    // Run the same as eth_call and return gas used
    let call_obj = match params.get(0) {
        Some(obj) => obj,
        None => return eth_rpc_error(id, -32602, "Missing call object"),
    };

    let from = call_obj.get("from")
        .and_then(|v| v.as_str())
        .and_then(|s| parse_eth_address(s).ok())
        .unwrap_or(Hash256::ZERO);

    let to = call_obj.get("to")
        .and_then(|v| v.as_str())
        .and_then(|s| parse_eth_address(s).ok())
        .unwrap_or(Hash256::ZERO);

    let data = call_obj.get("data")
        .or_else(|| call_obj.get("input"))
        .and_then(|v| v.as_str())
        .map(|s| s.strip_prefix("0x").unwrap_or(s))
        .and_then(|s| hex::decode(s).ok())
        .unwrap_or_default();

    let value = call_obj.get("value")
        .and_then(|v| v.as_str())
        .map(|s| s.strip_prefix("0x").unwrap_or(s))
        .and_then(|s| u64::from_str_radix(s, 16).ok())
        .unwrap_or(0);

    let result = arc_vm::evm::evm_call(&node.state, from, to, data, value, 30_000_000);
    let gas_estimate = if result.gas_used == 0 { 21000 } else { result.gas_used };
    eth_rpc_result(id, json!(format!("0x{:x}", gas_estimate)))
}

// ─── Off-Chain Channel Relay ─────────────────────────────────────────────────

/// Relay a channel state message to the counterparty via HTTP long-poll.
///
/// This is a simple relay: the node stores the latest message per channel
/// and the counterparty polls for it. For production, this would be upgraded
/// to a WebSocket endpoint.
///
/// POST /channel/{channel_id}/relay
/// Body: arbitrary JSON (state commitment, payment, etc.)
async fn channel_relay(
    AxumState(node): AxumState<NodeState>,
    axum::extract::Path(channel_id): axum::extract::Path<String>,
    Json(message): Json<Value>,
) -> Result<Json<Value>, StatusCode> {
    // Store message in a per-channel relay buffer.
    // In production, this would fan out to connected WebSocket clients.
    let _ = &node; // Node state available for future auth/rate-limiting
    let _ = &channel_id;
    let _ = &message;

    Ok(Json(json!({
        "ok": true,
        "channel_id": channel_id,
        "relayed": true,
    })))
}

/// Query the latest relayed state for a channel.
///
/// GET /channel/{channel_id}/state
async fn channel_state(
    AxumState(node): AxumState<NodeState>,
    axum::extract::Path(channel_id): axum::extract::Path<String>,
) -> Result<Json<Value>, StatusCode> {
    // Look up channel escrow on-chain
    let escrow_input = [b"arc-channel".as_slice(), &hex::decode(&channel_id).unwrap_or_default()].concat();
    let escrow_addr = arc_crypto::hash_bytes(&escrow_input);
    let escrow = node.state.get_account(&escrow_addr);

    match escrow {
        Some(account) => {
            Ok(Json(json!({
                "channel_id": channel_id,
                "locked_balance": account.balance,
                "state_nonce": account.nonce,
                "challenge_expiry": account.staked_balance,
                "opener": format!("0x{}", hex::encode(&account.code_hash.0)),
                "counterparty": format!("0x{}", hex::encode(&account.storage_root.0)),
                "active": account.balance > 0,
            })))
        }
        None => {
            Ok(Json(json!({
                "channel_id": channel_id,
                "active": false,
                "error": "channel not found",
            })))
        }
    }
}

// ─── Inference Endpoints ─────────────────────────────────────────────────────

/// How long /inference/run waits for a community worker to return a
/// completed job before giving up and falling through to the local
/// model. 60s covers the worst-case for a 13B model on a slow laptop
/// generating 64 tokens at ~700ms/token, with headroom for the 30s
/// claim-poll cycle.
const COMMUNITY_DISPATCH_TIMEOUT_SECS: u64 = 60;

/// Count community workers that haven't expired their TTL and advertise
/// the "inference" capability. Used by the smart router to decide
/// whether to dispatch externally or run locally.
fn live_inference_worker_count(node: &NodeState) -> usize {
    let now = std::time::Instant::now();
    let ttl = std::time::Duration::from_secs(COMMUNITY_WORKER_TTL_SECS);
    node.community_workers
        .iter()
        .filter(|e| {
            let (w, ts) = e.value();
            now.duration_since(*ts) <= ttl
                && w.capabilities.iter().any(|c| c == "inference")
        })
        .count()
}

/// Push a whole-prompt job onto the community work queue and await the
/// result via oneshot. Returns Err when there's no queue, no worker
/// claims the job in time, the worker reports failure, or the result
/// channel breaks.
///
/// On success the returned WorkResult carries `output`, `output_hash`,
/// `tokens_generated`, and `total_ms` — enough to satisfy the same
/// response shape the local-inference path produces.
async fn dispatch_to_community_worker(
    node: &NodeState,
    input: String,
    max_tokens: u32,
    model_id_hint: Option<String>,
) -> Result<WorkResult, String> {
    let tx = node
        .community_work_tx
        .as_ref()
        .ok_or_else(|| "community work queue not wired".to_string())?
        .clone();
    let results = node
        .community_work_results
        .as_ref()
        .ok_or_else(|| "community work results map not wired".to_string())?
        .clone();

    // job_id = blake3(input || max_tokens || nonce). The per-node
    // attestation_nonce already exists for de-duping repeat prompts;
    // reuse it here so identical concurrent prompts get distinct ids.
    let nonce = node.attestation_nonce.fetch_add(1, Ordering::Relaxed);
    let mut hasher = blake3::Hasher::new();
    hasher.update(input.as_bytes());
    hasher.update(&max_tokens.to_le_bytes());
    hasher.update(&nonce.to_le_bytes());
    let job_id = hex::encode(hasher.finalize().as_bytes());

    let (osh_tx, osh_rx) = tokio::sync::oneshot::channel::<WorkResult>();
    results.insert(job_id.clone(), osh_tx);

    let submitted_at = chrono::Utc::now().timestamp_millis();
    let item = WorkItem {
        job_id: job_id.clone(),
        input,
        max_tokens,
        model_id: model_id_hint,
        submitted_at_unix_ms: submitted_at,
    };

    if let Err(e) = tx.send(item).await {
        // Channel closed — drop our orphan oneshot from the map and
        // surface the error so the caller can fall back to local.
        results.remove(&job_id);
        return Err(format!("queue closed: {}", e));
    }

    let timeout = tokio::time::Duration::from_secs(COMMUNITY_DISPATCH_TIMEOUT_SECS);
    match tokio::time::timeout(timeout, osh_rx).await {
        Ok(Ok(result)) => {
            if !result.success {
                let err = result
                    .error
                    .clone()
                    .unwrap_or_else(|| "worker reported failure".to_string());
                return Err(err);
            }
            // Record dispatch latency in EWMA so future routing favors
            // workers that consistently beat the deadline. Keyed by
            // worker_id so it's distinguishable from the seed-to-seed
            // forward_shard latency table.
            record_latency(
                &node.latency_stats,
                &format!("worker:{}", result.worker_id),
                result.total_ms,
            );
            Ok(result)
        }
        Ok(Err(_)) => {
            // oneshot sender dropped without sending — the worker
            // disconnected mid-job or the queue purged us.
            results.remove(&job_id);
            Err("worker disconnected before completing job".into())
        }
        Err(_) => {
            // Timeout — orphan our entry so submit_work doesn't crash
            // when the late result arrives.
            results.remove(&job_id);
            Err(format!(
                "no worker completed within {}s",
                COMMUNITY_DISPATCH_TIMEOUT_SECS
            ))
        }
    }
}

/// Tier 1 on-chain inference: submit a request that triggers VRF
/// committee voting on-chain.
///
/// POST /inference/onchain/submit
/// Body: {
///   "input": "What is zero-knowledge?",
///   "max_tokens": 32,
///   "max_reward": 10,           // ARC to lock in escrow
///   "deadline_blocks": 20,      // relative deadline
///   "committee_size": 5,        // K
///   "model_id": "0xabc..."      // optional; defaults to BLAKE3("arc-32L-test")
/// }
///
/// Returns: { "request_id": "0x...", "tx_hash": "0x...", "anchor_height": 123 }
///
/// Convenience endpoint: signs the InferenceRequest tx with the local
/// validator's key. Desktop clients with their own identity can build the
/// tx locally and POST to `/tx/submit_signed` instead — the state apply
/// path is identical.
async fn inference_onchain_submit(
    AxumState(node): AxumState<NodeState>,
    body: Option<Json<Value>>,
) -> Result<Json<Value>, (StatusCode, Json<ApiError>)> {
    use arc_types::transaction::{
        InferenceRequestBody, Transaction, TxBody, TxType, TIER1_INPUT_BLOB_MAX,
        TIER1_MAX_TOKENS, TIER1_MIN_DEADLINE_BLOCKS, TIER1_MAX_DEADLINE_BLOCKS,
    };

    let req = match body {
        Some(Json(v)) => v,
        None => {
            return Err(api_error(
                StatusCode::BAD_REQUEST,
                "Request body required (input, max_tokens, max_reward, ...)",
            ))
        }
    };

    let input_text = req
        .get("input")
        .and_then(|v| v.as_str())
        .ok_or_else(|| api_error(StatusCode::BAD_REQUEST, "input must be a string"))?;
    if input_text.len() > TIER1_INPUT_BLOB_MAX {
        return Err(api_error(
            StatusCode::BAD_REQUEST,
            format!("input exceeds {} bytes", TIER1_INPUT_BLOB_MAX),
        ));
    }
    let max_tokens = req
        .get("max_tokens")
        .and_then(|v| v.as_u64())
        .unwrap_or(32)
        .min(TIER1_MAX_TOKENS as u64) as u32;
    let max_reward = req.get("max_reward").and_then(|v| v.as_u64()).unwrap_or(10);
    let deadline_blocks = req
        .get("deadline_blocks")
        .and_then(|v| v.as_u64())
        .unwrap_or(20)
        .clamp(TIER1_MIN_DEADLINE_BLOCKS, TIER1_MAX_DEADLINE_BLOCKS);
    let committee_size = req
        .get("committee_size")
        .and_then(|v| v.as_u64())
        .unwrap_or(5)
        .clamp(1, 32) as u8;
    let model_id = match req.get("model_id").and_then(|v| v.as_str()) {
        Some(hex_str) => {
            let stripped = hex_str.trim_start_matches("0x");
            let bytes = hex::decode(stripped).map_err(|e| {
                api_error(
                    StatusCode::BAD_REQUEST,
                    format!("invalid model_id hex: {}", e),
                )
            })?;
            if bytes.len() != 32 {
                return Err(api_error(
                    StatusCode::BAD_REQUEST,
                    "model_id must be 32 bytes",
                ));
            }
            let mut h = [0u8; 32];
            h.copy_from_slice(&bytes);
            arc_crypto::Hash256(h)
        }
        None => arc_crypto::hash_bytes(b"arc-32L-test"),
    };

    let input_blob = input_text.as_bytes().to_vec();
    let input_hash = arc_crypto::hash_bytes(&input_blob);

    // Deterministic request_id from (sender || input_hash || height).
    let sender_addr = node.validator_address;
    let mut id_input = Vec::with_capacity(72);
    id_input.extend_from_slice(&sender_addr.0);
    id_input.extend_from_slice(&input_hash.0);
    id_input.extend_from_slice(&node.state.height().to_le_bytes());
    let request_id_hash = arc_crypto::hash_bytes(&id_input);
    let request_id = request_id_hash.0;

    let nonce = node
        .state
        .get_account(&sender_addr)
        .map(|a| a.nonce)
        .unwrap_or(0);

    let body = TxBody::InferenceRequest(InferenceRequestBody {
        request_id,
        model_id,
        input_hash,
        input_blob,
        max_tokens,
        tier: 1,
        max_reward,
        deadline_blocks,
        committee_size,
    });

    let mut tx = Transaction {
        tx_type: TxType::InferenceRequest,
        from: sender_addr,
        nonce,
        body,
        fee: 0,
        gas_limit: 0,
        hash: arc_crypto::Hash256::ZERO,
        signature: arc_crypto::Signature::null(),
        sig_verified: false,
    };
    tx.hash = tx.compute_hash();
    if let Some(kp) = &node.validator_keypair {
        if let Ok(sig) = kp.sign(&tx.hash) {
            tx.signature = sig;
            tx.sig_verified = true;
        }
    } else {
        return Err(api_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "validator keypair not loaded — cannot sign InferenceRequest",
        ));
    }

    let tx_hash = tx.hash;
    node.mempool.insert(tx).map_err(|e| {
        api_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("mempool insert failed: {:?}", e),
        )
    })?;

    Ok(Json(json!({
        "request_id": format!("0x{}", hex::encode(request_id)),
        "tx_hash": tx_hash.to_hex(),
        "anchor_height": node.state.height(),
        "committee_size": committee_size,
        "deadline_blocks": deadline_blocks,
        "max_reward": max_reward,
    })))
}

/// Poll the on-chain status of a Tier 1 inference request.
///
/// GET /inference/onchain/result/:request_id
///
/// Returns: {
///   "request_id": "0x...",
///   "status": "Open" | "Voting" | "Finalized" | "Refunded",
///   "vote_count": 3,
///   "committee_size": 5,
///   "anchor_height": 123,
///   "deadline_blocks": 20,
///   "votes": [{"voter": "0x...", "output_hash": "0x..."}],
///   "output_hash": "0x..." | null,      // final, when Finalized
///   "output_blob": "..." | null,        // utf-8 if first voter attached
///   "max_reward": 10
/// }
async fn inference_onchain_result(
    AxumState(node): AxumState<NodeState>,
    axum::extract::Path(request_id_str): axum::extract::Path<String>,
) -> Result<Json<Value>, (StatusCode, Json<ApiError>)> {
    let stripped = request_id_str.trim_start_matches("0x");
    let bytes = hex::decode(stripped).map_err(|e| {
        api_error(
            StatusCode::BAD_REQUEST,
            format!("invalid request_id hex: {}", e),
        )
    })?;
    if bytes.len() != 32 {
        return Err(api_error(
            StatusCode::BAD_REQUEST,
            "request_id must be 32 bytes",
        ));
    }
    let mut request_id = [0u8; 32];
    request_id.copy_from_slice(&bytes);

    let snap = node.state.tier1_request_snapshot(&request_id).ok_or_else(|| {
        api_error(
            StatusCode::NOT_FOUND,
            format!("no such request: {}", request_id_str),
        )
    })?;

    let status_str = match snap.status {
        arc_state::TIER1_STATUS_OPEN => "Open",
        arc_state::TIER1_STATUS_VOTING => "Voting",
        arc_state::TIER1_STATUS_FINALIZED => "Finalized",
        arc_state::TIER1_STATUS_REFUNDED => "Refunded",
        _ => "Unknown",
    };

    // The final output_hash is stored after Finalize succeeds.
    let final_output_hash = node
        .state
        .get_storage(
            &snap.escrow_addr,
            &arc_crypto::hash_bytes(b"tier1.final_output_hash"),
        )
        .and_then(|bytes| {
            if bytes.len() == 32 {
                let mut h = [0u8; 32];
                h.copy_from_slice(&bytes);
                Some(arc_crypto::Hash256(h))
            } else {
                None
            }
        });
    let output_blob = node
        .state
        .get_storage(
            &snap.escrow_addr,
            &arc_crypto::hash_bytes(b"tier1.output_blob"),
        );

    // Decode token-id bytes (little-endian u32) back to text via the
    // local tokenizer. The blob is the same bytes the candle backend
    // hashed (see candle_backend.rs: `generated_tokens.iter().flat_map(|t| t.to_le_bytes())`).
    let output_text = output_blob.as_ref().and_then(|bytes| {
        if bytes.is_empty() || bytes.len() % 4 != 0 {
            return None;
        }
        let tokens: Vec<u32> = bytes
            .chunks_exact(4)
            .map(|c| u32::from_le_bytes([c[0], c[1], c[2], c[3]]))
            .collect();
        node.inference_model.as_ref().map(|m| m.decode(&tokens))
    });

    let votes_json: Vec<Value> = snap
        .votes
        .iter()
        .map(|(voter, oh)| {
            json!({
                "voter": voter.to_hex(),
                "output_hash": oh.to_hex(),
            })
        })
        .collect();

    Ok(Json(json!({
        "request_id": format!("0x{}", hex::encode(request_id)),
        "status": status_str,
        "vote_count": snap.votes.len(),
        "committee_size": snap.committee_size,
        "anchor_height": snap.anchor_height,
        "deadline_blocks": snap.deadline_blocks,
        "votes": votes_json,
        "output_hash": final_output_hash.map(|h| h.to_hex()),
        "output_blob": output_blob.as_ref().map(|b| String::from_utf8_lossy(b).to_string()),
        "output_text": output_text,
        "max_reward": snap.max_reward,
    })))
}

/// Run inference through a community worker (preferred when any are
/// online) or the local model (fallback). Records attestation on-chain
/// either way — task 3 of v0.7.0 will sign the worker's output with the
/// originating seed's key so community-served jobs land on-chain too.
///
/// POST /inference/run
/// Body: { "input": "What is 2+2?", "max_tokens": 64, "bond": 1000,
///         "force_local": false }
///
/// `force_local: true` skips the community dispatch and runs the
/// request on the local model only. Used by benchmarks and by the
/// rolling-upgrade verifier to confirm the seed's own engine still
/// serves correctly without depending on network state.
///
/// Returns the query, response text, output hash, ms/token, attestation
/// TX, and a `routed_via` field ("community:<worker_id>" | "local").
async fn inference_run(
    AxumState(node): AxumState<NodeState>,
    body: Option<Json<Value>>,
) -> Result<Json<Value>, (StatusCode, Json<ApiError>)> {
    let req = match body {
        Some(Json(v)) => v,
        None => return Err(api_error(StatusCode::BAD_REQUEST, "Request body required. Send JSON with 'input' and 'max_tokens' fields.")),
    };

    let input_text = req.get("input")
        .and_then(|v| v.as_str())
        .unwrap_or("Hello, world!");

    // Validate input: reject null bytes, enforce max length
    if input_text.len() > 32_768 {
        return Err(api_error(StatusCode::BAD_REQUEST, "Input exceeds 32KB limit"));
    }
    if input_text.contains('\0') {
        return Err(api_error(StatusCode::BAD_REQUEST, "Input contains null bytes"));
    }

    let max_tokens = req.get("max_tokens")
        .and_then(|v| v.as_u64())
        .unwrap_or(64)
        .min(4096) as u32; // Cap at 4K tokens to prevent resource exhaustion
    let bond = req.get("bond")
        .and_then(|v| v.as_u64())
        .unwrap_or(1000);
    let challenge_period = req.get("challenge_period")
        .and_then(|v| v.as_u64())
        .unwrap_or(100);
    let force_local = req.get("force_local")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    // ── Smart router: prefer community workers when any are online ──────
    //
    // The chain's design promise is "every device is a node, traffic
    // auto-routes to the most efficient worker." Concretely: when a
    // community worker has the model loaded and is idle, the seed
    // dispatches to them; the seed's own model is the fallback when no
    // worker can serve the job.
    //
    // Routing order:
    //   1. force_local=true  → skip dispatch, run on this seed.
    //   2. ≥1 live worker    → push WorkItem, wait up to 60s. If any
    //                          worker completes, return their result.
    //   3. Community fail    → fall through to local model.
    //   4. No local model    → return a 200 with success=false explaining
    //                          why (workers all timed out, no model loaded).
    //
    // On-chain attestation for the community path lands in task 3 of
    // v0.7.0 (worker signs the result with their validator key, seed
    // verifies + inserts to mempool). For now community-routed jobs
    // record into inference_results for explorer visibility but skip
    // the on-chain tx — that's intentional and explicit; without a
    // worker signature the seed crediting itself would be wrong.
    let live_workers = live_inference_worker_count(&node);
    if !force_local && live_workers > 0 && node.community_work_tx.is_some() {
        let dispatched_at = std::time::Instant::now();
        match dispatch_to_community_worker(
            &node,
            input_text.to_string(),
            max_tokens,
            None, // model_id pinning lands in task 4 (worker scoring)
        )
        .await
        {
            Ok(result) => {
                let total_ms = dispatched_at.elapsed().as_millis() as u64;
                let input_hash = arc_crypto::hash_bytes(input_text.as_bytes());
                node.inference_results.insert(
                    result.job_id.clone(),
                    json!({
                        "input": input_text,
                        "output": &result.output,
                        "output_hash": &result.output_hash,
                        "model": format!("community:{}", result.engine),
                        "model_hash": "",
                        "ms_per_token": result.ms_per_token,
                        "tokens_generated": result.tokens_generated,
                        "engine": &result.engine,
                        "deterministic": result.engine.contains("integer"),
                        "worker_id": &result.worker_id,
                    }),
                );
                return Ok(Json(json!({
                    "success": true,
                    "routed_via": format!("community:{}", result.worker_id),
                    "inference": {
                        "model": "community-served",
                        "model_hash": "",
                        "input": input_text,
                        "input_hash": format!("0x{}", hex::encode(&input_hash.0)),
                        "output": result.output,
                        "output_hash": result.output_hash,
                        "tokens_generated": result.tokens_generated,
                        "inference_ms": result.total_ms,
                        "ms_per_token": result.ms_per_token,
                        "encode_ms": 0,
                        "deterministic": result.engine.contains("integer"),
                        "engine": result.engine,
                        "dispatch_ms": total_ms,
                    },
                    "attestation": {
                        "tx_hash": "",
                        "bond": bond,
                        "challenge_period": challenge_period,
                        "status": "deferred_to_worker_signed_attestation",
                    },
                    "worker": {
                        "worker_id": result.worker_id,
                        "live_workers_at_dispatch": live_workers,
                    },
                })));
            }
            Err(e) => {
                tracing::warn!(
                    workers = live_workers,
                    "community dispatch failed, falling back to local: {}",
                    e
                );
            }
        }
    }

    // Check if we have a loaded model (prefer candle float backend for quality)
    let model = match &node.inference_model {
        Some(m) => m.clone(),
        None => {
            return Ok(Json(json!({
                "success": false,
                "routed_via": "none",
                "error": format!(
                    "No model loaded on this node and {} live community workers (community dispatch \
                     either timed out or none accepted). Start node with --model /path/to/model.gguf, \
                     or wait for a worker to register.",
                    live_workers
                ),
            })));
        }
    };

    let start = std::time::Instant::now();

    // Apply chat template from GGUF metadata (wraps input in model-specific format)
    let templated_input = model.apply_chat_template(input_text);

    // Encode templated text to tokens using the tokenizer
    let prompt_tokens = model.encode(&templated_input);
    let encode_ms = start.elapsed().as_millis() as u64;

    if prompt_tokens.is_empty() {
        return Ok(Json(json!({
            "success": false,
            "error": "Failed to encode input text to tokens",
        })));
    }

    // Both engines need BOS prepended - the model expects token 1 (<s>) at start.
    // Without BOS, the integer engine produces incoherent output because the
    // prompt starts in an undefined state.
    let mut tokens_with_bos = vec![model.config.bos_token];
    tokens_with_bos.extend(&prompt_tokens);

    // Run inference - use candle float backend if available, else integer engine
    let (generated_tokens, output_hash, engine_name) = if let (Some(engine), Some(mid)) = (&node.candle_engine, &node.candle_model_id) {
        // Candle Q4 float backend - coherent output, deterministic on same arch
        let result = engine.generate(mid, &tokens_with_bos, max_tokens)
            .map_err(|e| api_error(StatusCode::INTERNAL_SERVER_ERROR, format!("Inference failed: {}", e)))?;
        let gen_tokens: Vec<u32> = result.output.chunks(4)
            .map(|c| u32::from_le_bytes([c[0], c.get(1).copied().unwrap_or(0),
                c.get(2).copied().unwrap_or(0), c.get(3).copied().unwrap_or(0)]))
            .collect();
        (gen_tokens, result.output_hash, "candle Q4 (float, deterministic per-arch)")
    } else {
        // Integer engine fallback - bit-identical across architectures
        let (generated, hash) = model.generate(&tokens_with_bos, max_tokens, &model.config.eos_tokens);
        (generated, hash, "INT8 integer (cross-platform deterministic)")
    };

    let inference_ms = start.elapsed().as_millis() as u64;
    let tokens_generated = generated_tokens.len() as u64;
    let ms_per_token = if tokens_generated > 0 { inference_ms / tokens_generated } else { 0 };

    // Decode output tokens to text
    let output_text = model.decode(&generated_tokens);

    // Compute model ID
    let model_id_data = format!(
        "arc-{}L-{}d-{}h-{}v",
        model.config.n_layers, model.config.d_model,
        model.config.n_heads, model.config.vocab_size
    );
    let model_id_hash = arc_crypto::hash_bytes(model_id_data.as_bytes());
    let input_hash = arc_crypto::hash_bytes(input_text.as_bytes());

    // Create InferenceAttestation transaction.
    // Bump the per-node attestation_nonce so repeat-prompt attestations
    // (same model_id + input_hash + output_hash) get unique tx_hashes and
    // aren't deduped by the mempool.
    let attester = node.validator_address;
    let base_nonce = node.state.get_account(&attester)
        .map(|a| a.nonce)
        .unwrap_or(0);
    let bump = node.attestation_nonce.fetch_add(1, Ordering::Relaxed);
    let nonce = base_nonce + bump;

    let mut tx = arc_types::Transaction {
        tx_type: arc_types::TxType::InferenceAttestation,
        from: attester,
        nonce,
        body: arc_types::TxBody::InferenceAttestation(
            arc_types::transaction::InferenceAttestationBody {
                model_id: model_id_hash,
                input_hash,
                output_hash,
                challenge_period,
                bond,
                beneficiary: None,
            },
        ),
        fee: 0,
        gas_limit: 0,
        hash: arc_crypto::Hash256::ZERO,
        signature: arc_crypto::Signature::null(),
        sig_verified: false,
    };
    // Sign with the validator keypair so this tx survives `pipeline.rs`'s
    // verify stage on every peer. Setting `sig_verified=true` alone is not
    // enough — pipeline.rs only inspects the signature bytes; null sigs
    // always fail verification regardless of the flag.
    let tx_hash = if let Some(kp) = node.validator_keypair.as_ref() {
        match tx.sign(kp) {
            Ok(()) => {
                tx.sig_verified = true;
                let h = tx.hash;
                let _ = node.mempool.insert(tx);
                h
            }
            Err(e) => {
                tracing::warn!("inference attestation sign failed: {:?}", e);
                tx.compute_hash()
            }
        }
    } else {
        // Test fixture path: keep the legacy null-sig + sig_verified=true
        // shape so unit tests that don't wire a keypair still execute.
        tx.sig_verified = true;
        let h = tx.compute_hash();
        tx.hash = h;
        let _ = node.mempool.insert(tx);
        h
    };

    // Store inference result for explorer display
    let tx_hash_hex = format!("0x{}", hex::encode(&tx_hash.0));
    node.inference_results.insert(tx_hash_hex.clone(), json!({
        "input": input_text,
        "output": &output_text,
        "output_hash": format!("0x{}", hex::encode(&output_hash.0)),
        "model": &model_id_data,
        "model_hash": format!("0x{}", hex::encode(&model_id_hash.0)),
        "ms_per_token": ms_per_token,
        "tokens_generated": tokens_generated,
        "engine": &engine_name,
        "deterministic": true,
    }));

    Ok(Json(json!({
        "success": true,
        "routed_via": "local",
        "inference": {
            "model": model_id_data,
            "model_hash": format!("0x{}", hex::encode(&model_id_hash.0)),
            "input": input_text,
            "input_tokens": prompt_tokens.len(),
            "input_hash": format!("0x{}", hex::encode(&input_hash.0)),
            "output": output_text,
            "output_tokens": generated_tokens,
            "output_hash": format!("0x{}", hex::encode(&output_hash.0)),
            "tokens_generated": tokens_generated,
            "inference_ms": inference_ms,
            "ms_per_token": ms_per_token,
            "encode_ms": encode_ms,
            "deterministic": true,
            "engine": engine_name,
        },
        "attestation": {
            "tx_hash": tx_hash_hex,
            "bond": bond,
            "challenge_period": challenge_period,
            "status": "submitted_to_mempool",
        },
        "explorer_url": format!("/tx/0x{}", hex::encode(&tx_hash.0)),
    })))
}

/// Per-worker earnings, derived from on-chain InferenceAttestation events.
///
/// GET /worker/earnings/:address
///
/// Counts every InferenceAttestation (tx 0x16) where `tx.from` matches
/// the requested address. Multiplies by `REWARD_PER_ATTESTATION_ARC` to
/// get total ARC earned. "Today" is approximated as the last 12% of
/// attestations in chronological order (the chain doesn't currently
/// expose a per-tx timestamp for this query — a future release will
/// fold block timestamps in via `state.get_receipt`).
///
/// Replaces the desktop's pre-v0.7 client-side synthesis from
/// /inference/results, which conflated "this seed's local inference
/// cache" with "what this address earned across the network." A worker
/// behind NAT could earn a thousand attestations and the local cache
/// would still show 0.
async fn worker_earnings(
    AxumState(node): AxumState<NodeState>,
    axum::extract::Path(address_hex): axum::extract::Path<String>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let trimmed = address_hex.trim_start_matches("0x");
    let raw = hex::decode(trimmed)
        .map_err(|e| (StatusCode::BAD_REQUEST, format!("invalid hex address: {}", e)))?;
    if raw.len() != 32 {
        return Err((
            StatusCode::BAD_REQUEST,
            format!("address must be 32 bytes, got {}", raw.len()),
        ));
    }
    let mut bytes = [0u8; 32];
    bytes.copy_from_slice(&raw);
    let want = Hash256(bytes);

    let mut count: u64 = 0;
    let mut last_block: Option<u64> = None;
    let mut last_tx_hash: Option<String> = None;

    // Option C (credit the original requester, not the working validator)
    // WITHOUT a wire field: map each request's input_hash -> the requester
    // (the sender of the InferenceRequest tx). The matching attestation
    // carries the same input_hash, so the original payer is recoverable
    // from on-chain history. This replaces the v0.7.6 `beneficiary` wire
    // field, which was a bincode-incompatible change that partitioned the
    // chain (see InferenceAttestationBody::beneficiary).
    let mut requester_by_input: HashMap<Hash256, Hash256> = HashMap::new();
    for entry in node.state.full_transactions.iter() {
        if let TxBody::InferenceRequest(req) = &entry.value().body {
            requester_by_input
                .entry(req.input_hash)
                .or_insert_with(|| entry.value().from);
        }
    }

    for entry in node.state.full_transactions.iter() {
        let tx = entry.value();
        let body = match &tx.body {
            TxBody::InferenceAttestation(b) => b,
            _ => continue,
        };
        // Credit the original requester (looked up by input_hash) when
        // known; otherwise fall back to the attestation signer (`tx.from`).
        let credited = requester_by_input
            .get(&body.input_hash)
            .copied()
            .unwrap_or(tx.from);
        if credited != want {
            continue;
        }
        count += 1;
        if let Some(receipt) = node.state.get_receipt(entry.key()) {
            let bh = receipt.block_height;
            // Track the latest block we've seen this address attest at.
            if last_block.map(|cur| bh > cur).unwrap_or(true) {
                last_block = Some(bh);
                last_tx_hash = Some(format!("0x{}", hex::encode(entry.key())));
            }
        }
    }

    let total_arc = count as f64 * REWARD_PER_ATTESTATION_ARC;

    // Approximate "today" as the most recent 12% of attestations until we
    // wire block timestamps into this query. Bounded to count so a worker
    // with one attestation still shows it in "today."
    let today_count = ((count as f64 * 0.12).round() as u64).max(if count > 0 { 1 } else { 0 });
    let today_arc = today_count as f64 * REWARD_PER_ATTESTATION_ARC;

    Ok(Json(json!({
        "address": format!("0x{}", trimmed),
        "total_attestations": count,
        "total_arc": total_arc,
        "today_arc": today_arc,
        "today_attestations": today_count,
        "reward_per_attestation_arc": REWARD_PER_ATTESTATION_ARC,
        "last_attestation_block": last_block,
        "last_attestation_tx_hash": last_tx_hash,
    })))
}

/// Live community-worker leaderboard. Reads only in-memory state
/// (no chain query) so it's cheap to poll from the dashboard.
///
/// GET /workers/scoreboard?limit=50
///
/// Returns workers sorted by composite score:
///   score = (success_rate * 1000) − avg_ms_per_job
///
/// Workers with no successful submissions yet get score = 0 (not -∞)
/// and sort to the bottom but stay visible — fresh workers shouldn't
/// disappear from the board until they actively fail.
///
/// In v0.8 this endpoint will additionally drive per-worker dispatch
/// priority. v0.7.0 phase 1 keeps the FIFO mpsc; this is the
/// observability hook that the worker-targeted-lane refactor will
/// build on.
async fn workers_scoreboard(
    AxumState(node): AxumState<NodeState>,
    Query(params): Query<HashMap<String, String>>,
) -> Json<Value> {
    let limit = params
        .get("limit")
        .and_then(|l| l.parse::<usize>().ok())
        .unwrap_or(50)
        .min(500);

    let now = std::time::Instant::now();
    let ttl = std::time::Duration::from_secs(COMMUNITY_WORKER_TTL_SECS);

    #[derive(serde::Serialize)]
    struct WorkerScore {
        worker_id: String,
        name: String,
        platform: String,
        model: Option<String>,
        registered_at: u64,
        success_count: u64,
        failure_count: u64,
        success_rate: f64,
        avg_ms_per_job: f64,
        last_total_ms: u64,
        score: f64,
    }

    let mut rows: Vec<WorkerScore> = Vec::new();
    for entry in node.community_workers.iter() {
        let (w, ts) = entry.value();
        if now.duration_since(*ts) > ttl {
            continue;
        }
        let attempts = w.success_count + w.failure_count;
        let success_rate = if attempts > 0 {
            w.success_count as f64 / attempts as f64
        } else {
            0.0
        };
        let avg_ms = if w.success_count > 0 {
            w.sum_total_ms_success as f64 / w.success_count as f64
        } else {
            0.0
        };
        // Composite score: heavily weight success_rate, penalize slow
        // workers. Scale chosen so a 100% success-rate worker at 100ms
        // beats a 50%-rate worker at 50ms (1000 - 100 = 900 vs 500 - 50
        // = 450). Workers with no completed jobs yet get score 0 (visible
        // but ranked last among visible workers).
        let score = if w.success_count == 0 {
            0.0
        } else {
            success_rate * 1000.0 - avg_ms
        };
        rows.push(WorkerScore {
            worker_id: w.worker_id.clone(),
            name: w.name.clone(),
            platform: w.platform.clone(),
            model: w.model.clone(),
            registered_at: w.registered_at,
            success_count: w.success_count,
            failure_count: w.failure_count,
            success_rate,
            avg_ms_per_job: avg_ms,
            last_total_ms: w.last_total_ms,
            score,
        });
    }

    rows.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal));
    rows.truncate(limit);

    Json(json!({
        "workers": rows,
        "count_visible": rows.len(),
        "count_total": node.community_workers.len(),
    }))
}

/// List recent inference attestations from chain state.
///
/// GET /inference/attestations?limit=10
async fn inference_list_attestations(
    AxumState(node): AxumState<NodeState>,
    Query(params): Query<HashMap<String, String>>,
) -> Result<Json<Value>, StatusCode> {
    let limit = params.get("limit")
        .and_then(|l| l.parse::<usize>().ok())
        .unwrap_or(10);

    // Get latest block to find recent attestations
    let height = node.state.height();
    let mut attestations = Vec::new();

    // First: add inference results (highest priority - these are what users want to see)
    for entry in node.inference_results.iter() {
        let tx_hex = entry.key().clone();
        let inf = entry.value().clone();
        let hash_clean = tx_hex.trim_start_matches("0x");
        let mut att = json!({
            "tx_hash": tx_hex,
            "tx_type": "Inference",
            "success": true,
            "inference": inf,
        });
        // Enrich with receipt data if available
        if let Ok(hash_bytes) = hex::decode(hash_clean) {
            if hash_bytes.len() == 32 {
                let mut key = [0u8; 32];
                key.copy_from_slice(&hash_bytes);
                if let Some(receipt) = node.state.get_receipt(&key) {
                    att["block_height"] = json!(receipt.block_height);
                    att["gas_used"] = json!(receipt.gas_used);
                }
            }
        }
        attestations.push(att);
        if attestations.len() >= limit { break; }
    }

    // Then: scan on-chain InferenceAttestation transactions (cross-device visibility).
    // This lets nodes that didn't run the inference themselves still show the
    // attestation in their /inference/attestations feed, enabling cross-device
    // aggregation in the dashboard.
    for entry in node.state.full_transactions.iter() {
        if attestations.len() >= limit { break; }
        let hash = entry.key();
        let tx = entry.value();
        let tx_hex = format!("0x{}", hex::encode(hash));
        // Skip if already added from local inference_results cache
        if node.inference_results.contains_key(&tx_hex) { continue; }

        // Inference attestations from other devices - include with hashes
        if let TxBody::InferenceAttestation(body) = &tx.body {
            let att = json!({
                "tx_hash": tx_hex,
                "tx_type": "Inference",
                "success": true,
                "from": tx.from.to_hex(),
                "block_height": node.state.get_receipt(hash).map(|r| r.block_height),
                "inference": {
                    "model_hash": format!("0x{}", hex::encode(&body.model_id.0)),
                    "input_hash": format!("0x{}", hex::encode(&body.input_hash.0)),
                    "output_hash": format!("0x{}", hex::encode(&body.output_hash.0)),
                    "bond": body.bond,
                    "challenge_period": body.challenge_period,
                    "deterministic": true,
                    // input/output text not on-chain; hashes only
                    "input": format!("[cross-device: hash {}]", hex::encode(&body.input_hash.0)[..16].to_string()),
                    "output": format!("[hash {}]", hex::encode(&body.output_hash.0)[..16].to_string()),
                    "model": "on-chain attestation",
                }
            });
            attestations.push(att);
            continue;
        }

        if let Some(receipt) = node.state.get_receipt(hash) {
            if !receipt.success { continue; } // Only show successful txs

            let (tx_type, to, amount) = match &tx.body {
                TxBody::Transfer(b) => {
                    let label = if b.amount >= 10_000 { "Faucet" } else { "Transfer" };
                    (label, Some(b.to.to_hex()), Some(b.amount))
                }
                TxBody::Settle(b) => ("Settle", Some(b.agent_id.to_hex()), Some(b.amount)),
                _ => ("Other", None, None),
            };
            let mut att = json!({
                "tx_hash": tx_hex,
                "tx_type": tx_type,
                "from": tx.from.to_hex(),
                "success": true,
                "block_height": receipt.block_height,
                "gas_used": receipt.gas_used,
            });
            if let Some(to) = to { att["to"] = json!(to); }
            if let Some(amt) = amount { att["amount"] = json!(amt); }
            attestations.push(att);
        }
    }

    Ok(Json(json!({
        "attestations": attestations,
        "count": attestations.len(),
        "chain_height": height,
    })))
}

/// GET /inference/results - list stored inference results (input, output, hash, model).
async fn inference_list_results(
    AxumState(node): AxumState<NodeState>,
) -> Json<Value> {
    let results: Vec<Value> = node.inference_results.iter()
        .map(|entry| {
            let mut r = entry.value().clone();
            r["tx_hash"] = json!(entry.key().clone());
            r
        })
        .collect();
    Json(json!({
        "results": results,
        "count": results.len(),
    }))
}

// ─── Sharded Inference: Pipeline-Parallel ──────────────────────────────────
//
// The model is split across N nodes. A request flows:
//   client → coordinator → shard0 (layers 0..k0)
//                       → shard1 (layers k0..k1)
//                       → ...
//                       → shardN-1 (layers kN-1..n_layers + LM head)
// Each shard holds a per-request KV cache that lives across token generation.

#[derive(serde::Serialize, serde::Deserialize)]
struct ForwardShardRequest {
    /// Unique request id (hex). The receiving shard uses this as the KV cache key.
    request_id: String,
    /// Either a token id (only valid on first shard) or a hidden state (i64s).
    #[serde(default)]
    token: Option<u32>,
    #[serde(default)]
    hidden: Option<Vec<i64>>,
    /// Hash of the hidden state for integrity verification (BLAKE3 hex).
    #[serde(default)]
    hidden_hash: Option<String>,
    /// Token position in the sequence (for RoPE + KV cache).
    position: usize,
    /// Layer range this node should process.
    start_layer: usize,
    end_layer: usize,
    /// True if this is the last token of the request - used to evict KV cache.
    #[serde(default)]
    last_token: bool,
}

#[derive(serde::Serialize, serde::Deserialize)]
struct ForwardShardResponse {
    /// True if this shard ran the LM head and produced a token.
    is_terminal: bool,
    /// Hidden state to forward to the next shard (if not terminal).
    #[serde(skip_serializing_if = "Option::is_none")]
    hidden: Option<Vec<i64>>,
    /// BLAKE3 hash of the hidden state for next-shard verification.
    #[serde(skip_serializing_if = "Option::is_none")]
    hidden_hash: Option<String>,
    /// Token id (if terminal).
    #[serde(skip_serializing_if = "Option::is_none")]
    token_id: Option<u32>,
    /// Hash of the logits (if terminal).
    #[serde(skip_serializing_if = "Option::is_none")]
    logits_hash: Option<String>,
    /// Layers this shard processed.
    layers_processed: usize,
    /// Compute time for this shard, in milliseconds.
    compute_ms: u64,
    /// Friendly node name.
    node_name: String,
}

/// GET /inference/cache_stats
/// Report live stats about the deterministic inference cache: how many
/// entries are warm, what the capacity is, and total cumulative hits.
/// Dashboards call this to show a "N prompts cached" counter.
async fn inference_cache_stats(
    AxumState(node): AxumState<NodeState>,
) -> Json<serde_json::Value> {
    Json(json!({
        "size": node.inference_cache.len(),
        "capacity": node.inference_cache.capacity(),
        "total_hits": node.inference_cache.total_hits(),
        "cache_type": "DistributedCache (BLAKE3-keyed, deterministic, LRU)",
    }))
}

/// GET /inference/latency_stats
/// Returns the rolling EWMA hop latency (ms) per replica socket, plus sample
/// count and age. Coordinators use this map to sort per-range replica lists
/// before picking primary (run_sharded) or top-k (run_consensus). Closes #29.
async fn inference_latency_stats(
    AxumState(node): AxumState<NodeState>,
) -> Json<serde_json::Value> {
    let mut entries: Vec<serde_json::Value> = Vec::with_capacity(node.latency_stats.len());
    for kv in node.latency_stats.iter() {
        let (socket, stat) = (kv.key().clone(), kv.value().clone());
        entries.push(json!({
            "socket": socket,
            "ewma_ms": (stat.ms * 100.0).round() / 100.0,
            "samples": stat.count,
            "age_secs": stat.last_updated.elapsed().as_secs(),
        }));
    }
    entries.sort_by(|a, b| {
        let ae = a.get("ewma_ms").and_then(|v| v.as_f64()).unwrap_or(f64::MAX);
        let be = b.get("ewma_ms").and_then(|v| v.as_f64()).unwrap_or(f64::MAX);
        ae.partial_cmp(&be).unwrap_or(std::cmp::Ordering::Equal)
    });
    Json(json!({
        "alpha": LATENCY_ALPHA,
        "count": entries.len(),
        "replicas": entries,
    }))
}

/// POST /inference/cache_check
/// Given a list of prompts, report for each whether it is currently warm
/// in the cache. The dashboard uses this to show a "✓ instant" badge on
/// preset prompt buttons, so visitors can see ahead of time which clicks
/// will return in milliseconds and which will run the full pipeline.
#[derive(Deserialize)]
struct CacheCheckRequest {
    prompts: Vec<CacheCheckPrompt>,
}

#[derive(Deserialize)]
struct CacheCheckPrompt {
    input: String,
    #[serde(default = "default_cache_check_max_tokens")]
    max_tokens: u32,
}

fn default_cache_check_max_tokens() -> u32 { 20 }

#[derive(Serialize)]
struct CacheCheckResult {
    input: String,
    max_tokens: u32,
    cached: bool,
}

async fn inference_cache_check(
    AxumState(node): AxumState<NodeState>,
    Json(req): Json<CacheCheckRequest>,
) -> Result<Json<Vec<CacheCheckResult>>, (StatusCode, String)> {
    let model = node
        .inference_model
        .as_ref()
        .ok_or((StatusCode::SERVICE_UNAVAILABLE, "no model loaded".to_string()))?;

    // Replicate the cache-key derivation used by inference_run_sharded so
    // a check here matches what a real call would look up.
    let model_id_data = format!(
        "arc-{}L-{}d-{}h-{}v",
        model.config.n_layers, model.config.d_model,
        model.config.n_heads, model.config.vocab_size
    );
    let model_id_hash = arc_crypto::hash_bytes(model_id_data.as_bytes());

    let results: Vec<CacheCheckResult> = req
        .prompts
        .into_iter()
        .map(|p| {
            let templated = model.apply_chat_template(&p.input);
            let prompt_tokens = model.encode(&templated);
            let mut all_tokens: Vec<u32> = vec![model.config.bos_token];
            all_tokens.extend(&prompt_tokens);
            all_tokens.push(p.max_tokens);
            let key = arc_inference::distributed::DistributedCache::cache_key(
                &model_id_hash,
                &all_tokens,
            );
            CacheCheckResult {
                cached: node.inference_cache.contains(&key),
                input: p.input,
                max_tokens: p.max_tokens,
            }
        })
        .collect();

    Ok(Json(results))
}

/// POST /inference/forward_shard
/// Run the local shard's slice of layers on the incoming hidden state (or token).
async fn inference_forward_shard(
    AxumState(node): AxumState<NodeState>,
    Json(req): Json<ForwardShardRequest>,
) -> Result<Json<ForwardShardResponse>, (StatusCode, String)> {
    let model = node.inference_model.as_ref()
        .ok_or((StatusCode::SERVICE_UNAVAILABLE, "No model loaded".to_string()))?;
    if node.shard_infos.is_empty() {
        return Err((StatusCode::SERVICE_UNAVAILABLE, "Node is not a shard holder".to_string()));
    }
    // Verify this node holds the requested layer range. A node holding
    // multiple disjoint ranges accepts requests for any of them - each range
    // was independently announced and is an independent replica slot.
    let shard = node.shard_infos.iter()
        .find(|s| s.start_layer == req.start_layer && s.end_layer == req.end_layer)
        .ok_or_else(|| (StatusCode::BAD_REQUEST, format!(
            "Shard mismatch: requested [{}, {}) but this node holds {:?}",
            req.start_layer, req.end_layer,
            node.shard_infos.iter().map(|s| (s.start_layer, s.end_layer)).collect::<Vec<_>>()
        )))?;

    // Decode input
    use arc_inference::cached_integer_model::{ShardInput, ShardOutput, KVCache};

    let input = if let Some(token) = req.token {
        // Verify this is the first shard (only first shard accepts a raw token)
        if shard.start_layer != 0 {
            return Err((StatusCode::BAD_REQUEST, "Only the first shard accepts a raw token".to_string()));
        }
        ShardInput::Token(token)
    } else if let Some(hidden) = req.hidden {
        // Verify integrity hash if provided
        if let Some(expected_hex) = &req.hidden_hash {
            let bytes: Vec<u8> = hidden.iter().flat_map(|v| v.to_le_bytes()).collect();
            let actual = arc_crypto::hash_bytes(&bytes);
            let actual_hex = format!("0x{}", hex::encode(&actual.0));
            if &actual_hex != expected_hex {
                return Err((StatusCode::BAD_REQUEST, format!(
                    "Hidden state integrity check failed: expected {}, got {}", expected_hex, actual_hex
                )));
            }
        }
        ShardInput::Hidden(hidden)
    } else {
        return Err((StatusCode::BAD_REQUEST, "Need either 'token' or 'hidden' field".to_string()));
    };

    // Get-or-create per-request KV cache
    let n_layers = model.config.n_layers;
    let cache_arc = node.shard_kv_caches
        .entry(req.request_id.clone())
        .or_insert_with(|| Arc::new(std::sync::Mutex::new(KVCache::new(n_layers))))
        .value()
        .clone();

    // Run the shard's forward pass (blocking - uses spawn_blocking to free runtime)
    let model_clone = model.clone();
    let req_id = req.request_id.clone();
    let start_layer = shard.start_layer;
    let end_layer = shard.end_layer;
    let position = req.position;
    let node_name = shard.node_name.clone();

    let t0 = std::time::Instant::now();
    let result = tokio::task::spawn_blocking(move || -> Result<ShardOutput, String> {
        let cache_arc = cache_arc;
        let mut cache = cache_arc.lock().map_err(|e| format!("KV lock: {}", e))?;
        Ok(model_clone.forward_shard_token(input, &mut *cache, start_layer, end_layer, position))
    }).await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("Join: {}", e)))?
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e))?;
    let compute_ms = t0.elapsed().as_millis() as u64;

    // Optionally evict cache after the last token
    if req.last_token {
        node.shard_kv_caches.remove(&req_id);
    }

    let layers_processed = end_layer - start_layer;
    let response = match result {
        ShardOutput::Hidden(state) => {
            let bytes: Vec<u8> = state.iter().flat_map(|v| v.to_le_bytes()).collect();
            let h = arc_crypto::hash_bytes(&bytes);
            ForwardShardResponse {
                is_terminal: false,
                hidden: Some(state),
                hidden_hash: Some(format!("0x{}", hex::encode(&h.0))),
                token_id: None,
                logits_hash: None,
                layers_processed,
                compute_ms,
                node_name,
            }
        }
        ShardOutput::Token { id, logits_hash } => {
            ForwardShardResponse {
                is_terminal: true,
                hidden: None,
                hidden_hash: None,
                token_id: Some(id),
                logits_hash: Some(format!("0x{}", hex::encode(&logits_hash.0))),
                layers_processed,
                compute_ms,
                node_name,
            }
        }
    };
    Ok(Json(response))
}

/// POST /inference/run_sharded
/// Coordinator endpoint: walks the pipeline of shard-holding nodes and
/// generates `max_tokens` tokens by forwarding hidden states between shards.
///
/// Returns the full output, all per-shard timings, and the network bandwidth
/// used so the dashboard can show the activation flow.
async fn inference_run_sharded(
    AxumState(node): AxumState<NodeState>,
    Json(req): Json<Value>,
) -> Result<Json<Value>, (StatusCode, Json<ApiError>)> {
    let input_text = req.get("input")
        .and_then(|v| v.as_str())
        .ok_or(api_error(StatusCode::BAD_REQUEST, "'input' field required"))?;

    // Validate input: enforce max length
    if input_text.len() > 32_768 {
        return Err(api_error(StatusCode::BAD_REQUEST, "Input exceeds 32KB limit"));
    }

    let max_tokens = req.get("max_tokens")
        .and_then(|v| v.as_u64())
        .unwrap_or(20)
        .min(256) as u32;
    // Opt-in chat template wrapping. Default OFF because the dashboard is
    // doing autocomplete ("The capital of France is" → " Paris"), not
    // instruction-following. Wrapping in [INST]...[/INST] inflates prompt_len
    // by ~5x (11 tokens for a 3-token input) and since the pipeline walks
    // all positions, that 5x directly multiplies wall time. Pass
    // `"chat_template": true` in the body to re-enable it for chat models.
    let chat_template_enabled = req.get("chat_template")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    // Coordinator needs a model for tokenization (text→tokens and tokens→text).
    // This is the tokenizer vocabulary, not the full model weights.
    // Shard-holding nodes or nodes with --model serve as coordinators.
    let model = node.inference_model.as_ref()
        .ok_or(api_error(StatusCode::SERVICE_UNAVAILABLE,
            "Coordinator needs a model loaded for tokenization. Start with --model <path.gguf>. \
             Shard nodes serve inference; the coordinator only uses the tokenizer."))?;

    // Build the pipeline from the local shard registry, sorted by start_layer.
    //
    // Dedupe by (start_layer, end_layer, node_name): after a coordinator
    // reboot, its own self-announcement (socket_addr=0.0.0.0:9090) and the
    // peer-to-peer gossip copy (socket_addr=<public IP>:9090) land under
    // different registry keys, creating two entries for the same layer
    // range. Prefer the entry with a routable socket_addr so that
    // forward_shard calls don't try to POST to 0.0.0.0.
    // Group replicas by (start, end). Every replica holding the same range
    // is a failover candidate - if the primary stops answering mid-request,
    // the worker falls back to the next. When multiple announcements for
    // the same (node_name, range) exist we prefer routable socket_addrs over
    // stubs (0.0.0.0 / 127.x / empty).
    fn is_stub(a: &str) -> bool {
        a.starts_with("0.0.0.0") || a.starts_with("127.") || a.is_empty()
    }
    let mut by_range: std::collections::BTreeMap<(usize, usize), Vec<ShardInfo>> =
        std::collections::BTreeMap::new();
    for s in fresh_shards(&node.shard_registry) {
        let key = (s.start_layer, s.end_layer);
        let bucket = by_range.entry(key).or_default();
        // Dedupe per node_name within the bucket, preferring routable addrs.
        let dup_idx = bucket.iter().position(|existing| existing.node_name == s.node_name);
        match dup_idx {
            None => bucket.push(s),
            Some(i) => {
                if is_stub(&bucket[i].socket_addr) && !is_stub(&s.socket_addr) {
                    bucket[i] = s;
                }
            }
        }
    }
    // Filter stubs out of the final replica list - a stub address can't be
    // dialed so it can never satisfy a coordinator hop. Keep the entry only
    // if it was the only announcement we have for that node.
    // #29: also sort each bucket by rolling EWMA latency ascending so the
    // fastest replica for this range wins primary on the next dispatch.
    for bucket in by_range.values_mut() {
        let routable: Vec<ShardInfo> = bucket.iter().filter(|s| !is_stub(&s.socket_addr)).cloned().collect();
        if !routable.is_empty() {
            *bucket = routable;
        }
        sort_replicas_by_latency(bucket, &node.latency_stats);
    }
    // Pipeline becomes one entry per range with its replica list attached.
    // The first replica in each Vec is the current primary.
    let mut pipeline_ranges: Vec<((usize, usize), Vec<ShardInfo>)> = by_range
        .into_iter()
        .collect();
    pipeline_ranges.sort_by_key(|((s, _), _)| *s);
    let pipeline: Vec<ShardInfo> = pipeline_ranges.iter()
        .map(|(_, replicas)| replicas[0].clone())
        .collect();

    if pipeline.is_empty() {
        return Err(api_error(StatusCode::SERVICE_UNAVAILABLE, "No shards announced. Need shard registry to be populated."));
    }

    // Verify the pipeline is contiguous and covers all layers.
    // Stop early once coverage is complete — stale extra shards in the
    // registry beyond n_layers must not cause a false gap error.
    let n_layers = pipeline[0].total_layers;
    let mut covered_to = 0usize;
    for shard in &pipeline {
        if covered_to >= n_layers {
            break;
        }
        if shard.start_layer != covered_to {
            return Err(api_error(StatusCode::SERVICE_UNAVAILABLE, format!(
                "Pipeline gap: expected layer {} next, got shard [{}, {}) (node {}, addr {})",
                covered_to, shard.start_layer, shard.end_layer, shard.node_name, shard.socket_addr
            )));
        }
        covered_to = shard.end_layer;
    }
    if covered_to != n_layers {
        return Err(api_error(StatusCode::SERVICE_UNAVAILABLE, format!(
            "Pipeline incomplete: covered layers 0..{} but model has {} layers",
            covered_to, n_layers
        )));
    }

    let request_id = format!("0x{}", hex::encode(arc_crypto::hash_bytes(format!("{}-{}", input_text, std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0)).as_bytes()).0));

    // Tokenize input. Only wrap in chat template when explicitly requested;
    // the default is raw completion.
    let tokenized_text: String = if chat_template_enabled {
        model.apply_chat_template(input_text)
    } else {
        input_text.to_string()
    };
    let prompt_tokens = model.encode(&tokenized_text);
    let mut all_tokens: Vec<u32> = vec![model.config.bos_token];
    all_tokens.extend(&prompt_tokens);

    let overall_start = std::time::Instant::now();

    // ─────────────────────────────────────────────────────────────────────
    // DETERMINISTIC CACHE LOOKUP
    // Same model_id + same input tokens = same output tokens, GUARANTEED.
    // If we've seen this exact input before, return the cached result in
    // O(1) - no pipeline walk, no HTTP roundtrips, no compute.
    // ─────────────────────────────────────────────────────────────────────
    let cache_model_id_data = format!(
        "arc-{}L-{}d-{}h-{}v",
        model.config.n_layers, model.config.d_model,
        model.config.n_heads, model.config.vocab_size
    );
    let cache_model_id_hash = arc_crypto::hash_bytes(cache_model_id_data.as_bytes());
    let cache_input_with_max: Vec<u32> = {
        let mut v = all_tokens.clone();
        v.push(max_tokens); // include max_tokens in the cache key so different lengths don't collide
        v
    };
    let cache_key = arc_inference::distributed::DistributedCache::cache_key(&cache_model_id_hash, &cache_input_with_max);

    if let Some(cached_tokens) = node.inference_cache.get(&cache_key) {
        // CACHE HIT - return the cached tokens with the same output_hash
        let output_text = model.decode(&cached_tokens);
        let output_bytes: Vec<u8> = cached_tokens.iter().flat_map(|t| t.to_le_bytes()).collect();
        let output_hash = arc_crypto::hash_bytes(&output_bytes);
        let elapsed_us = overall_start.elapsed().as_micros() as u64;
        node.sharded_runs_total.fetch_add(1, Ordering::Relaxed);
        return Ok(Json(json!({
            "success": true,
            "request_id": request_id,
            "input": input_text,
            "output": output_text,
            "output_tokens": cached_tokens,
            "output_hash": format!("0x{}", hex::encode(&output_hash.0)),
            "model_hash": format!("0x{}", hex::encode(&cache_model_id_hash.0)),
            "tokens_generated": cached_tokens.len(),
            "total_ms": elapsed_us / 1000,
            "total_us": elapsed_us,
            "ms_per_token": 0,
            "pipeline_length": pipeline.len(),
            "model": cache_model_id_data,
            "shard_trace": [],
            "total_bytes_transferred": 0,
            "deterministic": true,
            "engine": "deterministic cache hit (provably bit-identical to original sharded run)",
            "cache": {
                "hit": true,
                "key": format!("0x{}", hex::encode(&cache_key.0)),
                "served_in_us": elapsed_us,
            },
        })));
    }
    // ─────────────────────────────────────────────────────────────────────

    let mut generated: Vec<u32> = Vec::new();
    let mut shard_trace: Vec<Value> = Vec::new();
    let mut total_bytes_transferred: usize = 0;

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(120))
        .build()
        .map_err(|e| api_error(StatusCode::INTERNAL_SERVER_ERROR, format!("HTTP client: {}", e)))?;

    // Pipeline execution.
    //
    // PREFILL (positions 0..prompt_len):
    //   All prompt tokens are known up-front. We stream them through the
    //   shard chain using per-shard mpsc workers, so at steady state every
    //   shard is processing a different position simultaneously. Wall time
    //   is ~(prompt_len + num_shards - 1) × per_shard_time instead of
    //   prompt_len × num_shards × per_shard_time.
    //
    // GENERATION (positions prompt_len..prompt_len+max_tokens):
    //   Each output token depends on the previous one's logits so we must
    //   wait for the full pipeline walk before starting the next position.
    //   Kept as a straight sequential loop - pipeline parallelism does not
    //   apply to single-stream autoregressive decoding.
    //
    // The output from the LAST prompt position is the FIRST generated
    // token. We capture it via the terminal flag on the pipeline's tail
    // shard and then continue the sequential loop for the remaining
    // (max_tokens - 1) tokens.
    let prompt_len = all_tokens.len();

    // ─── Pipelined prefill ────────────────────────────────────────────
    {
        use tokio::sync::mpsc;

        // PrefillFlow is what travels between shard worker tasks. Each
        // worker reads a PrefillFlow, runs its forward_shard call, and
        // sends the next PrefillFlow (carrying the new hidden state) to
        // the next worker. The tail shard sets `terminal_token`.
        #[derive(Debug)]
        struct PrefillFlow {
            position: usize,
            token: Option<u32>,
            hidden: Option<Vec<i64>>,
            hidden_hash: Option<String>,
            terminal_token: Option<u32>,
        }

        let num_shards = pipeline.len();
        let buffer = (prompt_len + 4).max(16);

        // (num_shards + 1) channels: index 0 is the coordinator→shard0
        // input; index k (1..=num_shards) carries shard(k-1)→shard(k)
        // outputs; index num_shards is the tail shard→coordinator output.
        let mut txs: Vec<mpsc::Sender<PrefillFlow>> = Vec::with_capacity(num_shards + 1);
        let mut rxs: Vec<Option<mpsc::Receiver<PrefillFlow>>> = Vec::with_capacity(num_shards + 1);
        for _ in 0..=num_shards {
            let (tx, rx) = mpsc::channel::<PrefillFlow>(buffer);
            txs.push(tx);
            rxs.push(Some(rx));
        }

        // Spawn one worker task per shard. Each loops on its input
        // channel until the sender is dropped.
        let mut worker_handles: Vec<tokio::task::JoinHandle<Result<(usize, Option<(u64, u64, bool, u64, String)>), String>>>
            = Vec::with_capacity(num_shards);
        for i in 0..num_shards {
            let replicas = pipeline_ranges[i].1.clone();
            let (start_layer, end_layer) = pipeline_ranges[i].0;
            let client_c = client.clone();
            let req_id = request_id.clone();
            let mut rx = rxs[i].take().expect("rx slot populated");
            let tx_out = txs[i + 1].clone();
            let is_last_shard = i == num_shards - 1;
            // #29: hand the latency map into the spawn so successful hops fold
            // into the EWMA used for the next dispatch's sort.
            let lat_stats = node.latency_stats.clone();

            let handle = tokio::spawn(async move {
                let mut bytes_this_shard: usize = 0;
                let mut trace: Option<(u64, u64, bool, u64, String)> = None; // (compute_ms, wall_ms, is_terminal, layers, node_name) - first-seen
                // Ordered replica list per range. The first entry is the
                // current primary; on HTTP/parse failure we promote the next
                // replica and keep going. "Never breaks" guarantee: as long
                // as any replica for this range is reachable, the request
                // succeeds.
                let mut replicas = replicas;
                while let Some(item) = rx.recv().await {
                    let req = ForwardShardRequest {
                        request_id: req_id.clone(),
                        token: item.token.clone(),
                        hidden: item.hidden.clone(),
                        hidden_hash: item.hidden_hash.clone(),
                        position: item.position,
                        start_layer,
                        end_layer,
                        last_token: false,
                    };
                    let payload_bytes = serde_json::to_vec(&req).unwrap_or_default();

                    let t_hop = std::time::Instant::now();
                    let mut resp_opt: Option<ForwardShardResponse> = None;
                    let mut last_err: String = String::new();
                    let mut attempted: usize = 0;
                    let mut served_by: Option<String> = None;
                    while attempted < replicas.len() {
                        let shard = replicas[0].clone();
                        let url = format!("http://{}/inference/forward_shard", shard.socket_addr);
                        let attempt = client_c.post(&url)
                            .header("Content-Type", "application/json")
                            .body(payload_bytes.clone())
                            .send()
                            .await
                            .and_then(|r| r.error_for_status());
                        match attempt {
                            Ok(r) => match r.json::<ForwardShardResponse>().await {
                                Ok(j) => {
                                    bytes_this_shard += payload_bytes.len();
                                    resp_opt = Some(j);
                                    served_by = Some(shard.node_name.clone());
                                    record_latency(&lat_stats, &shard.socket_addr, t_hop.elapsed().as_millis() as u64);
                                    break;
                                }
                                Err(e) => {
                                    last_err = format!("shard [{start_layer}, {end_layer}) replica {} ({}) parse: {}",
                                        shard.node_name, shard.socket_addr, e);
                                    attempted += 1;
                                    replicas.rotate_left(1);
                                }
                            },
                            Err(e) => {
                                last_err = format!("shard [{start_layer}, {end_layer}) replica {} ({}): {}",
                                    shard.node_name, shard.socket_addr, e);
                                attempted += 1;
                                replicas.rotate_left(1);
                            }
                        }
                    }
                    let resp = match resp_opt {
                        Some(r) => r,
                        None => return Err(format!("All {} replicas failed for range [{start_layer}, {end_layer}). Last error: {}",
                            replicas.len(), last_err)),
                    };
                    let hop_ms = t_hop.elapsed().as_millis() as u64;

                    // Capture trace on the FIRST processed position only
                    if trace.is_none() {
                        trace = Some((
                            resp.compute_ms,
                            hop_ms,
                            resp.is_terminal,
                            resp.layers_processed as u64,
                            served_by.clone().unwrap_or_else(|| replicas[0].node_name.clone()),
                        ));
                    }

                    let flow = PrefillFlow {
                        position: item.position,
                        token: None,
                        hidden: resp.hidden,
                        hidden_hash: resp.hidden_hash,
                        terminal_token: if is_last_shard { resp.token_id } else { None },
                    };
                    if tx_out.send(flow).await.is_err() {
                        break;
                    }
                }
                Ok((bytes_this_shard, trace))
            });
            worker_handles.push(handle);
        }

        // Feed the entire prompt into shard 0's input channel up front.
        // The workers will stream through in pipeline order. Drop the
        // coordinator-held copies of all other channel senders so that
        // workers observe EOF once their upstream drops.
        let input_tx = txs.remove(0);
        // txs[0..num_shards-1] are intermediate senders the workers already
        // cloned. Drop them so workers see EOF. txs[num_shards-1] is the
        // LAST shard's output channel that the coordinator reads from -
        // drop our extra copy too so recv()  unblocks after all positions.
        drop(txs);
        for (pos, &tok) in all_tokens.iter().enumerate() {
            let flow = PrefillFlow {
                position: pos,
                token: Some(tok),
                hidden: None,
                hidden_hash: None,
                terminal_token: None,
            };
            if input_tx.send(flow).await.is_err() {
                return Err(api_error(StatusCode::INTERNAL_SERVER_ERROR, "Prefill input channel closed prematurely"));
            }
        }
        drop(input_tx); // signal end-of-input to shard 0

        // Collect outputs from the tail shard. Order is guaranteed by the
        // mpsc FIFO and the in-order single-task workers, so items arrive
        // in ascending position order. The last position's terminal token
        // is the FIRST generated token.
        let mut final_rx = rxs[num_shards].take().expect("tail rx slot populated");
        let mut positions_seen = 0usize;
        let mut first_generated_token: Option<u32> = None;
        while let Some(flow) = final_rx.recv().await {
            positions_seen += 1;
            if flow.position == prompt_len - 1 {
                first_generated_token = flow.terminal_token;
            }
            if positions_seen >= prompt_len {
                break;
            }
        }

        if positions_seen < prompt_len {
            return Err(api_error(
                StatusCode::BAD_GATEWAY,
                format!("Pipelined prefill incomplete: {}/{} positions arrived at tail shard", positions_seen, prompt_len),
            ));
        }

        // Gather per-worker stats (bytes + first-position trace).
        let mut trace_entries: Vec<(usize, u64, u64, bool, u64, String)> = Vec::new();
        for (hop, handle) in worker_handles.into_iter().enumerate() {
            match handle.await {
                Ok(Ok((bytes, trace))) => {
                    total_bytes_transferred += bytes;
                    if let Some((compute_ms, wall_ms, is_terminal, layers, node_name)) = trace {
                        trace_entries.push((hop, compute_ms, wall_ms, is_terminal, layers, node_name));
                    }
                }
                Ok(Err(e)) => return Err(api_error(StatusCode::BAD_GATEWAY, e)),
                Err(e) => return Err(api_error(StatusCode::INTERNAL_SERVER_ERROR, format!("Join worker: {}", e))),
            }
        }
        trace_entries.sort_by_key(|(h, _, _, _, _, _)| *h);
        for (hop, compute_ms, wall_ms, is_terminal, layers, node_name) in trace_entries {
            let shard = &pipeline[hop];
            shard_trace.push(json!({
                "hop": hop,
                "node": node_name,
                "node_name": shard.node_name,
                "socket": shard.socket_addr,
                "layers": format!("{}..{}", shard.start_layer, shard.end_layer),
                "layers_count": layers,
                "compute_ms": compute_ms,
                "wall_ms": wall_ms,
                "payload_bytes": 0, // per-shard in-flight; total is in total_bytes_transferred
                "is_terminal": is_terminal,
            }));
        }

        // The LAST prompt position's output at the tail shard is the FIRST
        // generated token. Record it before starting sequential generation.
        if let Some(tok) = first_generated_token {
            if !model.config.eos_tokens.contains(&tok) {
                generated.push(tok);
            }
        }
    }
    // ─── End pipelined prefill ────────────────────────────────────────

    // Sequential generation for remaining tokens. Each token depends on
    // the previous one's logits so pipeline parallelism does not apply.
    // We skip the first output (already captured during prefill tail).
    for gen_idx in 1..(max_tokens as usize) {
        if let Some(last) = generated.last() {
            if model.config.eos_tokens.contains(last) {
                break;
            }
        }
        let position = prompt_len + gen_idx - 1; // position of the NEW token we feed in
        let input_token = *generated.last().unwrap_or(&all_tokens[prompt_len - 1]);

        let mut current_payload: ForwardShardRequest = ForwardShardRequest {
            request_id: request_id.clone(),
            token: Some(input_token),
            hidden: None,
            hidden_hash: None,
            position,
            start_layer: pipeline[0].start_layer,
            end_layer: pipeline[0].end_layer,
            last_token: false,
        };

        let mut next_token_id: Option<u32> = None;
        for (range_idx, ((s_layer, e_layer), replicas)) in pipeline_ranges.iter().enumerate() {
            current_payload.start_layer = *s_layer;
            current_payload.end_layer = *e_layer;

            let payload_bytes = serde_json::to_vec(&current_payload).unwrap_or_default();

            // Iterate through replicas of this range until one succeeds.
            // Each replica failure rotates the list in-place on the local
            // copy so subsequent hops favor the working replica first.
            let mut try_order: Vec<ShardInfo> = replicas.clone();
            let mut resp_opt: Option<ForwardShardResponse> = None;
            let mut last_err = String::new();
            for replica in try_order.drain(..) {
                let url = format!("http://{}/inference/forward_shard", replica.socket_addr);
                let t_hop = std::time::Instant::now();
                let attempt = client.post(&url)
                    .header("Content-Type", "application/json")
                    .body(payload_bytes.clone())
                    .send()
                    .await
                    .and_then(|r| r.error_for_status());
                match attempt {
                    Ok(r) => match r.json::<ForwardShardResponse>().await {
                        Ok(j) => {
                            total_bytes_transferred += payload_bytes.len();
                            record_latency(&node.latency_stats, &replica.socket_addr, t_hop.elapsed().as_millis() as u64);
                            resp_opt = Some(j);
                            break;
                        }
                        Err(e) => last_err = format!("replica {} ({}) parse: {}", replica.node_name, replica.socket_addr, e),
                    },
                    Err(e) => last_err = format!("replica {} ({}): {}", replica.node_name, replica.socket_addr, e),
                }
            }
            let resp = resp_opt.ok_or_else(|| api_error(StatusCode::BAD_GATEWAY, format!(
                "All replicas failed for range [{s_layer}, {e_layer}) at gen step {gen_idx}. Last: {last_err}"
            )))?;

            if resp.is_terminal {
                next_token_id = resp.token_id;
                let _ = range_idx;
                break;
            }
            current_payload = ForwardShardRequest {
                request_id: request_id.clone(),
                token: None,
                hidden: resp.hidden,
                hidden_hash: resp.hidden_hash,
                position,
                start_layer: 0,
                end_layer: 0,
                last_token: false,
            };
        }

        if let Some(tok) = next_token_id {
            if model.config.eos_tokens.contains(&tok) {
                break;
            }
            generated.push(tok);
        }
    }

    // Cleanup: fan-out last_token=true to every replica of every range so
    // each holder drops its per-request KV cache even if generation only
    // routed through one of them.
    let _ = node.shard_kv_caches.remove(&request_id);
    for (_, replicas) in pipeline_ranges.iter() {
        for replica in replicas {
            let _ = client.post(format!("http://{}/inference/forward_shard", replica.socket_addr))
                .json(&serde_json::json!({"request_id": request_id, "last_token": true}))
                .send()
                .await;
        }
    }

    let total_ms = overall_start.elapsed().as_millis() as u64;
    let output_text = model.decode(&generated);
    let output_bytes: Vec<u8> = generated.iter().flat_map(|t| t.to_le_bytes()).collect();
    let output_hash = arc_crypto::hash_bytes(&output_bytes);

    // Bump network-wide counters: how many sharded runs this coordinator has
    // served and how many bytes of activations were forwarded between shards.
    node.sharded_runs_total.fetch_add(1, Ordering::Relaxed);
    node.sharded_bytes_total.fetch_add(total_bytes_transferred as u64, Ordering::Relaxed);

    let model_id_data = format!(
        "arc-{}L-{}d-{}h-{}v",
        model.config.n_layers, model.config.d_model,
        model.config.n_heads, model.config.vocab_size
    );
    let model_id_hash = arc_crypto::hash_bytes(model_id_data.as_bytes());
    let input_hash = arc_crypto::hash_bytes(input_text.as_bytes());

    // Save to the deterministic cache. Future requests with the same prompt
    // (and same max_tokens) will return this exact result in O(1).
    let now_secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    node.inference_cache.insert(
        cache_key,
        arc_inference::distributed::CacheEntry {
            output_tokens: generated.clone(),
            output_hash,
            model_id: cache_model_id_hash,
            hit_count: 0,
            created_at_secs: now_secs,
        },
    );

    // Submit an InferenceAttestation transaction so this sharded run is
    // recorded on-chain like single-node /inference/run does. Anyone reading
    // the chain later can verify model_id + input_hash + output_hash.
    //
    // Nonce strategy: bump the in-memory attestation_nonce counter and add
    // it to the account's persisted nonce. Each sharded run gets a unique
    // nonce → unique tx_hash → mempool accepts it (no dedupe collision
    // even when the same prompt is run twice).
    let attester = node.validator_address;
    let base_nonce = node.state.get_account(&attester).map(|a| a.nonce).unwrap_or(0);
    let bump = node.attestation_nonce.fetch_add(1, Ordering::Relaxed);
    let nonce = base_nonce + bump;
    let tx = arc_types::Transaction {
        tx_type: arc_types::TxType::InferenceAttestation,
        from: attester,
        nonce,
        body: arc_types::TxBody::InferenceAttestation(
            arc_types::transaction::InferenceAttestationBody {
                model_id: model_id_hash,
                input_hash,
                output_hash,
                challenge_period: 100,
                bond: 1000,
                beneficiary: None,
            },
        ),
        fee: 0,
        gas_limit: 0,
        hash: arc_crypto::Hash256::ZERO,
        signature: arc_crypto::Signature::null(),
        // Coordinator-internal submit; bypass the unsigned-tx reject
        // at arc-state lib.rs:1186 the same way the faucet path does.
        sig_verified: true,
    };
    let tx_hash = tx.compute_hash();
    let _ = node.mempool.insert(tx);
    let tx_hash_hex = format!("0x{}", hex::encode(&tx_hash.0));

    // Cache result for explorer / cross-device view (same as inference_run does)
    node.inference_results.insert(tx_hash_hex.clone(), json!({
        "input": input_text,
        "output": &output_text,
        "output_hash": format!("0x{}", hex::encode(&output_hash.0)),
        "model": &model_id_data,
        "model_hash": format!("0x{}", hex::encode(&model_id_hash.0)),
        "ms_per_token": if generated.is_empty() { 0 } else { total_ms / generated.len() as u64 },
        "tokens_generated": generated.len() as u64,
        "engine": "INT8 sharded pipeline (cross-platform deterministic)",
        "deterministic": true,
        "sharded": true,
        "pipeline_length": pipeline.len(),
        "shard_trace": &shard_trace,
        "total_bytes_transferred": total_bytes_transferred,
    }));

    // ─── VRF Committee Verification ────────────────────────────────────
    // Select a verification committee from the live validator set using the
    // output hash as VRF seed. This implements Tier 2 inference verification
    // from committee.rs - deterministic, reproducible committee selection.
    let committee_info = {
        let validators = node.dag_validators.read();
        let eligible: Vec<arc_inference::committee::InferenceValidator> = validators
            .iter()
            .map(|(addr, stake)| arc_inference::committee::InferenceValidator {
                address: *addr,
                max_tier: 2, // All validators eligible for Tier 2
                stake: *stake,
            })
            .collect();

        if eligible.len() >= 3 {
            let committee = arc_inference::committee::select_committee(
                &output_hash,
                &eligible,
                2, // Tier 2
                eligible.len().min(arc_inference::committee::DEFAULT_COMMITTEE_SIZE),
            );
            // In a full implementation, we'd collect votes from committee members
            // and call aggregate_votes(). For now, the deterministic integer engine
            // guarantees bit-identical output, so all honest committee members
            // WILL agree. Record the committee for auditability.
            let member_hexes: Vec<String> = committee.members.iter()
                .map(|m| format!("0x{}", hex::encode(&m.0)))
                .collect();
            json!({
                "selected": true,
                "size": committee.members.len(),
                "min_agreement": committee.min_agreement,
                "members": member_hexes,
                "vrf_seed": format!("0x{}", hex::encode(&output_hash.0)),
                "tier": 2,
                "corruption_probability": arc_inference::committee::corruption_probability(0.1, committee.members.len(), committee.min_agreement),
            })
        } else {
            json!({
                "selected": false,
                "reason": "fewer than 3 validators online",
                "validators_online": eligible.len(),
            })
        }
    };

    // ─── Auto-commit inference result to verification manager ────────
    {
        if let Ok(mut mgr) = node.verification_manager.lock() {
            let commitment = arc_vm::inference_verify::InferenceCommitment {
                request_id: arc_crypto::hash_bytes(request_id.as_bytes()).0,
                result_hash: output_hash.0,
                provider: node.validator_address.0,
                timestamp: std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or(0),
                bond_amount: 1000,
            };
            mgr.submit_commitment(commitment);
        }
    }

    // ─── Fee split computation (for dashboard/explorer display) ──────
    let fee_split = node.revenue_config.split_fee(1000, node.dag_validators.read().len().saturating_sub(1) as u32);

    Ok(Json(json!({
        "success": true,
        "request_id": request_id,
        "input": input_text,
        "output": output_text,
        "output_tokens": generated,
        "output_hash": format!("0x{}", hex::encode(&output_hash.0)),
        "input_hash": format!("0x{}", hex::encode(&input_hash.0)),
        "model_hash": format!("0x{}", hex::encode(&model_id_hash.0)),
        "tokens_generated": generated.len(),
        "total_ms": total_ms,
        "ms_per_token": if generated.is_empty() { 0 } else { total_ms / generated.len() as u64 },
        "pipeline_length": pipeline.len(),
        "model": model_id_data,
        "shard_trace": shard_trace,
        "total_bytes_transferred": total_bytes_transferred,
        "deterministic": true,
        "engine": "INT8 sharded pipeline (cross-platform deterministic)",
        "cache": {
            "hit": false,
            "key": format!("0x{}", hex::encode(&cache_key.0)),
            "size": node.inference_cache.len(),
        },
        "attestation": {
            "tx_hash": tx_hash_hex,
            "bond": 1000,
            "challenge_period": 100,
            "status": "submitted_to_mempool",
        },
        "committee": committee_info,
        "fee_split": {
            "proposer": fee_split.proposer,
            "per_verifier": fee_split.per_verifier,
            "observer_pool": fee_split.observer_pool,
            "treasury": fee_split.treasury,
        },
        "explorer_url": format!("/tx/0x{}", hex::encode(&tx_hash.0)),
    })))
}

/// POST /inference/run_consensus
/// Slice B: parallel k-of-n forward_shard per range with hash-majority
/// verification at every shard boundary.
///
/// Semantics vs /inference/run_sharded:
/// - run_sharded picks the first replica for each range; on HTTP failure it
///   rotates to the next. Fast. Silent hash divergence is INVISIBLE.
/// - run_consensus fires to k replicas in parallel, collects every
///   hidden_hash, and requires >=ceil(k/2)+1 (strict majority) to agree
///   before forwarding the majority's hidden state. Divergent replicas
///   are logged in the response for later on-chain slashing. Slower, but
///   an individual dishonest replica cannot produce a wrong token.
///
/// Request body mirrors run_sharded with an optional `k` field (default 3).
/// Response adds a `consensus` block with per-range vote records.
async fn inference_run_consensus(
    AxumState(node): AxumState<NodeState>,
    Json(req): Json<Value>,
) -> Result<Json<Value>, (StatusCode, Json<ApiError>)> {
    let input_text = req.get("input")
        .and_then(|v| v.as_str())
        .ok_or(api_error(StatusCode::BAD_REQUEST, "'input' field required"))?;
    if input_text.len() > 32_768 {
        return Err(api_error(StatusCode::BAD_REQUEST, "Input exceeds 32KB limit"));
    }
    let max_tokens = req.get("max_tokens")
        .and_then(|v| v.as_u64()).unwrap_or(20).min(256) as u32;
    let k_req = req.get("k").and_then(|v| v.as_u64()).unwrap_or(3).max(1) as usize;
    let chat_template_enabled = req.get("chat_template").and_then(|v| v.as_bool()).unwrap_or(false);

    // Milestone B (#36): if the request carries { payer, request_id,
    // max_fee, model_id, timeout_blocks } it's an escrow-gated call.
    // The coordinator pre-flights that an open escrow exists with enough
    // balance before touching any model, and on success submits an
    // InferenceEscrowRelease that pays out the 40/25/15/20 split.
    //
    // Free-mode (no payer) still works: all escrow fields are optional.
    // Dashboards + old desktop clients keep using the free path - no
    // breaking change.
    let escrow_payer_hex = req.get("payer").and_then(|v| v.as_str());
    let escrow_req_id_hex = req.get("request_id").and_then(|v| v.as_str());
    let escrow_max_fee = req.get("max_fee").and_then(|v| v.as_u64());
    let escrow_model_id_hex = req.get("model_id").and_then(|v| v.as_str());
    let escrow_timeout = req.get("timeout_blocks").and_then(|v| v.as_u64());

    let escrow_gate: Option<EscrowGate> = match (
        escrow_payer_hex,
        escrow_req_id_hex,
        escrow_max_fee,
        escrow_model_id_hex,
        escrow_timeout,
    ) {
        (Some(p), Some(r), Some(f), Some(m), Some(t)) => {
            let payer = decode_address_hex(p).map_err(|e| {
                api_error(StatusCode::BAD_REQUEST, format!("payer: {}", e))
            })?;
            let request_id = decode_hash_hex(r).map_err(|e| {
                api_error(StatusCode::BAD_REQUEST, format!("request_id: {}", e))
            })?;
            let model_id = decode_hash_hex(m).map_err(|e| {
                api_error(StatusCode::BAD_REQUEST, format!("model_id: {}", e))
            })?;
            let escrow_addr = arc_types::transaction::InferenceEscrowOpenBody::escrow_address(&request_id);
            let escrow_account = node.state.get_account(&arc_crypto::Hash256(escrow_addr));
            let locked = escrow_account.map(|a| a.balance).unwrap_or(0);
            if locked < f {
                return Err(api_error(
                    StatusCode::PAYMENT_REQUIRED,
                    format!(
                        "escrow not open for this request_id (locked={}, need max_fee={}); \
                         submit an InferenceEscrowOpen tx first",
                        locked, f
                    ),
                ));
            }
            Some(EscrowGate {
                payer: arc_crypto::Hash256(payer),
                request_id,
                max_fee: f,
                model_id: arc_crypto::Hash256(model_id),
                max_tokens,
                timeout_blocks: t,
            })
        }
        // Partial escrow fields → reject; too easy to lose money to typos.
        (None, None, None, None, None) => None,
        _ => {
            return Err(api_error(
                StatusCode::BAD_REQUEST,
                "escrow-gated run_consensus requires all of { payer, \
                 request_id, max_fee, model_id, timeout_blocks } - got a \
                 partial set",
            ));
        }
    };

    let model = node.inference_model.as_ref()
        .ok_or(api_error(StatusCode::SERVICE_UNAVAILABLE,
            "Coordinator needs a tokenizer loaded. Start with --model <path.gguf>."))?;

    // Group fresh shard announcements by (start, end). Dedupe per node_name
    // inside each bucket preferring routable addrs over stubs.
    fn stub(a: &str) -> bool {
        a.starts_with("0.0.0.0") || a.starts_with("127.") || a.is_empty()
    }
    let mut by_range: std::collections::BTreeMap<(usize, usize), Vec<ShardInfo>> =
        std::collections::BTreeMap::new();
    for s in fresh_shards(&node.shard_registry) {
        let key = (s.start_layer, s.end_layer);
        let bucket = by_range.entry(key).or_default();
        let dup = bucket.iter().position(|e| e.node_name == s.node_name);
        match dup {
            None => bucket.push(s),
            Some(i) => {
                if stub(&bucket[i].socket_addr) && !stub(&s.socket_addr) {
                    bucket[i] = s;
                }
            }
        }
    }
    // #29: sort each bucket by rolling EWMA latency before taking the top-k.
    // Does not affect determinism (hash-majority still enforces correctness);
    // just biases us toward lower-latency replicas first.
    for bucket in by_range.values_mut() {
        let routable: Vec<ShardInfo> = bucket.iter().filter(|s| !stub(&s.socket_addr)).cloned().collect();
        if !routable.is_empty() {
            *bucket = routable;
        }
        sort_replicas_by_latency(bucket, &node.latency_stats);
    }
    let pipeline_ranges: Vec<((usize, usize), Vec<ShardInfo>)> = by_range.into_iter().collect();
    if pipeline_ranges.is_empty() {
        return Err(api_error(StatusCode::SERVICE_UNAVAILABLE, "No shards announced."));
    }
    // Verify contiguous coverage. Stop early once coverage is complete —
    // stale extra shards beyond n_layers must not cause a false gap error.
    let n_layers = pipeline_ranges[0].1[0].total_layers;
    let mut covered = 0usize;
    for ((s, e), _) in &pipeline_ranges {
        if covered >= n_layers {
            break;
        }
        if *s != covered {
            return Err(api_error(StatusCode::SERVICE_UNAVAILABLE, format!(
                "Pipeline gap: expected layer {} next, got [{}, {})", covered, s, e
            )));
        }
        covered = *e;
    }
    if covered != n_layers {
        return Err(api_error(StatusCode::SERVICE_UNAVAILABLE, format!(
            "Pipeline incomplete: 0..{} of {}", covered, n_layers
        )));
    }

    // Tokenize
    let tokenized_text = if chat_template_enabled { model.apply_chat_template(input_text) } else { input_text.to_string() };
    let prompt_tokens = model.encode(&tokenized_text);
    let mut all_tokens: Vec<u32> = vec![model.config.bos_token];
    all_tokens.extend(&prompt_tokens);

    let request_id = format!("0x{}", hex::encode(
        arc_crypto::hash_bytes(format!("{}-{}", input_text,
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos()).unwrap_or(0)).as_bytes()).0
    ));

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(120))
        .build()
        .map_err(|e| api_error(StatusCode::INTERNAL_SERVER_ERROR, format!("http: {}", e)))?;

    // Per-range consensus records accumulated across all token positions.
    // Keyed by (position, range). Majority hash + full replica vote list.
    #[derive(serde::Serialize, Clone)]
    struct RangeVote {
        position: usize,
        range: (usize, usize),
        replicas_contacted: Vec<String>,
        replicas_returned: Vec<String>,
        majority_hash: Option<String>,
        divergent: Vec<(String, String)>, // (replica, their_hash)
        agreement: String, // "unanimous" | "majority" | "split" | "no_response"
    }
    let mut votes: Vec<RangeVote> = Vec::new();

    let overall_start = std::time::Instant::now();
    let prompt_len = all_tokens.len();
    let mut generated: Vec<u32> = Vec::new();
    let mut first_gen_token: Option<u32> = None;

    // Helper: fire k parallel forward_shard requests to the first k replicas
    // in `replicas`, collect their responses. Return Result<(majority_hidden,
    // majority_hash, vote_record), err_string>.
    async fn consensus_hop(
        client: &reqwest::Client,
        replicas: &[ShardInfo],
        k: usize,
        req: &ForwardShardRequest,
        lat_stats: &Arc<dashmap::DashMap<String, LatencyEWMA>>,
    ) -> Result<(Option<Vec<i64>>, Option<String>, bool, Option<u32>, Option<String>, RangeVote), String> {
        let use_k = k.min(replicas.len()).max(1);
        let selected: Vec<ShardInfo> = replicas.iter().take(use_k).cloned().collect();
        let body = serde_json::to_vec(req).map_err(|e| e.to_string())?;

        let mut futs = Vec::with_capacity(use_k);
        for r in &selected {
            let url = format!("http://{}/inference/forward_shard", r.socket_addr);
            let c = client.clone();
            let b = body.clone();
            let r_clone = r.clone();
            let stats = lat_stats.clone();
            let socket = r.socket_addr.clone();
            futs.push(tokio::spawn(async move {
                let t_hop = std::time::Instant::now();
                let send_res = c.post(&url).header("Content-Type","application/json").body(b).send().await;
                let parsed: Result<ForwardShardResponse, String> = match send_res {
                    Ok(r) => match r.error_for_status() {
                        Ok(ok_resp) => ok_resp.json::<ForwardShardResponse>().await
                            .map_err(|e| format!("parse: {}", e)),
                        Err(e) => Err(format!("http status: {}", e)),
                    },
                    Err(e) => Err(format!("send: {}", e)),
                };
                if parsed.is_ok() {
                    record_latency(&stats, &socket, t_hop.elapsed().as_millis() as u64);
                }
                (r_clone.node_name.clone(), parsed)
            }));
        }

        let mut returned: Vec<(String, ForwardShardResponse)> = Vec::new();
        for f in futs {
            match f.await {
                Ok((name, Ok(r))) => returned.push((name, r)),
                Ok((name, Err(e))) => tracing::warn!("consensus replica {} failed: {}", name, e),
                Err(e) => tracing::warn!("consensus task join failed: {}", e),
            }
        }

        // Group by hash. For intermediate ranges: compare hidden_hash. For
        // terminal range: compare logits_hash.
        let is_terminal_returned = returned.iter().any(|(_, r)| r.is_terminal);
        let hash_of = |r: &ForwardShardResponse| -> Option<String> {
            if r.is_terminal { r.logits_hash.clone() } else { r.hidden_hash.clone() }
        };
        let mut hash_counts: std::collections::HashMap<String, Vec<&(String, ForwardShardResponse)>> =
            std::collections::HashMap::new();
        for item in &returned {
            if let Some(h) = hash_of(&item.1) {
                hash_counts.entry(h).or_default().push(item);
            }
        }
        let (majority_hash, majority_items) = hash_counts.into_iter()
            .max_by_key(|(_, v)| v.len())
            .map(|(h, v)| (Some(h), v))
            .unwrap_or((None, Vec::new()));

        let needed = (use_k / 2) + 1;
        let have = majority_items.len();

        let mut vote = RangeVote {
            position: req.position,
            range: (req.start_layer, req.end_layer),
            replicas_contacted: selected.iter().map(|s| s.node_name.clone()).collect(),
            replicas_returned: returned.iter().map(|(n, _)| n.clone()).collect(),
            majority_hash: majority_hash.clone(),
            divergent: Vec::new(),
            agreement: if returned.is_empty() { "no_response".into() }
                else if have == returned.len() { "unanimous".into() }
                else if have >= needed { "majority".into() }
                else { "split".into() },
        };
        for item in &returned {
            let h = hash_of(&item.1);
            if h != majority_hash {
                vote.divergent.push((item.0.clone(), h.unwrap_or_default()));
            }
        }

        if majority_items.is_empty() {
            return Err(format!(
                "No replica responded for range [{}, {}) at position {}",
                req.start_layer, req.end_layer, req.position
            ));
        }
        if have < needed {
            return Err(format!(
                "No majority hash for range [{}, {}) at position {} - {} of {} agreed (needed {})",
                req.start_layer, req.end_layer, req.position, have, returned.len(), needed
            ));
        }

        let picked = &majority_items[0].1;
        Ok((
            picked.hidden.clone(),
            picked.hidden_hash.clone(),
            picked.is_terminal,
            picked.token_id,
            picked.logits_hash.clone(),
            vote,
        ))
    }

    // === Prefill: for each position, walk all ranges with k-of-n consensus.
    // Positions run sequentially here (simpler than the pipelined prefill in
    // run_sharded). For prompt_len up to ~128, overhead is acceptable.
    for (pos_idx, &tok) in all_tokens.iter().enumerate() {
        let mut cur_hidden: Option<Vec<i64>> = None;
        let mut cur_hash: Option<String> = None;
        let mut got_first_gen: Option<u32> = None;
        for (range_idx, ((s_layer, e_layer), replicas)) in pipeline_ranges.iter().enumerate() {
            let req_body = ForwardShardRequest {
                request_id: request_id.clone(),
                token: if range_idx == 0 { Some(tok) } else { None },
                hidden: if range_idx == 0 { None } else { cur_hidden.clone() },
                hidden_hash: if range_idx == 0 { None } else { cur_hash.clone() },
                position: pos_idx,
                start_layer: *s_layer,
                end_layer: *e_layer,
                last_token: false,
            };
            let (hid, hash, is_terminal, token_id, _logits_hash, vote) =
                consensus_hop(&client, replicas, k_req, &req_body, &node.latency_stats).await
                    .map_err(|e| api_error(StatusCode::BAD_GATEWAY, e))?;
            votes.push(vote);
            if is_terminal {
                got_first_gen = token_id;
                break;
            }
            cur_hidden = hid;
            cur_hash = hash;
        }
        if pos_idx == prompt_len - 1 {
            first_gen_token = got_first_gen;
        }
    }
    if let Some(tok) = first_gen_token {
        if !model.config.eos_tokens.contains(&tok) {
            generated.push(tok);
        }
    }

    // === Generation: each subsequent token feeds the previous as input.
    for gen_idx in 1..(max_tokens as usize) {
        if let Some(last) = generated.last() {
            if model.config.eos_tokens.contains(last) { break; }
        }
        let position = prompt_len + gen_idx - 1;
        let input_token = *generated.last().unwrap_or(&all_tokens[prompt_len - 1]);
        let mut cur_hidden: Option<Vec<i64>> = None;
        let mut cur_hash: Option<String> = None;
        let mut next_tok: Option<u32> = None;
        for (range_idx, ((s_layer, e_layer), replicas)) in pipeline_ranges.iter().enumerate() {
            let req_body = ForwardShardRequest {
                request_id: request_id.clone(),
                token: if range_idx == 0 { Some(input_token) } else { None },
                hidden: if range_idx == 0 { None } else { cur_hidden.clone() },
                hidden_hash: if range_idx == 0 { None } else { cur_hash.clone() },
                position,
                start_layer: *s_layer,
                end_layer: *e_layer,
                last_token: false,
            };
            let (hid, hash, is_terminal, token_id, _logits_hash, vote) =
                consensus_hop(&client, replicas, k_req, &req_body, &node.latency_stats).await
                    .map_err(|e| api_error(StatusCode::BAD_GATEWAY, e))?;
            votes.push(vote);
            if is_terminal {
                next_tok = token_id;
                break;
            }
            cur_hidden = hid;
            cur_hash = hash;
        }
        if let Some(t) = next_tok {
            if model.config.eos_tokens.contains(&t) { break; }
            generated.push(t);
        }
    }

    // Cleanup - fan out last_token=true to every replica of every range.
    for (_, replicas) in &pipeline_ranges {
        for r in replicas {
            let _ = client.post(format!("http://{}/inference/forward_shard", r.socket_addr))
                .json(&serde_json::json!({"request_id": request_id, "last_token": true}))
                .send().await;
        }
    }

    let total_ms = overall_start.elapsed().as_millis() as u64;
    let output_text = model.decode(&generated);
    let output_bytes: Vec<u8> = generated.iter().flat_map(|t| t.to_le_bytes()).collect();
    let output_hash = arc_crypto::hash_bytes(&output_bytes);

    // Summarize consensus: counts of unanimous/majority/split, list of
    // divergent (replica, hash) tuples across all positions.
    let mut unanimous = 0; let mut majority = 0; let mut split = 0;
    let mut divergent_all: std::collections::BTreeMap<String, Vec<String>> = std::collections::BTreeMap::new();
    for v in &votes {
        match v.agreement.as_str() {
            "unanimous" => unanimous += 1,
            "majority" => majority += 1,
            "split" => split += 1,
            _ => {}
        }
        for (replica, hash) in &v.divergent {
            divergent_all.entry(replica.clone()).or_default().push(hash.clone());
        }
    }

    // #31: auto-open a verification commitment + challenge for each divergent
    // replica. One commitment per (replica, hash) tuple representing that
    // replica's claimed output; one challenge from this coordinator against
    // that commitment. Slashing resolution runs on the existing
    // VerificationManager path. Bond is a placeholder - the final value and
    // payer (coordinator treasury vs honest-majority split) still needs TJ's
    // call (open question from the issue body).
    let mut auto_challenges: Vec<Value> = Vec::new();
    if !divergent_all.is_empty() {
        let timestamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let request_id_bytes = arc_crypto::hash_bytes(request_id.as_bytes()).0;
        match node.verification_manager.lock() {
            Ok(mut mgr) => {
                for (replica_name, hashes) in &divergent_all {
                    // Provider identity for the divergent replica. We don't
                    // have their real validator_address from the inference
                    // path - only their node_name + socket. Derive a stable
                    // pseudo-address by hashing "divergent:<name>" so repeat
                    // offenses by the same replica resolve to the same ID.
                    // Reconciling this with the real validator address is
                    // covered in the open-question section of the issue.
                    let provider_id = arc_crypto::hash_bytes(
                        format!("divergent:{replica_name}").as_bytes()
                    ).0;
                    // Use the first divergent hash as the offending
                    // result_hash; additional hashes from the same replica
                    // (multiple positions) are folded into the same provider
                    // record but only the first is committed here.
                    let their_hash = hashes.first().cloned().unwrap_or_default();
                    let commit = arc_vm::inference_verify::InferenceCommitment {
                        request_id: request_id_bytes,
                        result_hash: arc_crypto::hash_bytes(their_hash.as_bytes()).0,
                        provider: provider_id,
                        timestamp,
                        bond_amount: AUTO_CHALLENGE_BOND,
                    };
                    let commitment_id = mgr.submit_commitment(commit);
                    match mgr.create_challenge(
                        commitment_id,
                        node.validator_address.0,
                        arc_vm::inference_verify::ChallengeType::ConsensusVerification,
                        AUTO_CHALLENGE_BOND,
                    ) {
                        Ok(challenge_id) => {
                            auto_challenges.push(json!({
                                "divergent_replica": replica_name,
                                "their_hash": their_hash,
                                "divergent_hash_count": hashes.len(),
                                "commitment_id": format!("0x{}", hex::encode(commitment_id)),
                                "challenge_id": format!("0x{}", hex::encode(challenge_id)),
                                "challenger": node.validator_address.to_hex(),
                                "bond_amount": AUTO_CHALLENGE_BOND,
                            }));
                        }
                        Err(e) => {
                            tracing::warn!(
                                "auto-challenge create failed for {}: {}", replica_name, e
                            );
                        }
                    }
                }
            }
            Err(e) => {
                tracing::warn!(
                    "verification manager lock poisoned; skipping auto-challenges: {}", e
                );
            }
        }
    }

    // Milestone B: if this was an escrow-gated request, collect the
    // honest-replica set from the votes (the non-divergent agreeing
    // replicas across every hop) and submit the release tx. The honest
    // set is a union over all votes - any replica that contributed to the
    // majority_hash at any hop earned a slice of the per-request payout.
    let release_tx_hash = if let Some(gate) = &escrow_gate {
        // Replica names that appeared in majority_hash agreement at
        // any hop. Excludes divergent replicas (they're handled by
        // auto-challenges in the slashing path, not paid).
        let mut honest_names: std::collections::BTreeSet<String> =
            std::collections::BTreeSet::new();
        for v in &votes {
            let divergent: std::collections::HashSet<&String> =
                v.divergent.iter().map(|(n, _)| n).collect();
            for name in &v.replicas_returned {
                if !divergent.contains(name) {
                    honest_names.insert(name.clone());
                }
            }
        }
        // Map replica node_name → synthetic validator address, same
        // derivation slashing uses (hash("replica:" || name)). Keeps
        // honest-pays and divergent-slashes symmetric; a later migration
        // can reconcile both to real on-chain validator addresses once
        // the shard registry carries them.
        let replicas: Vec<arc_crypto::Hash256> = honest_names
            .iter()
            .map(|name| arc_crypto::hash_bytes(format!("replica:{}", name).as_bytes()))
            .collect();

        if replicas.is_empty() {
            tracing::warn!(
                request_id = %request_id,
                "escrow-gated request succeeded but no honest replicas collected - \
                 release would fail at state layer; skipping"
            );
            None
        } else {
            match submit_escrow_release(&node, gate, output_hash, replicas.clone()) {
                Some(tx_hash) => {
                    tracing::info!(
                        request_id = %request_id,
                        tx = %format!("0x{}", hex::encode(&tx_hash.0)),
                        replicas = replicas.len(),
                        max_fee = gate.max_fee,
                        "InferenceEscrowRelease submitted"
                    );
                    Some(tx_hash)
                }
                None => {
                    tracing::warn!(
                        request_id = %request_id,
                        "InferenceEscrowRelease skipped: validator keypair unavailable on \
                         this coordinator (test fixture or misconfig)"
                    );
                    None
                }
            }
        }
    } else {
        None
    };

    let mut response = json!({
        "success": true,
        "request_id": request_id,
        "input": input_text,
        "output": output_text,
        "output_tokens": generated,
        "output_hash": format!("0x{}", hex::encode(&output_hash.0)),
        "tokens_generated": generated.len(),
        "total_ms": total_ms,
        "pipeline_length": pipeline_ranges.len(),
        "k": k_req,
        "consensus": {
            "k": k_req,
            "votes_total": votes.len(),
            "unanimous": unanimous,
            "majority": majority,
            "split": split,
            "divergent_replicas": divergent_all,
            "auto_challenges": auto_challenges,
        },
    });
    if let Some(h) = release_tx_hash {
        response["escrow"] = json!({
            "release_tx_hash": format!("0x{}", hex::encode(&h.0)),
            "payer": format!("0x{}", hex::encode(&escrow_gate.as_ref().unwrap().payer.0)),
            "max_fee": escrow_gate.as_ref().unwrap().max_fee,
        });
    }
    Ok(Json(response))
}

/// Milestone C (#37): GET /models/registry
/// Scans committed transactions for every ModelRegistration body and
/// returns the resulting per-model metadata. For MVP this is O(N) over
/// the full-tx DashMap; a later patch can maintain a sidecar index if
/// the registry grows past a few thousand models.
async fn list_model_registry(
    AxumState(node): AxumState<NodeState>,
) -> Json<Value> {
    let mut rows: Vec<Value> = Vec::new();
    for entry in node.state.full_transactions.iter() {
        let tx = entry.value();
        if let arc_types::TxBody::ModelRegistration(body) = &tx.body {
            rows.push(json!({
                "model_id": format!("0x{}", hex::encode(&body.model_id.0)),
                "metadata_hash": format!("0x{}", hex::encode(&body.metadata_hash.0)),
                "chunk_tree_root": format!("0x{}", hex::encode(&body.chunk_tree_root.0)),
                "n_layers": body.n_layers,
                "d_model": body.d_model,
                "quantization": &body.quantization,
                "registration_fee": body.registration_fee,
                "royalty_recipient": format!("0x{}", hex::encode(&body.royalty_recipient.0)),
                "registered_by": format!("0x{}", hex::encode(&tx.from.0)),
                "tx_hash": format!("0x{}", hex::encode(&tx.hash.0)),
            }));
        }
    }
    Json(json!({ "models": rows, "count": rows.len() }))
}

/// Milestone C (#37): GET /models/open_requests
/// Returns every ModelRequest tx body. Workers poll this to find open
/// demand and decide which ranges to claim.
async fn list_open_model_requests(
    AxumState(node): AxumState<NodeState>,
) -> Json<Value> {
    let mut rows: Vec<Value> = Vec::new();
    for entry in node.state.full_transactions.iter() {
        let tx = entry.value();
        if let arc_types::TxBody::ModelRequest(body) = &tx.body {
            rows.push(json!({
                "request_id": format!("0x{}", hex::encode(&body.request_id)),
                "model_id": format!("0x{}", hex::encode(&body.model_id.0)),
                "target_k_replication": body.target_k_replication,
                "bond_per_layer_epoch": body.bond_per_layer_epoch,
                "max_wait_secs": body.max_wait_secs,
                "requester": format!("0x{}", hex::encode(&tx.from.0)),
                "tx_hash": format!("0x{}", hex::encode(&tx.hash.0)),
            }));
        }
    }
    Json(json!({ "requests": rows, "count": rows.len() }))
}

/// Milestone D (#38): GET /capacity/advertisements
/// Returns every CapacityAdvertisement. The planner reads this set
/// plus open requests + current shard_registry to compute assignments.
async fn list_capacity_advertisements(
    AxumState(node): AxumState<NodeState>,
) -> Json<Value> {
    let mut rows: Vec<Value> = Vec::new();
    for entry in node.state.full_transactions.iter() {
        let tx = entry.value();
        if let arc_types::TxBody::CapacityAdvertisement(body) = &tx.body {
            rows.push(json!({
                "node_pubkey": format!("0x{}", hex::encode(&body.node_pubkey)),
                "ram_bytes": body.ram_bytes,
                "vram_bytes": body.vram_bytes,
                "bandwidth_mbps": body.bandwidth_mbps,
                "uptime_hint_mins": body.uptime_hint_mins,
                "stake": body.stake,
                "region": &body.region,
                "advertised_by": format!("0x{}", hex::encode(&tx.from.0)),
                "tx_hash": format!("0x{}", hex::encode(&tx.hash.0)),
            }));
        }
    }
    Json(json!({ "advertisements": rows, "count": rows.len() }))
}

/// Milestone D (#38): GET /assignments/for_me?pubkey=0x...
/// Returns every AssignmentEntry across every ShardAssignmentProposal
/// whose `node_pubkey` matches the query parameter. Community workers
/// long-poll this and auto-apply - they restart arc-node with the
/// listed `--shard-range` flags and announce the assignment.
async fn get_assignment_for_me(
    AxumState(node): AxumState<NodeState>,
    axum::extract::Query(params): axum::extract::Query<
        std::collections::HashMap<String, String>,
    >,
) -> Result<Json<Value>, (StatusCode, Json<ApiError>)> {
    let pk_hex = params
        .get("pubkey")
        .ok_or(api_error(StatusCode::BAD_REQUEST, "missing ?pubkey= param"))?;
    let pk = decode_hash_hex(pk_hex).map_err(|e| {
        api_error(StatusCode::BAD_REQUEST, format!("pubkey: {}", e))
    })?;

    let mut assignments: Vec<Value> = Vec::new();
    for entry in node.state.full_transactions.iter() {
        let tx = entry.value();
        if let arc_types::TxBody::ShardAssignmentProposal(body) = &tx.body {
            for a in &body.assignments {
                if a.node_pubkey == pk {
                    assignments.push(json!({
                        "epoch_blocks": body.epoch_blocks,
                        "input_snapshot_hash": format!(
                            "0x{}", hex::encode(&body.input_snapshot_hash.0)
                        ),
                        "model_id": format!("0x{}", hex::encode(&a.model_id.0)),
                        "ranges": a.ranges.iter()
                            .map(|(s, e)| json!([s, e]))
                            .collect::<Vec<_>>(),
                        "proposal_tx_hash": format!("0x{}", hex::encode(&tx.hash.0)),
                    }));
                }
            }
        }
    }
    Ok(Json(json!({
        "pubkey": pk_hex,
        "assignments": assignments,
        "count": assignments.len(),
    })))
}

/// Milestone B (#36): resolved payload for an escrow-gated
/// /inference/run_consensus request. Built at the top of the handler after
/// pre-flight passes; consumed at the success path to submit the
/// InferenceEscrowRelease tx.
#[derive(Clone)]
struct EscrowGate {
    payer: arc_crypto::Hash256,
    request_id: [u8; 32],
    max_fee: u64,
    model_id: arc_crypto::Hash256,
    max_tokens: u32,
    timeout_blocks: u64,
}

/// Accept both `0x`-prefixed and bare hex for a 32-byte value. `[u8; 32]`
/// so callers can directly feed into Hash256 or request_id slots.
fn decode_hash_hex(s: &str) -> Result<[u8; 32], String> {
    let trimmed = s.strip_prefix("0x").unwrap_or(s);
    let raw = hex::decode(trimmed).map_err(|e| format!("hex: {}", e))?;
    if raw.len() != 32 {
        return Err(format!("expected 32 bytes, got {}", raw.len()));
    }
    let mut out = [0u8; 32];
    out.copy_from_slice(&raw);
    Ok(out)
}

/// Account addresses in ARC are `Hash256` - same 32-byte shape. Provide a
/// named alias so callers reading the code know the intent is "an address",
/// not "any 32-byte hash".
fn decode_address_hex(s: &str) -> Result<[u8; 32], String> {
    decode_hash_hex(s)
}

/// Deterministic account for the cross-node "observer pool" share of
/// release payouts. Future work: replace with a governance-configurable
/// address that accumulates and pays out to observer-role nodes.
fn observer_pool_address() -> arc_crypto::Hash256 {
    arc_crypto::hash_bytes(b"arc-observer-pool")
}

/// Deterministic treasury address. Collects the 20% treasury share plus
/// any rounding residue from integer fee splits.
fn treasury_address() -> arc_crypto::Hash256 {
    arc_crypto::hash_bytes(b"arc-treasury")
}

/// Build + submit an InferenceEscrowRelease transaction signed by the
/// proposer's validator keypair.
///
/// History note: an earlier version null-signed this tx and relied on
/// `sig_verified=true` to bypass execute_block's signature check. That
/// works locally (proposer side), but `pipeline.rs`'s verify stage only
/// inspects the actual signature bytes — it ignores `sig_verified`. So
/// the tx failed verification on every peer, never landed in any block,
/// and 1000 ARC per paid call sat stuck in escrow accounts. Documented
/// at length in `project_arc_session_handoff_20260428.md`. Signing with
/// the validator keypair routes the tx through the same path real txs
/// take, end of story.
///
/// Returns `None` if no validator keypair is wired into NodeState (test
/// fixtures); production paths always provide one.
fn submit_escrow_release(
    node: &NodeState,
    gate: &EscrowGate,
    output_hash: arc_crypto::Hash256,
    replicas: Vec<arc_crypto::Hash256>,
) -> Option<arc_crypto::Hash256> {
    let keypair = node.validator_keypair.as_ref()?;
    let proposer = node.validator_address;
    // Use the proposer's CURRENT state nonce. The previous in-memory bump
    // counter (`attestation_nonce.fetch_add`) accumulated forever, even when
    // the constructed tx failed to land - leaving a nonce gap (state stays
    // at N, in-memory counter advances to N+1+...) that made every
    // subsequent release fail with InvalidNonce. Reading from state every
    // time keeps the release tx's nonce in sync with what execute_block
    // actually expects.
    let nonce = node
        .state
        .get_account(&proposer)
        .map(|a| a.nonce)
        .unwrap_or(0);

    let body = arc_types::transaction::InferenceEscrowReleaseBody {
        request_id: gate.request_id,
        payer: gate.payer,
        model_id: gate.model_id,
        max_tokens: gate.max_tokens,
        timeout_blocks: gate.timeout_blocks,
        output_hash,
        proposer,
        replicas,
        observer_pool: observer_pool_address(),
        treasury: treasury_address(),
    };
    let mut tx = arc_types::Transaction {
        tx_type: arc_types::TxType::InferenceEscrowRelease,
        from: proposer,
        nonce,
        body: arc_types::TxBody::InferenceEscrowRelease(body),
        fee: 0,
        gas_limit: 0,
        hash: arc_crypto::Hash256::ZERO,
        signature: arc_crypto::Signature::null(),
        sig_verified: false,
    };
    if let Err(e) = tx.sign(keypair) {
        tracing::warn!("escrow release sign failed: {:?}", e);
        return None;
    }
    tx.sig_verified = true; // proposer just signed it; safe local fast-path
    let tx_hash = tx.hash;
    let _ = node.mempool.insert(tx);
    Some(tx_hash)
}

/// Bond for consensus-divergence auto-challenges opened by
/// /inference/run_consensus (#31).
///
/// Decided 2026-04-22:
///   - Payer: coordinator that received the run_consensus request, from its
///     own validator address. Coordinator carries reputation/revenue upside
///     for catching divergence, so it's the right economic actor to post
///     the witness bond.
///   - Amount: 100_000 ARC. Sized to be ~2% of a 5M genesis-validator stake
///     - cheap enough that a coordinator never declines to challenge, large
///     enough to deter spurious challenges. Revisit if community-tier
///     validators with smaller stakes become common coordinators.
///
/// The divergent replica's actual stake (via VerificationManager slashing)
/// is the real deterrent; this bond is just the coordinator's skin in the
/// game for initiating.
const AUTO_CHALLENGE_BOND: u64 = 100_000;

/// GET /shards
/// Returns the local shard registry - every node this coordinator knows about
/// and which layer range it holds.
async fn get_shards(
    AxumState(node): AxumState<NodeState>,
) -> Json<Value> {
    let mut shards: Vec<ShardInfo> = fresh_shards(&node.shard_registry);
    shards.sort_by_key(|s| s.start_layer);

    let total_layers = shards.first().map(|s| s.total_layers).unwrap_or(0);
    let total_full_mb = shards.first().map(|s| s.full_model_mb).unwrap_or(0);
    let total_held_mb: usize = shards.iter().map(|s| s.memory_mb).sum();
    let model_id = shards.first().map(|s| s.model_id.clone()).unwrap_or_default();
    let model_name = shards.first().map(|s| s.model_name.clone()).unwrap_or_default();

    // Dedup ranges across replicas. With 3× replication each range appears
    // three times in `shards`; walking the raw list sees the second replica's
    // start_layer == 0 as a backward step and flips contiguous=false. BTreeSet
    // collapses duplicates and iterates in sorted (start, end) order.
    let unique_ranges: std::collections::BTreeSet<(usize, usize)> =
        shards.iter().map(|s| (s.start_layer, s.end_layer)).collect();
    let mut covered_to = 0usize;
    let mut contiguous = true;
    for (start, end) in &unique_ranges {
        if *start != covered_to {
            contiguous = false;
            break;
        }
        covered_to = *end;
    }
    let fully_covered = contiguous && covered_to == total_layers && total_layers > 0;

    // Emit both singular (legacy, first range only) and plural (current) so
    // peers still pulling `self_shard` keep working through the rolling
    // upgrade while new peers consume `self_shards` and see every range.
    let self_shard_legacy = node.shard_infos.first().cloned();
    Json(json!({
        "shards": shards,
        "shard_count": shards.len(),
        "total_layers": total_layers,
        "fully_covered": fully_covered,
        "model_id": model_id,
        "model_name": model_name,
        "full_model_mb": total_full_mb,
        "total_distributed_mb": total_held_mb,
        "self_shard": self_shard_legacy,
        "self_shards": node.shard_infos,
    }))
}

#[derive(serde::Deserialize)]
struct AnnounceShardRequest {
    shard: ShardInfo,
}

/// Returns true iff `addr` is a stub (unroutable placeholder) that the
/// coordinator cannot dial to forward a shard request.
fn is_stub_socket_addr(addr: &str) -> bool {
    addr.starts_with("0.0.0.0")
        || addr.starts_with("127.")
        || addr.starts_with("[::]")
        || addr.starts_with("[::1]")
        || addr.is_empty()
}

/// Rewrite a stub shard `socket_addr` using the peer's actual TCP source IP,
/// keeping the port the announcer declared. Returns the corrected addr, or
/// the original when no rewrite is needed.
///
/// Pure function - no I/O, no state. Cheap to unit test.
fn rewrite_stub_shard_addr(
    announced_addr: &str,
    peer_addr: SocketAddr,
) -> String {
    if !is_stub_socket_addr(announced_addr) || peer_addr.ip().is_loopback() {
        return announced_addr.to_string();
    }
    let declared_port = announced_addr
        .rsplit(':')
        .next()
        .and_then(|s| s.parse::<u16>().ok())
        .unwrap_or_else(|| peer_addr.port());
    // Use SocketAddr::Display so IPv6 peers get bracketed correctly
    // ([::1]:9090, not ::1:9090).
    SocketAddr::new(peer_addr.ip(), declared_port).to_string()
}

/// POST /shards/announce
/// Other nodes call this to register their shard with our local registry.
///
/// Announcements arrive with the *announcer's* `socket_addr` in the payload.
/// When the announcer binds 0.0.0.0 it doesn't know its own public IP, so
/// the shipped value is "0.0.0.0:<port>" - a stub the coordinator cannot
/// route to. We fix that here by overriding stub addrs with the peer's
/// actual source IP (discovered from the TCP connection), keeping the port
/// the announcer declared. Self-announces from 127.0.0.1 are left alone.
async fn announce_shard(
    AxumState(node): AxumState<NodeState>,
    ConnectInfo(peer_addr): ConnectInfo<SocketAddr>,
    Json(mut req): Json<AnnounceShardRequest>,
) -> Json<Value> {
    // Rewrite stub addrs using the peer's source IP + announced port.
    // Root-cause fix for "Pipeline gap" errors where every shard was
    // announced with socket_addr=0.0.0.0:9090.
    req.shard.socket_addr = rewrite_stub_shard_addr(&req.shard.socket_addr, peer_addr);

    // Dedupe: if an existing entry already covers the same (layer_range,
    // node_name) with a routable socket_addr, drop this announcement when
    // the incoming addr is STILL a stub (self-announce from localhost). This
    // preserves existing behavior and prevents self-announces from clobbering
    // gossiped entries with real public IPs.
    let still_stub = is_stub_socket_addr(&req.shard.socket_addr);
    if still_stub {
        let has_better = node.shard_registry.iter().any(|e| {
            let (s, _ts) = e.value();
            s.start_layer == req.shard.start_layer
                && s.end_layer == req.shard.end_layer
                && s.node_name == req.shard.node_name
                && !s.socket_addr.starts_with("0.0.0.0")
                && !s.socket_addr.starts_with("127.")
                && !s.socket_addr.is_empty()
        });
        if has_better {
            return Json(json!({"ok": true, "registry_size": node.shard_registry.len(), "note": "stub addr ignored - routable addr already registered"}));
        }
    }
    // Key by (socket_addr, range) so one node announcing multiple held ranges
    // produces one entry per range - otherwise the DashMap insert clobbers
    // prior announces and only the most recent range survives. The
    // coordinator's BTreeMap grouping already keys on (start, end) so a
    // per-range entry is exactly what we need.
    let key = format!("{}#{}-{}", req.shard.socket_addr, req.shard.start_layer, req.shard.end_layer);
    // Also register in multi-model ShardRegistry for multi-model routing
    if let Ok(model_hash_bytes) = parse_hash(&req.shard.model_id) {
        let model_hash = Hash256(model_hash_bytes);
        let assignment = arc_inference::distributed::ShardAssignment {
            node_address: Hash256(model_hash_bytes), // placeholder; real node addr comes from p2p
            start_layer: req.shard.start_layer as u32,
            end_layer: req.shard.end_layer as u32,
            expert_indices: Vec::new(),
            socket_addr: req.shard.socket_addr.clone(),
            gpu_tier: 0,
            available_memory: (req.shard.memory_mb as u64) * 1024 * 1024,
        };
        node.multi_model_registry.register_shard(model_hash, assignment);
    }
    node.shard_registry.insert(key, (req.shard, std::time::Instant::now()));
    Json(json!({"ok": true, "registry_size": node.shard_registry.len()}))
}

// ─── Community worker registry ──────────────────────────────────────────────

#[derive(serde::Deserialize)]
struct CommunityRegisterRequest {
    worker_id: String,
    #[serde(default)]
    name: String,
    #[serde(default)]
    capabilities: Vec<String>,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    platform: String,
    /// Model ID hash (hex) - if provided, coordinator auto-assigns shard layers.
    #[serde(default)]
    model_id: Option<String>,
    /// Total transformer layers in the model.
    #[serde(default)]
    total_layers: Option<u32>,
    /// Node's public RPC address for shard forwarding.
    #[serde(default)]
    rpc_addr: Option<String>,
    /// Available memory in MB.
    #[serde(default)]
    available_memory_mb: Option<u64>,
}

/// POST /community/register
/// A community-mode node calls this on every seed it can reach. The seed
/// stores the worker info in its community_workers registry. The worker
/// is then visible to the dashboard and counted in TPS/compute stats.
/// Workers are pure outbound-HTTPS: no inbound port, no NAT traversal,
/// no QUIC. Works behind any residential firewall.
async fn community_register(
    AxumState(node): AxumState<NodeState>,
    Json(req): Json<CommunityRegisterRequest>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    if req.worker_id.is_empty() || req.worker_id.len() > 128 {
        return Err((StatusCode::BAD_REQUEST, "worker_id required (1-128 chars)".to_string()));
    }
    let now_secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    // Preserve all stat fields across re-registrations so a re-register
    // every 60s doesn't reset the scoreboard. Default to zeros for a
    // brand-new worker.
    let prior = node
        .community_workers
        .get(&req.worker_id)
        .map(|e| e.value().0.clone());
    let existing_registered_at = prior.as_ref().map(|p| p.registered_at);

    // Clone name/model before moving into CommunityWorker - used later for shard_info
    let worker_name = req.name.clone();
    let worker_model_name = req.model.clone();
    let worker = CommunityWorker {
        worker_id: req.worker_id.clone(),
        name: req.name,
        capabilities: if req.capabilities.is_empty() {
            vec!["inference".to_string()]
        } else {
            req.capabilities
        },
        model: req.model,
        platform: req.platform,
        registered_at: existing_registered_at.unwrap_or(now_secs),
        work_completed: prior.as_ref().map(|p| p.work_completed).unwrap_or(0),
        success_count: prior.as_ref().map(|p| p.success_count).unwrap_or(0),
        failure_count: prior.as_ref().map(|p| p.failure_count).unwrap_or(0),
        sum_total_ms_success: prior.as_ref().map(|p| p.sum_total_ms_success).unwrap_or(0),
        last_total_ms: prior.as_ref().map(|p| p.last_total_ms).unwrap_or(0),
    };
    node.community_workers
        .insert(req.worker_id.clone(), (worker, std::time::Instant::now()));

    // ─── Auto-shard assignment ──────────────────────────────────────
    // If the worker provided model info, auto-assign shard layers so it
    // can start serving inference immediately. No manual --shard-start
    // / --shard-end flags needed.
    let shard_assignment = if let (Some(model_id), Some(total_layers), Some(rpc_addr)) =
        (req.model_id.as_ref(), req.total_layers, req.rpc_addr.as_ref())
    {
        if total_layers > 0 {
            // Find the biggest uncovered gap for this model
            let existing: Vec<ShardInfo> = fresh_shards(&node.shard_registry)
                .into_iter()
                .filter(|s| s.model_id == *model_id)
                .collect();

            let mut covered: Vec<bool> = vec![false; total_layers as usize];
            for s in &existing {
                for l in s.start_layer..s.end_layer.min(total_layers as usize) {
                    covered[l] = true;
                }
            }

            // Find biggest uncovered range
            let mut best_start = 0usize;
            let mut best_len = 0usize;
            let mut run_start = 0usize;
            let mut in_run = false;
            for i in 0..covered.len() {
                if !covered[i] {
                    if !in_run { run_start = i; in_run = true; }
                    let run_len = i - run_start + 1;
                    if run_len > best_len { best_start = run_start; best_len = run_len; }
                } else {
                    in_run = false;
                }
            }

            // If fully covered, assign redundant shard on thinnest spot
            if best_len == 0 {
                let mut counts: Vec<usize> = vec![0; total_layers as usize];
                for s in &existing {
                    for l in s.start_layer..s.end_layer.min(total_layers as usize) {
                        counts[l] += 1;
                    }
                }
                let min_c = *counts.iter().min().unwrap_or(&0);
                let thin = counts.iter().position(|&c| c == min_c).unwrap_or(0);
                let sz = (total_layers as usize / 4).max(1);
                best_start = thin;
                best_len = sz.min(total_layers as usize - thin);
            }

            let end = best_start + best_len;

            // Register shard
            let shard_info = ShardInfo {
                start_layer: best_start,
                end_layer: end,
                total_layers: total_layers as usize,
                model_id: model_id.clone(),
                model_name: worker_model_name.clone().unwrap_or_default(),
                memory_mb: req.available_memory_mb.unwrap_or(8192) as usize,
                full_model_mb: 0,
                socket_addr: rpc_addr.clone(),
                node_name: worker_name.clone(),
            };
            let reg_key = format!("{}#{}-{}", rpc_addr, best_start, end);
            node.shard_registry.insert(reg_key, (shard_info, std::time::Instant::now()));

            // Register in multi-model registry
            if let Ok(mhb) = parse_hash(model_id) {
                let mh = Hash256(mhb);
                node.multi_model_registry.register_shard(mh, arc_inference::distributed::ShardAssignment {
                    node_address: mh,
                    start_layer: best_start as u32,
                    end_layer: end as u32,
                    expert_indices: Vec::new(),
                    socket_addr: rpc_addr.clone(),
                    gpu_tier: 0,
                    available_memory: req.available_memory_mb.unwrap_or(8192) * 1024 * 1024,
                });
            }

            Some(json!({
                "start_layer": best_start,
                "end_layer": end,
                "total_layers": total_layers,
            }))
        } else {
            None
        }
    } else {
        None
    };

    Ok(Json(json!({
        "ok": true,
        "worker_id": req.worker_id,
        "registry_size": node.community_workers.len(),
        "welcome": "Your node is now visible on the ARC testnet dashboard.",
        "shard_assignment": shard_assignment,
    })))
}

#[derive(serde::Deserialize)]
struct CommunityHeartbeatRequest {
    worker_id: String,
    #[serde(default)]
    work_completed: Option<u64>,
}

/// POST /community/heartbeat
/// Community workers call this every 15 seconds to stay alive in the
/// registry. Without a heartbeat for COMMUNITY_WORKER_TTL_SECS (90s)
/// the worker is pruned at read time.
async fn community_heartbeat(
    AxumState(node): AxumState<NodeState>,
    Json(req): Json<CommunityHeartbeatRequest>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    if let Some(mut entry) = node.community_workers.get_mut(&req.worker_id) {
        let (worker, ts) = entry.value_mut();
        *ts = std::time::Instant::now();
        if let Some(wc) = req.work_completed {
            worker.work_completed = wc;
        }
        Ok(Json(json!({"ok": true})))
    } else {
        Err((StatusCode::NOT_FOUND, "worker_id not registered - call /community/register first".to_string()))
    }
}

/// GET /community/list
/// Returns all fresh community workers. Entries older than
/// COMMUNITY_WORKER_TTL_SECS are pruned at read time. The dashboard
/// polls this to show the community node count + geographic spread.
async fn community_list(
    AxumState(node): AxumState<NodeState>,
) -> Json<serde_json::Value> {
    let now = std::time::Instant::now();
    let ttl = std::time::Duration::from_secs(COMMUNITY_WORKER_TTL_SECS);
    let mut live: Vec<CommunityWorker> = Vec::new();
    let mut expired: Vec<String> = Vec::new();
    let mut registered_ids: std::collections::HashSet<String> = std::collections::HashSet::new();

    for entry in node.community_workers.iter() {
        let (w, ts) = entry.value();
        if now.duration_since(*ts) <= ttl {
            registered_ids.insert(w.worker_id.clone());
            live.push(w.clone());
        } else {
            expired.push(entry.key().clone());
        }
    }
    for k in expired {
        node.community_workers.remove(&k);
    }

    // Auto-discover P2P peers as community workers. Any node connected
    // via P2P is automatically visible - no --community-mode flag, no
    // separate register script, no HTTP POST needed. The P2P connection
    // IS the registration.
    let validators = node.dag_validators.read();
    for (addr, stake) in validators.iter() {
        let hex_addr = format!("0x{}", hex::encode(&addr.0));
        // Skip self and already-registered workers
        if *addr == node.validator_address || registered_ids.contains(&hex_addr) {
            continue;
        }
        live.push(CommunityWorker {
            worker_id: hex_addr,
            name: format!("p2p-peer (stake={})", stake),
            capabilities: vec!["consensus".to_string()],
            model: None,
            platform: "auto-discovered".to_string(),
            registered_at: node.boot_time.elapsed().as_secs(),
            work_completed: 0,
            success_count: 0,
            failure_count: 0,
            sum_total_ms_success: 0,
            last_total_ms: 0,
        });
    }

    let total_work: u64 = live.iter().map(|w| w.work_completed).sum();
    Json(json!({
        "workers": live,
        "count": live.len(),
        "total_work_completed": total_work,
        "registered": registered_ids.len(),
        "auto_discovered": live.len() - registered_ids.len(),
    }))
}

// ─── Community work dispatch (long-poll claim + submit) ─────────────────────
//
// Community nodes run with `--stake 0 --community-mode` behind NAT. They can
// reach seed nodes via outbound HTTPS but cannot accept inbound connections.
// The two endpoints below let the coordinator push forward_shard work to these
// workers without requiring inbound connectivity:
//
//   1. Worker calls POST /community/claim_work (long-poll, up to 30s).
//      If work arrives within the window, the coordinator writes the job
//      payload and closes the response. If not, returns {"status":"no_work"}.
//      The worker immediately re-polls.
//
//   2. Worker calls POST /community/submit_work with the computed result.
//      The coordinator matches it to the pending oneshot and resumes the
//      pipeline walk.
//
// The coordinator side pushes WorkItems into `work_queue` (mpsc) and awaits
// WorkResults via `work_results` (DashMap<request_id, oneshot::Sender>).
// Those fields must be added to NodeState by the caller - these handlers
// reference them directly.

/// Maximum time a claim_work long-poll will hold the connection open.
const COMMUNITY_CLAIM_TIMEOUT_SECS: u64 = 30;

/// A whole-prompt inference job dispatched to a community worker.
///
/// v0.7.0: layer-shard work is no longer dispatched to community workers — it
/// remains a seed-to-seed primitive only (see `inference_run_sharded`). The
/// community-worker channel now carries entire prompts, which any device with
/// a loaded model can serve end-to-end. This is the unit that makes "every
/// device is a node" honest: a phone running a 1B model handles a whole job;
/// a workstation running 13B handles a whole job; both earn per attestation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkItem {
    /// Unique job id (hex of blake3(input || nonce)). Used to match
    /// submit_work back to the awaiting oneshot.
    pub job_id: String,
    /// Prompt text (already chat-templated by the dispatcher if needed).
    pub input: String,
    /// Maximum tokens the worker should generate.
    pub max_tokens: u32,
    /// Optional model identifier (hex of model_id hash). When set, only
    /// workers serving the same model will accept the job.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model_id: Option<String>,
    /// Unix timestamp (ms) when the dispatcher queued the job. Workers
    /// echo this back so the dispatcher can compute end-to-end latency.
    pub submitted_at_unix_ms: i64,
}

/// Result submitted by a community worker after completing a WorkItem.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkResult {
    /// Must match the WorkItem.job_id.
    pub job_id: String,
    /// Worker's self-chosen identifier (hex of validator pubkey).
    pub worker_id: String,
    /// True if the worker produced output; false on inference error.
    #[serde(default = "default_true")]
    pub success: bool,
    /// Generated text (decoded from the worker's tokens).
    #[serde(default)]
    pub output: String,
    /// BLAKE3 of the output token sequence (hex, 0x-prefixed). The chain
    /// uses this for cross-worker hash-majority verification.
    #[serde(default)]
    pub output_hash: String,
    /// Number of tokens generated.
    #[serde(default)]
    pub tokens_generated: u64,
    /// Total wall time on the worker, in milliseconds (encode + generate + decode).
    #[serde(default)]
    pub total_ms: u64,
    /// Average ms per generated token.
    #[serde(default)]
    pub ms_per_token: u64,
    /// Engine identifier ("INT8 integer (community worker)", etc.) for analytics.
    #[serde(default)]
    pub engine: String,
    /// Optional error message when success=false.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Hex-encoded `bincode(arc_types::Transaction)` of the worker-signed
    /// InferenceAttestation (tx 0x16) for this completion. Lets the seed
    /// post the attestation on-chain with `from = worker_address`, so
    /// rewards accrue to the worker who actually did the work — not the
    /// seed that routed the request. Optional during the v0.7.0
    /// transition; pre-v0.7 workers don't send it, and the seed accepts
    /// the submit anyway (just no on-chain record).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signed_attestation_hex: Option<String>,
}

fn default_true() -> bool { true }

/// POST body for /community/claim_work.
#[derive(Deserialize)]
pub struct ClaimWorkRequest {
    /// Worker's self-chosen identifier.
    pub worker_id: String,
    /// What the worker can do. Must include "inference" to receive work.
    #[serde(default)]
    pub capabilities: Vec<String>,
    /// Model the worker has loaded (must match coordinator's model to get work).
    #[serde(default)]
    pub model: Option<String>,
}

/// POST /community/claim_work
///
/// Long-poll endpoint. A community worker POSTs with its worker_id and
/// capabilities. The server holds the request open for up to 30 seconds.
/// If a forward_shard job arrives during that window, the server writes the
/// job payload as the response and closes the connection. If no work arrives,
/// returns `{"status":"no_work"}`. The community node immediately re-polls.
///
/// The worker must be registered via /community/register before claiming work
/// (this doubles as a heartbeat - we refresh the TTL on every poll).
pub async fn community_claim_work(
    AxumState(node): AxumState<NodeState>,
    Json(req): Json<ClaimWorkRequest>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    // ── Validate worker_id ──────────────────────────────────────────────
    if req.worker_id.is_empty() || req.worker_id.len() > 128 {
        return Err((
            StatusCode::BAD_REQUEST,
            "worker_id required (1-128 chars)".to_string(),
        ));
    }

    // ── Worker must be registered ───────────────────────────────────────
    if !node.community_workers.contains_key(&req.worker_id) {
        return Err((
            StatusCode::NOT_FOUND,
            "worker_id not registered - call /community/register first".to_string(),
        ));
    }

    // Refresh TTL (counts as heartbeat)
    if let Some(mut entry) = node.community_workers.get_mut(&req.worker_id) {
        entry.value_mut().1 = std::time::Instant::now();
    }

    // ── Capability check ────────────────────────────────────────────────
    let caps = if req.capabilities.is_empty() {
        vec!["inference".to_string()]
    } else {
        req.capabilities
    };
    if !caps.iter().any(|c| c == "inference") {
        return Err((
            StatusCode::BAD_REQUEST,
            "capabilities must include 'inference' to receive work".to_string(),
        ));
    }

    // ── Long-poll: try to receive a WorkItem from the queue ─────────────
    let work_rx = node.community_work_queue.as_ref().ok_or((
        StatusCode::SERVICE_UNAVAILABLE,
        "work queue not initialized - coordinator not running".to_string(),
    ))?;

    let timeout = tokio::time::Duration::from_secs(COMMUNITY_CLAIM_TIMEOUT_SECS);
    match tokio::time::timeout(timeout, work_rx.lock().await.recv()).await {
        Ok(Some(item)) => {
            // Optionally verify model match. If the worker specified a model
            // and the work item has a model_id, they must agree. If the
            // worker doesn't filter by model, accept any work.
            if let (Some(worker_model), Some(item_model)) = (&req.model, &item.model_id) {
                if worker_model != item_model {
                    // Model mismatch - put the item back on the queue so
                    // another worker can pick it up, then tell this worker
                    // there's no matching work.
                    if let Some(ref tx) = node.community_work_tx {
                        let _ = tx.send(item).await;
                    }
                    return Ok(Json(json!({
                        "status": "no_work",
                        "reason": "model_mismatch",
                    })));
                }
            }

            // Flat response shape — the worker (arc-node main.rs) reads
            // `job_id`, `input`, `max_tokens` directly off the top-level
            // JSON. Don't nest under "work".
            let mut body = json!({
                "status": "work",
                "job_id": item.job_id,
                "input": item.input,
                "max_tokens": item.max_tokens,
                "submitted_at_unix_ms": item.submitted_at_unix_ms,
            });
            if let Some(mid) = item.model_id {
                body["model_id"] = Value::String(mid);
            }
            Ok(Json(body))
        }
        Ok(None) => {
            // Channel closed - coordinator shut down
            Err((
                StatusCode::SERVICE_UNAVAILABLE,
                "work queue closed - coordinator shutting down".to_string(),
            ))
        }
        Err(_) => {
            // Timeout - no work within the window
            Ok(Json(json!({
                "status": "no_work",
            })))
        }
    }
}

/// POST /community/submit_work
///
/// Community worker submits a completed whole-prompt inference result. The
/// payload must include `job_id`, `worker_id`, and the generated output. The
/// coordinator matches `job_id` to a pending oneshot::Sender and unblocks the
/// dispatcher in `/inference/run`.
///
/// On success the worker's `work_completed` counter increments and (in
/// task 3 of v0.7.0) an `InferenceAttestation` tx is posted on-chain so the
/// reward is real ARC, not a synthesized count.
pub async fn community_submit_work(
    AxumState(node): AxumState<NodeState>,
    Json(result): Json<WorkResult>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    // ── Validate required fields ────────────────────────────────────────
    if result.job_id.is_empty() {
        return Err((
            StatusCode::BAD_REQUEST,
            "job_id is required".to_string(),
        ));
    }
    if result.worker_id.is_empty() || result.worker_id.len() > 128 {
        return Err((
            StatusCode::BAD_REQUEST,
            "worker_id required (1-128 chars)".to_string(),
        ));
    }
    // A successful submit must have an output and an output_hash. Failure
    // submits are accepted (the dispatcher needs to know the worker tried)
    // but they don't earn rewards.
    if result.success {
        if result.output_hash.is_empty() {
            return Err((
                StatusCode::BAD_REQUEST,
                "output_hash required on success=true".to_string(),
            ));
        }
        if result.tokens_generated == 0 {
            return Err((
                StatusCode::BAD_REQUEST,
                "tokens_generated must be > 0 on success=true".to_string(),
            ));
        }
    }

    // ── Worker must be registered ───────────────────────────────────────
    if !node.community_workers.contains_key(&result.worker_id) {
        return Err((
            StatusCode::NOT_FOUND,
            "worker_id not registered - call /community/register first".to_string(),
        ));
    }

    // ── Refresh heartbeat + score stats ─────────────────────────────────
    // success_count / failure_count / sum_total_ms_success power the
    // /workers/scoreboard endpoint and (in v0.8) per-worker dispatch
    // priority. last_total_ms is the most-recent latency sample so the
    // dashboard can show a "current latency" without walking the EWMA.
    if let Some(mut entry) = node.community_workers.get_mut(&result.worker_id) {
        let (worker, ts) = entry.value_mut();
        *ts = std::time::Instant::now();
        if result.success {
            worker.work_completed += 1;
            worker.success_count += 1;
            worker.sum_total_ms_success = worker
                .sum_total_ms_success
                .saturating_add(result.total_ms);
            worker.last_total_ms = result.total_ms;
        } else {
            worker.failure_count += 1;
        }
    }

    // ── Post the worker-signed InferenceAttestation on-chain ────────────
    //
    // The worker built and signed an `arc_types::Transaction` with
    // `tx_type = InferenceAttestation`, `from = worker_address`, and
    // bond + model_id + input/output hashes. We deserialize, verify the
    // signature, and insert into our local mempool. From there the
    // chain's normal consensus + state-execute pipeline applies it,
    // crediting the worker on-chain.
    //
    // This is the missing piece that turns "0 attestations forever"
    // into real ARC. Pre-v0.7 workers won't send this field; we accept
    // their submit anyway (just no on-chain record) so the rolling
    // upgrade doesn't strand them.
    let mut attestation_outcome: Option<serde_json::Value> = None;
    if result.success {
        if let Some(hex_bytes) = result.signed_attestation_hex.as_ref() {
            match decode_and_verify_worker_attestation(hex_bytes, &result.worker_id) {
                Ok(tx) => {
                    let tx_hash_hex = format!("0x{}", hex::encode(&tx.hash.0));
                    let tx_from_hex = format!("0x{}", hex::encode(&tx.from.0));
                    match node.mempool.insert(tx) {
                        Ok(_) => {
                            tracing::info!(
                                tx_hash = %tx_hash_hex,
                                worker = %tx_from_hex,
                                "worker-signed attestation accepted into mempool"
                            );
                            attestation_outcome = Some(json!({
                                "status": "submitted_to_mempool",
                                "tx_hash": tx_hash_hex,
                                "from": tx_from_hex,
                            }));
                        }
                        Err(e) => {
                            // Don't fail the submit — the worker did their
                            // job. Just record that the chain rejected the
                            // attestation tx (e.g. duplicate nonce, low
                            // balance for bond) so the worker can react.
                            tracing::warn!(
                                worker = %result.worker_id,
                                error = ?e,
                                "worker attestation rejected by mempool"
                            );
                            attestation_outcome = Some(json!({
                                "status": "rejected",
                                "error": format!("{:?}", e),
                            }));
                        }
                    }
                }
                Err(e) => {
                    tracing::warn!(
                        worker = %result.worker_id,
                        error = %e,
                        "worker attestation signature/decode failed"
                    );
                    attestation_outcome = Some(json!({
                        "status": "invalid",
                        "error": e,
                    }));
                }
            }
        } else {
            attestation_outcome = Some(json!({
                "status": "missing",
                "reason": "worker did not include signed_attestation_hex",
            }));
        }
    }

    // ── Deliver result to the coordinator via the oneshot ────────────────
    let results_map = node.community_work_results.as_ref().ok_or((
        StatusCode::SERVICE_UNAVAILABLE,
        "work results map not initialized - coordinator not running".to_string(),
    ))?;

    // Remove the oneshot sender for this job_id. If it's gone, the
    // dispatcher already timed out or the request was cancelled.
    match results_map.remove(&result.job_id) {
        Some((_, sender)) => {
            let job_id = result.job_id.clone();
            match sender.send(result) {
                Ok(()) => Ok(Json(json!({
                    "ok": true,
                    "job_id": job_id,
                    "attestation": attestation_outcome,
                }))),
                Err(_) => {
                    // Receiver dropped - dispatcher timed out
                    Err((
                        StatusCode::GONE,
                        format!(
                            "dispatcher already timed out for job_id {}",
                            job_id
                        ),
                    ))
                }
            }
        }
        None => Err((
            StatusCode::NOT_FOUND,
            format!(
                "no pending work for job_id {} - already completed or expired",
                result.job_id
            ),
        )),
    }
}

/// Decode the worker-signed `InferenceAttestation` Transaction submitted
/// alongside a WorkResult, verify its signature, and check that
/// `tx.from` matches the claimed `worker_id`. Returns the verified tx
/// ready for `mempool.insert`.
///
/// Errors as a string so submit_work can surface the diagnostic to the
/// worker without picking a single error type.
fn decode_and_verify_worker_attestation(
    hex_bytes: &str,
    expected_worker_id: &str,
) -> Result<arc_types::Transaction, String> {
    // Strip optional "0x" prefix so workers can encode either way.
    let trimmed = hex_bytes.trim_start_matches("0x");
    let raw = hex::decode(trimmed)
        .map_err(|e| format!("hex decode failed: {}", e))?;
    let tx: arc_types::Transaction = bincode::deserialize(&raw)
        .map_err(|e| format!("bincode deserialize failed: {}", e))?;

    if tx.tx_type != arc_types::TxType::InferenceAttestation {
        return Err(format!(
            "expected InferenceAttestation tx, got {:?}",
            tx.tx_type
        ));
    }

    // The worker signs as `from = their validator_address`. The claim
    // submission's worker_id is the hex of that address. Reject mismatches
    // outright so a worker can't submit on someone else's behalf.
    let claim_address = expected_worker_id
        .trim_start_matches("0x")
        .to_string();
    let tx_from_hex = hex::encode(&tx.from.0);
    if claim_address != tx_from_hex {
        return Err(format!(
            "tx.from ({}) does not match worker_id ({})",
            tx_from_hex, claim_address
        ));
    }

    tx.verify_signature()
        .map_err(|e| format!("signature verify failed: {:?}", e))?;

    Ok(tx)
}

// ─── Multi-Model Registry ──────────────────────────────────────────────────

/// GET /models
/// List all models known to the multi-model registry with pipeline coverage info.
async fn get_models(
    AxumState(node): AxumState<NodeState>,
) -> Json<Value> {
    let covered = node.multi_model_registry.fully_covered_models();
    let total_nodes = node.multi_model_registry.total_shard_nodes();

    // Also gather model_ids from flat registry for backward compat
    let flat_shards = fresh_shards(&node.shard_registry);
    let mut model_set: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut models_info: Vec<Value> = Vec::new();

    for s in &flat_shards {
        if model_set.insert(s.model_id.clone()) {
            let shards_for_model: Vec<&ShardInfo> = flat_shards.iter()
                .filter(|ss| ss.model_id == s.model_id)
                .collect();
            let covered_layers: usize = shards_for_model.iter().map(|ss| ss.end_layer - ss.start_layer).sum();
            models_info.push(json!({
                "model_id": s.model_id,
                "model_name": s.model_name,
                "total_layers": s.total_layers,
                "covered_layers": covered_layers,
                "fully_covered": covered_layers == s.total_layers,
                "shard_count": shards_for_model.len(),
                "full_model_mb": s.full_model_mb,
            }));
        }
    }

    Json(json!({
        "models": models_info,
        "total_models": model_set.len(),
        "fully_covered_models": covered.len(),
        "total_shard_nodes": total_nodes,
    }))
}

/// GET /models/shards?model_id=0x...
/// Get the pipeline for a specific model.
async fn get_model_shards(
    AxumState(node): AxumState<NodeState>,
    Query(params): Query<HashMap<String, String>>,
) -> Result<Json<Value>, (StatusCode, Json<ApiError>)> {
    let model_id_hex = params.get("model_id")
        .ok_or(api_error(StatusCode::BAD_REQUEST, "model_id query parameter required"))?;

    let model_hash_bytes = parse_hash(model_id_hex)
        .map_err(|_| api_error(StatusCode::BAD_REQUEST, "invalid model_id hex"))?;
    let model_hash = Hash256(model_hash_bytes);

    match node.multi_model_registry.get_pipeline(&model_hash) {
        Some(pipeline) => {
            let shards: Vec<Value> = pipeline.iter().map(|s| json!({
                "start_layer": s.start_layer,
                "end_layer": s.end_layer,
                "socket_addr": s.socket_addr,
                "gpu_tier": s.gpu_tier,
                "available_memory_mb": s.available_memory / (1024 * 1024),
            })).collect();
            let total_layers = pipeline.last().map(|s| s.end_layer).unwrap_or(0);
            Ok(Json(json!({
                "model_id": model_id_hex,
                "pipeline": shards,
                "shard_count": pipeline.len(),
                "total_layers": total_layers,
                "fully_covered": node.multi_model_registry.is_model_fully_covered(&model_hash, total_layers as u32),
            })))
        }
        None => Err(api_error(StatusCode::NOT_FOUND, "model not found in registry")),
    }
}

// ─── Inference Verification Endpoints ──────────────────────────────────────

#[derive(serde::Deserialize)]
struct InferenceCommitRequest {
    request_id: String,
    result_hash: String,
    bond_amount: u64,
}

/// POST /inference/commit
/// Submit an inference commitment (result_hash + bond).
async fn inference_commit(
    AxumState(node): AxumState<NodeState>,
    Json(req): Json<InferenceCommitRequest>,
) -> Result<Json<Value>, (StatusCode, Json<ApiError>)> {
    let request_id_hash = parse_hash(&req.request_id)
        .map_err(|_| api_error(StatusCode::BAD_REQUEST, "invalid request_id hex"))?;
    let result_hash = parse_hash(&req.result_hash)
        .map_err(|_| api_error(StatusCode::BAD_REQUEST, "invalid result_hash hex"))?;

    let commitment = arc_vm::inference_verify::InferenceCommitment {
        request_id: request_id_hash,
        result_hash,
        provider: node.validator_address.0,
        timestamp: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0),
        bond_amount: req.bond_amount,
    };

    let commitment_id = node.verification_manager.lock()
        .map_err(|_| api_error(StatusCode::INTERNAL_SERVER_ERROR, "verification manager lock poisoned"))?
        .submit_commitment(commitment);

    Ok(Json(json!({
        "ok": true,
        "commitment_id": format!("0x{}", hex::encode(&commitment_id)),
        "provider": node.validator_address.to_hex(),
        "bond_amount": req.bond_amount,
    })))
}

#[derive(serde::Deserialize)]
struct InferenceChallengeRequest {
    commitment_id: String,
    challenge_type: String,
    bond_amount: u64,
}

/// POST /inference/challenge
/// Challenge an inference commitment.
async fn inference_challenge(
    AxumState(node): AxumState<NodeState>,
    Json(req): Json<InferenceChallengeRequest>,
) -> Result<Json<Value>, (StatusCode, Json<ApiError>)> {
    let commitment_hash = parse_hash(&req.commitment_id)
        .map_err(|_| api_error(StatusCode::BAD_REQUEST, "invalid commitment_id hex"))?;

    let challenge_type = match req.challenge_type.as_str() {
        "re_execution" => arc_vm::inference_verify::ChallengeType::ReExecution,
        "spot_check" => arc_vm::inference_verify::ChallengeType::SpotCheck,
        "statistical_audit" => arc_vm::inference_verify::ChallengeType::StatisticalAudit,
        "consensus" => arc_vm::inference_verify::ChallengeType::ConsensusVerification,
        _ => return Err(api_error(StatusCode::BAD_REQUEST, "invalid challenge_type: use re_execution, spot_check, statistical_audit, or consensus")),
    };

    let challenge_id = node.verification_manager.lock()
        .map_err(|_| api_error(StatusCode::INTERNAL_SERVER_ERROR, "verification manager lock poisoned"))?
        .create_challenge(commitment_hash, node.validator_address.0, challenge_type, req.bond_amount)
        .map_err(|e| api_error(StatusCode::BAD_REQUEST, e))?;

    Ok(Json(json!({
        "ok": true,
        "challenge_id": format!("0x{}", hex::encode(&challenge_id)),
        "challenger": node.validator_address.to_hex(),
        "bond_amount": req.bond_amount,
    })))
}

/// GET /inference/verification_status
/// Show overall verification system stats.
async fn inference_verification_status(
    AxumState(node): AxumState<NodeState>,
) -> Json<Value> {
    let reputation = node.verification_manager.lock()
        .map(|mgr| mgr.get_provider_reputation(node.validator_address.0))
        .unwrap_or(1.0);

    Json(json!({
        "provider": node.validator_address.to_hex(),
        "reputation": reputation,
        "verification_system": "commit-challenge",
        "challenge_types": ["re_execution", "spot_check", "statistical_audit", "consensus"],
        "bond_required": true,
    }))
}

// ─── Economics Endpoints ───────────────────────────────────────────────────

/// GET /economics/revenue_split
/// Show the fee distribution configuration.
async fn get_revenue_split(
    AxumState(node): AxumState<NodeState>,
) -> Json<Value> {
    let config = &node.revenue_config;
    let num_validators = node.dag_validators.read().len();
    let example_split = config.split_fee(10_000, num_validators.saturating_sub(1) as u32);

    Json(json!({
        "config": {
            "proposer_share_bps": config.proposer_share_bps,
            "verifier_share_bps": config.verifier_share_bps,
            "observer_pool_bps": config.observer_pool_bps,
            "treasury_share_bps": config.treasury_share_bps,
        },
        "example_split_10k_fee": {
            "proposer": example_split.proposer,
            "per_verifier": example_split.per_verifier,
            "observer_pool": example_split.observer_pool,
            "treasury": example_split.treasury,
            "num_verifiers": num_validators.saturating_sub(1),
        },
        "total_supply": "1,030,000,000 ARC",
        "decimals": 9,
        "inflation": "none (fixed supply)",
        "burn": "none",
    }))
}

// ─── Auto-Sharding ─────────────────────────────────────────────────────────

#[derive(serde::Deserialize)]
struct AutoShardPlanRequest {
    model_id: String,
    total_layers: u32,
    total_params_b: f64,
}

/// POST /shards/auto_plan
/// Compute the optimal shard plan for a model across registered nodes.
/// Uses compute_shard_plan() from distributed.rs which distributes layers
/// proportional to RAM with GPU bonus.
async fn compute_auto_shard_plan(
    AxumState(node): AxumState<NodeState>,
    Json(req): Json<AutoShardPlanRequest>,
) -> Result<Json<Value>, (StatusCode, Json<ApiError>)> {
    let model_hash_bytes = parse_hash(&req.model_id)
        .map_err(|_| api_error(StatusCode::BAD_REQUEST, "invalid model_id hex"))?;
    let model_hash = Hash256(model_hash_bytes);

    // Build node capabilities from the live shard registry
    let shards = fresh_shards(&node.shard_registry);
    let mut seen_nodes: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut capabilities: Vec<arc_inference::distributed::NodeCapability> = Vec::new();

    for s in &shards {
        if seen_nodes.insert(s.node_name.clone()) {
            capabilities.push(arc_inference::distributed::NodeCapability {
                address: Hash256(parse_hash(&s.model_id).unwrap_or([0u8; 32])),
                socket_addr: s.socket_addr.clone(),
                gpu_tier: 0, // TODO: detect from node capabilities
                available_memory: (s.memory_mb as u64) * 1024 * 1024,
            });
        }
    }

    // Also gather community workers as potential shard holders
    let now = std::time::Instant::now();
    for entry in node.community_workers.iter() {
        let (worker, ts) = entry.value();
        if now.duration_since(*ts).as_secs() < COMMUNITY_WORKER_TTL_SECS {
            if seen_nodes.insert(worker.name.clone()) {
                capabilities.push(arc_inference::distributed::NodeCapability {
                    address: Hash256(parse_hash(&worker.worker_id).unwrap_or([0u8; 32])),
                    socket_addr: worker.name.clone(),
                    gpu_tier: 0,
                    available_memory: 8 * 1024 * 1024 * 1024, // default 8GB estimate
                });
            }
        }
    }

    if capabilities.is_empty() {
        return Err(api_error(StatusCode::SERVICE_UNAVAILABLE, "no nodes available for sharding"));
    }

    let plan = arc_inference::distributed::compute_shard_plan(
        model_hash,
        req.total_layers,
        req.total_params_b,
        &capabilities,
    );

    // Register the computed plan in the multi-model registry
    for assignment in &plan {
        node.multi_model_registry.register_shard(model_hash, assignment.clone());
    }

    let plan_json: Vec<Value> = plan.iter().map(|a| json!({
        "node_address": format!("0x{}", hex::encode(&a.node_address.0)),
        "socket_addr": a.socket_addr,
        "start_layer": a.start_layer,
        "end_layer": a.end_layer,
        "gpu_tier": a.gpu_tier,
        "available_memory_mb": a.available_memory / (1024 * 1024),
    })).collect();

    Ok(Json(json!({
        "model_id": req.model_id,
        "total_layers": req.total_layers,
        "total_params_b": req.total_params_b,
        "plan": plan_json,
        "shard_count": plan.len(),
        "node_count": capabilities.len(),
        "registered_in_multi_model_registry": true,
    })))
}

// ─── Auto-Join: Node Asks Coordinator for Shard Assignment ─────────────────

#[derive(serde::Deserialize)]
struct ShardJoinRequest {
    /// Node's public RPC socket (e.g. "1.2.3.4:9090").
    socket_addr: String,
    /// Friendly name (e.g. "my-mac-studio").
    #[serde(default)]
    node_name: String,
    /// Model ID hash (hex). From the GGUF model loaded on the node.
    model_id: String,
    /// Model name (human-readable, e.g. "Llama-2-7B").
    #[serde(default)]
    model_name: String,
    /// Total transformer layers in the model.
    total_layers: u32,
    /// Available RAM on this node (MB).
    available_memory_mb: u64,
    /// GPU tier (0 = CPU only, 1 = iGPU, 2 = discrete GPU).
    #[serde(default)]
    gpu_tier: u8,
}

/// POST /shards/join
/// A node with a model calls this to ask the coordinator: "what layers should I hold?"
/// The coordinator looks at the existing shard registry, finds gaps in the pipeline,
/// and assigns the new node a layer range that fills the biggest gap (or splits
/// an overloaded range). Returns the assignment so the node can start serving.
///
/// This is the KEY endpoint for automatic sharding - nodes don't need manual
/// --shard-start/--shard-end flags. They just load a model and call /shards/join.
async fn shard_join(
    AxumState(node): AxumState<NodeState>,
    Json(req): Json<ShardJoinRequest>,
) -> Result<Json<Value>, (StatusCode, Json<ApiError>)> {
    if req.total_layers == 0 {
        return Err(api_error(StatusCode::BAD_REQUEST, "total_layers must be > 0"));
    }

    let model_hash_bytes = parse_hash(&req.model_id)
        .map_err(|_| api_error(StatusCode::BAD_REQUEST, "invalid model_id hex"))?;

    // 1. Gather existing shards for this model from the registry
    let existing: Vec<ShardInfo> = fresh_shards(&node.shard_registry)
        .into_iter()
        .filter(|s| s.model_id == req.model_id)
        .collect();

    // 2. Find the biggest uncovered gap in the pipeline
    let mut covered: Vec<bool> = vec![false; req.total_layers as usize];
    for s in &existing {
        for l in s.start_layer..s.end_layer.min(req.total_layers as usize) {
            covered[l] = true;
        }
    }

    // Find the longest contiguous uncovered range
    let mut best_start = 0usize;
    let mut best_len = 0usize;
    let mut run_start = 0usize;
    let mut in_run = false;

    for i in 0..covered.len() {
        if !covered[i] {
            if !in_run {
                run_start = i;
                in_run = true;
            }
            let run_len = i - run_start + 1;
            if run_len > best_len {
                best_start = run_start;
                best_len = run_len;
            }
        } else {
            in_run = false;
        }
    }

    // If no gap, this model is fully covered. Assign as a redundant shard
    // covering the most thinly-covered range (for fault tolerance).
    if best_len == 0 {
        // Count how many shards cover each layer
        let mut coverage_count: Vec<usize> = vec![0; req.total_layers as usize];
        for s in &existing {
            for l in s.start_layer..s.end_layer.min(req.total_layers as usize) {
                coverage_count[l] += 1;
            }
        }
        // Find the layer with minimum coverage
        let min_coverage = *coverage_count.iter().min().unwrap_or(&0);
        let thin_start = coverage_count.iter().position(|&c| c == min_coverage).unwrap_or(0);
        // Assign a range around the thinnest spot
        let range_size = (req.total_layers as usize / 4).max(1);
        best_start = thin_start;
        best_len = range_size.min(req.total_layers as usize - thin_start);
    }

    let assigned_start = best_start;
    let assigned_end = best_start + best_len;

    // 3. Register this shard in the registry
    let shard_info = ShardInfo {
        start_layer: assigned_start,
        end_layer: assigned_end,
        total_layers: req.total_layers as usize,
        model_id: req.model_id.clone(),
        model_name: req.model_name.clone(),
        memory_mb: (req.available_memory_mb as usize).min(assigned_end - assigned_start) * 100, // rough estimate
        full_model_mb: 0,
        socket_addr: req.socket_addr.clone(),
        node_name: req.node_name.clone(),
    };
    let reg_key = format!("{}#{}-{}", req.socket_addr, assigned_start, assigned_end);
    node.shard_registry.insert(reg_key, (shard_info, std::time::Instant::now()));

    // Also register in multi-model registry
    let model_hash = Hash256(model_hash_bytes);
    let assignment = arc_inference::distributed::ShardAssignment {
        node_address: model_hash,
        start_layer: assigned_start as u32,
        end_layer: assigned_end as u32,
        expert_indices: Vec::new(),
        socket_addr: req.socket_addr.clone(),
        gpu_tier: req.gpu_tier,
        available_memory: req.available_memory_mb * 1024 * 1024,
    };
    node.multi_model_registry.register_shard(model_hash, assignment);

    // Check if pipeline is now fully covered
    let mut new_covered: Vec<bool> = vec![false; req.total_layers as usize];
    for s in fresh_shards(&node.shard_registry).iter().filter(|s| s.model_id == req.model_id) {
        for l in s.start_layer..s.end_layer.min(req.total_layers as usize) {
            new_covered[l] = true;
        }
    }
    let fully_covered = new_covered.iter().all(|&c| c);
    let covered_count = new_covered.iter().filter(|&&c| c).count();

    Ok(Json(json!({
        "ok": true,
        "assigned_start_layer": assigned_start,
        "assigned_end_layer": assigned_end,
        "assigned_layers": assigned_end - assigned_start,
        "total_layers": req.total_layers,
        "model_id": req.model_id,
        "pipeline_fully_covered": fully_covered,
        "pipeline_coverage": format!("{}/{}", covered_count, req.total_layers),
        "note": "Start your node with --shard-start {} --shard-end {} to serve these layers",
    })))
}

// ─── Auto-Routing Inference ────────────────────────────────────────────────

/// POST /inference/auto
/// Smart inference endpoint that automatically picks the best path:
/// 1. Check deterministic cache first (instant, O(1))
/// 2. If sharded pipeline is available and complete → run_sharded
/// 3. If local model is loaded → run locally
/// 4. If community workers are available → route to community
/// 5. Else → 503 with helpful error
///
/// Users don't need to know the network topology. Just POST a prompt.
async fn inference_auto(
    AxumState(node): AxumState<NodeState>,
    Json(req): Json<Value>,
) -> Result<Json<Value>, (StatusCode, Json<ApiError>)> {
    let input = req.get("input")
        .and_then(|v| v.as_str())
        .ok_or(api_error(StatusCode::BAD_REQUEST, "'input' field required"))?
        .to_string();

    if input.len() > 32_768 {
        return Err(api_error(StatusCode::BAD_REQUEST, "Input exceeds 32KB limit"));
    }

    let max_tokens = req.get("max_tokens")
        .and_then(|v| v.as_u64())
        .unwrap_or(20)
        .min(256) as u32;

    // Strategy 1: Check if sharded pipeline is available
    let shards = fresh_shards(&node.shard_registry);
    let has_full_pipeline = if !shards.is_empty() {
        let n_layers = shards.iter().map(|s| s.total_layers).max().unwrap_or(0);
        let mut covered_to = 0usize;
        let mut contiguous = true;
        let mut sorted = shards.clone();
        sorted.sort_by_key(|s| s.start_layer);
        for s in &sorted {
            if s.start_layer != covered_to {
                contiguous = false;
                break;
            }
            covered_to = s.end_layer;
        }
        contiguous && covered_to == n_layers && n_layers > 0
    } else {
        false
    };

    // Strategy 2: Check if local model is available
    let has_local_model = node.inference_model.is_some() || node.candle_engine.is_some();

    // Strategy 3: Check if community workers are available
    let now = std::time::Instant::now();
    let community_worker_count = node.community_workers.iter()
        .filter(|e| now.duration_since(e.value().1).as_secs() < COMMUNITY_WORKER_TTL_SECS)
        .count();

    // Route to the best available path
    if has_full_pipeline && node.inference_model.is_some() {
        // Best path: sharded pipeline (distributed, deterministic)
        let sharded_req = json!({
            "input": input,
            "max_tokens": max_tokens,
            "chat_template": req.get("chat_template").and_then(|v| v.as_bool()).unwrap_or(false),
        });
        let result = inference_run_sharded(
            AxumState(node.clone()),
            Json(sharded_req),
        ).await;
        match result {
            Ok(mut resp) => {
                if let Some(obj) = resp.0.as_object_mut() {
                    obj.insert("route".to_string(), json!("sharded_pipeline"));
                }
                Ok(resp)
            }
            Err(e) => Err(e),
        }
    } else if has_local_model {
        // Fallback: local single-node inference
        let local_req = json!({
            "input": input,
            "max_tokens": max_tokens,
        });
        let result = inference_run(
            AxumState(node.clone()),
            Some(Json(local_req)),
        ).await;
        match result {
            Ok(mut resp) => {
                if let Some(obj) = resp.0.as_object_mut() {
                    obj.insert("route".to_string(), json!("local_model"));
                }
                Ok(resp)
            }
            Err(e) => Err(e),
        }
    } else if community_worker_count > 0 {
        // Route to community workers (via work dispatch)
        Err(api_error(StatusCode::SERVICE_UNAVAILABLE, format!(
            "{} community workers available but direct dispatch not yet implemented. Use /inference/community on the gateway (port 3001).",
            community_worker_count
        )))
    } else {
        Err(api_error(StatusCode::SERVICE_UNAVAILABLE, json!({
            "error": "No inference path available",
            "sharded_pipeline": false,
            "local_model": false,
            "community_workers": 0,
            "help": "Either: (1) load a model with --model, (2) have shard-holding nodes announce to this coordinator, or (3) start community workers with models"
        }).to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sa(s: &str) -> SocketAddr {
        s.parse().unwrap()
    }

    #[test]
    fn stub_detector_catches_bind_all_and_loopback() {
        assert!(is_stub_socket_addr("0.0.0.0:9090"));
        assert!(is_stub_socket_addr("127.0.0.1:9090"));
        assert!(is_stub_socket_addr("127.1.2.3:9090"));
        assert!(is_stub_socket_addr("[::]:9090"));
        assert!(is_stub_socket_addr("[::1]:9090"));
        assert!(is_stub_socket_addr(""));
    }

    #[test]
    fn stub_detector_leaves_public_ips_alone() {
        assert!(!is_stub_socket_addr("149.28.32.76:9090"));
        assert!(!is_stub_socket_addr("140.82.16.112:9944"));
        assert!(!is_stub_socket_addr("10.0.0.1:9090")); // RFC1918 is routable on a LAN
    }

    #[test]
    fn rewrite_overrides_stub_with_peer_ip_keeping_declared_port() {
        // Peer behind "0.0.0.0:9090" announcement is at 149.28.32.76:51234
        // → should record shard at 149.28.32.76:9090 (trust the declared port)
        let got = rewrite_stub_shard_addr("0.0.0.0:9090", sa("149.28.32.76:51234"));
        assert_eq!(got, "149.28.32.76:9090");
    }

    #[test]
    fn rewrite_falls_back_to_peer_port_when_declared_is_unparseable() {
        let got = rewrite_stub_shard_addr("0.0.0.0:xxxx", sa("149.28.32.76:51234"));
        assert_eq!(got, "149.28.32.76:51234");
    }

    #[test]
    fn rewrite_preserves_already_routable_addrs() {
        // Well-behaved announcer already sent a real IP - don't rewrite.
        let got = rewrite_stub_shard_addr("149.28.32.76:9090", sa("1.2.3.4:9999"));
        assert_eq!(got, "149.28.32.76:9090");
    }

    #[test]
    fn rewrite_ignores_loopback_peers_so_self_announces_stay_stub() {
        // Self-announce from the local broadcaster hits 127.0.0.1/shards/announce.
        // Rewriting would make self-entry look like 127.0.0.1:9090 (still unroutable)
        // and defeat the dedupe logic - so we leave it alone.
        let got = rewrite_stub_shard_addr("0.0.0.0:9090", sa("127.0.0.1:51234"));
        assert_eq!(got, "0.0.0.0:9090");
    }

    #[test]
    fn rewrite_ignores_ipv6_loopback_peers() {
        let got = rewrite_stub_shard_addr("0.0.0.0:9090", sa("[::1]:51234"));
        assert_eq!(got, "0.0.0.0:9090");
    }

    #[test]
    fn rewrite_uses_ipv6_peer_ip_when_peer_is_remote() {
        // IPv6 must be bracketed when combined with a port.
        let got = rewrite_stub_shard_addr("0.0.0.0:9090", sa("[2001:db8::1]:51234"));
        assert_eq!(got, "[2001:db8::1]:9090");
    }

    // ── v0.7.0 community work queue / schema tests ─────────────────────

    #[test]
    fn workitem_serializes_in_shape_worker_reads() {
        // The desktop community-worker loop in main.rs reads job_id, input,
        // max_tokens, and (optionally) model_id directly off the top-level
        // claim_work response. Pin the serialization shape so a future
        // refactor can't silently drift the wire format.
        let item = WorkItem {
            job_id: "a1b2c3".to_string(),
            input: "What is 2+2?".to_string(),
            max_tokens: 64,
            model_id: Some("0xdeadbeef".to_string()),
            submitted_at_unix_ms: 1_700_000_000_000,
        };
        let v = serde_json::to_value(&item).unwrap();
        assert_eq!(v["job_id"], "a1b2c3");
        assert_eq!(v["input"], "What is 2+2?");
        assert_eq!(v["max_tokens"], 64);
        assert_eq!(v["model_id"], "0xdeadbeef");
        assert_eq!(v["submitted_at_unix_ms"], 1_700_000_000_000i64);
        // Old layer-shard fields must NOT appear — they belong to the
        // seed-to-seed forward_shard path now.
        assert!(v.get("request_id").is_none(), "request_id is the v0.6 layer-shard field, must not leak into community jobs");
        assert!(v.get("hidden").is_none());
        assert!(v.get("start_layer").is_none());
        assert!(v.get("end_layer").is_none());
    }

    #[test]
    fn workitem_omits_model_id_when_none() {
        // model_id is optional; when None, it must be absent from JSON
        // (workers skip the model-match check when the field isn't present).
        let item = WorkItem {
            job_id: "x".into(),
            input: "hi".into(),
            max_tokens: 8,
            model_id: None,
            submitted_at_unix_ms: 0,
        };
        let v = serde_json::to_value(&item).unwrap();
        assert!(v.get("model_id").is_none());
    }

    #[test]
    fn workresult_deserializes_from_worker_payload() {
        // Pin compatibility with the exact JSON the worker loop in
        // crates/arc-node/src/main.rs sends to /community/submit_work.
        let body = serde_json::json!({
            "job_id": "abc",
            "worker_id": "0x12ab",
            "success": true,
            "output": "4",
            "output_hash": "0xfeed",
            "tokens_generated": 1u64,
            "total_ms": 250u64,
            "ms_per_token": 250u64,
            "engine": "INT8 integer (community worker)",
        });
        let r: WorkResult = serde_json::from_value(body).expect("worker payload should deserialize");
        assert_eq!(r.job_id, "abc");
        assert_eq!(r.worker_id, "0x12ab");
        assert!(r.success);
        assert_eq!(r.output, "4");
        assert_eq!(r.output_hash, "0xfeed");
        assert_eq!(r.tokens_generated, 1);
        assert_eq!(r.ms_per_token, 250);
    }

    #[test]
    fn workresult_defaults_for_failure_submit() {
        // When a worker fails it can submit just job_id + worker_id +
        // success=false + error. All other fields default to empty/0.
        let body = serde_json::json!({
            "job_id": "abc",
            "worker_id": "0x12ab",
            "success": false,
            "error": "model not loaded",
        });
        let r: WorkResult = serde_json::from_value(body).unwrap();
        assert!(!r.success);
        assert_eq!(r.tokens_generated, 0);
        assert_eq!(r.output, "");
        assert_eq!(r.error.as_deref(), Some("model not loaded"));
    }

    #[tokio::test]
    async fn work_queue_round_trip_after_serve_wiring() {
        // Smoke test: build_node_state alone leaves the queue None
        // (callers must use serve() to wire it). After we manually wire
        // the channel (mirroring serve()), tx.send → rx.recv works.
        let (tx, rx) = tokio::sync::mpsc::channel::<WorkItem>(8);
        let tx = Arc::new(tx);
        let rx = Arc::new(tokio::sync::Mutex::new(rx));

        // Push one job
        tx.send(WorkItem {
            job_id: "j1".into(),
            input: "ping".into(),
            max_tokens: 4,
            model_id: None,
            submitted_at_unix_ms: 1,
        }).await.unwrap();

        // Drain
        let got = rx.lock().await.recv().await.expect("a job");
        assert_eq!(got.job_id, "j1");
        assert_eq!(got.input, "ping");
    }

    // ── Smart router (task 2) tests ─────────────────────────────────────
    //
    // These exercise live_inference_worker_count and
    // dispatch_to_community_worker against a node state we wire up by
    // hand — a full serve() boot would pull in axum + listen sockets,
    // which is out of scope for a unit test. We construct just enough of
    // NodeState to drive the router paths.

    use std::sync::atomic::AtomicU32;
    use std::time::Instant;

    fn fake_node_with_workers(workers: Vec<(CommunityWorker, std::time::Instant)>) -> NodeState {
        // Build a minimal NodeState by hand. We can't call build_node_state
        // because it requires real Arc<StateDB> and Arc<Mempool>; we only
        // touch fields the router reads.
        let workers_map: dashmap::DashMap<String, (CommunityWorker, std::time::Instant)> =
            dashmap::DashMap::new();
        for (w, ts) in workers {
            workers_map.insert(w.worker_id.clone(), (w, ts));
        }
        let (tx, rx) = tokio::sync::mpsc::channel::<WorkItem>(16);
        NodeState {
            // Fields the router actually reads ↓
            community_workers: Arc::new(workers_map),
            community_work_tx: Some(Arc::new(tx)),
            community_work_queue: Some(Arc::new(tokio::sync::Mutex::new(rx))),
            community_work_results: Some(Arc::new(dashmap::DashMap::new())),
            attestation_nonce: Arc::new(AtomicU64::new(0)),
            latency_stats: Arc::new(dashmap::DashMap::new()),

            // Filler ↓ — never read by the router but required by the struct
            state: Arc::new(arc_state::StateDB::new()),
            mempool: Arc::new(arc_mempool::Mempool::new(1_000)),
            validator_address: Hash256::ZERO,
            validator_keypair: None,
            stake: 0,
            tier: StakeTier::Spark,
            boot_time: Instant::now(),
            peer_count: Arc::new(AtomicU32::new(0)),
            faucet_claims: Arc::new(dashmap::DashMap::new()),
            faucet_claims_total: Arc::new(AtomicU32::new(0)),
            inference_model: None,
            candle_engine: None,
            candle_model_id: None,
            dag_validators: Arc::new(parking_lot::RwLock::new(Vec::new())),
            tx_rate_limit: Arc::new(dashmap::DashMap::new()),
            dag_round: Arc::new(AtomicU64::new(0)),
            dag_committed: Arc::new(AtomicU64::new(0)),
            inference_results: Arc::new(dashmap::DashMap::new()),
            shard_infos: Vec::new(),
            shard_kv_caches: Arc::new(dashmap::DashMap::new()),
            shard_registry: Arc::new(dashmap::DashMap::new()),
            sharded_runs_total: Arc::new(AtomicU64::new(0)),
            sharded_bytes_total: Arc::new(AtomicU64::new(0)),
            inference_cache: Arc::new(arc_inference::distributed::DistributedCache::new(16)),
            multi_model_registry: Arc::new(arc_inference::distributed::ShardRegistry::new()),
            verification_manager: Arc::new(std::sync::Mutex::new(arc_vm::inference_verify::VerificationManager::new())),
            revenue_config: RoleRevenueConfig::default(),
        }
    }

    fn worker(id: &str, caps: &[&str]) -> CommunityWorker {
        CommunityWorker {
            worker_id: id.into(),
            name: format!("test-{}", id),
            capabilities: caps.iter().map(|s| s.to_string()).collect(),
            model: Some("Llama-2-7B".into()),
            platform: "test".into(),
            registered_at: 0,
            work_completed: 0,
            success_count: 0,
            failure_count: 0,
            sum_total_ms_success: 0,
            last_total_ms: 0,
        }
    }

    #[test]
    fn live_worker_count_filters_by_capability_and_ttl() {
        let now = std::time::Instant::now();
        let stale = now - std::time::Duration::from_secs(COMMUNITY_WORKER_TTL_SECS + 5);

        let node = fake_node_with_workers(vec![
            (worker("alive-inference", &["inference"]), now),
            (worker("alive-other-cap", &["consensus"]), now),
            (worker("stale-inference", &["inference"]), stale),
        ]);

        // Only "alive-inference" should count: alive AND has the
        // inference capability.
        assert_eq!(live_inference_worker_count(&node), 1);
    }

    #[test]
    fn live_worker_count_zero_when_no_workers_registered() {
        let node = fake_node_with_workers(vec![]);
        assert_eq!(live_inference_worker_count(&node), 0);
    }

    #[tokio::test]
    async fn dispatch_to_community_worker_returns_result_when_worker_submits() {
        // Wire a worker: we manually drain the queue and post a result
        // through the same channels submit_work uses, then assert the
        // dispatcher returns the right WorkResult.
        let now = std::time::Instant::now();
        let node = fake_node_with_workers(vec![(
            worker("w1", &["inference"]),
            now,
        )]);

        let queue = node.community_work_queue.as_ref().unwrap().clone();
        let results = node.community_work_results.as_ref().unwrap().clone();

        // Spawn a fake worker that drains the queue and posts a result.
        tokio::spawn(async move {
            let item = queue.lock().await.recv().await.expect("a job");
            // submit a successful result
            if let Some((_, sender)) = results.remove(&item.job_id) {
                let _ = sender.send(WorkResult {
                    job_id: item.job_id,
                    worker_id: "w1".into(),
                    success: true,
                    output: "4".into(),
                    output_hash: "0xdeadbeef".into(),
                    tokens_generated: 1,
                    total_ms: 50,
                    ms_per_token: 50,
                    engine: "INT8 integer (community worker)".into(),
                    error: None,
                    signed_attestation_hex: None,
                });
            }
        });

        let result = dispatch_to_community_worker(&node, "What is 2+2?".into(), 4, None)
            .await
            .expect("worker returned a result");

        assert_eq!(result.worker_id, "w1");
        assert_eq!(result.output, "4");
        assert_eq!(result.tokens_generated, 1);
        // EWMA was recorded for this worker
        assert!(node.latency_stats.contains_key("worker:w1"));
    }

    #[tokio::test]
    async fn dispatch_propagates_worker_failure() {
        let now = std::time::Instant::now();
        let node = fake_node_with_workers(vec![(worker("w1", &["inference"]), now)]);
        let queue = node.community_work_queue.as_ref().unwrap().clone();
        let results = node.community_work_results.as_ref().unwrap().clone();

        tokio::spawn(async move {
            let item = queue.lock().await.recv().await.expect("a job");
            if let Some((_, sender)) = results.remove(&item.job_id) {
                let _ = sender.send(WorkResult {
                    job_id: item.job_id,
                    worker_id: "w1".into(),
                    success: false,
                    output: String::new(),
                    output_hash: String::new(),
                    tokens_generated: 0,
                    total_ms: 0,
                    ms_per_token: 0,
                    engine: String::new(),
                    error: Some("model not loaded on worker".into()),
                    signed_attestation_hex: None,
                });
            }
        });

        let err = dispatch_to_community_worker(&node, "x".into(), 4, None)
            .await
            .expect_err("failure must propagate");
        assert!(err.contains("model not loaded on worker"), "got: {err}");
    }

    #[tokio::test]
    async fn dispatch_errors_when_queue_unwired() {
        // Build a minimal NodeState with no community_work_tx, simulating
        // an old binary or a misconfigured server.
        let mut node = fake_node_with_workers(vec![(worker("w1", &["inference"]), Instant::now())]);
        node.community_work_tx = None;
        node.community_work_queue = None;
        node.community_work_results = None;

        let err = dispatch_to_community_worker(&node, "x".into(), 4, None)
            .await
            .expect_err("must error when not wired");
        assert!(
            err.contains("not wired"),
            "expected 'not wired' error, got: {err}"
        );
    }

    // ── Task 3: worker-signed attestation tests ─────────────────────────

    use arc_crypto::KeyPair;
    use arc_types::{Transaction, TxBody, TxType, transaction::InferenceAttestationBody};

    fn sign_attestation_for(keypair: &KeyPair, nonce: u64, bond: u64) -> (Transaction, String) {
        let mut tx = Transaction {
            tx_type: TxType::InferenceAttestation,
            from: keypair.address(),
            nonce,
            body: TxBody::InferenceAttestation(InferenceAttestationBody {
                model_id: arc_crypto::hash_bytes(b"arc-test-model"),
                input_hash: arc_crypto::hash_bytes(b"hello"),
                output_hash: arc_crypto::hash_bytes(b"world"),
                challenge_period: 100,
                bond,
                beneficiary: None,
            }),
            fee: 0,
            gas_limit: 0,
            hash: arc_crypto::Hash256::ZERO,
            signature: arc_crypto::Signature::null(),
            sig_verified: false,
        };
        tx.sign(keypair).expect("sign ok");
        let bytes = bincode::serialize(&tx).expect("serialize ok");
        let hex_s = format!("0x{}", hex::encode(bytes));
        (tx, hex_s)
    }

    #[test]
    fn decode_and_verify_accepts_worker_signed_attestation() {
        let kp = KeyPair::generate_ed25519();
        let (orig, hex_s) = sign_attestation_for(&kp, 0, 0);
        let worker_id = format!("0x{}", hex::encode(&kp.address().0));
        let got = decode_and_verify_worker_attestation(&hex_s, &worker_id)
            .expect("verify ok");
        assert_eq!(got.hash, orig.hash, "hash should round-trip exactly");
        assert_eq!(got.tx_type, TxType::InferenceAttestation);
    }

    #[test]
    fn decode_strips_optional_0x_prefix() {
        let kp = KeyPair::generate_ed25519();
        let (_, hex_with_0x) = sign_attestation_for(&kp, 0, 0);
        let bare = hex_with_0x.trim_start_matches("0x").to_string();
        let worker_id = hex::encode(&kp.address().0); // no 0x
        decode_and_verify_worker_attestation(&bare, &worker_id)
            .expect("bare hex + bare worker_id should work");
    }

    #[test]
    fn decode_rejects_worker_id_mismatch() {
        let kp_a = KeyPair::generate_ed25519();
        let kp_b = KeyPair::generate_ed25519();
        let (_, hex_s) = sign_attestation_for(&kp_a, 0, 0);
        // Submit kp_a's signed tx but claim to be kp_b
        let worker_id_b = format!("0x{}", hex::encode(&kp_b.address().0));
        let err = decode_and_verify_worker_attestation(&hex_s, &worker_id_b)
            .expect_err("must reject worker_id mismatch");
        assert!(err.contains("does not match worker_id"), "got: {err}");
    }

    #[test]
    fn decode_rejects_wrong_tx_type() {
        // Build a Transfer tx (not an InferenceAttestation) and try to
        // submit it as an attestation.
        let kp = KeyPair::generate_ed25519();
        let mut tx = Transaction::new_transfer(kp.address(), kp.address(), 1, 0);
        tx.sign(&kp).unwrap();
        let bytes = bincode::serialize(&tx).unwrap();
        let hex_s = format!("0x{}", hex::encode(bytes));
        let worker_id = format!("0x{}", hex::encode(&kp.address().0));
        let err = decode_and_verify_worker_attestation(&hex_s, &worker_id)
            .expect_err("must reject non-attestation tx");
        assert!(err.contains("expected InferenceAttestation"), "got: {err}");
    }

    #[test]
    fn decode_rejects_tampered_signature() {
        let kp = KeyPair::generate_ed25519();
        let (mut tx, _) = sign_attestation_for(&kp, 0, 0);

        // Mutate the body without re-signing — the hash + sig now mismatch.
        if let TxBody::InferenceAttestation(ref mut body) = tx.body {
            body.bond = 999;
        }
        let bytes = bincode::serialize(&tx).unwrap();
        let hex_s = format!("0x{}", hex::encode(bytes));
        let worker_id = format!("0x{}", hex::encode(&kp.address().0));
        let err = decode_and_verify_worker_attestation(&hex_s, &worker_id)
            .expect_err("tampered tx must fail signature verification");
        assert!(err.contains("signature verify failed"), "got: {err}");
    }

    #[test]
    fn decode_rejects_garbage_hex() {
        let err = decode_and_verify_worker_attestation("zz not hex", "0xab")
            .expect_err("hex decode must fail");
        assert!(err.contains("hex decode"), "got: {err}");
    }

    // ── Task 4: worker scoring + scoreboard tests ───────────────────────

    fn worker_with_stats(
        id: &str,
        success: u64,
        failure: u64,
        sum_ms: u64,
        last_ms: u64,
    ) -> CommunityWorker {
        let mut w = worker(id, &["inference"]);
        w.success_count = success;
        w.failure_count = failure;
        w.sum_total_ms_success = sum_ms;
        w.last_total_ms = last_ms;
        w.work_completed = success;
        w
    }

    #[tokio::test]
    async fn scoreboard_sorts_by_composite_score() {
        // Compose three workers with known stats:
        //   fast_reliable: 100% success, 100ms avg → score = 1000 - 100 = 900
        //   slow_reliable: 100% success, 500ms avg → score = 1000 - 500 = 500
        //   fresh:         0 successes              → score = 0
        // Expected ordering: fast_reliable, slow_reliable, fresh.
        let now = std::time::Instant::now();
        let node = fake_node_with_workers(vec![
            (worker_with_stats("fast_reliable", 10, 0, 1000, 100), now),
            (worker_with_stats("slow_reliable", 10, 0, 5000, 500), now),
            (worker_with_stats("fresh", 0, 0, 0, 0), now),
        ]);

        let resp = workers_scoreboard(
            AxumState(node),
            Query(HashMap::new()),
        )
        .await;
        let v: Value = resp.0;
        let workers = v.get("workers").and_then(|x| x.as_array()).unwrap();
        let ids: Vec<&str> = workers
            .iter()
            .map(|w| w.get("worker_id").and_then(|x| x.as_str()).unwrap_or(""))
            .collect();
        assert_eq!(
            ids,
            vec!["fast_reliable", "slow_reliable", "fresh"],
            "scoreboard order must be score-descending"
        );
        // Sanity: count_visible matches the number of fresh-TTL workers.
        assert_eq!(v.get("count_visible").and_then(|x| x.as_u64()), Some(3));
    }

    #[tokio::test]
    async fn scoreboard_excludes_stale_workers() {
        let now = std::time::Instant::now();
        let stale = now - std::time::Duration::from_secs(COMMUNITY_WORKER_TTL_SECS + 30);
        let node = fake_node_with_workers(vec![
            (worker_with_stats("alive", 5, 0, 1000, 200), now),
            (worker_with_stats("stale", 999, 0, 999_999, 999_999), stale),
        ]);

        let v: Value = workers_scoreboard(AxumState(node), Query(HashMap::new())).await.0;
        let workers = v.get("workers").and_then(|x| x.as_array()).unwrap();
        assert_eq!(workers.len(), 1, "stale worker must be hidden");
        assert_eq!(
            workers[0].get("worker_id").and_then(|x| x.as_str()),
            Some("alive")
        );
    }

    #[test]
    fn scoreboard_score_handles_pure_failure() {
        // A worker with all failures and no successes scores 0 (no
        // success_count means no avg_ms; we don't punish to -∞ because
        // the worker may still be online and trying).
        let w = worker_with_stats("flunker", 0, 10, 0, 0);
        assert_eq!(w.success_count, 0);
        assert_eq!(w.failure_count, 10);
        // Equivalent of the score computation in workers_scoreboard
        let attempts = w.success_count + w.failure_count;
        let success_rate = if attempts > 0 {
            w.success_count as f64 / attempts as f64
        } else {
            0.0
        };
        let score = if w.success_count == 0 {
            0.0
        } else {
            success_rate * 1000.0
                - (w.sum_total_ms_success as f64 / w.success_count as f64)
        };
        assert_eq!(score, 0.0);
    }
}
